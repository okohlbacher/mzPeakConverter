# Native integer-TOF reader for Bruker TDF — design (mzdata-upstream-friendly)

Status: DRAFT v0.1, 2026-06-21. Resolves Blocker-1 (ims-compact "lossless" requires native
integer TOF, which mzdata 0.64 does not expose).

## Problem

mzdata's TDF path converts every `u32` TOF index to `f64` m/z in the hot loop
(`io/tdf/arrays.rs` `process_3d_slice`) and discards the integer; intensities are read as
`u32` at source but surfaced as `f32`. The calibration (`timsrust::Tof2MzConverter`) sits in
a private `Metadata`. So a lossless integer-TOF + delta encoder cannot get its inputs from
mzdata. BRFP works around this by *deriving* `tof = round((sqrt(mz)-a)/b)` from converted
m/z — which is lossless only relative to the formula, not to the raw instrument bins.

## Decision

**Vendor (fork) mzdata** as a path/patch dependency and add a small, PR-shaped capability to
read native integer TOF + native integer intensity + the calibration model. Design the
converter side against a capability trait so that when the change is upstreamed, the converter
code is unchanged.

## Upstream-shaped mzdata diff (kept minimal, 3 pieces)

1. **New array type**: `ArrayType::RawTimeOfFlight` (u32) — a CV-tagged auxiliary array
   alongside the existing `MZArray`/`IntensityArray`. (Intensity stays the existing
   `IntensityArray` but readable at native `u32`; add `ArrayType` dtype honoring rather than
   forced `f32` — see piece 2.)
2. **Reader mode flag**: `TDFSpectrumReaderBuilder::with_raw_tof(bool)` (default false). When
   true, `process_3d_slice` *additionally* pushes the raw `u32` tof and native-dtype intensity
   into the array map instead of discarding the integer. Zero cost when off.
3. **Public calibration accessor**: `TDFReader::tof_mz_model() -> TofMzModel { a, b, model,
   coeffs }` exposing the otherwise-private `Tof2MzConverter` coefficients (read-only).

These are independent, backward-compatible additions — good upstream PR candidates. Until
merged, carry as `[patch.crates-io] mzdata = { git = "...", branch = "mzpc-raw-tof" }`
(same mechanism BRFP already uses for its TDF fix).

## Converter-side capability trait (the upstream seam)

```rust
/// Implemented by readers that can yield raw vendor TOF coordinates.
/// mzdata's TDF reader implements this once the vendor patch lands;
/// if/when upstream mzdata exposes native TOF, the impl just delegates —
/// no converter code changes.
pub trait NativeTofSource {
    fn tof_mz_model(&self) -> TofMzModel;          // a, b, model id, coeffs
    /// One TIMS frame: per-scan offsets define mobility groups.
    fn next_raw_frame(&mut self) -> Option<RawFrame>;
}

pub struct RawFrame {
    pub spectrum_index: u32,
    pub tof:        Vec<u32>,   // native flight-time bin indices
    pub intensity:  Vec<u32>,   // native detector counts (NOT f32)
    pub scan_offsets: Vec<usize>, // boundaries → mobility scans
    pub mobility:   Vec<f64>,   // 1/K0 per scan (from im_converter)
}

pub struct TofMzModel { pub a: f64, pub b: f64, pub model: String, pub coeffs: Vec<f64> }
```

The generic path stays on mzdata's `SpectrumReader`. ims-compact uses `NativeTofSource` when
the reader advertises it (capability detection); otherwise it refuses or falls back to a
clearly-labeled *derived* (non-lossless) mode — never silently lossy.

## Encoder wiring (ims-compact, ported from BRFP)

- Use `RawFrame.tof` directly (no `round((sqrt(mz)-a)/b)`), per-mobility-scan delta-reset
  encoding; `intensity` as native `u32`; `mobility` f64. Write `TofMzModel` into the Parquet
  KV `ims_calibration` blob + the per-spectrum fine-cal sidecar (`.frames.parquet`).
- Columns/codecs unchanged from BRFP: `tof`/`intensity` BYTE_STREAM_SPLIT, `spectrum_index`
  DELTA, `mobility` RLE-dict, zstd (default 3–6).

## Losslessness test (closes the Blocker-1 critique)

Roundtrip asserts decoded `tof` equals the **native** `tof` read by an independent pass
(timsrust `FrameReader` directly), not the derived value. Same for intensity. m/z is
reconstructed `(a+b·tof)²` and compared to mzdata's converted m/z only as a sanity bound.

## Scope guards

- Feature-gated `bruker_native_tof`; only the TDF path. BAF/TSF/Thermo untouched.
- `--consolidate-ms2` remains explicitly lossy and must be recorded in metadata.
- If the vendor patch is unavailable at build, ims-compact compiles out (capability absent).

## Open questions for review

- Is a new `ArrayType::RawTimeOfFlight` the cleanest upstream shape, or should raw tof ride as
  a typed auxiliary array keyed by CV term? (Affects upstream acceptance.)
- Should `with_raw_tof` keep emitting converted m/z too (double memory) or raw-only when on?
- timsrust version pinning: BRFP pins `=0.4.1`; confirm the frame fields are stable across the
  mzdata version we vendor.

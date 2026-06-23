//! Pure-Rust FILE-DIRECT reader for Agilent Q-TOF **profile** `.d` (`AcqData/MSProfile.bin`).
//!
//! MassHunter stores a profile spectrum as a *uniform raw time-of-flight grid*: a per-spectrum
//! 16-byte header `(mz_min : f64, delta : f64)` defines the raw-TOF axis `t = mz_min + k·delta`
//! for integer bin ordinal `k`, followed by **integer** detector-count intensities (0x90 RLE for
//! Q-TOF, or LZF for the older HRMS path). The run-wide mass calibration in `MSMassCal.bin` is the
//! traditional quadratic `m/z = (coeff·(t − base))²`.
//!
//! Substituting `t = mz_min + k·delta` collapses to the converter's `tof_grid` form
//! `sqrt(m/z) = c0 + c1·k` with a CLOSED FORM — no lattice fitting:
//!   * `c0 = coeff·(mz_min − base)`
//!   * `c1 = coeff·delta`
//! (verified exact to floating-point noise against a real `.d`'s calibration, ~6.5e-10 ppm). So the
//! reader yields, per spectrum, `tof_index = k` (Int32), the **integer** intensity, and a
//! `TofGrid{c0,c1}` read straight from the file — fed through the existing `convert_file_tof_grid`
//! peak facet, bypassing MHDAC (which expands the grid to f64 and inflates the file ~4×).
//!
//! This is the lossless `tof_index` (Int32, DELTA_BINARY_PACKED) + per-run `{c0,c1}` encoding that
//! is ≈0.14× the msconvert lane and ≈0.56× the vendor `.d` on profile Q-TOF data (see
//! `AGILENT_FILEDIRECT.md`). It applies ONLY to profile acquisitions; centroid-only `.d`
//! (`MSProfile.bin` 0 bytes — all data in `MSPeak.bin`) is not griddable and is left to the
//! standard path.
//!
//! Reference: `evanyeyeye/rainbow` `rainbow/agilent/masshunter.py`; the byte layout, the RLE codec,
//! and the closed-form mapping were validated byte-exact in the spike (`spike/agilent/`).

use std::collections::HashMap;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};

use crate::tof_grid::TofGrid;

/// True for an Agilent `.d` whose `AcqData/MSProfile.bin` exists and is non-empty (profile mode).
/// Centroid-only `.d` write a 0-byte `MSProfile.bin` (all data in `MSPeak.bin`) → false.
pub fn has_profile(input: &Path) -> bool {
    let p = input.join("AcqData").join("MSProfile.bin");
    fs::metadata(&p).map(|m| m.len() > 0).unwrap_or(false)
}

/// One profile spectrum read straight from `MSProfile.bin`: the integer flight-time bin ordinals,
/// their integer intensities, the per-scan TOF grid (`sqrt(m/z)=c0+c1·k`), and the retention time.
pub struct ProfileSpectrum {
    /// Bin ordinal `k` for each stored (non-trailing-zero) point.
    pub tof_index: Vec<i32>,
    /// Integer detector-count intensity for each stored point (parallel to `tof_index`).
    pub intensity: Vec<u32>,
    /// Per-scan grid: `m/z = (c0 + c1·k)²`.
    pub grid: TofGrid,
    /// Retention time, minutes.
    pub scan_time: f64,
    /// 0-based scan index in the run.
    pub index: usize,
    /// MS level (1 for survey, 2 for product-ion scans); read from MSScan.bin when present.
    pub ms_level: u8,
}

/// A parsed Agilent profile `.d`, ready to iterate spectra. Holds the open `MSProfile.bin` handle
/// and the per-scan index/calibration so spectra are decoded lazily (one segment at a time).
pub struct AgilentProfileReader {
    profile: fs::File,
    profile_size: u64,
    /// Per-scan record metadata (offset/len/point-count/calibration into MSProfile.bin).
    scans: Vec<ScanInfo>,
    /// Per-scan calibration row: 10 doubles `[coeff, base, left, right, c0..c5]`.
    calib: Vec<[f64; 10]>,
    /// CalibrationID → polynomial ValueUseFlags (DefaultMassCal.xml); empty ⇒ no refinement.
    poly_flags: HashMap<i32, u32>,
    /// Index of the next scan to yield.
    cursor: usize,
}

struct ScanInfo {
    scan_time: f64,
    ms_level: u8,
    calibration_id: i32,
    /// Offset of the profile segment in MSProfile.bin.
    offset: u64,
    /// Compressed/stored byte length of the segment.
    byte_count: i64,
    /// Number of mz/intensity points (grid width).
    point_count: i64,
}

impl AgilentProfileReader {
    /// Open an Agilent profile `.d` (folder containing `AcqData/MSProfile.bin`).
    pub fn open(input: &Path) -> Result<Self> {
        let acq = input.join("AcqData");
        let xsd = acq.join("MSScan.xsd");
        let bin = acq.join("MSScan.bin");
        let profile_path = acq.join("MSProfile.bin");

        let schema = ScanSchema::parse(&xsd).with_context(|| format!("parsing {}", xsd.display()))?;
        let count_hint = count_scans(&acq);
        let scans = read_scan_records(&bin, &schema, count_hint)
            .with_context(|| format!("reading {}", bin.display()))?;
        if scans.is_empty() {
            bail!("Agilent profile reader: no scan records in {}", bin.display());
        }

        let calibration_ids: Vec<i32> = scans.iter().map(|s| s.calibration_id).collect();
        let (calib, poly_flags) = load_calibration(&acq, &calibration_ids)
            .with_context(|| format!("loading calibration from {}", acq.display()))?;

        let profile = fs::File::open(&profile_path)
            .with_context(|| format!("opening {}", profile_path.display()))?;
        let profile_size = profile.metadata()?.len();

        Ok(Self { profile, profile_size, scans, calib, poly_flags, cursor: 0 })
    }

    /// Number of scan records (an upper bound on the spectra yielded — truncated/empty segments are
    /// skipped during iteration).
    pub fn len(&self) -> usize {
        self.scans.len()
    }

    /// Decode the next non-empty profile spectrum, or `None` at end of run. Skips scans whose
    /// segment was never (fully) written (interrupted acquisition) or that have no profile points.
    pub fn next_spectrum(&mut self) -> Result<Option<ProfileSpectrum>> {
        while self.cursor < self.scans.len() {
            let i = self.cursor;
            self.cursor += 1;
            let info = &self.scans[i];
            // Skip truncated / empty / non-profile segments.
            if info.point_count <= 0 || info.byte_count <= 0 {
                continue;
            }
            if info.offset + info.byte_count as u64 > self.profile_size {
                // An interrupted acquisition: the rest of the run is unwritten — stop.
                break;
            }
            let n = info.point_count as usize;
            let mut seg = vec![0u8; info.byte_count as usize];
            self.profile.seek(SeekFrom::Start(info.offset))?;
            self.profile.read_exact(&mut seg)?;

            let (mz_min, delta, intens) = decode_segment(&seg, n)
                .with_context(|| format!("decoding MSProfile.bin segment for scan {i}"))?;

            // Closed-form Agilent → tof_grid mapping (no fit): m/z=(coeff·(t−base))², t=mz_min+k·delta
            // ⇒ sqrt(m/z) = coeff·(mz_min−base) + coeff·delta·k = c0 + c1·k.
            let row = &self.calib[i];
            let (coeff, base) = (row[0], row[1]);
            let grid = TofGrid { c0: coeff * (mz_min - base), c1: coeff * delta };

            // Keep only non-zero intensities: the RLE/LZF stream materializes a dense intensity
            // vector (zeros included); we store a sparse point list (tof_index, intensity), so drop
            // the zeros (and any trailing zeros the codec already omitted). c1 may be negative for a
            // descending TOF axis — keep k as-is (Int32+ΔBP store signed fine), reconstruction squares.
            let mut tof_index = Vec::new();
            let mut intensity = Vec::new();
            for (k, &v) in intens.iter().enumerate() {
                if v != 0 {
                    tof_index.push(k as i32);
                    intensity.push(v);
                }
            }
            if tof_index.is_empty() {
                continue;
            }
            return Ok(Some(ProfileSpectrum {
                tof_index,
                intensity,
                grid,
                scan_time: info.scan_time,
                index: i,
                ms_level: info.ms_level,
            }));
        }
        Ok(None)
    }

    /// The polynomial ValueUseFlags for scan `i`'s calibration (0 ⇒ traditional only). Exposed so
    /// the converter can report whether a sub-ppm polynomial refinement is present (the 2-coeff grid
    /// captures the traditional quadratic; the optional refinement rides the ppm gate).
    pub fn poly_flags_for(&self, i: usize) -> u32 {
        self.scans
            .get(i)
            .and_then(|s| self.poly_flags.get(&s.calibration_id).copied())
            .unwrap_or(0)
    }

    /// Calibration row `[coeff, base, left, right, c0..c5]` for scan `i` (for the polynomial-refined
    /// reference m/z used in the lossless check).
    pub fn calib_row(&self, i: usize) -> Option<&[f64; 10]> {
        self.calib.get(i)
    }
}

/// Reconstruct the polynomial-refined reference m/z for one raw-TOF value, exactly as MassHunter /
/// rainbow's `calibrate_mz`: `m/z = (coeff·(t−base))² − poly(clip(t, left, right))`. With
/// `use_flags == 0` this is just the traditional quadratic. Used to bound the grid's ppm error
/// against the vendor-faithful m/z (the grid captures only the quadratic; this adds the sub-ppm
/// polynomial term, so comparing against it is the honest lossless check).
pub fn calibrated_mz(row: &[f64; 10], use_flags: u32, t: f64) -> f64 {
    let (coeff, base, left, right) = (row[0], row[1], row[2], row[3]);
    let mz = (coeff * (t - base)).powi(2);
    if use_flags == 0 {
        return mz;
    }
    // Coefficients fill the orders whose bits are set in use_flags, ascending.
    let coeffs = &row[4..10];
    let mut poly: Vec<f64> = Vec::new();
    let mut ci = 0usize;
    for k in 0..32 {
        if use_flags >> k & 1 == 1 {
            while poly.len() <= k {
                poly.push(0.0);
            }
            if ci < coeffs.len() {
                poly[k] = coeffs[ci];
                ci += 1;
            }
        }
    }
    if poly.is_empty() {
        return mz;
    }
    let tc = t.clamp(left, right);
    // Horner on ascending-order coefficients.
    let mut corr = 0.0;
    for &c in poly.iter().rev() {
        corr = corr * tc + c;
    }
    mz - corr
}

// ───────────────────────── MSProfile.bin segment decode ─────────────────────────

/// Decode one MSProfile.bin segment → `(mz_min, delta, intensities[num_mz])`. The 16-byte header is
/// two f64 `(mz_min, delta)`; the body is either 0x90 RLE (Q-TOF) or LZF (HRMS). Detection mirrors
/// rainbow's `segment_is_rle` (self-validating: the RLE marker word must equal `(num_mz | 0x90<<24)`).
fn decode_segment(seg: &[u8], num_mz: usize) -> Result<(f64, f64, Vec<u32>)> {
    if seg.len() < 16 {
        bail!("MSProfile.bin segment too short ({} bytes)", seg.len());
    }
    if segment_is_rle(seg, num_mz) {
        let mz_min = f64::from_le_bytes(seg[0..8].try_into().unwrap());
        let delta = f64::from_le_bytes(seg[8..16].try_into().unwrap());
        let intens = decompress_rle(&seg[16..], num_mz)?;
        Ok((mz_min, delta, intens))
    } else {
        // LZF: the whole segment (header included) is LZF-compressed. After decompression the first
        // 16 bytes are the (mz_min, delta) header and the rest are num_mz contiguous u32.
        let decomp = lzf_decompress(seg, 16 + num_mz * 4)
            .context("LZF-decompressing MSProfile.bin segment")?;
        if decomp.len() < 16 + num_mz * 4 {
            bail!(
                "LZF segment decompressed to {} bytes, expected ≥ {}",
                decomp.len(),
                16 + num_mz * 4
            );
        }
        let mz_min = f64::from_le_bytes(decomp[0..8].try_into().unwrap());
        let delta = f64::from_le_bytes(decomp[8..16].try_into().unwrap());
        let mut intens = vec![0u32; num_mz];
        for (j, slot) in intens.iter_mut().enumerate() {
            let o = 16 + j * 4;
            *slot = u32::from_le_bytes(decomp[o..o + 4].try_into().unwrap());
        }
        Ok((mz_min, delta, intens))
    }
}

/// Self-validating RLE detection (rainbow `segment_is_rle`): after the 16-byte header, the 4-byte
/// marker word's low 3 bytes are the point count and its high byte is a fixed `0x90`.
fn segment_is_rle(seg: &[u8], num_mz: usize) -> bool {
    if seg.len() < 20 {
        return false;
    }
    let header = u32::from_le_bytes(seg[16..20].try_into().unwrap());
    (header & 0x00FF_FFFF) as usize == num_mz && (header >> 24) == 0x90
}

/// Decode the 0x90 run-length intensity stream (rainbow `decompress_inten_list`). `body` is the
/// segment bytes AFTER the 16-byte header. Layout:
///   * `[0..4]`  : marker word (point count | 0x90<<24) — already validated, skipped here.
///   * `[4..8]`  : i32 = -(initial leading-zero run).
///   * `[8..12]` : i32 = -(initial width flag ∈ {1,2,3,4} → {1,2,4,8}-byte signed int).
///   * values at the current width: `≥0` literal intensity; `<0` → `divmod(-v, 4)` = (zero-run, new
///     width flag). Trailing zeros are not stored (output pre-filled to `num_mz`).
fn decompress_rle(body: &[u8], num_mz: usize) -> Result<Vec<u32>> {
    if body.len() < 12 {
        bail!("MSProfile.bin RLE body too short ({} bytes)", body.len());
    }
    let init_zero = i32::from_le_bytes(body[4..8].try_into().unwrap());
    let init_width = i32::from_le_bytes(body[8..12].try_into().unwrap());
    let mut cur_idx: i64 = -(init_zero as i64);
    let mut width_flag = -(init_width as i64);
    if cur_idx < 0 {
        bail!("Malformed MSProfile.bin RLE segment: negative initial index");
    }

    let mut out = vec![0u32; num_mz];
    let mut off = 12usize;
    let end = body.len();
    let mut cur_size = width_size(width_flag)?;
    while off < end {
        if off + cur_size > end {
            bail!("Malformed MSProfile.bin RLE segment: truncated value");
        }
        let value: i64 = match cur_size {
            1 => body[off] as i8 as i64,
            2 => i16::from_le_bytes(body[off..off + 2].try_into().unwrap()) as i64,
            4 => i32::from_le_bytes(body[off..off + 4].try_into().unwrap()) as i64,
            8 => i64::from_le_bytes(body[off..off + 8].try_into().unwrap()),
            _ => unreachable!(),
        };
        off += cur_size;
        if value >= 0 {
            let idx = cur_idx as usize;
            if idx >= num_mz {
                bail!("Malformed MSProfile.bin RLE segment: index {idx} ≥ {num_mz}");
            }
            // Intensities are non-negative detector counts; widths up to 8 bytes are supported, but
            // an mzPeak intensity column is u32 — clamp the (vanishingly rare) >u32 count.
            out[idx] = value.min(u32::MAX as i64) as u32;
            cur_idx += 1;
        } else {
            let v = -value;
            let num_zeros = v / 4;
            width_flag = v % 4;
            cur_idx += num_zeros;
            cur_size = width_size(width_flag)?;
        }
    }
    Ok(out)
}

#[inline]
fn width_size(flag: i64) -> Result<usize> {
    match flag {
        1 => Ok(1),
        2 => Ok(2),
        3 => Ok(4),
        4 => Ok(8),
        _ => Err(anyhow!("Malformed MSProfile.bin RLE segment: bad width flag {flag}")),
    }
}

/// Minimal LZF decompressor (Marc Lehmann's libLZF format, as used by MassHunter HRMS MSProfile.bin
/// and python-lzf). A control byte either copies `ctrl+1` literal bytes, or back-references a run of
/// `len` bytes at distance `ref`. `expected` is the known output length (UncompressedByteCount).
fn lzf_decompress(input: &[u8], expected: usize) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(expected);
    let mut ip = 0usize;
    let n = input.len();
    while ip < n {
        let ctrl = input[ip] as usize;
        ip += 1;
        if ctrl < 32 {
            // Literal run of (ctrl + 1) bytes.
            let len = ctrl + 1;
            if ip + len > n {
                bail!("LZF: literal run overruns input");
            }
            out.extend_from_slice(&input[ip..ip + len]);
            ip += len;
        } else {
            // Back-reference. High 3 bits of ctrl are the (length-2) base; low 5 bits are the high
            // bits of the distance, with one more distance byte following (and, for len==7, an extra
            // length byte).
            let mut len = ctrl >> 5;
            if len == 7 {
                if ip >= n {
                    bail!("LZF: missing extended length byte");
                }
                len += input[ip] as usize;
                ip += 1;
            }
            if ip >= n {
                bail!("LZF: missing distance byte");
            }
            let dist = ((ctrl & 0x1f) << 8) | input[ip] as usize;
            ip += 1;
            let mut ref_pos = out
                .len()
                .checked_sub(dist + 1)
                .ok_or_else(|| anyhow!("LZF: back-reference before output start"))?;
            // length is (len + 2) bytes; copy byte-by-byte (overlapping copies are valid in LZF).
            for _ in 0..len + 2 {
                let b = out[ref_pos];
                out.push(b);
                ref_pos += 1;
            }
        }
    }
    Ok(out)
}

// ───────────────────────── MSMassCal.bin + DefaultMassCal.xml ─────────────────────────

/// Load per-scan calibration. Prefers the per-scan `MSMassCal.bin` (10 f64 from offset 0x4c, stride
/// 84); falls back to `DefaultMassCal.xml` rows by CalibrationID. Also returns the polynomial
/// ValueUseFlags per CalibrationID. Mirrors rainbow `_load_calibration`.
fn load_calibration(
    acq: &Path,
    calibration_ids: &[i32],
) -> Result<(Vec<[f64; 10]>, HashMap<i32, u32>)> {
    let masscal = acq.join("MSMassCal.bin");
    let n = calibration_ids.len();
    let calib = if masscal.is_file() {
        let mut f = fs::File::open(&masscal)
            .with_context(|| format!("opening {}", masscal.display()))?;
        f.seek(SeekFrom::Start(0x4c))?;
        let mut bytes = Vec::new();
        f.read_to_end(&mut bytes)?;
        let stride = 84usize;
        let mut rows = Vec::with_capacity(n);
        for i in 0..n {
            let mut row = [0.0f64; 10];
            let base = i * stride;
            if base + 10 * 8 > bytes.len() {
                bail!(
                    "MSMassCal.bin too short: scan {i} needs offset {} but file has {} bytes after 0x4c",
                    base + 10 * 8,
                    bytes.len()
                );
            }
            for (j, slot) in row.iter_mut().enumerate() {
                let o = base + j * 8;
                *slot = f64::from_le_bytes(bytes[o..o + 8].try_into().unwrap());
            }
            rows.push(row);
        }
        rows
    } else {
        let default_rows = read_default_masscal_rows(&acq.join("DefaultMassCal.xml"))?;
        if default_rows.is_empty() {
            bail!(
                "no MSMassCal.bin and no usable DefaultMassCal.xml in {} — cannot calibrate MSProfile.bin",
                acq.display()
            );
        }
        let fallback = *default_rows.values().next().unwrap();
        calibration_ids
            .iter()
            .map(|cid| *default_rows.get(cid).unwrap_or(&fallback))
            .collect()
    };
    let poly_flags = parse_default_masscal_flags(&acq.join("DefaultMassCal.xml"));
    Ok((calib, poly_flags))
}

/// Parse `DefaultMassCal.xml` for each calibration id's polynomial `ValueUseFlags` (bitmask of the
/// polynomial orders in use). Empty when the file is absent or has no Polynomial step. Lightweight
/// string scan (no XML dep): the file is tiny and the structure is flat.
fn parse_default_masscal_flags(xml_path: &Path) -> HashMap<i32, u32> {
    let mut out = HashMap::new();
    let Ok(text) = fs::read_to_string(xml_path) else {
        return out;
    };
    // Walk each <DefaultCalibration ...> block; within it, if a <Step> has
    // <CalibrationFormula>Polynomial</CalibrationFormula>, read its <ValueUseFlags>.
    for block in text.split("<DefaultCalibration").skip(1) {
        let Some(id) = attr_value(block, "DefaultCalibrationID") else { continue };
        let Ok(cid) = id.parse::<i32>() else { continue };
        // Only consider the Polynomial step's flags.
        if let Some(poly_pos) = block.find(">Polynomial<") {
            let tail = &block[poly_pos..];
            if let Some(flags) = element_text(tail, "ValueUseFlags") {
                if let Ok(f) = flags.trim().parse::<u32>() {
                    out.insert(cid, f);
                }
            }
        }
    }
    out
}

/// Fallback calibration rows from `DefaultMassCal.xml` (CalibrationID → 10 doubles
/// `[coeff, base, left, right, c0..c5]`). Used only when `MSMassCal.bin` is absent.
fn read_default_masscal_rows(xml_path: &Path) -> Result<HashMap<i32, [f64; 10]>> {
    let mut rows = HashMap::new();
    let Ok(text) = fs::read_to_string(xml_path) else {
        return Ok(rows);
    };
    for block in text.split("<DefaultCalibration").skip(1) {
        let Some(id) = attr_value(block, "DefaultCalibrationID") else { continue };
        let Ok(cid) = id.parse::<i32>() else { continue };
        let traditional = step_values(block, "Traditional");
        let polynomial = step_values(block, "Polynomial");
        if traditional.len() < 2 {
            continue;
        }
        let mut row = [0.0f64; 10];
        row[0] = traditional[0];
        row[1] = traditional[1];
        for (j, v) in polynomial.iter().take(8).enumerate() {
            row[2 + j] = *v;
        }
        rows.insert(cid, row);
    }
    Ok(rows)
}

/// Read all `<Value>…</Value>` numbers inside the `<Step>` whose `<CalibrationFormula>` matches
/// `formula`, within one `<DefaultCalibration>` block.
fn step_values(block: &str, formula: &str) -> Vec<f64> {
    let needle = format!(">{formula}<");
    let Some(fpos) = block.find(&needle) else { return Vec::new() };
    // The Step is the surrounding element; scan forward to its </Step>.
    let tail = &block[fpos..];
    let stop = tail.find("</Step>").unwrap_or(tail.len());
    let scope = &tail[..stop];
    let mut vals = Vec::new();
    for chunk in scope.split("<Value>").skip(1) {
        if let Some(end) = chunk.find("</Value>") {
            if let Ok(v) = chunk[..end].trim().parse::<f64>() {
                vals.push(v);
            }
        }
    }
    vals
}

/// Extract the text of the first `<tag>…</tag>` in `s`.
fn element_text<'a>(s: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = s.find(&open)? + open.len();
    let end = s[start..].find(&close)? + start;
    Some(&s[start..end])
}

/// Extract an XML attribute value (`name="value"`) from a tag fragment.
fn attr_value<'a>(s: &'a str, name: &str) -> Option<&'a str> {
    let key = format!("{name}=\"");
    let start = s.find(&key)? + key.len();
    let end = s[start..].find('"')? + start;
    Some(&s[start..end])
}

// ───────────────────────── MSScan.xsd / MSScan.bin ─────────────────────────

/// Byte size of each simple XSD type (mirrors rainbow `_SIMPLE_TYPE_SIZES`).
fn simple_size(t: &str) -> Option<usize> {
    match t {
        "xs:byte" => Some(1),
        "xs:short" => Some(2),
        "xs:int" => Some(4),
        "xs:long" => Some(8),
        "xs:float" => Some(4),
        "xs:double" => Some(8),
        _ => None,
    }
}

/// The parsed MSScan.xsd: each complexType → ordered `(field_name, type_name)` members. Enough to
/// compute record/block sizes and to read the scalar scan fields we need.
struct ScanSchema {
    types: HashMap<String, Vec<(String, String)>>,
}

impl ScanSchema {
    fn parse(xsd_path: &Path) -> Result<Self> {
        let text = fs::read_to_string(xsd_path)
            .with_context(|| format!("reading {}", xsd_path.display()))?;
        let mut types: HashMap<String, Vec<(String, String)>> = HashMap::new();
        // Each <xs:complexType name="X"> … contains <xs:element name="f" type="T"/> members until
        // its </xs:complexType>. The schema is small and flat; a forward scan is sufficient and
        // avoids an XML crate. Commented-out members (inside <!-- -->) must be ignored.
        let mut rest = text.as_str();
        while let Some(p) = rest.find("<xs:complexType") {
            rest = &rest[p..];
            let Some(name) = attr_value(rest, "name") else {
                rest = &rest[15..];
                continue;
            };
            let name = name.to_string();
            let end = rest.find("</xs:complexType>").unwrap_or(rest.len());
            let body = &rest[..end];
            let body = strip_comments(body);
            let mut members = Vec::new();
            for frag in body.split("<xs:element").skip(1) {
                // frag starts right after "<xs:element"; the tag ends at the next '>'.
                let tag_end = frag.find('>').unwrap_or(frag.len());
                let tag = &frag[..tag_end];
                if let (Some(fname), Some(ftype)) = (attr_value(tag, "name"), attr_value(tag, "type")) {
                    members.push((fname.to_string(), ftype.to_string()));
                }
            }
            types.insert(name, members);
            rest = &rest[end + "</xs:complexType>".len().min(rest.len() - end)..];
        }
        if !types.contains_key("ScanRecordType") || !types.contains_key("SpectrumParamsType") {
            bail!("MSScan.xsd missing ScanRecordType / SpectrumParamsType");
        }
        Ok(Self { types })
    }

    /// On-disk size of one record of `name`, counting each member once (so a ScanRecordType is sized
    /// with a single SpectrumParamValues block). Mirrors rainbow `type_size`.
    fn type_size(&self, name: &str) -> Result<usize> {
        let local = name.rsplit(':').next().unwrap_or(name);
        if let Some(s) = simple_size(name) {
            return Ok(s);
        }
        let members = self
            .types
            .get(local)
            .ok_or_else(|| anyhow!("unknown XSD type {name}"))?;
        let mut total = 0;
        for (_, t) in members {
            total += self.type_size(t)?;
        }
        Ok(total)
    }
}

/// Drop `<!-- … -->` comment regions from an XSD fragment (the schema comments out optional members
/// that are not on disk; counting them would corrupt the stride).
fn strip_comments(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(p) = rest.find("<!--") {
        out.push_str(&rest[..p]);
        if let Some(q) = rest[p..].find("-->") {
            rest = &rest[p + q + 3..];
        } else {
            rest = "";
            break;
        }
    }
    out.push_str(rest);
    out
}

/// Total scan count from MSTS.xml (sum of `<NumOfScans>`), or None if absent. Used only as a hint
/// for the record stride; the geometry is validated either way.
fn count_scans(acq: &Path) -> Option<usize> {
    let text = fs::read_to_string(acq.join("MSTS.xml")).ok()?;
    let mut total = 0usize;
    for chunk in text.split("<NumOfScans>").skip(1) {
        if let Some(end) = chunk.find("</NumOfScans>") {
            if let Ok(v) = chunk[..end].trim().parse::<usize>() {
                total += v;
            }
        }
    }
    if total > 0 { Some(total) } else { None }
}

/// Read scan records from MSScan.bin (rainbow `read_scan_records`). The record stride is
/// `scalar + n·block` for the block count `n` that tiles the record region exactly (a Q-TOF that
/// stores both profile and centroid writes two blocks per record). We read the scalar fields we
/// need (ScanTime, MSLevel, CalibrationID) plus the FIRST SpectrumParamValues block (the profile
/// block) of each record.
fn read_scan_records(
    msscan_path: &Path,
    schema: &ScanSchema,
    num_records: Option<usize>,
) -> Result<Vec<ScanInfo>> {
    let mut bytes = Vec::new();
    fs::File::open(msscan_path)
        .with_context(|| format!("opening {}", msscan_path.display()))?
        .read_to_end(&mut bytes)?;
    let file_size = bytes.len();

    let block = schema.type_size("SpectrumParamsType")?;
    let scalar = schema.type_size("ScanRecordType")? - block;

    if file_size < 0x5c {
        bail!("MSScan.bin too short ({file_size} bytes)");
    }
    let rec_start = u32::from_le_bytes(bytes[0x58..0x5c].try_into().unwrap()) as usize;
    if rec_start > file_size {
        bail!("MSScan.bin record start 0x{rec_start:x} past EOF");
    }
    let body = file_size - rec_start;

    const MAX_BLOCKS: usize = 8;
    let candidates: Vec<usize> = (1..=MAX_BLOCKS).map(|n| scalar + n * block).collect();
    let mut stride: Option<usize> = None;
    if let Some(nr) = num_records {
        if nr > 0 && body % nr == 0 {
            let hinted = body / nr;
            if candidates.contains(&hinted) {
                stride = Some(hinted);
            }
        }
    }
    if stride.is_none() {
        let divisors: Vec<usize> = candidates.iter().copied().filter(|&s| s > 0 && body % s == 0).collect();
        stride = divisors.into_iter().min_by_key(|&s| {
            num_records.map(|nr| (body / s).abs_diff(nr)).unwrap_or(s)
        });
    }

    let Some(stride) = stride else {
        bail!("MSScan.bin: no record stride tiles the {body}-byte record region");
    };

    // Resolve the byte offsets of the scalar fields we read from a record, and of the first block's
    // SpectrumOffset/ByteCount/PointCount, by walking the schema member sizes once.
    let layout = RecordLayout::resolve(schema, scalar)?;

    let n = body / stride;
    let mut scans = Vec::with_capacity(n);
    for i in 0..n {
        let base = rec_start + i * stride;
        if base + scalar + block > file_size {
            break;
        }
        let rec = &bytes[base..base + scalar + block];
        let scan_time = read_f64(rec, layout.scan_time);
        let ms_level = layout
            .ms_level
            .map(|o| read_i32(rec, o).clamp(0, 255) as u8)
            .filter(|&l| l > 0)
            .unwrap_or(1);
        let calibration_id = layout.calibration_id.map(|o| read_i32(rec, o)).unwrap_or(0);
        // First (profile) block fields, at `scalar + block-relative offset`.
        let bofs = scalar;
        let offset = read_i64(rec, bofs + layout.block_spectrum_offset) as u64;
        let byte_count = read_i32(rec, bofs + layout.block_byte_count) as i64;
        let point_count = read_i32(rec, bofs + layout.block_point_count) as i64;
        scans.push(ScanInfo {
            scan_time,
            ms_level,
            calibration_id,
            offset,
            byte_count,
            point_count,
        });
    }
    Ok(scans)
}

/// Resolved byte offsets (within a single-block record) of the scalar fields we read and the first
/// SpectrumParamsType block's offset/byte-count/point-count.
struct RecordLayout {
    scan_time: usize,
    ms_level: Option<usize>,
    calibration_id: Option<usize>,
    block_spectrum_offset: usize,
    block_byte_count: usize,
    block_point_count: usize,
}

impl RecordLayout {
    fn resolve(schema: &ScanSchema, _scalar: usize) -> Result<Self> {
        // Walk ScanRecordType members, summing sizes, recording the offsets of the fields we want.
        let members = schema
            .types
            .get("ScanRecordType")
            .ok_or_else(|| anyhow!("no ScanRecordType"))?;
        let mut off = 0usize;
        let mut scan_time = None;
        let mut ms_level = None;
        let mut calibration_id = None;
        for (name, ty) in members {
            match name.as_str() {
                "ScanTime" => scan_time = Some(off),
                "MSLevel" => ms_level = Some(off),
                "CalibrationID" => calibration_id = Some(off),
                _ => {}
            }
            if name == "SpectrumParamValues" {
                break; // the block follows; its sub-offsets are resolved separately
            }
            off += schema.type_size(ty)?;
        }
        let scan_time = scan_time.ok_or_else(|| anyhow!("ScanRecordType has no ScanTime"))?;

        // Offsets WITHIN a SpectrumParamsType block.
        let block_members = schema
            .types
            .get("SpectrumParamsType")
            .ok_or_else(|| anyhow!("no SpectrumParamsType"))?;
        let mut boff = 0usize;
        let mut block_spectrum_offset = None;
        let mut block_byte_count = None;
        let mut block_point_count = None;
        for (name, ty) in block_members {
            match name.as_str() {
                "SpectrumOffset" => block_spectrum_offset = Some(boff),
                "ByteCount" => block_byte_count = Some(boff),
                "PointCount" => block_point_count = Some(boff),
                _ => {}
            }
            boff += schema.type_size(ty)?;
        }
        Ok(Self {
            scan_time,
            ms_level,
            calibration_id,
            block_spectrum_offset: block_spectrum_offset
                .ok_or_else(|| anyhow!("SpectrumParamsType has no SpectrumOffset"))?,
            block_byte_count: block_byte_count
                .ok_or_else(|| anyhow!("SpectrumParamsType has no ByteCount"))?,
            block_point_count: block_point_count
                .ok_or_else(|| anyhow!("SpectrumParamsType has no PointCount"))?,
        })
    }
}

#[inline]
fn read_f64(rec: &[u8], off: usize) -> f64 {
    f64::from_le_bytes(rec[off..off + 8].try_into().unwrap())
}
#[inline]
fn read_i32(rec: &[u8], off: usize) -> i32 {
    i32::from_le_bytes(rec[off..off + 4].try_into().unwrap())
}
#[inline]
fn read_i64(rec: &[u8], off: usize) -> i64 {
    i64::from_le_bytes(rec[off..off + 8].try_into().unwrap())
}

/// The `.d` directory path for a possibly-nested input (kept for symmetry with other readers).
#[allow(dead_code)]
pub fn d_dir(input: &Path) -> PathBuf {
    input.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rle_roundtrip_simple() {
        // Build a body: marker word, init_zero=2, width=1, then literals 5,7, a zero-run of 3
        // (encoded as -(3*4+1)=-13 at width 1), then literal 9.
        let num_mz = 8usize;
        let mut body = Vec::new();
        let marker = (num_mz as u32 & 0x00FF_FFFF) | (0x90u32 << 24);
        body.extend_from_slice(&marker.to_le_bytes());
        body.extend_from_slice(&(-(2i32)).to_le_bytes()); // init_zero = 2
        body.extend_from_slice(&(-(1i32)).to_le_bytes()); // width flag 1
        body.push(5i8 as u8);
        body.push(7i8 as u8);
        body.push((-13i8) as u8); // zero-run 3, stay width 1
        body.push(9i8 as u8);
        let out = decompress_rle(&body, num_mz).unwrap();
        // indices: 0,1 zero; 2=5; 3=7; 4,5,6 zero; 7=9
        assert_eq!(out, vec![0, 0, 5, 7, 0, 0, 0, 9]);
    }

    #[test]
    fn lzf_literal_and_backref() {
        // "abcabcabc": literal "abc" (ctrl=2 → 3 bytes), then backref dist=2 (0-based dist+1=3),
        // len=6 (len field 4 → +2). LZF backrefs allow overlap.
        // Encode literal run of 3: ctrl=2, then 'a','b','c'.
        let mut enc = vec![2u8, b'a', b'b', b'c'];
        // Backref: copy 6 bytes from distance 3 back. ctrl high3 = len-2 = 4, low5 = (dist-1)>>8 = 0;
        // next byte = (dist-1)&0xff = 2.
        enc.push((4u8 << 5) | 0);
        enc.push(2u8);
        let out = lzf_decompress(&enc, 9).unwrap();
        assert_eq!(out, b"abcabcabc");
    }

    #[test]
    fn closed_form_mapping_matches_traditional() {
        // m/z = (coeff·(t−base))² on a uniform grid t=mz_min+k·delta must equal (c0+c1·k)².
        let (coeff, base) = (1.234e-3, 1500.0);
        let (mz_min, delta) = (40000.0, 1.0);
        let grid = TofGrid { c0: coeff * (mz_min - base), c1: coeff * delta };
        for k in [0i32, 10, 1000, 250_000] {
            let t = mz_min + k as f64 * delta;
            let direct = (coeff * (t - base)).powi(2);
            let viagrid = grid.mz(k);
            let ppm = (direct - viagrid).abs() / direct * 1e6;
            assert!(ppm < 1e-6, "k={k} ppm={ppm}");
        }
    }
}

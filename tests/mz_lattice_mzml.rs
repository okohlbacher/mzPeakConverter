//! End-to-end contract for the fixed-point m/z lattice on the ORDINARY mzML lane (`convert_file`).
//!
//! `tests/shimadzu_lattice_peaks.rs` pins the archive SHAPE by driving the vendored writer API
//! directly, because the Shimadzu reader is `#[cfg(windows)]`. This one goes through the real
//! binary on a committed fixture, so it also pins the DETECTION (which scale, from the data alone),
//! the per-spectrum fallback, and the summary columns.
//!
//! Fixtures (`tests/data/`): `mz_lattice_1e9.mzML` — 12 centroid spectra of 90 peaks spanning a
//! realistic 120–1900 Da on a 1e-9 Da lattice (Shimadzu `MassHigh` / the LabSolutions mzML
//! export), one of which (index 7) carries a single interpolated apex 0.3 of a step off the
//! lattice; `mz_lattice_1e4.mzML` — 8 spectra over the same range on the coarse 1e-4 lattice, to
//! prove the scale is read off the data rather than hard-coded.
//! `mixed_precision.mzML` is the NON-lattice control.
//!
//! Asserted here:
//!   * the `mz_calibration` index block (`mz-grid`, the detected scale, `applies_to spectra_peaks`);
//!   * `point.tof_index` as INT64, DELTA_BINARY_PACKED, ZSTD, no dictionary, `LinearMz` with
//!     `transform_params == [1/scale]`, beside a Float64 `point.mz` fallback column;
//!   * the vendored reader hands back m/z BIT-IDENTICAL to the SOURCE mzML's — the archive's
//!     contract is `m/z = tof_index / scale`, which is the exact inverse of `round(m/z · scale)`,
//!     and the fixtures span 120–1900 Da precisely so that a reader multiplying by `1/scale`
//!     instead (one ulp off on ~40 % of values) fails this — and exactly f64-equal on the
//!     off-lattice spectrum: nothing is snapped, nothing is refused;
//!   * the per-spectrum summary columns (MS:1000285 / 504 / 505 / 527 / 528) are REAL, and equal to
//!     the same file converted without the lattice. This is the bc8497c regression: a route that
//!     leaves the writer an m/z-less array map ships `total_ion_current = 0`, a null base peak and
//!     null observed-m/z bounds on every routed spectrum;
//!   * a non-lattice input converts to a BYTE-IDENTICAL set of parquet members with the lattice on
//!     and off.

use std::path::{Path, PathBuf};
use std::process::Command;

use arrow::datatypes::DataType;
use mzdata::prelude::*;
use mzdata::spectrum::PeakDataLevel;
use mzpeak_prototyping::buffer_descriptors::BufferTransform;
use mzpeak_prototyping::MzPeakReader;
use parquet::basic::{Compression, Encoding, Type as PhysicalType};
use parquet::file::reader::{FileReader, SerializedFileReader};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data").join(name)
}

/// A scratch dir unique to this test binary run (the suite runs more than once per `cargo test`).
fn scratch(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("mzpc-mzlat-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Convert `input` with the real binary. `lattice = false` sets `$MZPC_NO_MZ_LATTICE`, which is the
/// same switch as `--no-mz-lattice` but leaves argv (embedded in the archive index) untouched — so
/// the two archives of the byte-identity check differ ONLY in the thing under test.
fn convert(input: &Path, output: &Path, lattice: bool) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_mzpeak-convert"));
    cmd.arg(input).arg("-o").arg(output).arg("-q");
    if !lattice {
        cmd.env("MZPC_NO_MZ_LATTICE", "1");
    }
    let st = cmd.status().expect("failed to run mzpeak-convert");
    assert!(st.success(), "converting {} failed: {st}", input.display());
}

fn member(archive: &Path, name: &str) -> Vec<u8> {
    let mut zip = zip::ZipArchive::new(std::fs::File::open(archive).unwrap()).unwrap();
    let mut e = zip.by_name(name).unwrap_or_else(|_| panic!("{name} missing from the archive"));
    let mut buf = Vec::new();
    std::io::Read::read_to_end(&mut e, &mut buf).unwrap();
    buf
}

fn extract(archive: &Path, name: &str, dir: &Path) -> PathBuf {
    let out = dir.join(name);
    std::fs::write(&out, member(archive, name)).unwrap();
    out
}

/// The source mzML's m/z, spectrum by spectrum, read with mzdata itself.
fn source_mzs(input: &Path) -> Vec<Vec<f64>> {
    let mut reader = mzdata::MZReader::open_path(input).expect("opening the source mzML");
    reader.iter().map(|s| s.arrays.as_ref().unwrap().mzs().unwrap().to_vec()).collect()
}

fn peak_mzs(level: PeakDataLevel) -> Vec<f64> {
    match level {
        PeakDataLevel::Centroid(peaks) => peaks.iter().map(|p| p.mz).collect(),
        PeakDataLevel::RawData(arrays) => arrays.mzs().unwrap().to_vec(),
        other => panic!("unexpected peak level with {} points", other.len()),
    }
}

/// Every per-spectrum summary column the grid routes are required to keep real, in `index` order.
struct Summaries {
    tic: Vec<Option<f32>>,
    bp_mz: Vec<Option<f64>>,
    bp_int: Vec<Option<f32>>,
    lo_mz: Vec<Option<f64>>,
    hi_mz: Vec<Option<f64>>,
}

fn summaries(archive: &Path, dir: &Path) -> Summaries {
    use arrow::array::{Array, AsArray};
    use arrow::datatypes::{Float32Type, Float64Type};
    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

    let path = extract(archive, "spectra_metadata.parquet", dir);
    let rdr = ParquetRecordBatchReaderBuilder::try_new(std::fs::File::open(&path).unwrap())
        .unwrap()
        .with_batch_size(1 << 16)
        .build()
        .unwrap();
    let mut s = Summaries {
        tic: Vec::new(),
        bp_mz: Vec::new(),
        bp_int: Vec::new(),
        lo_mz: Vec::new(),
        hi_mz: Vec::new(),
    };
    for batch in rdr {
        let batch = batch.unwrap();
        let col = |n: &str| batch.column_by_name(n).unwrap_or_else(|| panic!("no `{n}` column")).clone();
        let f32s = |n: &str| {
            let c = col(n);
            let a = c.as_primitive::<Float32Type>();
            (0..a.len()).map(|i| a.is_valid(i).then(|| a.value(i))).collect::<Vec<_>>()
        };
        let f64s = |n: &str| {
            let c = col(n);
            let a = c.as_primitive::<Float64Type>();
            (0..a.len()).map(|i| a.is_valid(i).then(|| a.value(i))).collect::<Vec<_>>()
        };
        s.tic.extend(f32s("total_ion_current"));
        s.bp_mz.extend(f64s("base_peak_mz"));
        s.bp_int.extend(f32s("base_peak_intensity"));
        s.lo_mz.extend(f64s("lowest_observed_mz"));
        s.hi_mz.extend(f64s("highest_observed_mz"));
    }
    s
}

/// One fixture, end to end: detection, round trip, column contract, summaries.
///
/// `off_lattice` is the index of the spectrum whose centroids miss the lattice (it must come back
/// as EXACT f64, not snapped); `None` when every spectrum fits.
fn lattice_fixture(name: &str, scale: f64, params: &str, off_lattice: Option<usize>) {
    let dir = scratch(name);
    let input = fixture(name);
    assert!(input.exists(), "fixture missing: {}", input.display());
    let out = dir.join("lattice.mzpeak");
    let plain = dir.join("plain.mzpeak");
    convert(&input, &out, true);
    convert(&input, &plain, false);

    let src = source_mzs(&input);
    assert!(src.len() >= 8 && src[0].len() >= 64, "fixture too small to arm the detector");

    // (a) The `mz_calibration` index block the viewer's `mz-grid` codec gates on.
    let index: serde_json::Value =
        serde_json::from_slice(&member(&out, "mzpeak_index.json")).unwrap();
    let cal = &index["metadata"]["mz_calibration"];
    assert_eq!(cal["codec"], "mz-grid", "no mz_calibration block in {index:#}");
    assert_eq!(cal["applies_to"], "spectra_peaks");
    assert_eq!(cal["lossless"], "tof_index");
    assert_eq!(cal["scale"].as_f64(), Some(scale), "the block must name the DETECTED scale");
    // ... and it is absent when the lattice is off, so a reader cannot be told to un-scale f64 m/z.
    let plain_index: serde_json::Value =
        serde_json::from_slice(&member(&plain, "mzpeak_index.json")).unwrap();
    assert!(plain_index["metadata"]["mz_calibration"].is_null());

    // (b) Read back through the vendored reader: m/z reconstructed from the column metadata alone.
    let mut reader = MzPeakReader::new(&out).unwrap();
    assert_eq!(reader.len(), src.len());
    // BIT-FOR-BIT, not within an epsilon. The archive's contract is `m/z = tof_index / scale`
    // (`mz_calibration.mz_from_tof_index`), and `round(m/z·scale) / scale` is the identity on every
    // value that is genuinely on the lattice — so the round trip must reproduce the source f64
    // exactly, at any m/z. A tolerance here would have to be at least one ulp of the m/z (1.14e-13
    // at m/z 512, 2.27e-13 above 1024) and so could not tell the exact quotient apart from the
    // one-ulp-off `tof_index · (1/scale)`, which is the whole thing being pinned. The fixtures
    // therefore span a realistic 120–1900 Da, where those two differ.
    for (i, want) in src.iter().enumerate() {
        let got = peak_mzs(reader.get_spectrum_peaks_for(i as u64).unwrap().expect("peaks"));
        assert_eq!(got.len(), want.len(), "spectrum {i}: peak count");
        if off_lattice == Some(i) {
            assert_eq!(&got, want, "the off-lattice spectrum must keep its EXACT f64 m/z");
            continue;
        }
        for (j, (g, w)) in got.iter().zip(want).enumerate() {
            // The encode step is exact by construction ...
            let k = (w * scale).round();
            assert!(
                (w * scale - k).abs() < 1e-3,
                "fixture bug: spectrum {i} peak {j} ({w:.15}) is not on the 1/{scale:e} lattice"
            );
            // ... and the decode step reproduces the source, to the last bit.
            assert_eq!(
                *g, k / scale,
                "spectrum {i} peak {j}: reader gave {g:.17e}, contract says tof_index / scale"
            );
            assert_eq!(
                g.to_bits(),
                w.to_bits(),
                "spectrum {i} peak {j}: {g:.17e} is not bit-identical to source {w:.17e} \
                 (delta {:e}, ulp {:e})",
                g - w,
                f64::from_bits(w.to_bits() + 1) - *w
            );
        }
    }

    // (c) The declared columns: an Int64 lattice axis with the right transform, and the f64
    // fallback beside it.
    let peak_index = reader.metadata.peak_array_indices().expect("peaks facet array index");
    let tof = peak_index
        .iter()
        .find(|e| e.path.ends_with("tof_index"))
        .expect("spectrum_array_index must list point.tof_index");
    assert_eq!(tof.path, "point.tof_index");
    assert_eq!(tof.data_type, DataType::Int64);
    assert_eq!(tof.transform, Some(BufferTransform::LinearMz));
    let want_params: f64 = params.parse().unwrap();
    assert_eq!(tof.transform_params, Some(vec![want_params]));
    assert!((want_params * scale - 1.0).abs() < 1e-12, "params must be 1/scale");
    assert!(
        peak_index.iter().any(|e| e.path == "point.mz" && e.data_type == DataType::Float64),
        "the f64 `mz` fallback column must be declared beside the lattice"
    );

    // (d) Parquet encoding: this is the whole point of naming the column `*_index`.
    let extracted = extract(&out, "spectra_peaks.parquet", &dir);
    let pq = SerializedFileReader::new(std::fs::File::open(&extracted).unwrap()).unwrap();
    let (mut tof_nulls, mut mz_nulls, mut rows) = (0i64, 0i64, 0i64);
    for rg in pq.metadata().row_groups() {
        rows += rg.num_rows();
        let tof = rg
            .columns()
            .iter()
            .find(|c| c.column_path().string() == "point.tof_index")
            .expect("point.tof_index column");
        assert_eq!(tof.column_type(), PhysicalType::INT64);
        assert!(matches!(tof.compression(), Compression::ZSTD(_)), "tof_index is {}", tof.compression());
        let encodings: Vec<Encoding> = tof.encodings().collect();
        assert!(
            encodings.contains(&Encoding::DELTA_BINARY_PACKED),
            "tof_index must be DELTA_BINARY_PACKED (encodings: {encodings:?})"
        );
        assert!(
            !encodings
                .iter()
                .any(|e| matches!(e, Encoding::RLE_DICTIONARY | Encoding::PLAIN_DICTIONARY)),
            "tof_index must not be dictionary-encoded (encodings: {encodings:?})"
        );
        tof_nulls += tof.statistics().and_then(|s| s.null_count_opt()).unwrap_or(0) as i64;
        let mz = rg
            .columns()
            .iter()
            .find(|c| c.column_path().string() == "point.mz")
            .expect("point.mz column");
        assert_eq!(mz.column_type(), PhysicalType::DOUBLE);
        mz_nulls += mz.statistics().and_then(|s| s.null_count_opt()).unwrap_or(0) as i64;
    }
    let total: i64 = src.iter().map(|v| v.len() as i64).sum();
    assert_eq!(rows, total, "every source peak must be stored");
    let n_fallback: i64 = off_lattice.map_or(0, |i| src[i].len() as i64);
    assert_eq!(tof_nulls, n_fallback, "tof_index is NULL on exactly the fallback rows");
    assert_eq!(mz_nulls, total - n_fallback, "the f64 mz fallback is NULL on every lattice row");

    // (e) THE SUMMARY CONTRACT (bc8497c). A lattice-routed spectrum stores no m/z in its facet, so
    // this is exactly where `tic = 0` / null base peak / null m/z bounds crept in before. They must
    // be real, and identical to the same file converted without the lattice.
    let a = summaries(&out, &dir);
    let b = summaries(&plain, &dir);
    assert_eq!(a.tic.len(), src.len());
    for i in 0..src.len() {
        assert!(a.tic[i].is_some_and(|v| v > 0.0), "spectrum {i}: total_ion_current is {:?}", a.tic[i]);
        assert!(a.bp_mz[i].is_some_and(|v| v > 0.0), "spectrum {i}: base_peak_mz is {:?}", a.bp_mz[i]);
        assert!(a.bp_int[i].is_some_and(|v| v > 0.0), "spectrum {i}: base_peak_intensity");
        assert!(a.lo_mz[i].is_some_and(|v| v > 0.0), "spectrum {i}: lowest_observed_mz");
        assert!(a.hi_mz[i].is_some_and(|v| v > 0.0), "spectrum {i}: highest_observed_mz");
        assert_eq!(a.tic[i], b.tic[i], "spectrum {i}: TIC differs from the non-lattice archive");
        assert_eq!(a.bp_mz[i], b.bp_mz[i], "spectrum {i}: base peak m/z differs");
        assert_eq!(a.bp_int[i], b.bp_int[i], "spectrum {i}: base peak intensity differs");
        assert_eq!(a.lo_mz[i], b.lo_mz[i], "spectrum {i}: lowest observed m/z differs");
        assert_eq!(a.hi_mz[i], b.hi_mz[i], "spectrum {i}: highest observed m/z differs");
    }

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_1e9_lattice_mzml_round_trips_through_the_generic_lane() {
    // Spectrum 7 carries one interpolated apex 0.3 of a step off: it keeps exact f64 m/z.
    lattice_fixture("mz_lattice_1e9.mzML", 1e9, "1e-9", Some(7));
}

#[test]
fn a_coarse_1e4_lattice_mzml_is_detected_at_its_own_scale() {
    lattice_fixture("mz_lattice_1e4.mzML", 1e4, "1e-4", None);
}

/// A non-lattice input must be untouched by all of this — the same converter decisions, the same
/// bytes. The two runs differ only in `$MZPC_NO_MZ_LATTICE`, so argv (which the archive index
/// records verbatim) is identical and the comparison is meaningful down to the byte.
///
/// The index JSON is compared as JSON, not as bytes: its object key order and the
/// instrument-configuration list order vary between two runs of the SAME binary (a pre-existing
/// HashMap iteration order), so a byte comparison there would be a coin flip, not a regression test.
#[test]
fn a_non_lattice_mzml_converts_identically_with_the_lattice_on_and_off() {
    let dir = scratch("control");
    let input = fixture("mixed_precision.mzML");
    // Both runs write to the SAME path and are renamed afterwards: the converter records its own
    // argv in each facet's `data_processing_method_list`, so an `on.mzpeak` / `off.mzpeak` pair
    // would differ by the one byte of the output filename and prove nothing.
    let staged = dir.join("out.mzpeak");
    let on = dir.join("on.mzpeak");
    let off = dir.join("off.mzpeak");
    convert(&input, &staged, true);
    std::fs::rename(&staged, &on).unwrap();
    convert(&input, &staged, false);
    std::fs::rename(&staged, &off).unwrap();

    let names: Vec<String> = {
        let z = zip::ZipArchive::new(std::fs::File::open(&on).unwrap()).unwrap();
        z.file_names().map(str::to_string).collect()
    };
    let off_names: Vec<String> = {
        let z = zip::ZipArchive::new(std::fs::File::open(&off).unwrap()).unwrap();
        z.file_names().map(str::to_string).collect()
    };
    assert_eq!(names, off_names, "the archive member list must not change");

    let mut compared = 0;
    for name in &names {
        let a = member(&on, name);
        let b = member(&off, name);
        if name.ends_with(".parquet") {
            assert_eq!(a, b, "{name} differs with the lattice enabled on a NON-lattice input");
            compared += 1;
        } else {
            let ja: serde_json::Value = serde_json::from_slice(&a).unwrap();
            let jb: serde_json::Value = serde_json::from_slice(&b).unwrap();
            assert!(ja["metadata"]["mz_calibration"].is_null(), "no lattice, no calibration block");
            assert_eq!(ja["files"], jb["files"], "{name}: the file list must not change");
        }
    }
    assert!(compared >= 8, "expected the full facet set, compared {compared} parquet members");
    let _ = std::fs::remove_dir_all(&dir);
}

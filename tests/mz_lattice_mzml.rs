//! End-to-end contract for fixed-point-lattice m/z on the ORDINARY mzML lane (`convert_file`).
//!
//! Goes through the real binary on committed fixtures, so it pins the DETECTION (from the data
//! alone), what the detection selects — the reference implementation's fitted linear grid on the
//! peaks facet, `MS:1003826` rows under per-spectrum `MS:1003824` models (vendoring exit, item 1;
//! through 0.13 this was an exact Int64 point lattice of the converter's own) — its declared bound,
//! and the summary columns.
//!
//! Fixtures (`tests/data/`): `mz_lattice_1e9.mzML` — 12 centroid spectra of 90 peaks spanning a
//! realistic 120–1900 Da on a 1e-9 Da lattice (Shimadzu `MassHigh` / the LabSolutions mzML
//! export), one of which (index 7) carries a single interpolated apex 0.3 of a step off the
//! lattice; `mz_lattice_1e4.mzML` — 8 spectra over the same range on the coarse 1e-4 lattice, to
//! prove the scale is read off the data rather than hard-coded. `mixed_precision.mzML` is the
//! NON-lattice control.
//!
//! Asserted here:
//!   * `transformations` declares `grid-fit:1e-6Da` (and no 0.13 `mz_calibration` block exists);
//!   * every peak row is an `MS:1003826` grid row under an `MS:1003824` model, the index lists
//!     byte-stream-split without a dictionary — the off-lattice spectrum included: the fit does not
//!     care about the lattice, the detection only arms it;
//!   * the vendored reader hands back every m/z within the declared 1e-6 Da of the SOURCE (in
//!     practice ≤ 3e-7 Da: the fit spreads the spectrum's padded range over 2³² slots), and never
//!     changes a peak count or an intensity;
//!   * the per-spectrum summary columns (MS:1000285 / 504 / 505 / 527 / 528) are REAL, and equal to
//!     the same file converted without the lattice (the writer derives them from the source arrays
//!     on both lanes);
//!   * a non-lattice input converts to a BYTE-IDENTICAL set of parquet members with the lattice on
//!     and off.

use std::path::{Path, PathBuf};
use std::process::Command;

use arrow::array::{Array, AsArray};
use mzdata::prelude::*;
use mzdata::spectrum::PeakDataLevel;
use mzpeak_prototyping::MzPeakReader;
use parquet::basic::{Compression, Encoding};
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

/// A string column as `StringArray`, whether Arrow materialised it as plain or dictionary-encoded
/// Utf8 (the grid struct's `grid_type` comes back dictionary-encoded).
fn strings(col: &arrow::array::ArrayRef) -> arrow::array::StringArray {
    arrow::compute::cast(col, &arrow::datatypes::DataType::Utf8).unwrap().as_string::<i32>().clone()
}

/// One fixture, end to end: detection, round trip, row contract, summaries.
fn lattice_fixture(name: &str, scale: f64) {
    let dir = scratch(name);
    let input = fixture(name);
    assert!(input.exists(), "fixture missing: {}", input.display());
    let out = dir.join("lattice.mzpeak");
    let plain = dir.join("plain.mzpeak");
    convert(&input, &out, true);
    convert(&input, &plain, false);

    let src = source_mzs(&input);
    assert!(src.len() >= 8 && src[0].len() >= 64, "fixture too small to arm the detector");
    for (i, mz) in src.iter().enumerate() {
        let off = mz.iter().filter(|w| (*w * scale - (*w * scale).round()).abs() >= 1e-3).count();
        assert!(off <= usize::from(i == 7), "fixture bug: spectrum {i} has {off} values off the 1/{scale:e} lattice");
    }

    // (a) The transformation is declared, with its bound; the 0.13 block is gone, and nothing is
    // declared when the lattice is off.
    let index: serde_json::Value = serde_json::from_slice(&member(&out, "mzpeak_index.json")).unwrap();
    let applied = |index: &serde_json::Value| -> Vec<String> {
        index["metadata"]["transformations"].as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect()).unwrap_or_default()
    };
    assert!(applied(&index).iter().any(|e| e == "grid-fit:1e-6Da"), "no grid-fit entry in {:?}", applied(&index));
    assert!(index["metadata"]["mz_calibration"].is_null(), "the point-lattice block is gone");
    let plain_index: serde_json::Value = serde_json::from_slice(&member(&plain, "mzpeak_index.json")).unwrap();
    assert!(!applied(&plain_index).iter().any(|e| e.starts_with("grid-fit")), "{:?}", applied(&plain_index));

    // (b) Read back through the vendored reader: every m/z within the declared bound of the
    // source — and far inside it — with the peak counts and intensities untouched.
    let mut reader = MzPeakReader::new(&out).unwrap();
    assert_eq!(reader.len(), src.len());
    let mut worst = 0.0f64;
    for (i, want) in src.iter().enumerate() {
        let got = peak_mzs(reader.get_spectrum_peaks_for(i as u64).unwrap().expect("peaks"));
        assert_eq!(got.len(), want.len(), "spectrum {i}: peak count");
        for (j, (g, w)) in got.iter().zip(want).enumerate() {
            let d = (g - w).abs();
            assert!(d <= 1e-6, "spectrum {i} peak {j}: reader gave {g:.12}, source {w:.12} (Δ {d:e} > 1e-6 Da)");
            worst = worst.max(d);
        }
    }
    assert!(worst <= 3e-7, "the fit over 2^32 slots of a ~1,900 Th span must land within 3e-7 Da; worst {worst:e}");
    assert!(worst > 0.0, "a fitted grid quantizes: bit-identity here would mean the lattice was not routed");

    // (c) The rows: grid rows under the linear model on EVERY spectrum, the off-lattice one included.
    let extracted = extract(&out, "spectra_peaks.parquet", &dir);
    let batches: Vec<arrow::record_batch::RecordBatch> =
        parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(std::fs::File::open(&extracted).unwrap())
            .unwrap()
            .build()
            .unwrap()
            .map(|b| b.unwrap())
            .collect();
    let (mut rows, mut points) = (0usize, 0usize);
    for b in &batches {
        let chunk = b.column_by_name("chunk").expect("a chunk facet").as_struct();
        let enc = strings(chunk.column_by_name("chunk_encoding").unwrap());
        let grid = chunk.column_by_name("mz_grid").expect("mz_grid").as_struct();
        let kind = strings(grid.column_by_name("grid_type").unwrap());
        let indices = grid.column_by_name("indices").unwrap().as_list::<i64>();
        for i in 0..b.num_rows() {
            rows += 1;
            assert_eq!(enc.value(i), "MS:1003826", "row {rows}: a grid row");
            assert_eq!(kind.value(i), "MS:1003824", "row {rows}: the linear model");
            points += indices.value(i).len();
        }
    }
    let total: usize = src.iter().map(Vec::len).sum();
    assert_eq!(points, total, "every source peak is stored on the grid");
    assert!(rows >= src.len(), "at least one chunk per spectrum");

    // (d) Parquet encoding of the index lists: byte-stream-split, no dictionary, ZSTD.
    let pq = SerializedFileReader::new(std::fs::File::open(&extracted).unwrap()).unwrap();
    for rg in pq.metadata().row_groups() {
        let col = rg
            .columns()
            .iter()
            .find(|c| c.column_path().string() == "chunk.mz_grid.indices.list.item")
            .expect("chunk.mz_grid.indices.list.item column");
        assert!(matches!(col.compression(), Compression::ZSTD(_)), "indices are {}", col.compression());
        let encodings: Vec<Encoding> = col.encodings().collect();
        assert!(encodings.contains(&Encoding::BYTE_STREAM_SPLIT), "indices must be BYTE_STREAM_SPLIT: {encodings:?}");
        assert!(
            !encodings.iter().any(|e| matches!(e, Encoding::RLE_DICTIONARY | Encoding::PLAIN_DICTIONARY)),
            "indices must not be dictionary-encoded: {encodings:?}"
        );
    }

    // (e) THE SUMMARY CONTRACT (bc8497c). They must be real, and identical to the same file
    // converted without the lattice: the writer derives them from the source arrays on both lanes.
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
    // Spectrum 7 carries one interpolated apex 0.3 of a step off: the fit takes it like the others.
    lattice_fixture("mz_lattice_1e9.mzML", 1e9);
}

#[test]
fn a_coarse_1e4_lattice_mzml_is_detected_at_its_own_scale() {
    lattice_fixture("mz_lattice_1e4.mzML", 1e4);
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
            let applied = ja["metadata"]["transformations"].to_string();
            assert!(!applied.contains("grid-fit"), "no lattice, no grid fit: {applied}");
            assert_eq!(ja["files"], jb["files"], "{name}: the file list must not change");
        }
    }
    assert!(compared >= 8, "expected the full facet set, compared {compared} parquet members");
    let _ = std::fs::remove_dir_all(&dir);
}

//! The mzPeak-input lane: `.mzpeak → .mzpeak` (src/filter.rs) and `.mzpeak → .mzML`
//! (`filter_mzpeak_to_mzml`). The rewrite lane shipped without a single test, and four defects sat
//! in it:
//!
//!   * every filter, even a pure `--sdrf` inject, refused an archive holding wavelength (UV/PDA)
//!     spectra with exit 1;
//!   * `--drop-aux` could delete a core facet and exit 0;
//!   * `--ms-level` / `--rt` defaulted a missing or retyped column (level 0 / NaN) and kept nothing;
//!   * `--rt` never refreshed `number_of_data_points` in the flat `chromatograms_metadata`.
//!
//! Each test converts its fixture into a scratch directory that belongs to that test alone.

use arrow::array::{Array, RecordBatch, StructArray, UInt8Array, UInt64Array};
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const TINY: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/tiny.pwiz.1.1.mzML");
const PDA_UV: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/pda_uv.pwiz.mzML");

/// A fresh directory for ONE test. The tests in a binary run in parallel under a single process id,
/// so a pid-only name let one test's cleanup delete another test's archive mid-run.
fn scratch(test: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("mzpc-filter-lane-{}-{test}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// `mzpeak-convert <input> -o <output> --force <extra…>`
fn mzpc(input: &Path, output: &Path, extra: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_mzpeak-convert"))
        .arg(input)
        .arg("-o")
        .arg(output)
        .arg("--force")
        .args(extra)
        .output()
        .expect("failed to run mzpeak-convert")
}

fn ok(r: &Output) {
    assert!(r.status.success(), "exit {:?}; stderr:\n{}", r.status.code(), String::from_utf8_lossy(&r.stderr));
}

/// Convert `fixture` into `dir/src.mzpeak`.
fn convert(fixture: &str, dir: &Path) -> PathBuf {
    let archive = dir.join("src.mzpeak");
    ok(&mzpc(Path::new(fixture), &archive, &[]));
    archive
}

fn member(archive: &Path, name: &str) -> Vec<u8> {
    let mut zip = zip::ZipArchive::new(File::open(archive).unwrap()).unwrap();
    let mut v = Vec::new();
    zip.by_name(name)
        .unwrap_or_else(|_| panic!("{name} missing from {}", archive.display()))
        .read_to_end(&mut v)
        .unwrap();
    v
}

fn table(archive: &Path, name: &str) -> RecordBatch {
    let b = ParquetRecordBatchReaderBuilder::try_new(bytes::Bytes::from(member(archive, name))).unwrap();
    let schema = b.schema().clone();
    let batches: Vec<_> = b.build().unwrap().map(Result::unwrap).collect();
    arrow::compute::concat_batches(&schema, &batches).unwrap()
}

fn footer(archive: &Path, name: &str, key: &str) -> Option<String> {
    let b = ParquetRecordBatchReaderBuilder::try_new(bytes::Bytes::from(member(archive, name))).unwrap();
    b.metadata().file_metadata().key_value_metadata()?.iter().find(|kv| kv.key == key)?.value.clone()
}

fn column<T: From<arrow::array::ArrayData>>(t: &RecordBatch, name: &str) -> T {
    T::from(t.column_by_name(name).unwrap_or_else(|| panic!("no `{name}` in {:?}", t.schema())).to_data())
}

/// (a) `--ms-level 2` keeps the one MS2 spectrum and nulls its reference to the MS1 it came from.
#[test]
fn ms_level_keeps_matching_spectra_and_nulls_dropped_parent_refs() {
    let dir = scratch("ms_level");
    let src = convert(TINY, &dir);
    let out = dir.join("f.mzpeak");
    ok(&mzpc(&src, &out, &["--ms-level", "2"]));

    let meta = table(&out, "spectra_metadata.parquet");
    assert_eq!(meta.num_rows(), 1);
    assert_eq!(column::<UInt8Array>(&meta, "ms_level").value(0), 2);

    let precursors = table(&out, "spectra_metadata_precursors.parquet");
    assert_eq!(precursors.num_rows(), 1);
    assert!(column::<UInt64Array>(&precursors, "precursor_index").is_null(0), "the MS1 parent was filtered out");
    let _ = std::fs::remove_dir_all(&dir);
}

/// (b) `--rt` keeps the spectra in the window, truncates the chromatogram traces to it, and rewrites
/// each chromatogram's `number_of_data_points` to what is left.
#[test]
fn rt_window_truncates_chromatograms_and_refreshes_point_counts() {
    let dir = scratch("rt_window");
    let src = convert(TINY, &dir);
    let out = dir.join("f.mzpeak");
    ok(&mzpc(&src, &out, &["--rt", "0-0.0001"]));

    assert_eq!(table(&out, "spectra_metadata.parquet").num_rows(), 1);

    let data = table(&out, "chromatograms_data.parquet");
    let point: StructArray = column(&data, "point");
    let idx = UInt64Array::from(point.column_by_name("chromatogram_index").unwrap().to_data());
    let mut left: HashMap<u64, u64> = HashMap::new();
    for r in 0..idx.len() {
        *left.entry(idx.value(r)).or_default() += 1;
    }
    assert_eq!(left.values().sum::<u64>(), 2, "points left in the window: {left:?}");

    let meta = table(&out, "chromatograms_metadata.parquet");
    let (index, n) = (column::<UInt64Array>(&meta, "index"), column::<UInt64Array>(&meta, "number_of_data_points"));
    for r in 0..meta.num_rows() {
        let c = index.value(r);
        assert_eq!(n.value(r), left.get(&c).copied().unwrap_or(0), "chromatogram {c}: stale number_of_data_points");
    }
    let total = footer(&out, "chromatograms_metadata.parquet", "chromatogram_data_point_count");
    assert_eq!(total.as_deref(), Some("2"), "the metadata footer total follows the truncation");
    let _ = std::fs::remove_dir_all(&dir);
}

/// (c) The same two filters through `-o f.mzML`.
#[test]
fn mzml_output_applies_the_same_filters() {
    let dir = scratch("mzml");
    let src = convert(TINY, &dir);
    let out = dir.join("f.mzML");
    for args in [["--ms-level", "2"], ["--rt", "0-0.0001"]] {
        ok(&mzpc(&src, &out, &args));
        let xml = std::fs::read_to_string(&out).unwrap();
        assert_eq!(xml.matches("<spectrum ").count(), 1, "{args:?}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// (d) `--rt` parsing, driven through the CLI (the crate has no library target to call into): an
/// omitted bound is open, and a reversed or non-numeric range exits 1 without writing anything. The
/// bounds are read back from the filter's data-processing entry. `--rt=` because clap reads a bare
/// `-30` as a flag.
#[test]
fn rt_parses_open_bounds_and_refuses_bad_ranges() {
    let dir = scratch("parse_rt");
    let src = convert(TINY, &dir);
    let out = dir.join("f.mzpeak");
    // tiny's spectra sit at 0.0, 0.70, 5.89 and 5.99 min.
    for (arg, recorded, kept) in [("10-", "rt=10-inf", 0), ("-30", "rt=-inf-30", 4)] {
        ok(&mzpc(&src, &out, &[&format!("--rt={arg}")]));
        let index = String::from_utf8(member(&out, "mzpeak_index.json")).unwrap();
        assert!(index.contains(recorded), "--rt {arg}: expected `{recorded}` in the index:\n{index}");
        assert_eq!(table(&out, "spectra_metadata.parquet").num_rows(), kept, "--rt {arg}");
    }
    for arg in ["5-1", "a-b"] {
        let _ = std::fs::remove_file(&out);
        let r = mzpc(&src, &out, &[&format!("--rt={arg}")]);
        let stderr = String::from_utf8_lossy(&r.stderr);
        assert_eq!(r.status.code(), Some(1), "--rt {arg}; stderr:\n{stderr}");
        assert!(stderr.contains("--rt"), "--rt {arg}: the error must name the flag; stderr:\n{stderr}");
        assert!(!out.exists(), "--rt {arg} wrote output");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// Item 1: an archive with wavelength spectra filters by MS level and takes an SDRF. The UV facets
/// are copied whole; they used to fail classification and abort every filter.
#[test]
fn wavelength_archive_filters_by_ms_level_and_takes_an_sdrf() {
    let dir = scratch("wavelength");
    let src = convert(PDA_UV, &dir);
    let uv = table(&src, "wavelength_spectra_metadata.parquet").num_rows();
    assert_eq!(uv, 8, "the fixture holds 8 UV spectra");
    let uv_scans = table(&src, "wavelength_spectra_metadata_scans.parquet").num_rows();

    let sdrf = dir.join("s.tsv");
    std::fs::write(&sdrf, "source name\tcomment[data file]\nsample 1\tpda_uv.raw\n").unwrap();
    let out = dir.join("f.mzpeak");
    ok(&mzpc(&src, &out, &["--ms-level", "1", "--sdrf", sdrf.to_str().unwrap()]));

    let meta = table(&out, "spectra_metadata.parquet");
    assert_eq!(meta.num_rows(), 1);
    assert_eq!(column::<UInt8Array>(&meta, "ms_level").value(0), 1);
    assert_eq!(table(&out, "wavelength_spectra_metadata.parquet").num_rows(), uv);
    assert_eq!(table(&out, "wavelength_spectra_metadata_scans.parquet").num_rows(), uv_scans);
    assert_eq!(member(&out, "sample_metadata/sdrf.tsv"), std::fs::read(&sdrf).unwrap());
    let _ = std::fs::remove_dir_all(&dir);
}

/// Item 2: `--drop-aux` must not take a core facet with it — not by name, not by a careless glob.
#[test]
fn drop_aux_refuses_to_remove_a_core_facet() {
    let dir = scratch("drop_core");
    let src = convert(TINY, &dir);
    let out = dir.join("f.mzpeak");
    for glob in ["spectra_data.parquet", "spectra_metadata_precursors.parquet", "*.parquet"] {
        let r = mzpc(&src, &out, &["--drop-aux", glob]);
        let stderr = String::from_utf8_lossy(&r.stderr);
        assert_eq!(r.status.code(), Some(1), "--drop-aux {glob} must be refused; stderr:\n{stderr}");
        assert!(!out.exists() && !dir.join("f.mzpeak.tmp").exists(), "--drop-aux {glob} wrote output");
    }
    // The refusal is about core facets, not the flag: `--no-vendor` (a `vendor*` drop) still runs.
    ok(&mzpc(&src, &out, &["--no-vendor"]));
    let _ = std::fs::remove_dir_all(&dir);
}

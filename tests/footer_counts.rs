//! Regression pin for the data-point counters in the Parquet footers.
//!
//! Both layouts got this wrong before 0.9.1, and a `.mzpeak` would declare
//! `chromatogram_data_point_count = 0` while holding thousands of rows:
//!
//! * point layout — `PointBuffers`' inherent `add_arrays` shadows the `ArrayBufferWriter` impl
//!   that did the counting, so enum dispatch landed on the inherent one and nothing incremented;
//! * chunk layout — five call sites passed `chunks.len()`, the number of chunk ROWS, as the point
//!   count.
//!
//! The tiny pwiz fixture exercises both: its chromatograms take the point layout (count == rows)
//! and its spectra take the chunk layout (count must exceed the chunk-row count).

use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::Command;

use parquet::file::reader::{FileReader, SerializedFileReader};

fn convert_fixture() -> PathBuf {
    let out = std::env::temp_dir().join(format!("mzpc-footer-{}.mzpeak", std::process::id()));
    let _ = std::fs::remove_file(&out);
    let status = Command::new(env!("CARGO_BIN_EXE_mzpeak-convert"))
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/tiny.pwiz.1.1.mzML"))
        .arg("-o")
        .arg(&out)
        .arg("--force")
        .status()
        .expect("failed to run mzpeak-convert");
    assert!(status.success(), "conversion failed: {status}");
    out
}

/// `(num_rows, declared count)` for one facet inside the archive.
fn facet(archive: &Path, member: &str, key: &str) -> (i64, i64) {
    let mut zip = zip::ZipArchive::new(File::open(archive).unwrap()).unwrap();
    let extracted = std::env::temp_dir().join(format!("mzpc-footer-{}-{member}", std::process::id()));
    {
        let mut src = zip.by_name(member).unwrap_or_else(|_| panic!("{member} missing"));
        let mut dst = File::create(&extracted).unwrap();
        std::io::copy(&mut src, &mut dst).unwrap();
    }
    let reader = SerializedFileReader::new(File::open(&extracted).unwrap()).unwrap();
    let meta = reader.metadata().file_metadata();
    let declared = meta
        .key_value_metadata()
        .and_then(|kvs| kvs.iter().find(|kv| kv.key == key))
        .and_then(|kv| kv.value.as_ref())
        .unwrap_or_else(|| panic!("{member} has no {key}"))
        .parse::<i64>()
        .unwrap();
    let rows = meta.num_rows();
    let _ = std::fs::remove_file(&extracted);
    (rows, declared)
}

#[test]
fn footer_point_counts_are_points_not_rows() {
    let archive = convert_fixture();

    // Point layout: one row IS one point, so the counter must equal the row count — and must not
    // be the zero it silently reported while the trait impl was shadowed.
    let (rows, declared) = facet(&archive, "chromatograms_data.parquet", "chromatogram_data_point_count");
    assert!(rows > 0, "fixture no longer carries chromatogram points");
    assert_eq!(
        declared, rows,
        "chromatogram_data_point_count ({declared}) != stored rows ({rows}) — the point-layout \
         counter regressed (PointBuffers::add_arrays shadowing)"
    );

    // Chunk layout: each row holds many points, so a counter equal to the row count means the
    // `chunks.len()` bug is back.
    let (rows, declared) = facet(&archive, "spectra_data.parquet", "spectrum_data_point_count");
    assert!(rows > 0, "fixture no longer carries profile chunks");
    assert!(
        declared > rows,
        "spectrum_data_point_count ({declared}) <= chunk rows ({rows}) — the chunked counter is \
         reporting chunk rows again instead of points"
    );

    let _ = std::fs::remove_file(&archive);
}

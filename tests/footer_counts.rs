//! Regression pins for the count keys in the Parquet footers.
//!
//! Two generations of defect live here:
//!
//! * before 0.9.1 the data-point counters were wrong in both layouts — `PointBuffers`' inherent
//!   `add_arrays` shadowed the `ArrayBufferWriter` impl that did the counting, and five chunk
//!   call sites passed `chunks.len()` (chunk ROWS) as the point count;
//! * before the per-facet fix (mzPeakConverter issue #1) every spectrum facet was stamped with the
//!   ARCHIVE-wide spectrum ordinal, and `spectra_data` with the SUM of both data facets' points.
//!   A centroid-only run declared every spectrum and every peak on an empty `spectra_data`, and a
//!   footer-planned reader queried hundreds of thousands of spectra that were not there.
//!
//! The definition pinned here: on a DATA facet, `<entity>_count` is the number of entities with at
//! least one row in THIS file (a cardinality — indices may be sparse — not the run total, which
//! stays on the primary metadata facet), and `<entity>_data_point_count` is the points in THIS file.
//! A spectrum handed to a facet with zero points is not an entry.
//!
//! `tiny.pwiz.1.1.mzML` has 4 spectra: index 1 is profile (10 points, chunk layout), 0 and 3 are
//! centroid (15 peaks each), 2 is an EMPTY centroid spectrum. `tiny_centroid_only.mzML` is the same
//! file without the profile spectrum (indices renumbered 0..3).

use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::Command;

use parquet::file::reader::{FileReader, SerializedFileReader};

/// `tag` keeps the parallel test threads (same pid) off each other's output file.
fn convert_fixture(name: &str, tag: &str) -> PathBuf {
    let out = std::env::temp_dir().join(format!("mzpc-footer-{}-{tag}-{name}.mzpeak", std::process::id()));
    let _ = std::fs::remove_file(&out);
    let status = Command::new(env!("CARGO_BIN_EXE_mzpeak-convert"))
        .arg(format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR")))
        .arg("-o")
        .arg(&out)
        .arg("--force")
        .status()
        .expect("failed to run mzpeak-convert");
    assert!(status.success(), "conversion of {name} failed: {status}");
    out
}

/// `(num_rows, declared value of `key`)` for one facet inside the archive.
fn facet(archive: &Path, member: &str, key: &str) -> (i64, i64) {
    let mut zip = zip::ZipArchive::new(File::open(archive).unwrap()).unwrap();
    let extracted = std::env::temp_dir().join(format!(
        "mzpc-footer-{}-{}-{member}",
        std::process::id(),
        archive.file_name().unwrap().to_string_lossy()
    ));
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

fn declared(archive: &Path, member: &str, key: &str) -> i64 {
    facet(archive, member, key).1
}

#[test]
fn footer_point_counts_are_points_not_rows() {
    let archive = convert_fixture("tiny.pwiz.1.1.mzML", "points");

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

#[test]
fn data_facet_counts_describe_this_file_not_the_run() {
    let archive = convert_fixture("tiny.pwiz.1.1.mzML", "facets");

    // The run total lives on the primary metadata facet only.
    assert_eq!(facet(&archive, "spectra_metadata.parquet", "spectrum_count"), (4, 4));

    // spectra_data holds ONE profile spectrum (index 1) with 10 points — not 4 / 40 (the run
    // total and the sum of both data facets' points).
    assert_eq!(declared(&archive, "spectra_data.parquet", "spectrum_count"), 1);
    assert_eq!(declared(&archive, "spectra_data.parquet", "spectrum_data_point_count"), 10);

    // spectra_peaks holds indices {0, 3}; the empty centroid spectrum 2 was handed to the peaks
    // writer but stored no row, so it is not an entry — 2, not 3.
    assert_eq!(declared(&archive, "spectra_peaks.parquet", "spectrum_count"), 2);
    assert_eq!(declared(&archive, "spectra_peaks.parquet", "spectrum_data_point_count"), 30);

    // chromatograms_data now carries its own entity count, same definition.
    assert_eq!(declared(&archive, "chromatograms_data.parquet", "chromatogram_count"), 2);
    assert_eq!(declared(&archive, "chromatograms_metadata.parquet", "chromatogram_count"), 2);

    let _ = std::fs::remove_file(&archive);
}

#[test]
fn empty_data_facet_declares_zero() {
    let archive = convert_fixture("tiny_centroid_only.mzML", "empty");

    // Issue #1 verbatim: a centroid-only run has an empty spectra_data. It must say so.
    assert_eq!(facet(&archive, "spectra_data.parquet", "spectrum_count"), (0, 0));
    assert_eq!(facet(&archive, "spectra_data.parquet", "spectrum_data_point_count"), (0, 0));

    // …while the peaks facet and the metadata facet keep their own, correct numbers.
    assert_eq!(declared(&archive, "spectra_peaks.parquet", "spectrum_count"), 2);
    assert_eq!(declared(&archive, "spectra_peaks.parquet", "spectrum_data_point_count"), 30);
    assert_eq!(facet(&archive, "spectra_metadata.parquet", "spectrum_count"), (3, 3));

    let _ = std::fs::remove_file(&archive);
}

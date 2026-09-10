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
//! The definition pinned here: on a DATA facet, `<entity>_count` is one past the largest
//! `<entity>_index` with at least one row in THIS file, and 0 when the file has no rows — an index
//! bound, so `0..count` reaches every entity the file holds although its indices are sparse. It is
//! not the run total, which stays on the primary metadata facet. 0.11.2–0.11.5 stamped the number
//! of entities with rows instead, and a reader bounding by that stopped early. The
//! `<entity>_data_point_count` is the points in THIS file. A spectrum handed to a facet with zero
//! points does not raise the count.
//!
//! `tiny.pwiz.1.1.mzML` has 4 spectra: index 1 is profile (10 points, chunk layout), 0 and 3 are
//! centroid (15 peaks each), 2 is an EMPTY centroid spectrum. `tiny_centroid_only.mzML` is the same
//! file without the profile spectrum (indices renumbered 0..3). `pda_uv.pwiz.mzML` (Waters PDA via
//! ProteoWizard, 2 MS spectra + 8 wavelength spectra) exercises the wavelength facets. The metadata
//! secondaries (`_scans`, `_precursors`, `_selected_ions`) carry no entity count at all.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::Command;

use parquet::file::reader::{FileReader, SerializedFileReader};

/// `tag` keeps the parallel test threads (same pid) off each other's output file.
fn convert_fixture(name: &str, tag: &str) -> PathBuf {
    convert_fixture_with(name, tag, &[])
}

fn convert_fixture_with(name: &str, tag: &str, extra: &[&str]) -> PathBuf {
    let out = std::env::temp_dir().join(format!("mzpc-footer-{}-{tag}-{name}.mzpeak", std::process::id()));
    let _ = std::fs::remove_file(&out);
    let status = Command::new(env!("CARGO_BIN_EXE_mzpeak-convert"))
        .arg(format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR")))
        .args(extra)
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
    let (rows, value) = footer_key(archive, member, key);
    (rows, value.unwrap_or_else(|| panic!("{member} has no {key}")).parse().unwrap())
}

/// `(num_rows, value of `key` if the footer has it)` for one facet inside the archive.
fn footer_key(archive: &Path, member: &str, key: &str) -> (i64, Option<String>) {
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
    let value = meta.key_value_metadata().and_then(|kvs| kvs.iter().find(|kv| kv.key == key)).and_then(|kv| kv.value.clone());
    let rows = meta.num_rows();
    let _ = std::fs::remove_file(&extracted);
    (rows, value)
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

    // spectra_data holds ONE profile spectrum, index 1, with 10 points: the bound is 2 — not 4 / 40
    // (the run total and the sum of both data facets' points), and not the 1 spectrum it holds,
    // which as a bound would stop before index 1.
    assert_eq!(declared(&archive, "spectra_data.parquet", "spectrum_count"), 2);
    assert_eq!(declared(&archive, "spectra_data.parquet", "spectrum_data_point_count"), 10);

    // spectra_peaks holds indices {0, 3}; the empty centroid spectrum 2 was handed to the peaks
    // writer but stored no row. The bound is 4: the 2 spectra it holds would stop before index 3.
    assert_eq!(declared(&archive, "spectra_peaks.parquet", "spectrum_count"), 4);
    assert_eq!(declared(&archive, "spectra_peaks.parquet", "spectrum_data_point_count"), 30);

    // chromatograms_data carries its own bound, same definition: the synthesized TIC and base peak
    // plus the source's `sic` (its `tic` is superseded by the synthesized one), indices 0..3.
    assert_eq!(declared(&archive, "chromatograms_data.parquet", "chromatogram_count"), 3);
    assert_eq!(declared(&archive, "chromatograms_metadata.parquet", "chromatogram_count"), 3);

    let _ = std::fs::remove_file(&archive);
}

#[test]
fn empty_data_facet_declares_zero() {
    let archive = convert_fixture("tiny_centroid_only.mzML", "empty");

    // Issue #1 verbatim: a centroid-only run has an empty spectra_data. It must say so.
    assert_eq!(facet(&archive, "spectra_data.parquet", "spectrum_count"), (0, 0));
    assert_eq!(facet(&archive, "spectra_data.parquet", "spectrum_data_point_count"), (0, 0));

    // …while the peaks facet and the metadata facet keep their own, correct numbers. The peaks sit at
    // indices {0, 2} around the empty spectrum 1: the bound is 3, where the 2 spectra with rows —
    // what 0.11.2–0.11.5 declared on centroid-only runs — would stop before index 2.
    assert_eq!(declared(&archive, "spectra_peaks.parquet", "spectrum_count"), 3);
    assert_eq!(declared(&archive, "spectra_peaks.parquet", "spectrum_data_point_count"), 30);
    assert_eq!(facet(&archive, "spectra_metadata.parquet", "spectrum_count"), (3, 3));

    let _ = std::fs::remove_file(&archive);
}

#[test]
fn wavelength_facets_carry_their_own_counts() {
    let archive = convert_fixture("pda_uv.pwiz.mzML", "pda");

    // The metadata facet carries the run total of wavelength spectra …
    assert_eq!(facet(&archive, "wavelength_spectra_metadata.parquet", "wavelength_spectrum_count"), (8, 8));
    // Its scans facet is a secondary and carries no count (0 on 8 rows before the drain-order fix,
    // then the run total).
    assert_eq!(footer_key(&archive, "wavelength_spectra_metadata_scans.parquet", "wavelength_spectrum_count"), (8, None));
    // The data facet: entities with rows in this file, and its own point count (point layout).
    assert_eq!(declared(&archive, "wavelength_spectra_data.parquet", "wavelength_spectrum_count"), 8);
    let (rows, points) = facet(&archive, "wavelength_spectra_data.parquet", "wavelength_spectrum_data_point_count");
    assert!(rows > 0, "fixture no longer carries wavelength points");
    assert_eq!(points, rows);

    let _ = std::fs::remove_file(&archive);
}

/// Issue #1, decision D2: the metadata secondaries carry no entity count. They carried the run total,
/// also where the facet has no rows (tiny's chromatograms have no precursors).
#[test]
fn secondary_facets_carry_no_entity_count() {
    let archive = convert_fixture("tiny.pwiz.1.1.mzML", "secondaries");
    for (member, key) in [
        ("spectra_metadata_scans.parquet", "spectrum_count"),
        ("spectra_metadata_precursors.parquet", "spectrum_count"),
        ("spectra_metadata_selected_ions.parquet", "spectrum_count"),
        ("chromatograms_metadata_precursors.parquet", "chromatogram_count"),
        ("chromatograms_metadata_selected_ions.parquet", "chromatogram_count"),
    ] {
        assert_eq!(footer_key(&archive, member, key).1, None, "{member} declares {key}");
    }
    let _ = std::fs::remove_file(&archive);
}

/// The library's unpacked (directory) writer, which mzpeak-convert does not use, stamps the same
/// data-facet counts; its `spectra_data` used to carry none at all.
#[test]
fn unpacked_writer_stamps_the_same_data_facet_counts() {
    use mzdata::prelude::*;
    let dir = std::env::temp_dir().join(format!("mzpc-footer-{}-unpacked", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    {
        let mut reader =
            mzdata::MZReader::open_path(format!("{}/tests/fixtures/tiny.pwiz.1.1.mzML", env!("CARGO_MANIFEST_DIR"))).unwrap();
        let mut writer = mzpeak_prototyping::writer::MzPeakWriterBuilder::default().build_unpacked(dir.clone(), false);
        for spectrum in reader.iter() {
            writer.write_spectrum(&spectrum).unwrap();
        }
        writer.finish().unwrap();
    }
    let kv = |member: &str, key: &str| -> i64 {
        let reader = SerializedFileReader::new(File::open(dir.join(member)).unwrap()).unwrap();
        let kvs = reader.metadata().file_metadata().key_value_metadata().cloned().unwrap_or_default();
        let kv = kvs.iter().find(|kv| kv.key == key).unwrap_or_else(|| panic!("{member} has no {key}"));
        kv.value.as_ref().unwrap().parse().unwrap()
    };
    assert_eq!(kv("spectra_data.parquet", "spectrum_count"), 2);
    assert_eq!(kv("spectra_data.parquet", "spectrum_data_point_count"), 10);
    assert_eq!(kv("spectra_peaks.parquet", "spectrum_count"), 4);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn chromatogram_data_count_is_zero_when_nothing_was_written() {
    // With synthesis off the fixture's (unindexed, unread) chromatogramList yields a metadata row
    // with no points: the DATA facet must say 0 entities / 0 points, not repeat the metadata's 1.
    // Not `tiny.pwiz.1.1.mzML`: that one is indexed, and its `tic` + `sic` are read.
    let archive = convert_fixture_with("tiny_centroid_only.mzML", "nochrom", &["--no-chromatograms"]);
    assert_eq!(facet(&archive, "chromatograms_data.parquet", "chromatogram_count"), (0, 0));
    assert_eq!(facet(&archive, "chromatograms_data.parquet", "chromatogram_data_point_count"), (0, 0));
    assert_eq!(declared(&archive, "chromatograms_metadata.parquet", "chromatogram_count"), 1);
    let _ = std::fs::remove_file(&archive);
}

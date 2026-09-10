//! An indexed mzML declaring a non-UTF-8 encoding lost every chromatogram, with exit code 0.
//!
//! mzdata's reader is UTF-8 only, so such an input is transcoded into a UTF-8 temp copy first. That
//! rewrite changes byte lengths — `encoding="ISO-8859-1"` becomes the five-bytes-shorter
//! `encoding="UTF-8"`, and every high byte becomes two — so every offset in the copy's
//! `<indexList>` and its `<indexListOffset>` pointed at the wrong byte. mzdata failed to read the
//! index, fell back to a scan that finds spectra, and could no longer enumerate chromatograms, which
//! it reaches only through that index. The fixture is ISO-8859-1 `<indexedmzML>` with chromatograms
//! `tic` and `sic`; `sic` (10 points) is the one no TIC/BPC synthesis brings back.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::Command;

use parquet::file::reader::{FileReader, SerializedFileReader};
use parquet::record::Field;

const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/tiny.pwiz.1.1.mzML");

/// A per-test scratch dir: cargo runs these tests in parallel inside one process.
fn scratch(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("mzpc-latin1idx-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Convert and return stderr (the log), asserting success.
fn convert(input: &Path, output: &Path) -> String {
    let r = Command::new(env!("CARGO_BIN_EXE_mzpeak-convert"))
        .arg(input)
        .arg("-o")
        .arg(output)
        .arg("--force")
        .output()
        .expect("failed to run mzpeak-convert");
    let log = String::from_utf8_lossy(&r.stderr).into_owned();
    assert!(r.status.success(), "conversion to {} failed: {log}", output.display());
    log
}

/// `(id, number_of_data_points)` for every chromatogram in the archive.
fn chromatograms(archive: &Path, dir: &Path) -> Vec<(String, u64)> {
    let mut zip = zip::ZipArchive::new(File::open(archive).unwrap()).unwrap();
    let member = dir.join("chromatograms_metadata.parquet");
    std::io::copy(
        &mut zip.by_name("chromatograms_metadata.parquet").unwrap(),
        &mut File::create(&member).unwrap(),
    )
    .unwrap();
    let reader = SerializedFileReader::new(File::open(&member).unwrap()).unwrap();
    reader
        .get_row_iter(None)
        .unwrap()
        .map(|row| {
            let row = row.unwrap();
            let (mut id, mut points) = (String::new(), 0);
            for (name, field) in row.get_column_iter() {
                match (name.as_str(), field) {
                    ("id", Field::Str(s)) => id = s.clone(),
                    ("number_of_data_points", Field::ULong(n)) => points = *n,
                    _ => {}
                }
            }
            (id, points)
        })
        .collect()
}

/// Both lanes must carry `sic` with its 10 points out of `input`.
fn assert_sic_survives(input: &Path, dir: &Path) -> String {
    let archive = dir.join("out.mzpeak");
    let log = convert(input, &archive);
    let chroms = chromatograms(&archive, dir);
    assert!(
        chroms.contains(&("sic".to_string(), 10)),
        "mzPeak lane lost the source's `sic` chromatogram: {chroms:?}\n{log}"
    );

    let mzml = dir.join("out.mzML");
    let log = convert(input, &mzml);
    let xml = std::fs::read_to_string(&mzml).unwrap();
    let tag = xml
        .find(r#"id="sic""#)
        .map(|at| &xml[xml[..at].rfind('<').unwrap()..at + xml[at..].find('>').unwrap()])
        .unwrap_or_else(|| panic!("mzML lane lost the source's `sic` chromatogram\n{log}"));
    assert!(tag.contains(r#"defaultArrayLength="10""#), "sic lost points: {tag}");
    xml
}

#[test]
fn latin1_indexed_mzml_keeps_its_chromatograms() {
    let dir = scratch("decl");
    assert_sic_survives(Path::new(FIXTURE), &dir);
    let _ = std::fs::remove_dir_all(&dir);
}

/// High bytes widen to two in UTF-8, so offsets after them shift by a different amount than the
/// declaration's constant five: only a rebuilt index gets every one right.
#[test]
fn latin1_high_bytes_keep_the_chromatograms_and_decode() {
    let dir = scratch("high");
    let src = std::fs::read(FIXTURE).unwrap();
    // Same byte count in Latin-1, so the source's own index stays valid: é ø ä ä.
    let from: &[u8] = b"spectrum with no data";
    let to: &[u8] = b"sp\xE9ctrum with n\xF8 d\xE4t\xE4";
    let at = src.windows(from.len()).position(|w| w == from).expect("fixture userParam moved");
    let mut latin1 = src.clone();
    latin1[at..at + from.len()].copy_from_slice(to);
    let input = dir.join("tiny-highbytes.mzML");
    std::fs::write(&input, &latin1).unwrap();

    let xml = assert_sic_survives(&input, &dir);
    assert!(xml.contains("spéctrum with nø dätä"), "Latin-1 userParam not decoded to UTF-8");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A reader that yields fewer chromatograms than `<chromatogramList count>` declares must say so.
/// UTF-8 declared (no transcode, so nothing rebuilds the index) with `<indexListOffset>` five bytes
/// late: the exact stale index the transcode used to leave behind.
#[test]
fn unreadable_chromatograms_are_reported() {
    let dir = scratch("warn");
    let src = String::from_utf8(std::fs::read(FIXTURE).unwrap()).unwrap();
    let broken = src
        .replacen(r#"encoding="ISO-8859-1"?>"#, r#"encoding="UTF-8"     ?>"#, 1)
        .replacen("<indexListOffset>24498<", "<indexListOffset>24503<", 1);
    assert_eq!(broken.len(), src.len());
    assert_ne!(broken, src, "fixture declaration or index offset moved");
    let input = dir.join("tiny-staleindex.mzML");
    std::fs::write(&input, broken).unwrap();

    for out in ["out.mzpeak", "out.mzML"] {
        let log = convert(&input, &dir.join(out));
        assert!(
            log.contains("declares 2 chromatograms but only 0"),
            "{out}: no warning for the unread chromatograms\n{log}"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

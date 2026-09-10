//! An indexed mzML with an empty `<referenceableParamGroup id="…"/>` lost every chromatogram, with
//! exit code 0.
//!
//! mzdata panics when such a group is referenced (ProteomeDiscoverer emits them), so the input is read
//! from a sanitized copy in which each empty group is written as an open/close pair. Only the header
//! changes, but it grows, so every `<offset>` in the copy's `<indexList>` and its `<indexListOffset>`
//! pointed short of the element they name. mzdata failed to read the index, fell back to a scan that
//! finds spectra, and could no longer enumerate chromatograms, which it reaches only through that
//! index. `sic` (10 points) is the chromatogram no TIC/BPC synthesis brings back.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::Command;

use parquet::file::reader::{FileReader, SerializedFileReader};
use parquet::record::Field;

const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/tiny.pwiz.1.1.mzML");

/// A per-test scratch dir: cargo runs tests in parallel inside one process.
fn scratch(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("mzpc-emptygroup-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Convert with debug logging and return stderr (the log), asserting success.
fn convert(input: &Path, output: &Path) -> String {
    let r = Command::new(env!("CARGO_BIN_EXE_mzpeak-convert"))
        .arg(input)
        .arg("-o")
        .arg(output)
        .arg("--force")
        .env("RUST_LOG", "debug")
        .output()
        .expect("failed to run mzpeak-convert");
    let log = String::from_utf8_lossy(&r.stderr).into_owned();
    assert!(r.status.success(), "conversion to {} failed: {log}", output.display());
    assert!(
        log.contains("sanitized empty referenceableParamGroup"),
        "the reader was not handed the sanitized copy, so this pins nothing\n{log}"
    );
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

/// The fixture declared UTF-8 (same length, so nothing is transcoded) with an empty group in its
/// header, and its own index recomputed by id, so the SOURCE is a valid indexed mzML.
fn input_with_empty_group(dir: &Path) -> PathBuf {
    let src = std::fs::read_to_string(FIXTURE).unwrap();
    let (list, group) = (r#"<referenceableParamGroupList count="2">"#, r#"<referenceableParamGroup id="EmptyGroup"/>"#);
    let doc = src
        .replacen(r#"encoding="ISO-8859-1"?>"#, r#"encoding="UTF-8"     ?>"#, 1)
        .replacen(list, &format!("{list}{group}"), 1);
    assert_eq!(doc.len(), src.len() + group.len(), "fixture declaration or group list moved");
    assert!(!doc.contains("ISO-8859-1"), "fixture declaration moved");

    let (body, tail) = doc.split_at(doc.find("<indexList ").unwrap());
    let start_tag = |id: &str| {
        body.match_indices(&format!(r#" id="{id}""#))
            .map(|(i, _)| body[..i].rfind('<').unwrap())
            .find(|&t| body[t..].starts_with("<spectrum ") || body[t..].starts_with("<chromatogram "))
            .unwrap_or_else(|| panic!("no element with id {id}"))
    };
    let mut rebuilt = body.to_string();
    for piece in tail.split_inclusive("</offset>") {
        if let Some(stated) = piece.strip_suffix("</offset>") {
            let gt = stated.rfind('>').unwrap() + 1;
            let id = stated[stated.rfind(r#"idRef=""#).unwrap() + 7..].split('"').next().unwrap();
            rebuilt += &format!("{}{}</offset>", &stated[..gt], start_tag(id));
        } else {
            let (a, b) = (piece.find("<indexListOffset>").unwrap() + 17, piece.find("</indexListOffset>").unwrap());
            rebuilt += &format!("{}{}{}", &piece[..a], body.len(), &piece[b..]);
        }
    }
    let input = dir.join("tiny-emptygroup.mzML");
    std::fs::write(&input, rebuilt).unwrap();
    input
}

#[test]
fn empty_param_group_keeps_the_indexed_chromatograms() {
    let dir = scratch("sic");
    let input = input_with_empty_group(&dir);

    let archive = dir.join("out.mzpeak");
    let log = convert(&input, &archive);
    let chroms = chromatograms(&archive, &dir);
    assert!(
        chroms.contains(&("sic".to_string(), 10)),
        "mzPeak lane lost the source's `sic` chromatogram: {chroms:?}\n{log}"
    );

    let mzml = dir.join("out.mzML");
    let log = convert(&input, &mzml);
    let xml = std::fs::read_to_string(&mzml).unwrap();
    let tag = xml
        .find(r#"id="sic""#)
        .map(|at| &xml[xml[..at].rfind('<').unwrap()..at + xml[at..].find('>').unwrap()])
        .unwrap_or_else(|| panic!("mzML lane lost the source's `sic` chromatogram\n{log}"));
    assert!(tag.contains(r#"defaultArrayLength="10""#), "sic lost points: {tag}");
    let _ = std::fs::remove_dir_all(&dir);
}

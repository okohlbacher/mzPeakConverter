//! A non-indexed mzML's chromatograms were never read.
//!
//! mzdata reaches an mzML's chromatograms only through the `<indexList>` of an `<indexedmzML>`, so a
//! plain `<mzML>` gave none: 614 chromatograms over 68 corpus archives, SRM and device traces among
//! them, exit 0 behind a warning, synthesized TIC/BPC in their place. The converter now scans the
//! `<chromatogramList>` and hands mzdata the index the file lacks (`recover_chromatogram_index`).
//!
//! The fixture is `pda_uv.pwiz.mzML`: non-indexed, with a source TIC and four device traces that no
//! synthesis brings back. Each test edits its own copy in a scratch directory of its own.

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use parquet::file::reader::{FileReader, SerializedFileReader};
use parquet::record::Field;

const PDA_UV: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/pda_uv.pwiz.mzML");
const TINY: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/tiny.pwiz.1.1.mzML");

/// The source's device traces and their points: none is a TIC or BPC, so only the source has them.
const TRACES: [(&str, u64); 4] =
    [("System Pressure", 901), ("A", 91), ("(1) CM Selected Column Temp", 90), ("(3) ELSD Signal", 901)];

/// A scratch directory for ONE test: cargo runs a binary's tests in parallel under one process id.
fn scratch(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("mzpc-nonindexed-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// `mzpeak-convert <input> [-o <output> --force]`, asserting success.
fn run(input: &Path, output: Option<&Path>) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_mzpeak-convert"));
    cmd.arg(input);
    if let Some(output) = output {
        cmd.arg("-o").arg(output).arg("--force");
    }
    let r = cmd.output().expect("failed to run mzpeak-convert");
    assert!(r.status.success(), "{} failed ({:?}):\n{}", input.display(), r.status.code(), log(&r));
    r
}

fn log(r: &Output) -> String {
    String::from_utf8_lossy(&r.stderr).into_owned()
}

/// The fixture's text, and a copy of it in `dir` with `edit` applied.
fn edited(dir: &Path, name: &str, edit: impl FnOnce(&str) -> String) -> PathBuf {
    let src = std::fs::read_to_string(PDA_UV).unwrap();
    let out = edit(&src);
    assert_ne!(out, src, "{name}: the edit changed nothing, so the fixture moved");
    let path = dir.join(name);
    std::fs::write(&path, out).unwrap();
    path
}

/// `(id, number_of_data_points, the chromatogram_type field as parquet shows it)` for every
/// chromatogram in an archive.
fn archive_chromatograms(archive: &Path) -> Vec<(String, u64, String)> {
    let mut zip = zip::ZipArchive::new(File::open(archive).unwrap()).unwrap();
    let member = archive.with_extension("chromatograms_metadata.parquet");
    std::io::copy(&mut zip.by_name("chromatograms_metadata.parquet").unwrap(), &mut File::create(&member).unwrap())
        .unwrap();
    let reader = SerializedFileReader::new(File::open(&member).unwrap()).unwrap();
    reader
        .get_row_iter(None)
        .unwrap()
        .map(|row| {
            let row = row.unwrap();
            let (mut id, mut points, mut kind) = (String::new(), 0, String::new());
            for (name, field) in row.get_column_iter() {
                match (name.as_str(), field) {
                    ("id", Field::Str(s)) => id = s.clone(),
                    ("number_of_data_points", Field::ULong(n)) => points = *n,
                    ("chromatogram_type", f) => kind = format!("{f:?}"),
                    _ => {}
                }
            }
            (id, points, kind)
        })
        .collect()
}

fn has(chroms: &[(String, u64, String)], id: &str, points: u64) -> bool {
    chroms.iter().any(|(i, p, _)| i == id && *p == points)
}

/// `(id, defaultArrayLength)` for every chromatogram in an mzML.
fn mzml_chromatograms(mzml: &Path) -> Vec<(String, usize)> {
    let xml = std::fs::read_to_string(mzml).unwrap();
    xml.match_indices("<chromatogram ")
        .map(|(at, _)| {
            let tag = &xml[at..at + xml[at..].find('>').unwrap()];
            let attr = |name: &str| {
                tag.split(&format!("{name}=\"")).nth(1).and_then(|v| v.split('"').next()).unwrap_or_default().to_string()
            };
            (attr("id"), attr("defaultArrayLength").parse().unwrap())
        })
        .collect()
}

#[test]
fn both_lanes_read_a_nonindexed_mzmls_chromatograms() {
    let dir = scratch("lanes");
    let archive = dir.join("pda.mzpeak");
    let r = run(Path::new(PDA_UV), Some(&archive));
    let chroms = archive_chromatograms(&archive);
    for (id, points) in TRACES {
        assert!(has(&chroms, id, points), "the archive lost {id} ({points} points): {chroms:?}\n{}", log(&r));
    }
    assert!(!log(&r).contains("could be read"), "{}", log(&r));

    let mzml = dir.join("pda.mzML");
    let r = run(Path::new(PDA_UV), Some(&mzml));
    let chroms = mzml_chromatograms(&mzml);
    for (id, points) in TRACES {
        assert!(chroms.contains(&(id.to_string(), points as usize)), "the mzML lost {id}: {chroms:?}\n{}", log(&r));
    }
    assert!(std::fs::read_to_string(&mzml).unwrap().contains(r#"<chromatogramList count="6""#), "{chroms:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_gzipped_nonindexed_mzml_keeps_its_chromatograms() {
    let dir = scratch("gz");
    let gz = dir.join("pda.mzML.gz");
    let mut enc = flate2::write::GzEncoder::new(File::create(&gz).unwrap(), flate2::Compression::default());
    enc.write_all(&std::fs::read(PDA_UV).unwrap()).unwrap();
    enc.finish().unwrap();
    let archive = dir.join("pda.mzpeak");
    let r = run(&gz, Some(&archive));
    let chroms = archive_chromatograms(&archive);
    assert!(TRACES.iter().all(|(id, p)| has(&chroms, id, *p)), "{chroms:?}\n{}", log(&r));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_inspection_report_counts_them() {
    let r = run(Path::new(PDA_UV), None);
    let report = String::from_utf8_lossy(&r.stdout);
    assert!(report.contains("chromatograms: 5"), "{report}");
}

/// A file of chromatograms alone, an SRM run's shape: its TIC is the only one there is. The mzML
/// lane dropped it for the writer's own TIC over no spectra, written empty.
#[test]
fn a_chromatogram_only_file_keeps_its_own_tic() {
    let dir = scratch("chromatograms-only");
    let input = edited(&dir, "chromatograms-only.mzML", |s| {
        let (from, to) = (s.find("<spectrumList").unwrap(), s.find("</spectrumList>").unwrap() + "</spectrumList>".len());
        format!("{}{}", &s[..from], &s[to..])
    });

    let archive = dir.join("chromatograms-only.mzpeak");
    let r = run(&input, Some(&archive));
    let chroms = archive_chromatograms(&archive);
    assert!(has(&chroms, "TIC", 2360), "{chroms:?}\n{}", log(&r));
    assert_eq!(chroms.len(), 5, "{chroms:?}");

    let mzml = dir.join("chromatograms-only.out.mzML");
    let r = run(&input, Some(&mzml));
    let chroms = mzml_chromatograms(&mzml);
    assert!(chroms.contains(&("TIC".to_string(), 2360)), "{chroms:?}\n{}", log(&r));
    assert!(chroms.iter().all(|(_, n)| *n > 0), "an empty summary chromatogram was written: {chroms:?}");
    assert_eq!(chroms.len(), 5, "{chroms:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A `<chromatogram>` in a comment, an escaped tag in an attribute value, and a real element with
/// its id first, single-quoted, entity-escaped and broken over two lines.
#[test]
fn the_scan_takes_only_elements_for_chromatograms() {
    let dir = scratch("lookalikes");
    let input = edited(&dir, "lookalikes.mzML", |s| {
        s.replacen(
            r#"<chromatogram index="0" id="TIC""#,
            "<!-- <chromatogram index=\"99\" id=\"fake-in-comment\" defaultArrayLength=\"0\"> -->\n      <chromatogram index=\"0\" id=\"TIC\"",
            1,
        )
        .replacen(r#"<chromatogram index="2" id="A""#, "<chromatogram\n        id='A&gt;B \"q\" &amp; >lit' index=\"2\"", 1)
        .replacen(
            r#"<chromatogram index="1" id="System Pressure" defaultArrayLength="901">"#,
            "<chromatogram index=\"1\" id=\"System Pressure\" defaultArrayLength=\"901\">\n        <userParam name=\"note\" value=\"&lt;chromatogram index=&quot;7&quot; id=&quot;fake-in-attr&quot;&gt;\"/>",
            1,
        )
    });
    let archive = dir.join("lookalikes.mzpeak");
    let r = run(&input, Some(&archive));
    let chroms = archive_chromatograms(&archive);
    assert!(chroms.iter().all(|(id, _, _)| !id.contains("fake")), "{chroms:?}");
    // A byte search would index the commented tag too: 6 for 5 elements, and mzdata would parse the
    // commented one on into TIC, which the synthesized TIC then hides.
    assert!(log(&r).contains("5 chromatograms located by scanning"), "{}", log(&r));
    assert_eq!(chroms.len(), 6, "the synthesized TIC and BPC, and the four traces: {chroms:?}");
    for (id, points) in [("System Pressure", 901), (r#"A>B "q" & >lit"#, 91), ("(1) CM Selected Column Temp", 90), ("(3) ELSD Signal", 901)] {
        assert!(has(&chroms, id, points), "lost {id}: {chroms:?}\n{}", log(&r));
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// mzdata's index keeps one position per id, so the earlier of two chromatograms named alike lost
/// its data even in an indexed file.
#[test]
fn a_repeated_id_keeps_both_chromatograms() {
    let dir = scratch("repeated");
    let input = edited(&dir, "repeated.mzML", |s| s.replacen(r#"id="(3) ELSD Signal""#, r#"id="A""#, 1));
    let archive = dir.join("repeated.mzpeak");
    let r = run(&input, Some(&archive));
    let chroms = archive_chromatograms(&archive);
    assert!(has(&chroms, "A", 91) && has(&chroms, "A", 901), "{chroms:?}\n{}", log(&r));
    assert!(log(&r).contains("occurs more than once"), "{}", log(&r));
    let _ = std::fs::remove_dir_all(&dir);
}

/// `iter_chromatograms` ended at the first chromatogram that failed to parse.
#[test]
fn one_unreadable_chromatogram_does_not_hide_the_rest() {
    let dir = scratch("unreadable");
    let input = edited(&dir, "unreadable.mzML", |s| {
        let end = s.find(r#"id="A""#).and_then(|a| s[a..].find("</binaryDataArrayList>").map(|b| a + b)).unwrap();
        format!("{}</binaryDataArrayLisX>{}", &s[..end], &s[end + "</binaryDataArrayList>".len()..])
    });
    let archive = dir.join("unreadable.mzpeak");
    let r = run(&input, Some(&archive));
    let chroms = archive_chromatograms(&archive);
    for (id, points) in [("System Pressure", 901), ("(1) CM Selected Column Temp", 90), ("(3) ELSD Signal", 901)] {
        assert!(has(&chroms, id, points), "lost {id}: {chroms:?}\n{}", log(&r));
    }
    assert!(log(&r).contains("declares 5 chromatograms but only 4"), "{}", log(&r));

    let mzml = dir.join("unreadable.out.mzML");
    let r = run(&input, Some(&mzml));
    let chroms = mzml_chromatograms(&mzml);
    for (id, points) in [("System Pressure", 901), ("(1) CM Selected Column Temp", 90), ("(3) ELSD Signal", 901)] {
        assert!(chroms.contains(&(id.to_string(), points)), "the mzML lost {id}: {chroms:?}\n{}", log(&r));
    }
    assert!(log(&r).contains("declares 5 chromatograms but only 4"), "{}", log(&r));
    let _ = std::fs::remove_dir_all(&dir);
}

/// A streaming writer that never learned the count writes `count="0"` (OpenMS's consumer does):
/// the list's elements are what counts.
#[test]
fn a_declared_count_of_zero_is_not_believed() {
    let dir = scratch("count-zero");
    let input = edited(&dir, "count-zero.mzML", |s| s.replacen(r#"<chromatogramList count="5""#, r#"<chromatogramList count="0""#, 1));
    let archive = dir.join("count-zero.mzpeak");
    let r = run(&input, Some(&archive));
    let chroms = archive_chromatograms(&archive);
    assert!(TRACES.iter().all(|(id, p)| has(&chroms, id, *p)), "{chroms:?}\n{}", log(&r));
    assert!(log(&r).contains("declares 0 chromatograms but holds 5"), "{}", log(&r));
    let _ = std::fs::remove_dir_all(&dir);
}

/// A file cut off inside its chromatogramList keeps the chromatograms before the cut.
#[test]
fn a_truncated_list_keeps_what_comes_before_the_cut() {
    let dir = scratch("truncated");
    let input = edited(&dir, "truncated.mzML", |s| {
        let at = s.find(r#"<chromatogram index="4""#).unwrap();
        s[..at + 20].to_string()
    });
    let archive = dir.join("truncated.mzpeak");
    let r = run(&input, Some(&archive));
    let chroms = archive_chromatograms(&archive);
    for (id, points) in [("System Pressure", 901), ("A", 91), ("(1) CM Selected Column Temp", 90)] {
        assert!(has(&chroms, id, points), "lost {id}: {chroms:?}\n{}", log(&r));
    }
    assert!(log(&r).contains("not well-formed"), "{}", log(&r));
    let _ = std::fs::remove_dir_all(&dir);
}

/// mzdata `expect`s a chromatogram's `index` attribute, and this binary aborts on panic: one such
/// element must cost that chromatogram, not the conversion.
#[test]
fn a_non_integer_index_attribute_costs_only_that_chromatogram() {
    let dir = scratch("bad-index");
    let input = edited(&dir, "bad-index.mzML", |s| {
        s.replacen(r#"<chromatogram index="1" id="System Pressure""#, r#"<chromatogram index="one" id="System Pressure""#, 1)
    });
    let archive = dir.join("bad-index.mzpeak");
    let r = run(&input, Some(&archive));
    let chroms = archive_chromatograms(&archive);
    for (id, points) in [("A", 91), ("(1) CM Selected Column Temp", 90), ("(3) ELSD Signal", 901)] {
        assert!(has(&chroms, id, points), "lost {id}: {chroms:?}\n{}", log(&r));
    }
    assert!(log(&r).contains("\"System Pressure\" is left out"), "{}", log(&r));
    let _ = std::fs::remove_dir_all(&dir);
}

/// An index that parses but points one byte past a chromatogram read the wrong bytes, and the mzPeak
/// lanes lost the trace without a warning.
#[test]
fn a_stale_chromatogram_offset_is_recovered() {
    let dir = scratch("stale-offset");
    let src = std::fs::read_to_string(TINY).unwrap();
    let stale = src
        .replacen(r#"encoding="ISO-8859-1"?>"#, r#"encoding="UTF-8"     ?>"#, 1)
        .replacen(r#"<offset idRef="sic">22253</offset>"#, r#"<offset idRef="sic">22254</offset>"#, 1);
    assert_eq!(stale.len(), src.len());
    assert!(stale.contains("22254</offset>"), "the fixture's sic offset moved");
    let input = dir.join("stale-offset.mzML");
    std::fs::write(&input, stale).unwrap();

    let archive = dir.join("stale-offset.mzpeak");
    let r = run(&input, Some(&archive));
    assert!(has(&archive_chromatograms(&archive), "sic", 10), "{}", log(&r));
    let mzml = dir.join("stale-offset.out.mzML");
    let r = run(&input, Some(&mzml));
    assert!(mzml_chromatograms(&mzml).contains(&("sic".to_string(), 10)), "{}", log(&r));
    let _ = std::fs::remove_dir_all(&dir);
}

/// mzdata's writer, like ThermoRawFileParser, indexes the indentation before each element. That
/// index is good and is kept, not rescanned.
#[test]
fn an_index_at_the_indentation_is_kept() {
    let dir = scratch("indentation");
    let mzml = dir.join("tiny.mzML");
    run(Path::new(TINY), Some(&mzml));
    let archive = dir.join("tiny.mzpeak");
    let r = run(&mzml, Some(&archive));
    assert!(!log(&r).contains("located by scanning"), "{}", log(&r));
    assert!(has(&archive_chromatograms(&archive), "sic", 10), "{}", log(&r));
    let _ = std::fs::remove_dir_all(&dir);
}

/// mzdata writes an SRM chromatogram as MS:1000473, which PSI-MS defines as an Agilent instrument and
/// which mzdata reads back as nothing, so the export wrote the trace with no type at all.
#[test]
fn an_srm_trace_keeps_the_srm_chromatogram_term() {
    let dir = scratch("srm");
    let input = edited(&dir, "srm.mzML", |s| {
        let at = s.find(r#"id="A""#).unwrap();
        let term = r#"accession="MS:1000811" name="electromagnetic radiation chromatogram""#;
        let t = at + s[at..].find(term).unwrap();
        format!("{}accession=\"MS:1001473\" name=\"selected reaction monitoring chromatogram\"{}", &s[..t], &s[t + term.len()..])
    });
    let archive = dir.join("srm.mzpeak");
    let r = run(&input, Some(&archive));
    let chroms = archive_chromatograms(&archive);
    let kind = &chroms.iter().find(|(id, _, _)| id == "A").unwrap_or_else(|| panic!("no A: {chroms:?}\n{}", log(&r))).2;
    assert!(kind.contains("1001473"), "A is stored as {kind}");

    let export = dir.join("srm.export.mzML");
    run(&archive, Some(&export));
    let xml = std::fs::read_to_string(&export).unwrap();
    let a = xml.find(r#"id="A""#).unwrap();
    assert!(xml[a..a + xml[a..].find("</chromatogram>").unwrap()].contains(r#"accession="MS:1001473""#), "the export wrote A untyped");
    let _ = std::fs::remove_dir_all(&dir);
}

/// quick-xml reads past a byte-order mark without counting it, so a scan from byte 0 records every
/// offset three bytes early; and an id byte that is not UTF-8 has to stay a readable element.
#[test]
fn a_byte_order_mark_and_a_latin1_id_byte_lose_nothing() {
    let dir = scratch("bom");
    let src = std::fs::read(PDA_UV).unwrap();
    let (from, to): (&[u8], &[u8]) = (br#"id="System Pressure""#, b"id=\"System Pressur\xE9\"");
    let at = src.windows(from.len()).position(|w| w == from).unwrap();
    let mut body = src[..at].to_vec();
    body.extend_from_slice(to);
    body.extend_from_slice(&src[at + from.len()..]);
    // Unindented, so an offset three bytes early lands inside the previous element, not on the
    // whitespace mzdata would skip.
    let indented: &[u8] = b"\n      <chromatogram ";
    assert_eq!(body.windows(indented.len()).filter(|w| *w == indented).count(), 5, "the fixture's indentation moved");
    let body = String::from_utf8_lossy(&body).replace("\n      <chromatogram ", "\n<chromatogram ");
    let body = body.replace("System Pressur\u{FFFD}", "System Pressur");
    let mut bytes = b"\xEF\xBB\xBF".to_vec();
    let at = body.find(r#"id="System Pressur""#).unwrap() + r#"id="System Pressur"#.len();
    bytes.extend_from_slice(&body.as_bytes()[..at]);
    bytes.push(0xE9);
    bytes.extend_from_slice(&body.as_bytes()[at..]);
    let input = dir.join("bom.mzML");
    std::fs::write(&input, bytes).unwrap();

    let archive = dir.join("bom.mzpeak");
    let r = run(&input, Some(&archive));
    let chroms = archive_chromatograms(&archive);
    for (id, points) in [("A", 91), ("(1) CM Selected Column Temp", 90), ("(3) ELSD Signal", 901)] {
        assert!(has(&chroms, id, points), "lost {id}: {chroms:?}\n{}", log(&r));
    }
    assert!(chroms.iter().any(|(id, p, _)| id.starts_with("System Pressur") && *p == 901), "{chroms:?}");
    assert!(log(&r).contains("5 chromatograms located by scanning"), "{}", log(&r));
    let _ = std::fs::remove_dir_all(&dir);
}

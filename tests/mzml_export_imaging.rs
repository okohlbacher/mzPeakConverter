//! The header of an mzML export: the vocabularies it declares, its `<scanSettingsList>` and its
//! `<fileContent>`, on the direct lane (`convert_to_mzml`) and on the export of an archive
//! (`filter_mzpeak_to_mzml`). Through 0.16.0 (HUPO-PSI/mzPeak-specification#23):
//!
//!   * an imaging run's pixel positions were written as `cvRef="IMS"` params while the `<cvList>`
//!     declared MS and UO alone — a document naming a vocabulary it does not declare, which the
//!     mzML schema's own key reference rejects;
//!   * no export had a `<scanSettingsList>`, so an imaging run lost its grid (pixel counts, pixel
//!     size) and any run its inclusion list;
//!   * the export of an archive had an empty `<fileContent>`, whatever the archive's
//!     `file_description.contents` stated.
//!
//! mzdata's writer cannot write the first two as the schema has them; `src/mzml_header.rs` rewrites
//! the header under it and moves the index's offsets by what that adds, which these tests check on
//! the files the binary writes.

use std::path::{Path, PathBuf};
use std::process::Command;

use mzdata::prelude::*;

const TINY: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/tiny.pwiz.1.1.mzML");
const IMZML: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/imaging/Synthetic_DeclaredGrid");
const IMS_URI: &str = "https://raw.githubusercontent.com/imzML/imzML/2c28b05ca297430303627d8c7d192cac1a2b1374/imagingMS.obo";

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("mzpc-export-imaging-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn convert(input: &Path, output: &Path) {
    let out = Command::new(env!("CARGO_BIN_EXE_mzpeak-convert")).arg(input).arg("-o").arg(output).arg("--force").output().unwrap();
    assert!(out.status.success(), "{} -> {}: {}", input.display(), output.display(), String::from_utf8_lossy(&out.stderr));
}

/// The text of an mzML, gunzipped when its name ends in `.gz`.
fn text(mzml: &Path) -> String {
    let bytes = std::fs::read(mzml).unwrap();
    if mzml.extension().is_some_and(|e| e == "gz") {
        let mut s = String::new();
        std::io::Read::read_to_string(&mut flate2::read::GzDecoder::new(&bytes[..]), &mut s).unwrap();
        s
    } else {
        String::from_utf8(bytes).unwrap()
    }
}

/// The value of attribute `name` of the element text `element`.
fn attr<'a>(element: &'a str, name: &str) -> Option<&'a str> {
    let key = format!(" {name}=\"");
    let from = element.find(&key)? + key.len();
    Some(&element[from..from + element[from..].find('"')?])
}

/// Each `<tag …>` start tag (or empty element) of `xml`.
fn tags<'a>(xml: &'a str, tag: &str) -> Vec<&'a str> {
    let open = format!("<{tag} ");
    xml.match_indices(&open).map(|(at, _)| &xml[at..at + xml[at..].find('>').unwrap() + 1]).collect()
}

/// The inside of the first `<tag>…</tag>` of `xml`.
fn inside<'a>(xml: &'a str, tag: &str) -> Option<&'a str> {
    let from = xml.find(&format!("<{tag}"))?;
    let from = from + xml[from..].find('>')? + 1;
    Some(&xml[from..from + xml[from..].find(&format!("</{tag}>"))?])
}

/// What every mzML header must satisfy, whatever it holds: the `<cvList>` is counted, each
/// vocabulary a param names is declared in it, the `<scanSettingsList>` is counted and its source
/// file references are attributes naming a listed source file — and the index points at its
/// elements, which a rewritten header would have moved. Returns the header (up to `<run `).
fn check_document(doc: &str, what: &str) -> String {
    let header = &doc[..doc.find("<run ").unwrap()];
    let declared: Vec<&str> = tags(header, "cv").iter().map(|cv| attr(cv, "id").unwrap()).collect();
    assert_eq!(attr(&tags(header, "cvList")[0], "count").unwrap().parse::<usize>().unwrap(), declared.len(), "{what}: cvList count");
    for key in ["cvRef", "unitCvRef"] {
        let pattern = format!(" {key}=\"");
        for (at, _) in doc.match_indices(&pattern) {
            let used = &doc[at + pattern.len()..];
            let used = &used[..used.find('"').unwrap()];
            assert!(declared.contains(&used), "{what}: {key}=\"{used}\" names a vocabulary the cvList does not declare ({declared:?})");
        }
    }
    if let Some(list) = tags(header, "scanSettingsList").first() {
        assert_eq!(attr(list, "count").unwrap().parse::<usize>().unwrap(), tags(header, "scanSettings").len(), "{what}: scanSettingsList count");
    }
    assert!(!header.contains("<sourceFileRef>"), "{what}: a source file reference written as text");
    let source_files: Vec<&str> = tags(header, "sourceFile").iter().map(|sf| attr(sf, "id").unwrap()).collect();
    for r in tags(header, "sourceFileRef") {
        assert!(source_files.contains(&attr(r, "ref").unwrap()), "{what}: {r} names no listed source file ({source_files:?})");
    }
    // The index.
    let mut offsets = 0;
    for (at, open) in doc.match_indices("<offset idRef=\"") {
        let (id, rest) = doc[at + open.len()..].split_once("\">").unwrap();
        let offset: usize = rest[..rest.find('<').unwrap()].parse().unwrap();
        let element = doc[offset..].trim_start();
        assert!(element.starts_with("<spectrum ") || element.starts_with("<chromatogram "), "{what}: offset of {id} points at {:?}", &element[..element.len().min(30)]);
        assert_eq!(attr(&element[..element.find('>').unwrap()], "id"), Some(id), "{what}: offset of {id}");
        offsets += 1;
    }
    assert!(offsets > 0, "{what}: no index");
    let list_offset: usize = inside(doc, "indexListOffset").unwrap().parse().unwrap();
    assert!(doc[list_offset..].trim_start().starts_with("<indexList "), "{what}: indexListOffset");
    header.to_string()
}

/// `accession → value` of the `<cvParam>`s in `xml`.
fn cv_params(xml: &str) -> Vec<(&str, Option<&str>)> {
    tags(xml, "cvParam").iter().map(|p| (attr(p, "accession").unwrap(), attr(p, "value"))).collect()
}

/// An imzML and its archive, exported to mzML: both declare the imaging vocabulary, pinned as the
/// archive's `cv_list` pins it, carry the grid in a `<scanSettingsList>` and the file's content —
/// its imzML provenance included — in `<fileContent>`; mzdata reads both back with every position;
/// and an archive made from the export has its grid as DECLARED counts, where it had to count the
/// positions.
#[test]
fn an_imaging_export_declares_ims_and_carries_its_grid() {
    let dir = scratch("imzml");
    for ext in ["imzML", "ibd"] {
        std::fs::copy(format!("{IMZML}.{ext}"), dir.join(format!("grid.{ext}"))).unwrap();
    }
    let archive = dir.join("grid.mzpeak");
    convert(&dir.join("grid.imzML"), &archive);
    let (direct, exported, zipped) = (dir.join("direct.mzML"), dir.join("exported.mzML"), dir.join("exported.mzML.gz"));
    convert(&dir.join("grid.imzML"), &direct);
    convert(&archive, &exported);
    convert(&archive, &zipped);

    for (path, what) in [(&direct, "imzML → mzML"), (&exported, "archive → mzML"), (&zipped, "archive → mzML.gz")] {
        let doc = text(path);
        let header = check_document(&doc, what);
        assert!(doc.matches(" cvRef=\"IMS\"").count() >= 18, "{what}: positions and grid are IMS params");
        // The vocabulary: declared once, pinned to the commit.
        let ims: Vec<&str> = tags(&header, "cv").into_iter().filter(|cv| attr(cv, "id") == Some("IMS")).collect();
        assert_eq!(ims.len(), 1, "{what}: {header}");
        assert_eq!((attr(ims[0], "URI"), attr(ims[0], "version"), attr(ims[0], "fullName")), (Some(IMS_URI), Some("1.1.0"), Some("Imaging Mass Spectrometry Ontology")), "{what}");
        assert_eq!(attr(&tags(&header, "cvList")[0], "count"), Some("3"), "{what}");
        // The grid.
        let settings = inside(&header, "scanSettingsList").unwrap_or_else(|| panic!("{what}: no scanSettingsList\n{header}"));
        let grid = cv_params(settings);
        for (accession, value) in [("IMS:1000042", "3"), ("IMS:1000043", "3"), ("IMS:1000046", "100"), ("IMS:1000047", "100")] {
            assert!(grid.contains(&(accession, Some(value))), "{what}: {accession} = {value} missing from {grid:?}");
        }
        assert!(tags(settings, "cvParam").iter().any(|p| attr(p, "accession") == Some("IMS:1000046") && attr(p, "unitAccession") == Some("UO:0000017")), "{what}: the pixel size keeps its unit");
        // The file's content: what it holds, and the imzML provenance (storage mode, UUID, checksum).
        let content = cv_params(inside(&header, "fileContent").unwrap());
        for expected in [
            ("MS:1000579", None),
            ("IMS:1000031", None),
            ("IMS:1000080", Some("{1a2b3c4d-5e6f-7081-9203-b4c5d6e7f8a9}")),
            ("IMS:1000091", Some("fd5c5dae18095ba7ab55a6ad1bd1175180b292a8")),
        ] {
            assert!(content.contains(&expected), "{what}: {expected:?} missing from fileContent {content:?}");
        }
    }

    // mzdata reads the export back: the scan settings, and every pixel's position.
    for path in [&direct, &exported] {
        let mut reader = mzdata::io::mzml::MzMLReader::open_path(path).unwrap();
        let settings = reader.scan_settings().unwrap().clone();
        assert_eq!(settings.len(), 1);
        assert_eq!(settings[0].get_param_by_curie(&mzdata::curie!(IMS:1000042)).unwrap().value.to_i64().unwrap(), 3);
        let positions: Vec<(i64, i64)> = reader
            .iter()
            .map(|s| {
                let scan = &s.acquisition().scans[0];
                let v = |c| scan.get_param_by_curie(&c).unwrap().value.to_i64().unwrap();
                (v(mzdata::curie!(IMS:1000050)), v(mzdata::curie!(IMS:1000051)))
            })
            .collect();
        assert_eq!(positions.len(), 9, "{}", path.display());
        assert!(positions.contains(&(1, 1)) && positions.contains(&(3, 3)), "{positions:?}");
    }

    // The export is an imaging run again, now with the grid it was given.
    let back = dir.join("back.mzpeak");
    convert(&exported, &back);
    let index = |a: &Path| -> serde_json::Value {
        let mut zip = zip::ZipArchive::new(std::fs::File::open(a).unwrap()).unwrap();
        serde_json::from_reader(zip.by_name("mzpeak_index.json").unwrap()).unwrap()
    };
    let (was, is) = (index(&archive)["metadata"].clone(), index(&back)["metadata"].clone());
    assert_eq!(is["imaging"]["pixel_count"], serde_json::json!({"x": 3, "y": 3}));
    assert_eq!(is["imaging"]["pixel_count_source"], "declared", "the export states the counts: {:#}", is["imaging"]);
    assert_eq!(is["imaging"]["pixel_size_um"], was["imaging"]["pixel_size_um"]);
    let accessions = |m: &serde_json::Value| -> Vec<String> {
        m["file_description"]["contents"].as_array().unwrap().iter().filter_map(|p| p["accession"].as_str().map(str::to_string)).filter(|a| a.starts_with("IMS:")).collect()
    };
    assert_eq!(accessions(&is), accessions(&was), "the imzML provenance survives archive → mzML → archive");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A run that is not imaging: its scan settings (tiny.pwiz states an inclusion list of two targets
/// and the source file its parameters came from) are exported as the schema has them, its archive's
/// export states the file's content, and neither declares a vocabulary it does not use.
#[test]
fn scan_settings_and_file_content_of_a_plain_run_are_exported() {
    let dir = scratch("tiny");
    let archive = dir.join("tiny.mzpeak");
    convert(Path::new(TINY), &archive);
    let (direct, exported) = (dir.join("direct.mzML"), dir.join("exported.mzML"));
    convert(Path::new(TINY), &direct);
    convert(&archive, &exported);

    for (path, what) in [(&direct, "mzML → mzML"), (&exported, "archive → mzML")] {
        let doc = text(path);
        let header = check_document(&doc, what);
        assert_eq!(attr(&tags(&header, "cvList")[0], "count"), Some("2"), "{what}: MS and UO, as before");
        assert!(!doc.contains("IMS"), "{what}");
        let list = &tags(&header, "scanSettingsList")[0];
        assert_eq!(attr(list, "count"), Some("1"), "{what}");
        let settings = inside(&header, "scanSettingsList").unwrap();
        let targets: Vec<_> = cv_params(inside(settings, "targetList").unwrap()).into_iter().filter(|(a, _)| *a == "MS:1000744").collect();
        assert_eq!(targets, [("MS:1000744", Some("1000")), ("MS:1000744", Some("1200"))], "{what}");
        assert!(!cv_params(inside(&header, "fileContent").unwrap()).is_empty(), "{what}: an empty fileContent");
    }
    // The direct export lists the source's files, so the reference stays, as an attribute; the
    // archive's export lists the archive alone, so it states none.
    let header = text(&direct);
    assert!(header.contains("<sourceFileRefList count=\"1\">") && header.contains("<sourceFileRef ref=\"sf_parameters\"/>"), "{}", inside(&header, "scanSettingsList").unwrap());
    assert!(!text(&exported).contains("<sourceFileRef"));
    // mzdata reads the list back as the source states it.
    let source = mzdata::io::mzml::MzMLReader::open_path(TINY).unwrap().scan_settings().unwrap().clone();
    let back = mzdata::io::mzml::MzMLReader::open_path(&direct).unwrap().scan_settings().unwrap().clone();
    assert_eq!((back.len(), &back[0].id, &back[0].source_file_refs, back[0].targets.len()), (1, &source[0].id, &source[0].source_file_refs, source[0].targets.len()));
    assert_eq!(back[0].source_file_refs, ["sf_parameters"]);
    let _ = std::fs::remove_dir_all(&dir);
}

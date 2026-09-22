//! What an archive says about its RUN — the software that made it, the processing it went
//! through, the instrument it came off, the files it was made from — on the way out again.
//!
//! An archive keeps all of that in its index (`mzpeak_index.json` → `metadata`), but the vendored
//! reader never read it back: `ParquetIndexExtractor::mz_metadata` stayed empty, so
//! `copy_metadata_from(&reader)` copied nothing and every `.mzpeak` → mzML export came out with
//! `<softwareList count="0">`, `<dataProcessingList count="0">`, one blank instrument
//! configuration, and the `.mzpeak` itself as its only source file — the source's own provenance
//! dropped on the floor. These tests pin the restored lists, and the two invariants that only
//! start to matter once a list is carried across rather than rebuilt: **every id in an mzML is
//! unique in its document, and every reference resolves**. Both are checked mechanically on every
//! lane here, including after a second pass through the tool (a re-conversion, a second filter),
//! which is where a fixed id would collide with the one it inherited.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const TINY: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/tiny.pwiz.1.1.mzML");

/// A fresh directory for ONE test (the tests in a binary share a process id).
fn scratch(test: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("mzpc-run-metadata-{}-{test}", std::process::id()));
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

fn run(input: &Path, output: &Path, extra: &[&str]) -> PathBuf {
    let r = mzpc(input, output, extra);
    assert!(
        r.status.success(),
        "{} → {} {extra:?} failed with {:?}:\n{}",
        input.display(),
        output.display(),
        r.status.code(),
        String::from_utf8_lossy(&r.stderr)
    );
    output.to_path_buf()
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

/// The archive index's `metadata` object.
fn index_metadata(archive: &Path) -> serde_json::Value {
    let index: serde_json::Value = serde_json::from_slice(&member(archive, "mzpeak_index.json")).unwrap();
    index.get("metadata").cloned().expect("the index states its metadata")
}

/// The `id` of every entry of an index metadata list, in order.
fn ids_of(meta: &serde_json::Value, list: &str) -> Vec<String> {
    meta.get(list)
        .and_then(|v| v.as_array())
        .unwrap_or_else(|| panic!("the index has no `{list}`"))
        .iter()
        .map(|e| e["id"].as_str().unwrap_or_default().to_string())
        .collect()
}

/// Every `software_reference` an index metadata's processing methods and instrument configurations
/// name must be one of its `software_list` ids, and no id may repeat across the two lists.
fn index_ids_are_unique_and_resolve(archive: &Path) {
    let meta = index_metadata(archive);
    let softwares = ids_of(&meta, "software_list");
    let processings = ids_of(&meta, "data_processing_method_list");
    let mut seen = BTreeSet::new();
    for id in softwares.iter().chain(processings.iter()) {
        assert!(seen.insert(id.clone()), "{}: id {id} is used twice: {softwares:?} {processings:?}", archive.display());
    }
    let refs = meta["data_processing_method_list"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|dp| dp["methods"].as_array().cloned().unwrap_or_default())
        .filter_map(|m| m["software_reference"].as_str().map(str::to_string))
        .chain(
            meta["instrument_configuration_list"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|ic| ic["software_reference"].as_str().map(str::to_string))
                // An instrument whose acquisition software nobody stated names none.
                .filter(|r| !r.is_empty()),
        );
    for r in refs {
        assert!(softwares.contains(&r), "{}: software_reference {r} resolves to nothing in {softwares:?}", archive.display());
    }
}

// ── the mzML side ───────────────────────────────────────────────────────────────────────────────

/// The ids an mzML declares (`xs:ID`, by the element that declares them) and the references it
/// makes to them (`xs:IDREF`, by element and attribute). Spectrum and chromatogram ids are neither:
/// they are plain strings in mzML 1.1, and `scan/@spectrumRef` names one.
struct Ids {
    declared: BTreeMap<String, Vec<String>>,
    references: Vec<(String, String, String)>,
}

/// The elements whose `id` is an `xs:ID`.
const DECLARE: [&str; 9] = [
    "cv",
    "sourceFile",
    "referenceableParamGroup",
    "sample",
    "software",
    "scanSettings",
    "instrumentConfiguration",
    "dataProcessing",
    "run",
];

/// `(element, attribute)` of every `xs:IDREF` mzML 1.1 has.
const REFERENCE: [(&str, &str); 14] = [
    ("run", "defaultInstrumentConfigurationRef"),
    ("run", "defaultSourceFileRef"),
    ("run", "sampleRef"),
    ("spectrumList", "defaultDataProcessingRef"),
    ("chromatogramList", "defaultDataProcessingRef"),
    ("softwareRef", "ref"),
    ("sourceFileRef", "ref"),
    ("referenceableParamGroupRef", "ref"),
    ("processingMethod", "softwareRef"),
    ("scan", "instrumentConfigurationRef"),
    ("spectrum", "dataProcessingRef"),
    ("spectrum", "sourceFileRef"),
    ("chromatogram", "dataProcessingRef"),
    ("binaryDataArray", "dataProcessingRef"),
];

fn read_ids(mzml: &Path) -> Ids {
    use quick_xml::events::Event;
    let text = std::fs::read_to_string(mzml).unwrap();
    let mut xml = quick_xml::Reader::from_str(&text);
    let mut this = Ids { declared: BTreeMap::new(), references: Vec::new() };
    loop {
        let (element, attributes) = match xml.read_event() {
            Ok(Event::Start(tag)) | Ok(Event::Empty(tag)) => (
                String::from_utf8_lossy(tag.name().as_ref()).into_owned(),
                tag.attributes()
                    .flatten()
                    .map(|a| {
                        (
                            String::from_utf8_lossy(a.key.as_ref()).into_owned(),
                            String::from_utf8_lossy(&a.value).into_owned(),
                        )
                    })
                    .collect::<Vec<_>>(),
            ),
            Ok(Event::Eof) => break,
            Ok(_) => continue,
            Err(e) => panic!("{}: {e}", mzml.display()),
        };
        for (key, value) in attributes {
            if key == "id" && DECLARE.contains(&element.as_str()) {
                this.declared.entry(element.clone()).or_default().push(value);
            } else if REFERENCE.contains(&(element.as_str(), key.as_str())) {
                this.references.push((element.clone(), key, value));
            }
        }
    }
    this
}

impl Ids {
    fn of(&self, element: &str) -> &[String] {
        self.declared.get(element).map(Vec::as_slice).unwrap_or(&[])
    }

    /// Every declared id is unique in the document and every reference names one of them. An EMPTY
    /// reference is passed over: mzdata's writer emits `<softwareRef ref=""/>` for an instrument
    /// configuration whose acquisition software nobody stated, on every lane, and that is not this
    /// change's to fix — it writes the element unconditionally, and there is no software to name.
    fn unique_and_resolving(&self, what: &str) {
        let mut seen = BTreeSet::new();
        for (element, ids) in self.declared.iter() {
            for id in ids {
                assert!(seen.insert(id.clone()), "{what}: <{element} id={id:?}> repeats an id");
            }
        }
        for (element, attribute, value) in self.references.iter().filter(|(_, _, v)| !v.is_empty()) {
            assert!(
                seen.contains(value),
                "{what}: <{element} {attribute}={value:?}> resolves to nothing ({:?})",
                seen
            );
        }
    }
}

/// `tiny.pwiz.1.1.mzML` with ProteoWizard's escaping on a software id, as its Waters and Shimadzu
/// examples carry it (`MassLynx_x0020_software`): the archive holds the DECODED id, and the mzML
/// export has to escape it again or write an id that is not an XML name.
fn tiny_with_an_escaped_software_id(dir: &Path) -> PathBuf {
    let text = std::fs::read_to_string(TINY).unwrap();
    let text = text
        .replace("id=\"CompassXtract\"", "id=\"Compass_x0020_Xtract\"")
        .replace("softwareRef=\"CompassXtract\"", "softwareRef=\"Compass_x0020_Xtract\"")
        .replace("ref=\"CompassXtract\"", "ref=\"Compass_x0020_Xtract\"");
    let path = dir.join("escaped.mzML");
    std::fs::write(&path, text).unwrap();
    path
}

/// Copy `archive` to `out` with its index edited.
fn with_edited_index(archive: &Path, out: &Path, edit: impl FnOnce(&mut serde_json::Value)) {
    let mut index: serde_json::Value = serde_json::from_slice(&member(archive, "mzpeak_index.json")).unwrap();
    edit(&mut index);
    let mut zin = zip::ZipArchive::new(File::open(archive).unwrap()).unwrap();
    let mut zout = zip::ZipWriter::new(File::create(out).unwrap());
    for i in 0..zin.len() {
        let f = zin.by_index_raw(i).unwrap();
        if f.name() == "mzpeak_index.json" {
            continue;
        }
        zout.raw_copy_file(f).unwrap();
    }
    let stored = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    zout.start_file("mzpeak_index.json", stored).unwrap();
    zout.write_all(serde_json::to_string(&index).unwrap().as_bytes()).unwrap();
    zout.finish().unwrap();
}

// ── the lanes ───────────────────────────────────────────────────────────────────────────────────

/// `.mzpeak` → mzML carries the SOURCE's provenance: its three software entries and two processing
/// methods, its instrument configuration (with the software that ran it), its source files — and
/// the entry of the conversion that wrote the archive. Every id unique, every reference resolving.
#[test]
fn an_export_carries_the_sources_software_processing_and_instrument() {
    let dir = scratch("export");
    let archive = run(Path::new(TINY), &dir.join("tiny.mzpeak"), &[]);
    let mzml = run(&archive, &dir.join("tiny.mzML"), &["--to", "mzml"]);
    let text = std::fs::read_to_string(&mzml).unwrap();
    let ids = read_ids(&mzml);

    let softwares = ids.of("software");
    for want in ["Bioworks", "pwiz", "CompassXtract"] {
        assert!(softwares.contains(&want.to_string()), "the source's software {want} is missing from {softwares:?}");
    }
    assert!(softwares.contains(&"mzpeak-convert".to_string()), "the conversion's own software is missing from {softwares:?}");

    let processings = ids.of("dataProcessing");
    for want in ["CompassXtract_x0020_processing", "pwiz_processing", "mzpeak_convert_conversion"] {
        assert!(processings.contains(&want.to_string()), "dataProcessing {want} is missing from {processings:?}");
    }

    // The instrument is the source's, not the blank one this lane used to mint, and it names the
    // software that ran it.
    assert_eq!(ids.of("instrumentConfiguration").len(), 1, "one instrument configuration");
    assert!(text.contains("accession=\"MS:1000554\""), "the LCQ Deca the source states");
    assert!(text.contains("value=\"23433\""), "the instrument serial the source states");
    assert!(
        ids.references.contains(&("softwareRef".into(), "ref".into(), "CompassXtract".into())),
        "the instrument configuration names CompassXtract: {:?}",
        ids.references
    );

    // The source files are the source's, digests and all; the archive is not one of them — no lane
    // records its own input over the members the input already states.
    let sources: Vec<String> = text
        .match_indices("<sourceFile ")
        .map(|(at, _)| text[at..].split("name=\"").nth(1).unwrap().split('"').next().unwrap().to_string())
        .collect();
    assert_eq!(sources, ["tiny1.yep", "tiny.wiff", "parameters.par"], "the source's own members");
    assert!(
        ids.references.contains(&("run".into(), "defaultSourceFileRef".into(), "tiny1.yep".into())),
        "the run's default source file is the source's"
    );

    // mzML's ParamGroup takes its cvParams before its userParams. The archive stores this tool's
    // own step the other way round (the `conversion options` userParam, then the cvParam the
    // vendored writer adds behind it), and a list written in the order it is held is one XSD error.
    let step = text
        .split("<dataProcessing id=\"mzpeak_convert_conversion\"")
        .nth(1)
        .expect("the conversion's own step")
        .split("</dataProcessing>")
        .next()
        .unwrap();
    assert!(
        step.find("<cvParam").unwrap() < step.find("<userParam").unwrap(),
        "the step's cvParam comes before its userParam:{step}"
    );

    ids.unique_and_resolving("the export of an archive");
}

/// A ProteoWizard-escaped software id is decoded into the archive and escaped again on the way out,
/// with every reference to it following: an mzML id must be an XML name.
#[test]
fn an_escaped_software_id_comes_back_escaped() {
    let dir = scratch("escaped");
    let source = tiny_with_an_escaped_software_id(&dir);
    let archive = run(&source, &dir.join("escaped.mzpeak"), &[]);
    let meta = index_metadata(&archive);
    assert!(
        ids_of(&meta, "software_list").contains(&"Compass Xtract".to_string()),
        "the archive holds the decoded id: {:?}",
        ids_of(&meta, "software_list")
    );

    let mzml = run(&archive, &dir.join("escaped.out.mzML"), &["--to", "mzml"]);
    let ids = read_ids(&mzml);
    assert!(
        ids.of("software").contains(&"Compass_x0020_Xtract".to_string()),
        "the export escapes it again: {:?}",
        ids.of("software")
    );
    ids.unique_and_resolving("the export of an archive with an escaped software id");
}

/// Converting an export back into an archive must not collide with the entries that export carries:
/// this tool's software and processing ids were fixed strings, and an inherited list already holds
/// them. Two round trips, so the second conversion inherits the first one's entry.
#[test]
fn a_re_conversion_keeps_the_ids_unique() {
    let dir = scratch("reconvert");
    let first = run(Path::new(TINY), &dir.join("first.mzpeak"), &[]);
    let exported = run(&first, &dir.join("first.mzML"), &["--to", "mzml"]);
    let second = run(&exported, &dir.join("second.mzpeak"), &[]);

    let meta = index_metadata(&second);
    let processings = ids_of(&meta, "data_processing_method_list");
    assert!(
        processings.contains(&"mzpeak_convert_conversion".to_string())
            && processings.contains(&"mzpeak_convert_conversion_2".to_string()),
        "the inherited step and this conversion's own are both there: {processings:?}"
    );
    assert_eq!(
        ids_of(&meta, "software_list").iter().filter(|id| id.starts_with("mzpeak-convert")).count(),
        1,
        "the same version's software entry is reused, not duplicated: {:?}",
        ids_of(&meta, "software_list")
    );
    index_ids_are_unique_and_resolve(&second);

    let again = run(&second, &dir.join("second.mzML"), &["--to", "mzml"]);
    read_ids(&again).unique_and_resolving("the export of a re-converted archive");
}

/// The `.mzpeak` → `.mzpeak` filter lane keeps the lists the index holds and adds its own step —
/// under an id nothing else in the index holds, naming a software entry that is in the list.
#[test]
fn the_filter_lane_keeps_the_lists_and_its_own_ids_unique() {
    let dir = scratch("filter");
    let archive = run(Path::new(TINY), &dir.join("tiny.mzpeak"), &[]);
    let once = run(&archive, &dir.join("once.mzpeak"), &["--ms-level", "1"]);

    let meta = index_metadata(&once);
    let softwares = ids_of(&meta, "software_list");
    for want in ["Bioworks", "pwiz", "CompassXtract", "mzpeak-convert"] {
        assert!(softwares.contains(&want.to_string()), "the filtered archive keeps software {want}: {softwares:?}");
    }
    let processings = ids_of(&meta, "data_processing_method_list");
    for want in ["CompassXtract_x0020_processing", "pwiz_processing", "mzpeak_convert_conversion", "mzpeak_convert_filter"] {
        assert!(processings.contains(&want.to_string()), "the filtered archive states {want}: {processings:?}");
    }
    index_ids_are_unique_and_resolve(&once);

    // Filtering a filtered archive: the step's id may not be the one it inherited.
    let twice = run(&once, &dir.join("twice.mzpeak"), &["--ms-level", "1"]);
    let processings = ids_of(&index_metadata(&twice), "data_processing_method_list");
    assert!(
        processings.contains(&"mzpeak_convert_filter".to_string())
            && processings.contains(&"mzpeak_convert_filter_2".to_string()),
        "both filter steps are recorded: {processings:?}"
    );
    index_ids_are_unique_and_resolve(&twice);

    let mzml = run(&twice, &dir.join("twice.mzML"), &["--to", "mzml"]);
    let ids = read_ids(&mzml);
    assert!(ids.of("dataProcessing").contains(&"mzpeak_convert_filter_2".to_string()), "the filter steps reach the mzML");
    ids.unique_and_resolving("the export of a twice-filtered archive");
}

/// An index whose run-level metadata is absent, or does not parse, must not fail an export that
/// worked before any of it was read: the archive opens with the empty metadata the reader always
/// returned, and the lane synthesises the source file and instrument configuration it always did.
/// A half-read index is not an option — its blocks name one another.
#[test]
fn an_unreadable_index_leaves_the_export_as_it_was() {
    let dir = scratch("older");
    let archive = run(Path::new(TINY), &dir.join("tiny.mzpeak"), &[]);

    // A zone-less acquisition time, as a writer that does not know RFC 3339 offsets would leave it.
    let broken = dir.join("broken.mzpeak");
    with_edited_index(&archive, &broken, |index| {
        index["metadata"]["run"]["start_time"] = serde_json::json!("2007-06-27T15:23:45");
    });
    let out = dir.join("broken.mzML");
    let r = mzpc(&broken, &out, &["--to", "mzml"]);
    assert!(r.status.success(), "an unreadable index must not fail the export:\n{}", String::from_utf8_lossy(&r.stderr));
    assert!(
        String::from_utf8_lossy(&r.stderr).contains("mzpeak_index.json"),
        "the export says the metadata could not be read:\n{}",
        String::from_utf8_lossy(&r.stderr)
    );
    let ids = read_ids(&out);
    // Nothing of the archive's own is restored. What the export writes about ITSELF is another
    // matter and not asserted here, so the test holds however that grows.
    let restored: Vec<&String> = ids
        .of("software")
        .iter()
        .chain(ids.of("dataProcessing"))
        .filter(|id| ["Bioworks", "pwiz", "CompassXtract", "pwiz_processing", "CompassXtract_x0020_processing"].contains(&id.as_str()))
        .collect();
    assert!(restored.is_empty(), "nothing is restored from an index that does not parse: {restored:?}");
    ids.unique_and_resolving("the export of an archive with an unreadable index");

    // Nothing at all: the export is the one this lane wrote before any of it was read.
    let bare = dir.join("bare.mzpeak");
    with_edited_index(&archive, &bare, |index| index["metadata"] = serde_json::json!({}));
    let mzml = run(&bare, &dir.join("bare.mzML"), &["--to", "mzml"]);
    let ids = read_ids(&mzml);
    assert_eq!(ids.of("instrumentConfiguration").len(), 1, "the synthesised instrument configuration");
    assert_eq!(ids.of("sourceFile"), ["sourceFile"], "the archive itself, as any input with no stated source");
    ids.unique_and_resolving("the export of an archive whose index states no metadata");
}

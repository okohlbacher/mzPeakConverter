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

    /// The value of the first `<element attribute=…>` reference in the document.
    fn reference(&self, element: &str, attribute: &str) -> Option<&str> {
        self.references.iter().find(|(e, a, _)| e == element && a == attribute).map(|(_, _, v)| v.as_str())
    }

    /// Every declared id is an XML name, as an `xs:ID` is. `run` is passed over: mzdata's writer
    /// hardcodes `<run id="1">`, which is not one, on every lane it writes — pre-existing, and not
    /// this change's to fix.
    fn are_xml_names(&self, what: &str) {
        for (element, ids) in self.declared.iter().filter(|(e, _)| e.as_str() != "run") {
            for id in ids {
                let mut bytes = id.bytes();
                let ok = bytes.next().is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
                    && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'.');
                assert!(ok, "{what}: <{element} id={id:?}> is not an XML name");
            }
        }
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

/// Of the processing entries an ARCHIVE brought into an export, the one its run block declares
/// comes first, and no other of the archive's entries is named as a list's default.
///
/// That is the whole of what `front_run_defaults` promises, and it is stated this way on purpose:
/// mzdata's writer reads `defaultDataProcessingRef` off `data_processings.first()`, so a lane that
/// records a step of ITS OWN puts that step at the head and states it — which is a different claim
/// ("this export converted the file"), not a wrong one. `origin/fix/mzml-dataprocessing-im-order`
/// does exactly that (`add_mzml_conversion_step` inserts `mzpeak_convert_to_mzml` at index 0), and
/// what must not happen either way is the export naming one of the ARCHIVE's own steps that the
/// archive does not call its default — the vendor's extraction step standing in for the conversion.
fn the_archives_default_comes_first(ids: &Ids, declared: &str, from_the_archive: &[&str]) {
    let first = ids.of("dataProcessing").iter().find(|id| from_the_archive.contains(&id.as_str()));
    assert_eq!(
        first.map(String::as_str),
        Some(declared),
        "of the entries the archive brought, the one it declares comes first: {:?}",
        ids.of("dataProcessing")
    );
    for list in ["spectrumList", "chromatogramList"] {
        if let Some(named) = ids.reference(list, "defaultDataProcessingRef") {
            assert!(
                named == declared || !from_the_archive.contains(&named),
                "<{list} defaultDataProcessingRef={named:?}> is one of the archive's entries, and \
                 not the one the archive declares ({declared:?})"
            );
        }
    }
}

/// The entries the archive of `tiny.pwiz.1.1.mzML` brings to an export: the source's two, and the
/// step of the conversion that wrote the archive.
const FROM_THE_ARCHIVE: [&str; 3] = ["pwiz_processing", "CompassXtract_x0020_processing", "mzpeak_convert_conversion"];

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

/// An id an archive holds is a plain string; an mzML id is an `xs:ID`. The native SCIEX, Waters
/// and Bruker lanes name a source file after a file on disk, so `20230830 sample.wiff` — a space
/// and a leading digit, ordinary Analyst naming — is a source file id in any archive one of them
/// wrote. Those ids reached no mzML while this lane synthesised its own single `<sourceFile>`;
/// carrying them across means escaping them, with the run's default that names one.
#[test]
fn an_id_that_is_no_xml_name_is_escaped_on_the_way_out() {
    let dir = scratch("xmlnames");
    let archive = run(Path::new(TINY), &dir.join("tiny.mzpeak"), &[]);
    let sciex = dir.join("sciex_ids.mzpeak");
    with_edited_index(&archive, &sciex, |index| {
        let meta = &mut index["metadata"];
        meta["file_description"]["source_files"][0]["id"] = serde_json::json!("20230830 sample.wiff");
        meta["file_description"]["source_files"][0]["name"] = serde_json::json!("20230830 sample.wiff");
        meta["file_description"]["source_files"][1]["id"] = serde_json::json!("20230830 sample.wiff.scan");
        meta["run"]["default_source_file_id"] = serde_json::json!("20230830 sample.wiff");
        // A sample and a processing step named the same way, for the other two id kinds this lane
        // now carries out of an index.
        meta["sample_list"][0]["id"] = serde_json::json!("Sample 1 (a)");
        meta["data_processing_method_list"][0]["id"] = serde_json::json!("1st processing");
        meta["run"]["default_data_processing_id"] = serde_json::json!("1st processing");
    });

    let mzml = run(&sciex, &dir.join("sciex_ids.mzML"), &["--to", "mzml"]);
    let ids = read_ids(&mzml);
    ids.are_xml_names("the export of an archive with vendor-named source files");
    ids.unique_and_resolving("the export of an archive with vendor-named source files");
    assert!(
        ids.of("sourceFile").contains(&"_x0032_0230830_x0020_sample.wiff".to_string()),
        "the source file id is escaped, not dropped: {:?}",
        ids.of("sourceFile")
    );
    assert_eq!(
        ids.reference("run", "defaultSourceFileRef"),
        Some("_x0032_0230830_x0020_sample.wiff"),
        "the run's default names the escaped id"
    );
    assert!(
        ids.of("dataProcessing").contains(&"_x0031_st_x0020_processing".to_string()),
        "the data processing id is escaped, not dropped: {:?}",
        ids.of("dataProcessing")
    );
    the_archives_default_comes_first(
        &ids,
        "_x0031_st_x0020_processing",
        &["_x0031_st_x0020_processing", "pwiz_processing", "mzpeak_convert_conversion"],
    );
    // The NAME stays as the archive states it — only the id is an XML name.
    let text = std::fs::read_to_string(&mzml).unwrap();
    assert!(text.contains("name=\"20230830 sample.wiff\""), "the file's own name is untouched");
}

/// An export states what the ARCHIVE calls its defaults. mzdata's writer takes
/// `defaultDataProcessingRef` and `defaultSourceFileRef` from the first entry of their list and
/// never reads the run block, and an index holds its lists oldest-first — so an export that only
/// copied them claimed the spectra came out of the first processing the source ever recorded
/// (`CompassXtract_x0020_processing`: deisotoping, charge deconvolution, peak picking) instead of
/// the `pwiz_processing` the source declares. Asserted three times: on the archive as written, on
/// one whose run block names a different entry (so this pins the run block being honoured and not
/// an accident of order), and on one whose run block names nothing that is there.
#[test]
fn the_export_states_the_defaults_the_archive_declares() {
    let dir = scratch("defaults");
    let archive = run(Path::new(TINY), &dir.join("tiny.mzpeak"), &[]);
    let meta = index_metadata(&archive);
    assert_eq!(
        meta["run"]["default_data_processing_id"].as_str(),
        Some("pwiz_processing"),
        "the archive keeps the source's default"
    );
    assert_ne!(
        ids_of(&meta, "data_processing_method_list").first().map(String::as_str),
        Some("pwiz_processing"),
        "…and does not hold it first, or this test would pass on the order alone"
    );

    let mzml = run(&archive, &dir.join("tiny.mzML"), &["--to", "mzml"]);
    let ids = read_ids(&mzml);
    the_archives_default_comes_first(&ids, "pwiz_processing", &FROM_THE_ARCHIVE);
    assert_eq!(ids.reference("run", "defaultSourceFileRef"), Some("tiny1.yep"));
    ids.unique_and_resolving("an export that states the archive's defaults");

    // The run block, not the order: name the last entry of each list instead.
    let moved = dir.join("moved.mzpeak");
    with_edited_index(&archive, &moved, |index| {
        index["metadata"]["run"]["default_data_processing_id"] = serde_json::json!("mzpeak_convert_conversion");
        index["metadata"]["run"]["default_source_file_id"] = serde_json::json!("sf_parameters");
    });
    let ids = read_ids(&run(&moved, &dir.join("moved.mzML"), &["--to", "mzml"]));
    the_archives_default_comes_first(&ids, "mzpeak_convert_conversion", &FROM_THE_ARCHIVE);
    assert_eq!(ids.reference("run", "defaultSourceFileRef"), Some("sf_parameters"));
    ids.unique_and_resolving("an export whose archive names other defaults");

    // A default that names nothing is dropped rather than written: the lane fills it from the list,
    // as it does for an input that states none.
    let dangling = dir.join("dangling.mzpeak");
    with_edited_index(&archive, &dangling, |index| {
        index["metadata"]["run"]["default_data_processing_id"] = serde_json::json!("no_such_processing");
        index["metadata"]["run"]["default_source_file_id"] = serde_json::json!("no_such_file");
    });
    let ids = read_ids(&run(&dangling, &dir.join("dangling.mzML"), &["--to", "mzml"]));
    ids.unique_and_resolving("an export whose archive names a default that is not there");
}

/// An index that PARSES but does not hold together is dropped whole, like one that does not parse:
/// `as_file_metadata` reads the keys that are there, one at a time, so an index missing its
/// `software_list` hands back the processing methods and the instrument configuration that name
/// its entries — the half-read state the all-or-none rule exists to prevent. Three shapes, each of
/// which put a dangling `IDREF` or a repeated `xs:ID` in the mzML before this check: no
/// `software_list`, an empty one, and a `data_processing_method_list` holding an entry twice.
#[test]
fn an_index_that_does_not_hold_together_leaves_the_export_as_it_was() {
    let dir = scratch("inconsistent");
    let archive = run(Path::new(TINY), &dir.join("tiny.mzpeak"), &[]);

    let breakages: [(&str, fn(&mut serde_json::Value)); 3] = [
        ("no_software", |index| {
            index["metadata"].as_object_mut().unwrap().remove("software_list");
        }),
        ("empty_software", |index| {
            index["metadata"]["software_list"] = serde_json::json!([]);
        }),
        ("repeated_processing", |index| {
            let first = index["metadata"]["data_processing_method_list"][0].clone();
            index["metadata"]["data_processing_method_list"].as_array_mut().unwrap().push(first);
        }),
    ];
    for (name, edit) in breakages {
        let broken = dir.join(format!("{name}.mzpeak"));
        with_edited_index(&archive, &broken, edit);
        let out = dir.join(format!("{name}.mzML"));
        let r = mzpc(&broken, &out, &["--to", "mzml"]);
        assert!(r.status.success(), "{name}: an inconsistent index must not fail the export:\n{}", String::from_utf8_lossy(&r.stderr));
        let stderr = String::from_utf8_lossy(&r.stderr);
        assert!(stderr.contains("mzpeak_index.json"), "{name}: the export says why it read none of it:\n{stderr}");

        let ids = read_ids(&out);
        let restored: Vec<&String> = ids
            .of("software")
            .iter()
            .chain(ids.of("dataProcessing"))
            .filter(|id| ["Bioworks", "pwiz", "CompassXtract", "pwiz_processing", "CompassXtract_x0020_processing"].contains(&id.as_str()))
            .collect();
        assert!(restored.is_empty(), "{name}: nothing is restored from an index that does not hold together: {restored:?}");
        assert_eq!(ids.of("sourceFile"), ["sourceFile"], "{name}: the archive itself, as for any input with no stated source");
        ids.unique_and_resolving(&format!("the export of an archive whose index is inconsistent ({name})"));
    }
}

/// One list left out of an index costs that list, not the run-level metadata entire. The structs
/// the index parses into take a missing field as an empty one (`#[serde(default)]`), so an older or
/// foreign writer that omitted an empty `parameters` array keeps its instrument, its source files
/// and its processing history; all-or-none is for an index whose types are wrong.
#[test]
fn a_list_left_out_of_an_index_costs_only_that_list() {
    let dir = scratch("sparse");
    let archive = run(Path::new(TINY), &dir.join("tiny.mzpeak"), &[]);
    let sparse = dir.join("sparse.mzpeak");
    with_edited_index(&archive, &sparse, |index| {
        for entry in index["metadata"]["software_list"].as_array_mut().unwrap() {
            entry.as_object_mut().unwrap().remove("parameters");
        }
        index["metadata"]["file_description"].as_object_mut().unwrap().remove("contents");
    });

    let mzml = run(&sparse, &dir.join("sparse.mzML"), &["--to", "mzml"]);
    let ids = read_ids(&mzml);
    for want in ["Bioworks", "pwiz", "CompassXtract"] {
        assert!(ids.of("software").contains(&want.to_string()), "software {want} survives a missing field: {:?}", ids.of("software"));
    }
    assert!(ids.of("dataProcessing").contains(&"pwiz_processing".to_string()), "and so does the processing list");
    ids.unique_and_resolving("the export of an archive whose index leaves a list out");
}

/// Every scan states `instrumentConfigurationRef="IC{id+1}"` from the id its spectrum carries —
/// which only had to resolve against a list this lane invented (one blank configuration `0`, which
/// answered to every spectrum). Against a RESTORED list it has to resolve against what the archive
/// declares, and an archive whose spectra name a configuration its index does not hold would put a
/// dangling reference on every scan.
#[test]
fn a_spectrum_naming_an_undeclared_instrument_still_resolves() {
    let dir = scratch("instrument");
    let archive = run(Path::new(TINY), &dir.join("tiny.mzpeak"), &[]);
    let shifted = dir.join("shifted.mzpeak");
    // The spectra are untouched, so they still carry configuration 0.
    with_edited_index(&archive, &shifted, |index| {
        index["metadata"]["instrument_configuration_list"][0]["id"] = serde_json::json!(3);
        index["metadata"]["run"]["default_instrument_id"] = serde_json::json!(3);
    });

    let mzml = run(&shifted, &dir.join("shifted.mzML"), &["--to", "mzml"]);
    let ids = read_ids(&mzml);
    assert_eq!(ids.of("instrumentConfiguration"), ["IC4"], "the archive's own configuration, under the id it states");
    for (element, attribute, value) in ids.references.iter() {
        if attribute.contains("nstrumentConfiguration") {
            assert_eq!(value, "IC4", "<{element} {attribute}> states the one configuration there is");
        }
    }
    ids.unique_and_resolving("the export of an archive whose spectra name an undeclared instrument");
}

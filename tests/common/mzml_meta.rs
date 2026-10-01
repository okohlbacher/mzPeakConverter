//! What an mzML this tool writes states about its processing, its header's ids and references and,
//! per spectrum, about its ion-mobility window, read back from the file with quick-xml — and the
//! contracts every such file must meet ([`assert_processing_contract`], [`assert_header_contract`]).
//!
//! One implementation for the unit tests in `src/` and the integration tests in `tests/`, pulled in
//! with `#[path]` as `corpus.rs` is (the crate has no library target to share it through).
#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use quick_xml::events::{BytesStart, Event};

/// The name `bruker_native::add_isolation_mobility_band` gives MZP:1000006, which an mzML carries as
/// a `userParam` on the selected ion.
pub const BAND_LOWER: &str = "isolation window inverse reduced ion mobility lower limit";
/// … and MZP:1000007.
pub const BAND_UPPER: &str = "isolation window inverse reduced ion mobility upper limit";

/// One `processingMethod`: its `order`, its `softwareRef` and the accessions of its `cvParam`s.
#[derive(Debug, Default, Clone)]
pub struct Method {
    pub order: Option<i64>,
    pub software_ref: String,
    pub accessions: Vec<String>,
}

/// One `spectrum`: what the ion-mobility window checks need.
#[derive(Debug, Default, Clone)]
pub struct Spectrum {
    pub id: String,
    pub ms_level: Option<u8>,
    /// The spectrum-level `userParam`s `ion mobility lower limit` / `ion mobility upper limit`.
    pub im_lower: Option<f64>,
    pub im_upper: Option<f64>,
    /// MS:1000827 `isolation window target m/z` of the first precursor.
    pub isolation_target: Option<f64>,
    /// MS:1002815 `inverse reduced ion mobility` of the first selected ion.
    pub ion_mobility: Option<f64>,
    /// The selected ion's window band ([`BAND_LOWER`] / [`BAND_UPPER`]).
    pub band_lower: Option<f64>,
    pub band_upper: Option<f64>,
}

/// One `instrumentConfiguration` of the header.
#[derive(Debug, Default, Clone)]
pub struct Configuration {
    pub id: String,
    /// Its `<componentList count>` and the components the list holds; `None` without the element.
    pub components: Option<(usize, usize)>,
    /// Its `<softwareRef ref>`; `None` without the element.
    pub software_ref: Option<String>,
}

#[derive(Debug, Default)]
pub struct Mzml {
    /// The attributes of the `<run>` start tag.
    pub run: BTreeMap<String, String>,
    /// Every `sourceFile` id, `sample` id and `scanSettings` id, in document order.
    pub source_files: Vec<String>,
    pub samples: Vec<String>,
    pub scan_settings: Vec<String>,
    /// Every `<sourceFileRef ref>` of the scan settings.
    pub source_file_refs: Vec<String>,
    pub configurations: Vec<Configuration>,
    /// Each `<scan>`'s `instrumentConfigurationRef`, in document order; `None` where it states none.
    pub scan_configurations: Vec<Option<String>>,
    /// Every `software` id with its version, in document order.
    pub softwares: Vec<(String, String)>,
    /// The `count` attribute of `dataProcessingList`.
    pub data_processing_count: Option<usize>,
    /// Every `dataProcessing` id with its methods, in document order.
    pub data_processings: Vec<(String, Vec<Method>)>,
    /// `defaultDataProcessingRef` of `spectrumList` / `chromatogramList`; the outer `Option` is
    /// whether the list is there at all.
    pub spectrum_list_default: Option<Option<String>>,
    pub chromatogram_list_default: Option<Option<String>>,
    /// Every element-level `dataProcessingRef` (on a spectrum, a chromatogram, a
    /// `binaryDataArray`), with the element's name.
    pub data_processing_refs: Vec<(String, String)>,
    pub spectra: Vec<Spectrum>,
}

fn attr(tag: &BytesStart, key: &[u8]) -> Option<String> {
    tag.attributes().flatten().find(|a| a.key.as_ref() == key).map(|a| {
        a.normalized_value(quick_xml::XmlVersion::Implicit1_0)
            .map(|v| v.into_owned())
            .unwrap_or_else(|_| String::from_utf8_lossy(&a.value).into_owned())
    })
}

fn num(tag: &BytesStart) -> Option<f64> {
    attr(tag, b"value").and_then(|v| v.parse().ok())
}

/// Read `path` (an mzML or an indexedmzML) once, start to end.
pub fn read(path: &Path) -> Mzml {
    let file = std::io::BufReader::new(std::fs::File::open(path).unwrap_or_else(|e| panic!("{}: {e}", path.display())));
    let mut xml = quick_xml::Reader::from_reader(file);
    xml.config_mut().trim_text(true);
    let mut out = Mzml::default();
    let mut stack: Vec<Vec<u8>> = Vec::new();
    let mut spectrum: Option<Spectrum> = None;
    let mut buf = Vec::new();
    loop {
        let open = match xml.read_event_into(&mut buf) {
            Ok(Event::Start(t)) => Some((t.into_owned(), false)),
            Ok(Event::Empty(t)) => Some((t.into_owned(), true)),
            Ok(Event::End(t)) => {
                let name = stack.pop().unwrap_or_default();
                assert_eq!(name.as_slice(), t.name().as_ref(), "{}: mismatched end tag", path.display());
                if name == b"spectrum" {
                    out.spectra.push(spectrum.take().expect("an open spectrum"));
                }
                None
            }
            Ok(Event::Eof) => break,
            Ok(_) => None,
            Err(e) => panic!("{}: not well-formed XML: {e}", path.display()),
        };
        buf.clear();
        let Some((tag, empty)) = open else { continue };
        let name = tag.name().as_ref().to_vec();
        if let Some(r) = attr(&tag, b"dataProcessingRef") {
            out.data_processing_refs.push((String::from_utf8_lossy(&name).into_owned(), r));
        }
        let parent = stack.last().map(Vec::as_slice);
        let in_ion = stack.iter().any(|n| n == b"selectedIon");
        match name.as_slice() {
            b"run" => {
                out.run = tag
                    .attributes()
                    .flatten()
                    .map(|a| (String::from_utf8_lossy(a.key.as_ref()).into_owned(), attr(&tag, a.key.as_ref()).unwrap_or_default()))
                    .collect()
            }
            b"sourceFile" => out.source_files.push(attr(&tag, b"id").unwrap_or_default()),
            b"sample" => out.samples.push(attr(&tag, b"id").unwrap_or_default()),
            b"scanSettings" => out.scan_settings.push(attr(&tag, b"id").unwrap_or_default()),
            b"sourceFileRef" => out.source_file_refs.push(attr(&tag, b"ref").unwrap_or_default()),
            b"instrumentConfiguration" => out.configurations.push(Configuration { id: attr(&tag, b"id").unwrap_or_default(), ..Default::default() }),
            b"componentList" => {
                let count = attr(&tag, b"count").and_then(|c| c.parse().ok()).unwrap_or(usize::MAX);
                out.configurations.last_mut().expect("componentList outside instrumentConfiguration").components = Some((count, 0));
            }
            b"source" | b"analyzer" | b"detector" if matches!(parent, Some(b"componentList")) => {
                let held = out.configurations.last_mut().and_then(|c| c.components.as_mut()).expect("an open componentList");
                held.1 += 1;
            }
            b"softwareRef" => out.configurations.last_mut().expect("softwareRef outside instrumentConfiguration").software_ref = attr(&tag, b"ref"),
            b"scan" => out.scan_configurations.push(attr(&tag, b"instrumentConfigurationRef")),
            b"software" => out.softwares.push((attr(&tag, b"id").unwrap_or_default(), attr(&tag, b"version").unwrap_or_default())),
            b"dataProcessingList" => out.data_processing_count = attr(&tag, b"count").and_then(|c| c.parse().ok()),
            b"dataProcessing" => out.data_processings.push((attr(&tag, b"id").unwrap_or_default(), Vec::new())),
            b"processingMethod" => {
                let m = Method {
                    order: attr(&tag, b"order").and_then(|o| o.parse().ok()),
                    software_ref: attr(&tag, b"softwareRef").unwrap_or_default(),
                    accessions: Vec::new(),
                };
                out.data_processings.last_mut().expect("processingMethod outside dataProcessing").1.push(m);
            }
            b"spectrumList" => out.spectrum_list_default = Some(attr(&tag, b"defaultDataProcessingRef")),
            b"chromatogramList" => out.chromatogram_list_default = Some(attr(&tag, b"defaultDataProcessingRef")),
            b"spectrum" => spectrum = Some(Spectrum { id: attr(&tag, b"id").unwrap_or_default(), ..Default::default() }),
            b"cvParam" if matches!(parent, Some(b"processingMethod")) => {
                let m = out.data_processings.last_mut().and_then(|dp| dp.1.last_mut()).expect("an open processingMethod");
                m.accessions.push(attr(&tag, b"accession").unwrap_or_default());
            }
            b"cvParam" | b"userParam" => {
                if let Some(s) = spectrum.as_mut() {
                    let (acc, pname) = (attr(&tag, b"accession"), attr(&tag, b"name").unwrap_or_default());
                    match (parent, acc.as_deref(), pname.as_str()) {
                        (Some(b"spectrum"), Some("MS:1000511"), _) => s.ms_level = attr(&tag, b"value").and_then(|v| v.parse().ok()),
                        (Some(b"spectrum"), None, "ion mobility lower limit") => s.im_lower = num(&tag),
                        (Some(b"spectrum"), None, "ion mobility upper limit") => s.im_upper = num(&tag),
                        (Some(b"isolationWindow"), Some("MS:1000827"), _) if s.isolation_target.is_none() => s.isolation_target = num(&tag),
                        (_, Some("MS:1002815"), _) if in_ion && s.ion_mobility.is_none() => s.ion_mobility = num(&tag),
                        (_, None, BAND_LOWER) if in_ion && s.band_lower.is_none() => s.band_lower = num(&tag),
                        (_, None, BAND_UPPER) if in_ion && s.band_upper.is_none() => s.band_upper = num(&tag),
                        _ => {}
                    }
                }
            }
            _ => {}
        }
        if !empty {
            stack.push(name);
        }
    }
    out
}

/// The mzML 1.1 processing contract, and this tool's step in it; returns the id of the
/// `dataProcessing` holding that step. Panics, naming `what`, unless:
/// * `dataProcessingList` holds at least one `dataProcessing`, as many as its `count` says;
/// * every `processingMethod`'s `softwareRef` names a `software` of `softwareList`;
/// * `spectrumList` and `chromatogramList` are there and name the same `dataProcessing` in
///   `defaultDataProcessingRef`, one whose LAST method does MS:1000544 `Conversion to mzML` with
///   the software `mzpeak-convert` of this very version — the export is the last step of the
///   default processing;
/// * every element-level `dataProcessingRef` names an existing `dataProcessing`;
/// * no id is used twice among the `software` and `dataProcessing` entries.
pub fn assert_processing_contract(m: &Mzml, what: &str) -> String {
    let n = m.data_processings.len();
    assert!(n >= 1, "{what}: empty dataProcessingList");
    assert_eq!(m.data_processing_count, Some(n), "{what}: dataProcessingList count");
    let sw: BTreeMap<&str, &str> = m.softwares.iter().map(|(i, v)| (i.as_str(), v.as_str())).collect();
    for (id, methods) in &m.data_processings {
        assert!(!methods.is_empty(), "{what}: dataProcessing {id:?} has no processingMethod");
        for meth in methods {
            assert!(sw.contains_key(meth.software_ref.as_str()), "{what}: {id:?} names software {:?}, not in softwareList {sw:?}", meth.software_ref);
        }
    }
    let dp_ids: BTreeSet<&str> = m.data_processings.iter().map(|(id, _)| id.as_str()).collect();
    let mut defaults = Vec::new();
    for (list, default) in [("spectrumList", &m.spectrum_list_default), ("chromatogramList", &m.chromatogram_list_default)] {
        let default = default.as_ref().unwrap_or_else(|| panic!("{what}: no {list}"));
        let default = default.as_deref().unwrap_or_else(|| panic!("{what}: {list} has no defaultDataProcessingRef"));
        assert!(dp_ids.contains(default), "{what}: {list} defaultDataProcessingRef {default:?} is not a dataProcessing id {dp_ids:?}");
        defaults.push(default);
    }
    assert_eq!(defaults[0], defaults[1], "{what}: the two lists' default processing");
    let (ours, methods) = m.data_processings.iter().find(|(id, _)| id == defaults[0]).unwrap();
    let last = methods.iter().max_by_key(|meth| meth.order).unwrap();
    assert!(
        last.accessions.iter().any(|a| a == "MS:1000544")
            && last.software_ref.starts_with("mzpeak-convert")
            && sw.get(last.software_ref.as_str()) == Some(&env!("CARGO_PKG_VERSION")),
        "{what}: the default processing {ours:?} does not end in mzpeak-convert {}'s Conversion to mzML: {methods:?}",
        env!("CARGO_PKG_VERSION")
    );
    for (element, r) in &m.data_processing_refs {
        assert!(dp_ids.contains(r.as_str()), "{what}: a {element}'s dataProcessingRef {r:?} is not a dataProcessing id {dp_ids:?}");
    }
    let mut seen = BTreeSet::new();
    for id in m.softwares.iter().map(|(i, _)| i).chain(m.data_processings.iter().map(|(i, _)| i)) {
        assert!(seen.insert(id.as_str()), "{what}: id {id:?} used twice");
    }
    ours.clone()
}

/// Is `id` an `xs:ID` as ProteoWizard writes one: an ASCII letter or `_`, then those, digits, `.`, `-`?
pub fn is_xml_id(id: &str) -> bool {
    let mut bytes = id.bytes();
    bytes.next().is_some_and(|b| b.is_ascii_alphabetic() || b == b'_') && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'.' || b == b'-')
}

/// What mzML 1.1's schema asks of the header's ids and references. Panics, naming `what`, unless:
/// * the run has an id, and it, each source file's, sample's, software's, data processing's, scan
///   settings' and instrument configuration's id is an XML name ([`is_xml_id`]), none twice in its
///   list;
/// * the run's `defaultInstrumentConfigurationRef` and every scan's `instrumentConfigurationRef`
///   name a declared configuration, its `defaultSourceFileRef` and every scan settings'
///   `sourceFileRef` a listed source file;
/// * no configuration holds an empty `softwareRef` (it names a listed software or is absent) or an
///   empty `componentList` (it counts the components it holds, at least one, or is absent);
/// * a `startTimeStamp`, when there, is an `xs:dateTime`.
pub fn assert_header_contract(m: &Mzml, what: &str) {
    let run_id = m.run.get("id").unwrap_or_else(|| panic!("{what}: the run has no id"));
    let software: Vec<String> = m.softwares.iter().map(|(id, _)| id.clone()).collect();
    let processing: Vec<String> = m.data_processings.iter().map(|(id, _)| id.clone()).collect();
    let configurations: Vec<String> = m.configurations.iter().map(|c| c.id.clone()).collect();
    for (list, ids) in [
        ("run", std::slice::from_ref(run_id)),
        ("sourceFile", &m.source_files[..]),
        ("sample", &m.samples[..]),
        ("software", &software[..]),
        ("dataProcessing", &processing[..]),
        ("scanSettings", &m.scan_settings[..]),
        ("instrumentConfiguration", &configurations[..]),
    ] {
        let mut seen = BTreeSet::new();
        for id in ids {
            assert!(is_xml_id(id), "{what}: {list} id {id:?} is not an XML name");
            assert!(seen.insert(id.as_str()), "{what}: {list} id {id:?} used twice");
        }
    }
    let default = m.run.get("defaultInstrumentConfigurationRef").unwrap_or_else(|| panic!("{what}: no defaultInstrumentConfigurationRef"));
    assert!(configurations.contains(default), "{what}: the run's default configuration {default:?} is not one of {configurations:?}");
    for named in m.scan_configurations.iter().flatten() {
        assert!(configurations.contains(named), "{what}: a scan names configuration {named:?}, not one of {configurations:?}");
    }
    if let Some(file) = m.run.get("defaultSourceFileRef") {
        assert!(m.source_files.contains(file), "{what}: the run's default source file {file:?} is not one of {:?}", m.source_files);
    }
    for file in &m.source_file_refs {
        assert!(m.source_files.contains(file), "{what}: a scan settings names source file {file:?}, not one of {:?}", m.source_files);
    }
    for c in &m.configurations {
        if let Some(r) = &c.software_ref {
            assert!(software.contains(r), "{what}: configuration {} names software {r:?}, not one of {software:?}", c.id);
        }
        if let Some((count, held)) = c.components {
            assert!(held > 0 && count == held, "{what}: configuration {} has a componentList of count {count} holding {held}", c.id);
        }
    }
    if let Some(t) = m.run.get("startTimeStamp") {
        let zoned = chrono::DateTime::parse_from_rfc3339(t).is_ok();
        let plain = chrono::NaiveDateTime::parse_from_str(t, "%Y-%m-%dT%H:%M:%S%.f").is_ok();
        assert!(zoned || plain, "{what}: startTimeStamp {t:?} is not an xs:dateTime");
    }
}

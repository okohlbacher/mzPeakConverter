//! The cross-references an mzML or imzML source states between its own lists, checked while an
//! archive lane copies them.
//!
//! An mzML ties its parts together by id: a `<scan>` names its `instrumentConfiguration`, a
//! `processingMethod` and an `instrumentConfiguration` their `software`, the spectrum list its
//! default `dataProcessing`, the run its default configuration and source file. mzdata copies each
//! reference as it reads it, whether or not the list holds the id — a configuration reference it has
//! not seen gets a fresh number — so a source whose writer got one wrong passed it into the archive,
//! where it names nothing: the scans of a pyimzML export (GBM `Test_P15_r2`) name
//! `instrumentConfiguration0` while its list holds `IC1`, and were stored as configuration 1 of a list
//! that holds only 0. [`DanglingRefs`] finds those as the metadata and the scans are copied and drops
//! each one: a software reference becomes empty (the spec's instrument configuration requires the
//! string), a run default absent — [`crate::fixup_run_metadata`] then names the list's first entry, as
//! for a source that states none, since the spec's run requires all three — and a scan's
//! configuration null (the vendored writer's [`NO_INSTRUMENT_CONFIGURATION`]). The archive declares
//! [`DROPPED`] in `transformations`, and the run warns once, counting each kind and naming the ids as
//! the source states them.
//!
//! mzdata also loses entries itself: it acts on a start tag only, so a self-closing
//! `<software id="…" version="…"/>`, `<sourceFile id="…" name="…" location="…"/>` or
//! `<instrumentConfiguration id="…"/>` is never read, and a reference to one looked dangling although
//! the source is whole — MALDIquantForeign's imzML export (LA-ESI `Thaliana`) writes its software that
//! way. [`Header`] reads every entry of the three lists from the header itself, and the entries mzdata
//! skipped are put back where the source states them before anything is checked, so only a reference
//! the source cannot resolve is dropped. A configuration mzdata skipped has no number until a
//! reference names it: one the run's default names is numbered with the header (mzdata's numbering is
//! replayed), one only scans name the first time a scan does — the scan's own id is then read from
//! the source ([`DanglingRefs::check_scans`]), and the configuration is put back once the scans are
//! written ([`DanglingRefs::restore_scan_configurations`]).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::io::BufRead;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use mzdata::meta::{InstrumentConfiguration, MSDataFileMetadata, Software, SourceFile};
use mzdata::spectrum::SpectrumDescription;
use mzpeak_prototyping::writer::NO_INSTRUMENT_CONFIGURATION;
use quick_xml::events::{BytesStart, Event};

use crate::pwiz_id;

/// The `transformations` entry of an archive from which a dangling reference was dropped.
pub const DROPPED: &str = "mzml:dangling-reference-dropped";

/// One entry of a header list, as the source states it.
#[derive(Debug, Default, Clone, PartialEq)]
struct Entry {
    id: String,
    /// `<… />`: mzdata acts on a start tag only, so it skipped this entry.
    self_closing: bool,
    /// A software's `version`; empty for the other lists, and when unstated.
    version: String,
    /// A source file's `name` and `location`; empty for the other lists, and when unstated.
    name: String,
    location: String,
}

/// What an mzML or imzML header states before `<run>`, read with quick_xml rather than mzdata: every
/// `<software>`, `<sourceFile>` and `<instrumentConfiguration>` in document order, self-closing or
/// not, and the run's `defaultInstrumentConfigurationRef`.
#[derive(Debug, Default)]
pub struct Header {
    software: Vec<Entry>,
    source_files: Vec<Entry>,
    configurations: Vec<Entry>,
    default_configuration: Option<String>,
}

impl Header {
    pub fn read(path: &Path) -> Result<Self> {
        let file = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
        Self::parse(std::io::BufReader::new(file))
    }

    fn parse(input: impl BufRead) -> Result<Self> {
        let mut reader = quick_xml::Reader::from_reader(input);
        let mut buf = Vec::new();
        let mut this = Self::default();
        loop {
            match reader.read_event_into(&mut buf).context("parsing the mzML header")? {
                // mzdata reads the run's attributes from its start tag, then the spectra.
                Event::Start(e) if e.name().as_ref() == b"run" => {
                    this.default_configuration = value(&e, b"defaultInstrumentConfigurationRef");
                    break;
                }
                Event::Start(e) => this.entry(&e, false),
                Event::Empty(e) => this.entry(&e, true),
                Event::Eof => break,
                _ => {}
            }
            buf.clear();
        }
        Ok(this)
    }

    fn entry(&mut self, e: &BytesStart, self_closing: bool) {
        let entry = || Entry { id: value(e, b"id").unwrap_or_default(), self_closing, ..Default::default() };
        match e.name().as_ref() {
            b"software" => self.software.push(Entry { version: value(e, b"version").unwrap_or_default(), ..entry() }),
            b"sourceFile" => self.source_files.push(Entry {
                name: value(e, b"name").unwrap_or_default(),
                location: value(e, b"location").unwrap_or_default(),
                ..entry()
            }),
            b"instrumentConfiguration" => self.configurations.push(entry()),
            _ => {}
        }
    }

    /// mzdata's numbers for the configuration ids the header names, replayed: its reader numbers an
    /// id the first time it reads it — each `<instrumentConfiguration>` start tag in document order,
    /// then the run's default.
    fn numbering(&self) -> HashMap<String, u32> {
        let mut numbers = HashMap::new();
        let mut number = |id: &str| {
            let next = numbers.len() as u32;
            numbers.entry(id.to_string()).or_insert(next);
        };
        for c in self.configurations.iter().filter(|c| !c.self_closing) {
            number(&c.id);
        }
        if let Some(d) = &self.default_configuration {
            number(d);
        }
        numbers
    }

    /// Put back the `<software/>` entries mzdata skipped, each at its place in the list: id (decoded
    /// as `decode_pwiz_ids` decodes the others) and version, with no params — the element states
    /// none. Returns how many.
    fn restore_software(&self, target: &mut impl MSDataFileMetadata) -> usize {
        let list = target.softwares_mut();
        let mut restored = 0;
        for (at, e) in self.software.iter().enumerate().filter(|(_, e)| e.self_closing) {
            let id = pwiz_id::decode(&e.id);
            if !list.iter().any(|s| s.id == id) {
                list.insert(at.min(list.len()), Software::new(id, e.version.clone(), Vec::new()));
                restored += 1;
            }
        }
        restored
    }

    /// Put back the `<sourceFile/>` entries mzdata skipped, each at its place in the list: id, name
    /// and location, with no params. Returns how many.
    fn restore_source_files(&self, target: &mut impl MSDataFileMetadata) -> usize {
        let list = &mut target.file_description_mut().source_files;
        let mut restored = 0;
        for (at, e) in self.source_files.iter().enumerate().filter(|(_, e)| e.self_closing) {
            if !list.iter().any(|sf| sf.id == e.id) {
                let sf = SourceFile { id: e.id.clone(), name: e.name.clone(), location: e.location.clone(), ..Default::default() };
                list.insert(at.min(list.len()), sf);
                restored += 1;
            }
        }
        restored
    }
}

/// An attribute's value as mzdata reads it (entities resolved, whitespace normalised).
fn value(e: &BytesStart, key: &[u8]) -> Option<String> {
    e.attributes().flatten().find(|a| a.key.as_ref() == key).map(|a| {
        a.normalized_value(quick_xml::XmlVersion::Implicit1_0)
            .map(|v| v.into_owned())
            .unwrap_or_else(|_| String::from_utf8_lossy(&a.value).into_owned())
    })
}

/// The ids a source could not resolve, as they were dropped. Built by [`DanglingRefs::check`] and fed
/// each spectrum through [`DanglingRefs::check_scans`].
#[derive(Debug, Default)]
pub struct DanglingRefs {
    /// The configuration numbers a scan may name: the list's, or 0 alone for an empty list, which
    /// `fixup_run_metadata` gives an empty configuration 0.
    configurations: HashSet<u32>,
    /// The source's id for each number mzdata gave one while reading the header.
    names: HashMap<u32, String>,
    /// The configuration ids the header states that mzdata skipped and no header reference numbers:
    /// a scan naming one gets a fresh number, so a number outside the list may be one of them.
    pending: HashSet<String>,
    /// The source to read a scan's own configuration id from, once one names a number outside the list.
    source: Option<PathBuf>,
    /// The first scan naming each configuration id the header does not number, by (spectrum id, scan
    /// position): read from `source` on first need.
    first_references: Option<HashMap<(String, usize), String>>,
    /// The configurations a scan names that the source states and mzdata skipped, to put back.
    restored: BTreeSet<u32>,
    /// The numbers outside the list found to name nothing, with the id the source states.
    dangling: HashMap<u32, String>,
    /// Per mzML attribute: how many references were dropped, and the ids they named.
    dropped: BTreeMap<&'static str, (usize, BTreeSet<String>)>,
}

impl DanglingRefs {
    /// Read `path`'s header, put back the entries mzdata skipped ([`Header`]) and drop the run-level
    /// references that still name nothing ([`Self::check_metadata`]). Scans whose configuration is
    /// outside the list are later read back from `path`. After `decode_pwiz_ids`, before this
    /// conversion adds its own entries.
    pub fn check(path: &Path, target: &mut impl MSDataFileMetadata) -> Self {
        let header = Header::read(path)
            .inspect_err(|e| log::warn!("{}: header lists not read back, references are checked as mzdata read them: {e:#}", path.display()))
            .ok();
        let mut this = Self::check_metadata(target, header.as_ref());
        this.source = Some(path.to_path_buf());
        this
    }

    /// Put back what `header` states and mzdata skipped, then drop the run-level references of the
    /// copied metadata that name no entry of its lists: a `processingMethod`'s or an
    /// `instrumentConfiguration`'s `softwareRef`, and the run's `defaultInstrumentConfigurationRef`,
    /// `defaultDataProcessingRef` and `defaultSourceFileRef`. Without a header, references are checked
    /// against what mzdata read.
    pub fn check_metadata(target: &mut impl MSDataFileMetadata, header: Option<&Header>) -> Self {
        let mut this = Self::default();
        if let Some(header) = header {
            let restored = [header.restore_software(target), header.restore_source_files(target), this.number_configurations(header, target)];
            if restored.iter().any(|n| *n > 0) {
                let [software, files, configurations] = restored;
                log::info!(
                    "read back from the header (mzdata skips self-closing entries): {software} <software/>, \
                     {files} <sourceFile/>, {configurations} <instrumentConfiguration/>"
                );
            }
        }
        this.configurations = target.instrument_configurations().keys().copied().collect();
        if this.configurations.is_empty() {
            this.configurations.insert(0);
        }
        let software: HashSet<String> = target.softwares().iter().map(|s| s.id.clone()).collect();
        let drop_software = |this: &mut Self, reference: &mut String| {
            if !reference.is_empty() && !software.contains(reference.as_str()) {
                this.note("softwareRef", std::mem::take(reference));
            }
        };
        for dp in target.data_processings_mut() {
            for method in dp.methods.iter_mut() {
                drop_software(&mut this, &mut method.software_reference);
            }
        }
        for ic in target.instrument_configurations_mut().values_mut() {
            drop_software(&mut this, &mut ic.software_reference);
        }
        let processing: HashSet<String> = target.data_processings().iter().map(|dp| dp.id.clone()).collect();
        let files: HashSet<String> = target.file_description().source_files.iter().map(|sf| sf.id.clone()).collect();
        if let Some(run) = target.run_description_mut() {
            if let Some(id) = run.default_instrument_id.filter(|id| !this.configurations.contains(id)) {
                run.default_instrument_id = None;
                let name = this.name(id);
                this.note("defaultInstrumentConfigurationRef", name);
            }
            if let Some(id) = run.default_data_processing_id.take_if(|id| !processing.contains(id.as_str())) {
                this.note("defaultDataProcessingRef", id);
            }
            if let Some(id) = run.default_source_file_id.take_if(|id| !files.contains(id.as_str())) {
                this.note("defaultSourceFileRef", id);
            }
        }
        this
    }

    /// Replay mzdata's numbering of `header`'s configuration ids; put back, empty, each one mzdata
    /// skipped that the run's default numbers, and keep the others it skipped as pending. Returns how
    /// many were put back. Nothing, when the replay does not give the numbers mzdata read — the
    /// references are then checked by number alone.
    fn number_configurations(&mut self, header: &Header, target: &mut impl MSDataFileMetadata) -> usize {
        let numbers = header.numbering();
        let started: BTreeSet<u32> = header.configurations.iter().filter(|c| !c.self_closing).map(|c| numbers[&c.id]).collect();
        let read: BTreeSet<u32> = target.instrument_configurations().keys().copied().collect();
        if started != read {
            log::debug!("the header's configuration ids number {started:?}, mzdata read {read:?}: scan references checked by number");
            return 0;
        }
        let mut restored = 0;
        for c in header.configurations.iter().filter(|c| c.self_closing) {
            match numbers.get(&c.id) {
                Some(&n) => {
                    if let std::collections::hash_map::Entry::Vacant(v) = target.instrument_configurations_mut().entry(n) {
                        v.insert(InstrumentConfiguration { id: n, ..Default::default() });
                        restored += 1;
                    }
                }
                None => {
                    self.pending.insert(c.id.clone());
                }
            }
        }
        self.names = numbers.into_iter().map(|(id, n)| (n, id)).collect();
        restored
    }

    /// Null the configuration of each scan of `descr` that names none of the source's. A number
    /// outside the list is classified once, by the id the source's own scan states (read from the
    /// source the first time one is needed): one the header states and mzdata skipped is kept and put
    /// back ([`Self::restore_scan_configurations`]), any other dropped.
    pub fn check_scans(&mut self, descr: &mut SpectrumDescription) {
        for at in 0..descr.acquisition.scans.len() {
            let n = descr.acquisition.scans[at].instrument_configuration_id;
            if n == NO_INSTRUMENT_CONFIGURATION || self.configurations.contains(&n) {
                continue;
            }
            let id = match self.dangling.get(&n) {
                Some(id) => id.clone(),
                None => match self.classify(n, &descr.id, at) {
                    Some(id) => {
                        self.dangling.insert(n, id.clone());
                        id
                    }
                    None => continue,
                },
            };
            descr.acquisition.scans[at].instrument_configuration_id = NO_INSTRUMENT_CONFIGURATION;
            self.note("instrumentConfigurationRef", id);
        }
    }

    /// Whether configuration `n`, outside the list, first named by scan `at` of `spectrum`, names
    /// nothing: the source's id when it does, `None` (and `n` joins the list) when it names a
    /// configuration the source states. When the scan's id cannot be read back, a number is kept only
    /// while the header states a configuration it may be.
    fn classify(&mut self, n: u32, spectrum: &str, at: usize) -> Option<String> {
        // A header number outside the list: the run's default named an id no entry has.
        if let Some(id) = self.names.get(&n) {
            return Some(id.clone());
        }
        let id = self.first_reference(spectrum, at);
        let stated = match &id {
            Some(id) => self.pending.contains(id),
            None => !self.pending.is_empty(),
        };
        if stated {
            log::debug!("scan configuration {n} ({}) is one the header states; kept", id.as_deref().unwrap_or("id not read back"));
            self.configurations.insert(n);
            self.restored.insert(n);
            None
        } else {
            Some(id.unwrap_or_else(|| self.name(n)))
        }
    }

    /// The configuration id scan `at` of `spectrum` states, when it is the first scan of the source
    /// naming an id the header does not number.
    fn first_reference(&mut self, spectrum: &str, at: usize) -> Option<String> {
        if self.first_references.is_none() {
            let numbered: HashSet<&str> = self.names.values().map(String::as_str).collect();
            let found = match &self.source {
                Some(path) => first_scan_references(path, &numbered).unwrap_or_else(|e| {
                    log::warn!("{}: scan configuration ids not read back: {e:#}", path.display());
                    HashMap::new()
                }),
                None => HashMap::new(),
            };
            self.first_references = Some(found);
        }
        self.first_references.as_ref()?.get(&(spectrum.to_string(), at)).cloned()
    }

    /// Put back, empty, each configuration a scan names that the source states and mzdata skipped (and
    /// the empty configuration 0 an empty list resolves to). After the last [`Self::check_scans`],
    /// before `fixup_run_metadata`.
    pub fn restore_scan_configurations(&self, target: &mut impl MSDataFileMetadata) {
        let list = target.instrument_configurations_mut();
        let placeholder = list.is_empty().then_some(0);
        for n in self.restored.iter().copied().chain(placeholder.filter(|_| !self.restored.is_empty())) {
            list.entry(n).or_insert_with(|| InstrumentConfiguration { id: n, ..Default::default() });
        }
    }

    /// How the warning names configuration `n`: the source's id when known.
    fn name(&self, n: u32) -> String {
        self.names.get(&n).cloned().unwrap_or_else(|| format!("an id the reader numbered {n}"))
    }

    fn note(&mut self, kind: &'static str, id: String) {
        let (n, ids) = self.dropped.entry(kind).or_default();
        *n += 1;
        ids.insert(id);
    }

    /// [`DROPPED`] when a reference was dropped.
    pub fn transformation(&self) -> Option<&'static str> {
        (!self.dropped.is_empty()).then_some(DROPPED)
    }

    /// The one warning of a run, counting each kind: `2826 instrumentConfigurationRef
    /// (instrumentConfiguration0)`. Silent when nothing was dropped.
    pub fn warn(&self, input: &Path) {
        if let Some(what) = self.summary() {
            log::warn!(
                "{}: dropped references that name no entry of the source's lists: {what}; declared as {DROPPED}",
                input.display()
            );
        }
    }

    fn summary(&self) -> Option<String> {
        (!self.dropped.is_empty()).then(|| {
            self.dropped
                .iter()
                .map(|(kind, (n, ids))| {
                    let named: Vec<&str> = ids.iter().take(3).map(String::as_str).collect();
                    let more = if ids.len() > 3 { format!(" and {} more", ids.len() - 3) } else { String::new() };
                    format!("{n} {kind} ({}{more})", named.join(", "))
                })
                .collect::<Vec<_>>()
                .join(", ")
        })
    }
}

/// The first scan naming each configuration id that `numbered` lacks, by (spectrum id, position among
/// the spectrum's `<scan>` start tags — mzdata skips a self-closing one): the source's own id for a
/// number mzdata gave on first sight.
fn first_scan_references(path: &Path, numbered: &HashSet<&str>) -> Result<HashMap<(String, usize), String>> {
    let file = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    scan_references(std::io::BufReader::new(file), numbered)
}

fn scan_references(input: impl BufRead, numbered: &HashSet<&str>) -> Result<HashMap<(String, usize), String>> {
    let mut reader = quick_xml::Reader::from_reader(input);
    let mut buf = Vec::new();
    let mut found = HashMap::new();
    let mut seen = HashSet::new();
    let (mut spectrum, mut at) = (String::new(), 0usize);
    loop {
        match reader.read_event_into(&mut buf).context("parsing the source's scans")? {
            Event::Start(e) => match e.name().as_ref() {
                b"spectrum" => (spectrum, at) = (value(&e, b"id").unwrap_or_default(), 0),
                b"scan" => {
                    if let Some(id) = value(&e, b"instrumentConfigurationRef") {
                        if !numbered.contains(id.as_str()) && seen.insert(id.clone()) {
                            found.insert((spectrum.clone(), at), id);
                        }
                    }
                    at += 1;
                }
                _ => {}
            },
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mzdata::meta::{DataProcessing, FileMetadataConfig, ProcessingMethod};

    fn header(xml: &str) -> Header {
        Header::parse(xml.as_bytes()).unwrap()
    }

    #[test]
    fn the_header_states_every_entry_self_closing_or_not() {
        let h = header(
            r#"<mzML><fileDescription><sourceFileList count="2">
              <sourceFile id="sf1" name="a.raw" location="file:///d"><cvParam accession="MS:1000563" name="Thermo RAW format"/></sourceFile>
              <sourceFile id="sf&amp;2" name="b.raw" location="file:///e"/>
            </sourceFileList></fileDescription><softwareList count="3">
            <software id="MALDIquantForeign" version="0.12"/>
            <software id="pwiz" version="3"><cvParam accession="MS:1000615" name="ProteoWizard software"/></software>
            <software id="noversion"/>
          </softwareList><instrumentConfigurationList count="2"><instrumentConfiguration id="IC1"><cvParam accession="MS:1000031"/></instrumentConfiguration>
            <instrumentConfiguration id="IC2"/></instrumentConfigurationList>
          <run id="r" defaultInstrumentConfigurationRef="IC2"><spectrumList><spectrum><software id="late"/></spectrum></spectrumList></run></mzML>"#,
        );
        let ids = |l: &[Entry]| l.iter().map(|e| (e.id.clone(), e.self_closing)).collect::<Vec<_>>();
        assert_eq!(ids(&h.software), [("MALDIquantForeign".into(), true), ("pwiz".into(), false), ("noversion".into(), true)]);
        assert_eq!((h.software[0].version.as_str(), h.software[2].version.as_str()), ("0.12", ""));
        assert_eq!(ids(&h.source_files), [("sf1".into(), false), ("sf&2".into(), true)]);
        assert_eq!((h.source_files[1].name.as_str(), h.source_files[1].location.as_str()), ("b.raw", "file:///e"));
        assert_eq!(ids(&h.configurations), [("IC1".into(), false), ("IC2".into(), true)]);
        assert_eq!(h.default_configuration.as_deref(), Some("IC2"));
        // IC1's start tag first, then the run's default IC2, which mzdata never read as an entry.
        assert_eq!(h.numbering(), HashMap::from([("IC1".to_string(), 0), ("IC2".to_string(), 1)]));
    }

    /// Every run-level kind is dropped exactly when it names nothing, and a scan's configuration
    /// becomes the null sentinel; what resolves is kept.
    #[test]
    fn each_dangling_kind_is_dropped_and_counted() {
        let mut meta = FileMetadataConfig::default();
        meta.softwares_mut().push(Software::new("pwiz".into(), "3".into(), Vec::new()));
        meta.instrument_configurations_mut()
            .insert(0, InstrumentConfiguration { id: 0, software_reference: "vendor".into(), ..Default::default() });
        let method = |sw: &str| ProcessingMethod { order: 0, software_reference: sw.into(), params: Vec::new() };
        meta.data_processings_mut().push(DataProcessing { id: "dp".into(), methods: vec![method("pwiz"), method("ghost"), method("")] });
        meta.file_description_mut().source_files.push(SourceFile { id: "sf".into(), ..Default::default() });
        let run = meta.run_description_mut().unwrap();
        run.default_instrument_id = Some(3);
        run.default_data_processing_id = Some("dp1".into());
        run.default_source_file_id = Some("sf".into());

        let mut refs = DanglingRefs::check_metadata(&mut meta, None);
        let methods: Vec<&str> = meta.data_processings()[0].methods.iter().map(|m| m.software_reference.as_str()).collect();
        assert_eq!(methods, ["pwiz", "", ""]);
        assert_eq!(meta.instrument_configurations()[&0].software_reference, "");
        let run = meta.run_description().unwrap();
        assert_eq!((run.default_instrument_id, run.default_data_processing_id.as_deref()), (None, None));
        assert_eq!(run.default_source_file_id.as_deref(), Some("sf"), "a resolving default stays");

        let mut descr = SpectrumDescription::default();
        descr.acquisition.scans = vec![Default::default(), Default::default()];
        descr.acquisition.scans[1].instrument_configuration_id = 1;
        refs.check_scans(&mut descr);
        assert_eq!(descr.acquisition.scans[0].instrument_configuration_id, 0);
        assert_eq!(descr.acquisition.scans[1].instrument_configuration_id, NO_INSTRUMENT_CONFIGURATION);
        refs.check_scans(&mut descr);
        assert_eq!(refs.transformation(), Some(DROPPED));
        assert_eq!(
            refs.summary().unwrap(),
            "1 defaultDataProcessingRef (dp1), 1 defaultInstrumentConfigurationRef (an id the reader numbered 3), \
             1 instrumentConfigurationRef (an id the reader numbered 1), 2 softwareRef (ghost, vendor)"
        );
    }

    /// A run whose configuration list read back empty gets the empty configuration 0, so a scan or a
    /// default naming 0 resolves; nothing is declared for a whole source.
    #[test]
    fn an_empty_configuration_list_resolves_configuration_zero() {
        let mut meta = FileMetadataConfig::default();
        meta.run_description_mut().unwrap().default_instrument_id = Some(0);
        let mut refs = DanglingRefs::check_metadata(&mut meta, None);
        let mut descr = SpectrumDescription::default();
        descr.acquisition.scans = vec![Default::default()];
        refs.check_scans(&mut descr);
        assert_eq!(descr.acquisition.scans[0].instrument_configuration_id, 0);
        assert_eq!(meta.run_description().unwrap().default_instrument_id, Some(0));
        assert_eq!(refs.transformation(), None);
        assert_eq!(refs.summary(), None);
    }

    /// The entries mzdata skips for being self-closing are put back where the header states them, so
    /// a reference to one resolves: a source file the run's default names, a software a method names,
    /// a configuration the run's default numbers. Nothing is dropped from a whole source.
    #[test]
    fn self_closing_entries_are_put_back_and_their_references_kept() {
        let h = header(
            r#"<mzML><sourceFileList><sourceFile id="sf1" name="a.raw" location="file://"><cvParam/></sourceFile>
            <sourceFile id="sf9" name="other.raw" location="file:///data"/></sourceFileList>
            <softwareList><software id="exporter" version="0.12"/><software id="pwiz" version="3"><cvParam/></software></softwareList>
            <instrumentConfigurationList><instrumentConfiguration id="IC1"><cvParam/></instrumentConfiguration>
            <instrumentConfiguration id="IC2"/></instrumentConfigurationList>
            <run id="r" defaultInstrumentConfigurationRef="IC2" defaultSourceFileRef="sf9">"#,
        );
        // What mzdata read: the start-tag entries only, the run default IC2 numbered 1.
        let mut meta = FileMetadataConfig::default();
        meta.file_description_mut().source_files.push(SourceFile { id: "sf1".into(), ..Default::default() });
        meta.softwares_mut().push(Software::new("pwiz".into(), "3".into(), Vec::new()));
        meta.instrument_configurations_mut().insert(0, InstrumentConfiguration { id: 0, ..Default::default() });
        let method = ProcessingMethod { order: 0, software_reference: "exporter".into(), params: Vec::new() };
        meta.data_processings_mut().push(DataProcessing { id: "dp".into(), methods: vec![method] });
        let run = meta.run_description_mut().unwrap();
        run.default_instrument_id = Some(1);
        run.default_source_file_id = Some("sf9".into());
        run.default_data_processing_id = Some("dp".into());

        let refs = DanglingRefs::check_metadata(&mut meta, Some(&h));
        let files: Vec<(&str, &str, &str)> =
            meta.file_description().source_files.iter().map(|sf| (sf.id.as_str(), sf.name.as_str(), sf.location.as_str())).collect();
        assert_eq!(files, [("sf1", "", ""), ("sf9", "other.raw", "file:///data")]);
        let software: Vec<(&str, &str)> = meta.softwares().iter().map(|s| (s.id.as_str(), s.version.as_str())).collect();
        assert_eq!(software, [("exporter", "0.12"), ("pwiz", "3")], "put back at its place in the list");
        assert!(meta.instrument_configurations().contains_key(&1), "IC2, numbered by the run's default, is back");
        let run = meta.run_description().unwrap();
        assert_eq!((run.default_instrument_id, run.default_source_file_id.as_deref()), (Some(1), Some("sf9")));
        assert_eq!(meta.data_processings()[0].methods[0].software_reference, "exporter");
        assert_eq!(refs.summary(), None);
    }

    /// A scan naming a configuration mzdata skipped gets a number outside the list: the scan's own id,
    /// read back from the source, tells it from one that names nothing. The stated one is kept and its
    /// configuration put back; the other is dropped under the id the source states.
    #[test]
    fn a_scan_naming_a_skipped_configuration_is_kept_and_one_naming_nothing_dropped() {
        let h = header(
            r#"<mzML><instrumentConfigurationList><instrumentConfiguration id="IC1"><cvParam/></instrumentConfiguration>
            <instrumentConfiguration id="IC2"/></instrumentConfigurationList><run id="r" defaultInstrumentConfigurationRef="IC1">"#,
        );
        let mut meta = FileMetadataConfig::default();
        meta.instrument_configurations_mut().insert(0, InstrumentConfiguration { id: 0, ..Default::default() });
        meta.run_description_mut().unwrap().default_instrument_id = Some(0);
        let mut refs = DanglingRefs::check_metadata(&mut meta, Some(&h));
        assert_eq!(refs.pending, HashSet::from(["IC2".to_string()]));

        // mzdata numbered IC9 1 and IC2 2 (first sight, in whatever order it read them).
        let scans = br#"<run><spectrumList>
            <spectrum id="s1"><scanList><scan instrumentConfigurationRef="IC1"/><scan instrumentConfigurationRef="IC9"></scan></scanList></spectrum>
            <spectrum id="s2"><scanList><scan instrumentConfigurationRef="IC2"></scan></scanList></spectrum>
            <spectrum id="s3"><scanList><scan instrumentConfigurationRef="IC9"></scan></scanList></spectrum></spectrumList></run>"#;
        let numbered = HashSet::from(["IC1"]);
        refs.first_references = Some(scan_references(&scans[..], &numbered).unwrap());
        assert_eq!(
            refs.first_references.as_ref().unwrap(),
            &HashMap::from([(("s1".to_string(), 0), "IC9".to_string()), (("s2".to_string(), 0), "IC2".to_string())]),
            "a self-closing <scan/> is not counted, as mzdata skips it"
        );

        let spectrum = |id: &str, ns: &[u32]| {
            let mut d = SpectrumDescription { id: id.into(), ..Default::default() };
            d.acquisition.scans = ns.iter().map(|&n| mzdata::spectrum::ScanEvent { instrument_configuration_id: n, ..Default::default() }).collect();
            d
        };
        let (mut s1, mut s2, mut s3) = (spectrum("s1", &[1]), spectrum("s2", &[2]), spectrum("s3", &[1]));
        for s in [&mut s1, &mut s2, &mut s3] {
            refs.check_scans(s);
        }
        let ns = |d: &SpectrumDescription| d.acquisition.scans[0].instrument_configuration_id;
        assert_eq!([ns(&s1), ns(&s2), ns(&s3)], [NO_INSTRUMENT_CONFIGURATION, 2, NO_INSTRUMENT_CONFIGURATION]);
        assert_eq!(refs.summary().unwrap(), "2 instrumentConfigurationRef (IC9)");

        refs.restore_scan_configurations(&mut meta);
        let mut listed: Vec<u32> = meta.instrument_configurations().keys().copied().collect();
        listed.sort();
        assert_eq!(listed, [0, 2], "IC2 is back as configuration 2");
    }
}

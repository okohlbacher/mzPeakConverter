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
//!
//! The mzML exports run the same check (`convert_to_mzml` on an mzML or imzML source,
//! `filter_mzpeak_to_mzml` on an archive's index). An mzML states its header before its scans and
//! has no null for a scan's configuration, so there a configuration only scans name is numbered
//! AHEAD of them ([`DanglingRefs::number_scans_ahead`]), a scan whose reference was dropped is
//! written under the run's default — what an mzML scan without the attribute means — and the warning
//! is the only declaration ([`DanglingRefs::warn_mzml`]). Through 0.17.0-rc.1 the direct export
//! copied every reference as mzdata read it: `Test_P15_r2.imzML` came out with 2,826 scans naming
//! `IC2` under a list declaring `IC1` alone.
//!
//! Three more things the header (or a `<spectrum>` start tag) states and mzdata does not hand over as
//! stated are read back here, with the same parser, for the archive lanes and the direct mzML export
//! alike ([`DanglingRefs::check`] is the one entry of both):
//!
//! * a source file's **checksums** (`MS:1000569` SHA-1, `MS:1000568` MD5, `MS:1003151` SHA-256).
//!   mzdata types a param's value by trial parse, so a digest of decimal digits only became an
//!   integer (`…0123` → 123) and one reading as a float a float (`…e9` → 1.2e46).
//!   [`Header::restore_checksums`] writes the text back as a string, as the imzML lane does for the
//!   `.ibd` checksums;
//! * the run's **`startTimeStamp`** ([`DanglingRefs::start_time_stamp`]). mzdata keeps one only when
//!   it is RFC 3339 with an offset and drops any other with an ERROR line; `crate::mzml_start_time`
//!   reads the stamp for every lane: one without a zone is stored as the vendor lanes store an
//!   unzoned clock (the archive's `acquisition_time` block), and an mzML output writes that clock as
//!   its own `startTimeStamp`, zone-less as stated;
//! * a spectrum's **`sourceFileRef`** attribute, which mzdata does not read at all: the DESI ColAd
//!   imzML names one of 135 raw line files on each of 17,820 spectra. A file that mentions the
//!   attribute is read once more for it ([`spectrum_source_files`]) and each spectrum gets it as the
//!   parameter [`SOURCE_FILE_REF`] ([`DanglingRefs::check_spectrum`]), which an mzML output writes as
//!   a `userParam` (mzdata's writer has no such attribute); one naming no source file is dropped and
//!   declared like the other references.

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

/// The `software` id an mzML export declares, without version or term, for a `processingMethod`
/// whose software the source does not state: mzML requires the method's `softwareRef` to name an
/// entry, where an archive's `software_reference` may be empty.
pub const UNSTATED_SOFTWARE: &str = "software_not_stated";

/// The name of the spectrum parameter that carries an mzML `<spectrum sourceFileRef="…">`: the
/// attribute's own name, its value the id of an entry of `file_description.source_files`. A
/// parameter without an accession — PSI-MS has no term for the attribute, and the spec no column.
pub const SOURCE_FILE_REF: &str = "sourceFileRef";

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
    /// A source file's checksums ([`CHECKSUMS`]): accession and the value's text as the element
    /// states it, in document order.
    checksums: Vec<(String, String)>,
}

/// The PSI-MS terms that state a source file's checksum (the children of MS:1000561 `data file
/// checksum type`): MD5, SHA-1, SHA-256. Each is a digest of hex digits, so a text.
const CHECKSUMS: [&str; 3] = ["MS:1000568", "MS:1000569", "MS:1003151"];

/// What an mzML or imzML header states before `<run>`, read with quick_xml rather than mzdata: every
/// `<software>`, `<sourceFile>` (with its checksums) and `<instrumentConfiguration>` in document order,
/// self-closing or not, and the run's `defaultInstrumentConfigurationRef` and `startTimeStamp`.
#[derive(Debug, Default)]
pub struct Header {
    software: Vec<Entry>,
    source_files: Vec<Entry>,
    configurations: Vec<Entry>,
    default_configuration: Option<String>,
    /// The run's `startTimeStamp` as written; `None` for an empty one. mzdata keeps one only when
    /// it carries a UTC offset (RFC 3339) and discards an `xs:dateTime` without one.
    start_time_stamp: Option<String>,
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
        // Inside a `<sourceFile>…</sourceFile>`: its cvParams are that entry's.
        let mut in_source_file = false;
        loop {
            match reader.read_event_into(&mut buf).context("parsing the mzML header")? {
                // mzdata reads the run's attributes from its start tag, then the spectra.
                Event::Start(e) | Event::Empty(e) if e.name().as_ref() == b"run" => {
                    this.default_configuration = value(&e, b"defaultInstrumentConfigurationRef");
                    this.start_time_stamp = value(&e, b"startTimeStamp").filter(|t| !t.trim().is_empty());
                    break;
                }
                Event::Start(e) => {
                    in_source_file |= e.name().as_ref() == b"sourceFile";
                    this.entry(&e, false);
                    this.checksum(&e, in_source_file);
                }
                Event::Empty(e) => {
                    this.entry(&e, true);
                    this.checksum(&e, in_source_file);
                }
                Event::End(e) if e.name().as_ref() == b"sourceFile" => in_source_file = false,
                Event::Eof => break,
                _ => {}
            }
            buf.clear();
        }
        Ok(this)
    }

    /// A `<cvParam accession="MS:1000569" value="…"/>` inside a `<sourceFile>`: that entry's SHA-1;
    /// its MD5 (`MS:1000568`) and SHA-256 (`MS:1003151`) likewise.
    fn checksum(&mut self, e: &BytesStart, in_source_file: bool) {
        if !in_source_file || e.name().as_ref() != b"cvParam" {
            return;
        }
        let Some(accession) = value(e, b"accession").filter(|a| CHECKSUMS.contains(&a.as_str())) else { return };
        if let Some(sf) = self.source_files.last_mut() {
            sf.checksums.push((accession, value(e, b"value").unwrap_or_default()));
        }
    }

    /// The run's `startTimeStamp`, as stated; `None` when the run states none (or an empty one).
    pub fn start_time_stamp(&self) -> Option<&str> {
        self.start_time_stamp.as_deref()
    }

    /// Write each source file's checksums ([`CHECKSUMS`]: SHA-1, MD5, SHA-256) as the strings the
    /// header states. mzdata types a value by trial parse: 40 decimal digits become an integer or a
    /// float, and a digest was stored as `123` or `1.2345678901234568e46`. A term stated more than
    /// once on a file is matched in document order. Returns how many values were put back.
    pub fn restore_checksums(&self, target: &mut impl MSDataFileMetadata) -> usize {
        let mut restored = 0;
        for e in self.source_files.iter() {
            let Some(sf) = target.file_description_mut().source_files.iter_mut().find(|sf| sf.id == e.id) else { continue };
            for accession in CHECKSUMS {
                let stated = e.checksums.iter().filter(|(a, _)| a == accession).map(|(_, v)| v.as_str());
                let term: Option<mzdata::params::CURIE> = accession.parse().ok();
                let read = sf.params.iter_mut().filter(|p| term.is_some() && p.curie() == term);
                for (p, stated) in read.zip(stated) {
                    // An empty value states no digest (the DESI ColAd parameter file's): nothing to restore.
                    if !stated.is_empty() && !matches!(&p.value, mzdata::params::Value::String(v) if v == stated) {
                        p.value = mzdata::params::Value::String(stated.to_string());
                        restored += 1;
                    }
                }
            }
        }
        restored
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
    /// The ids of the source's source files, once the skipped entries are back.
    files: HashSet<String>,
    /// The ids of the source's data processings, which an array may name ([`Self::check_arrays`]).
    processing: HashSet<String>,
    /// The run's default processing, when it named nothing and was dropped: mzdata hands it to
    /// every array that states none of its own.
    dropped_default_processing: Option<String>,
    /// The `sourceFileRef` each `<spectrum>` states, by spectrum id; empty for a source that states
    /// none ([`spectrum_source_files`]).
    spectrum_files: HashMap<String, String>,
    /// The run's `startTimeStamp` as the source's header writes it ([`Header`]).
    start_time_stamp: Option<String>,
}

impl DanglingRefs {
    /// Read `path`'s header, put back the entries mzdata skipped ([`Header`]) and drop the run-level
    /// references that still name nothing ([`Self::check_metadata`]). Scans whose configuration is
    /// outside the list are later read back from `path`. After `decode_pwiz_ids`, before this
    /// conversion adds its own entries.
    pub fn check(path: &Path, target: &mut impl MSDataFileMetadata) -> Self {
        let spectrum_files = spectrum_source_files(path).unwrap_or_else(|e| {
            log::warn!("{}: the spectra's sourceFileRef attributes were not read: {e:#}", path.display());
            HashMap::new()
        });
        Self::check_with_spectrum_files(path, target, spectrum_files)
    }

    /// [`Self::check`] for a lane that has read the spectra's `sourceFileRef` attributes already
    /// (the direct mzML export, in `crate::mzml_unstated::SourceText`'s one pass over the text):
    /// the header is read, the spectra are not.
    pub fn check_with_spectrum_files(path: &Path, target: &mut impl MSDataFileMetadata, spectrum_files: HashMap<String, String>) -> Self {
        let header = Header::read(path)
            .inspect_err(|e| log::warn!("{}: header lists not read back, references are checked as mzdata read them: {e:#}", path.display()))
            .ok();
        let mut this = Self::check_metadata(target, header.as_ref());
        this.source = Some(path.to_path_buf());
        this.spectrum_files = spectrum_files;
        if !this.spectrum_files.is_empty() {
            log::info!("{} spectra name a source file (sourceFileRef); kept as the spectrum parameter {SOURCE_FILE_REF:?}", this.spectrum_files.len());
        }
        this
    }

    /// The run's `startTimeStamp` as the source's header writes it, for `crate::mzml_start_time`:
    /// an archive stores a clock without a zone in its `acquisition_time` block, and an mzML output
    /// writes it as stated (`xs:dateTime` has the zone-less form, mzdata's run model does not).
    pub fn start_time_stamp(&self) -> Option<&str> {
        self.start_time_stamp.as_deref()
    }

    /// For a lane that writes its configuration list BEFORE its scans (an mzML output): give each
    /// configuration the header states, mzdata skipped and only scans name ([`Self::pending`]) the
    /// number mzdata will give it, and put it back now. mzdata numbers an id on first sight, so the
    /// ids the header does not number get the next numbers in the order their first scans stand in
    /// the source, which is read once here; an id that names nothing is noted under its number, to
    /// be dropped by name when its scans arrive. Nothing to do — and the source is not read — when
    /// no such configuration is pending. Scans must then be read in document order; one whose
    /// number still falls outside the list is classified as [`Self::check_scans`] always does, too
    /// late for the header (the caller writes it under the run's default).
    pub fn number_scans_ahead(&mut self, target: &mut impl MSDataFileMetadata) {
        // `pending` holds an id only when the replay of the header's numbering matched mzdata's.
        if self.pending.is_empty() {
            return;
        }
        let Some(path) = self.source.clone() else { return };
        let numbered: HashSet<&str> = self.names.values().map(String::as_str).collect();
        let found = match first_scan_references(&path, &numbered) {
            Ok(found) => found,
            Err(e) => {
                log::warn!("{}: scan configuration ids not read back: {e:#}", path.display());
                return;
            }
        };
        let next = self.names.len() as u32;
        for (k, (_, id)) in found.iter().enumerate() {
            let n = next + k as u32;
            if self.pending.contains(id) {
                self.configurations.insert(n);
                self.restored.insert(n);
            } else {
                self.dangling.insert(n, id.clone());
            }
        }
        self.first_references = Some(found.into_iter().collect());
        self.restore_scan_configurations(target);
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
            let checksums = header.restore_checksums(target);
            if checksums > 0 {
                log::info!("{checksums} source file SHA-1 (MS:1000569) written as the text the header states (mzdata read a number)");
            }
            this.start_time_stamp = header.start_time_stamp().map(str::to_string);
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
        this.files = files.clone();
        this.processing = processing.clone();
        if let Some(run) = target.run_description_mut() {
            if let Some(id) = run.default_instrument_id.filter(|id| !this.configurations.contains(id)) {
                run.default_instrument_id = None;
                let name = this.name(id);
                this.note("defaultInstrumentConfigurationRef", name);
            }
            if let Some(id) = run.default_data_processing_id.take_if(|id| !processing.contains(id.as_str())) {
                this.dropped_default_processing = Some(id.clone());
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

    /// One spectrum on its way into the archive or an mzML output: its scans' configurations
    /// ([`Self::check_scans`]) and the source file its `<spectrum>` names. mzdata does not read `spectrum@sourceFileRef`; the id
    /// read back from the source becomes the parameter [`SOURCE_FILE_REF`] when the source lists that
    /// file, and is dropped and counted like any other reference when it does not.
    pub fn check_spectrum(&mut self, descr: &mut SpectrumDescription) {
        self.check_scans(descr);
        if self.spectrum_files.is_empty() {
            return;
        }
        let Some(file) = self.spectrum_files.get(&descr.id).cloned() else { return };
        if self.files.contains(&file) {
            // A string whatever it spells: an id of digits must not become a number.
            descr.params.push(source_file_ref_param(file));
        } else {
            self.note("sourceFileRef", file);
        }
    }

    /// Clear the `dataProcessingRef` of each array of a spectrum or chromatogram that names no
    /// entry of the processing list, for a lane that writes the arrays as the source holds them (an
    /// mzML output; an archive stores no such reference for a signal array). mzdata gives an array
    /// that states none its spectrum's, else its list's default: so the list default that was
    /// dropped already ([`Self::check_metadata`]) comes back on every array and is cleared without
    /// being counted again, and any other id that names nothing is counted as a `dataProcessingRef`
    /// of its own. A cleared array is written without the attribute and falls under the output's
    /// default processing.
    pub fn check_arrays(&mut self, arrays: &mut mzdata::spectrum::BinaryArrayMap) {
        for (_, array) in arrays.iter_mut() {
            let Some(id) = array.data_processing_reference().filter(|id| !self.processing.contains(*id)).map(str::to_string) else {
                continue;
            };
            array.set_data_processing_reference(None);
            if self.dropped_default_processing.as_deref() != Some(id.as_str()) {
                self.note("dataProcessingRef", id);
            }
        }
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
                    Vec::new()
                }),
                None => Vec::new(),
            };
            self.first_references = Some(found.into_iter().collect());
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

    /// [`Self::warn`] for an mzML output, which has no `transformations` list: the warning is the
    /// declaration, and says what the export writes in a dropped reference's place. `lists` names
    /// whose lists the references were checked against (`source`, `archive`).
    pub fn warn_mzml(&self, input: &Path, lists: &str) {
        if let Some(what) = self.summary() {
            log::warn!(
                "{}: dropped references that name no entry of the {lists}'s lists: {what}; such a scan is \
                 written under the run's default configuration, a run default names the list's first \
                 entry, a software reference is left out (a processing method names `{UNSTATED_SOFTWARE}`), \
                 a spectrum's sourceFileRef parameter is not written, and an array's dataProcessingRef is \
                 left out (the array falls under the default processing). \
                 mzML has no transformations list to declare this in",
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

/// The spectrum parameter for a `sourceFileRef`.
fn source_file_ref_param(file: String) -> mzdata::params::Param {
    mzdata::params::Param::builder().name(SOURCE_FILE_REF).value(mzdata::params::Value::String(file)).build()
}

/// The `sourceFileRef` each `<spectrum>` of an mzML or imzML states, by spectrum id. The file is
/// first searched for the attribute as bytes (`sourceFileRef=`: the run's `defaultSourceFileRef`
/// has a capital S, a scan settings' `<sourceFileRef ref=…>` no `=` after the name), and only one
/// that mentions it is parsed — a `<scan>` or `<precursor>` naming an external spectrum's file is
/// such a mention too, and then the result is empty.
pub fn spectrum_source_files(path: &Path) -> Result<HashMap<String, String>> {
    let [mentioned] = crate::imaging::file_mentions(path, ["sourceFileRef="]).with_context(|| format!("reading {}", path.display()))?;
    if !mentioned {
        return Ok(HashMap::new());
    }
    let file = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    spectrum_source_files_from(std::io::BufReader::new(file))
}

fn spectrum_source_files_from(input: impl BufRead) -> Result<HashMap<String, String>> {
    let mut reader = quick_xml::Reader::from_reader(input);
    let mut buf = Vec::new();
    let mut found = HashMap::new();
    loop {
        match reader.read_event_into(&mut buf).context("parsing the source's spectra")? {
            Event::Start(e) | Event::Empty(e) if e.name().as_ref() == b"spectrum" => {
                if let (Some(id), Some(file)) = (value(&e, b"id"), value(&e, b"sourceFileRef")) {
                    found.insert(id, file);
                }
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    Ok(found)
}

/// The first scan naming each configuration id that `numbered` lacks, by (spectrum id, position among
/// the spectrum's `<scan>` start tags — mzdata skips a self-closing one): the source's own id for a
/// number mzdata gave on first sight. In document order, the order mzdata numbers them in when it
/// reads the spectra from the first on.
type FirstReferences = Vec<((String, usize), String)>;

fn first_scan_references(path: &Path, numbered: &HashSet<&str>) -> Result<FirstReferences> {
    let file = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    scan_references(std::io::BufReader::new(file), numbered)
}

fn scan_references(input: impl BufRead, numbered: &HashSet<&str>) -> Result<FirstReferences> {
    let mut reader = quick_xml::Reader::from_reader(input);
    let mut buf = Vec::new();
    let mut found = Vec::new();
    let mut seen = HashSet::new();
    let (mut spectrum, mut at) = (String::new(), 0usize);
    loop {
        match reader.read_event_into(&mut buf).context("parsing the source's scans")? {
            Event::Start(e) => match e.name().as_ref() {
                b"spectrum" => (spectrum, at) = (value(&e, b"id").unwrap_or_default(), 0),
                b"scan" => {
                    if let Some(id) = value(&e, b"instrumentConfigurationRef") {
                        if !numbered.contains(id.as_str()) && seen.insert(id.clone()) {
                            found.push(((spectrum.clone(), at), id));
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
          <run id="r" defaultInstrumentConfigurationRef="IC2" startTimeStamp="2009-08-11T15:59:44"><spectrumList><spectrum><software id="late"/></spectrum></spectrumList></run></mzML>"#,
        );
        let ids = |l: &[Entry]| l.iter().map(|e| (e.id.clone(), e.self_closing)).collect::<Vec<_>>();
        assert_eq!(ids(&h.software), [("MALDIquantForeign".into(), true), ("pwiz".into(), false), ("noversion".into(), true)]);
        assert_eq!((h.software[0].version.as_str(), h.software[2].version.as_str()), ("0.12", ""));
        assert_eq!(ids(&h.source_files), [("sf1".into(), false), ("sf&2".into(), true)]);
        assert_eq!((h.source_files[1].name.as_str(), h.source_files[1].location.as_str()), ("b.raw", "file:///e"));
        assert_eq!(ids(&h.configurations), [("IC1".into(), false), ("IC2".into(), true)]);
        assert_eq!(h.default_configuration.as_deref(), Some("IC2"));
        assert_eq!(h.start_time_stamp.as_deref(), Some("2009-08-11T15:59:44"), "as written, zone or not");
        // IC1's start tag first, then the run's default IC2, which mzdata never read as an entry.
        assert_eq!(h.numbering(), HashMap::from([("IC1".to_string(), 0), ("IC2".to_string(), 1)]));
    }

    /// The header's source-file digests and the run's start time stamp, as stated; a digest mzdata
    /// read as a number is written back as the text, an empty one and a matching string are left.
    #[test]
    fn checksums_and_the_start_time_stamp_are_read_as_stated() {
        let h = header(
            r#"<mzML><fileDescription><sourceFileList count="4">
              <sourceFile id="digits" name="a.raw" location="file:///d"><cvParam accession="MS:1000563" name="Thermo RAW format"/>
                <cvParam cvRef="MS" accession="MS:1000569" name="SHA-1" value="0000000000000000000000000000000000000123"/></sourceFile>
              <sourceFile id="hex" name="b.raw" location="file:///d"><cvParam accession="MS:1000569" value="71be39fb2700ab2f3c8b2234b91274968b6899b1"/>
                <cvParam accession="MS:1000568" name="MD5" value="00000000000000000000000000000456"/>
                <cvParam accession="MS:1003151" name="SHA-256" value="1e63"/></sourceFile>
              <sourceFile id="empty" name="p" location=""><cvParam accession="MS:1000569" value=""/></sourceFile>
              <sourceFile id="none" name="c.raw" location="file:///d"/>
            </sourceFileList></fileDescription>
            <softwareList><software id="pwiz" version="3"><cvParam accession="MS:1000569" value="not a source file's"/></software></softwareList>
            <run id="r" startTimeStamp="2009-08-11T15:59:44"><spectrumList/></run></mzML>"#,
        );
        let stated: Vec<(&str, Vec<(&str, &str)>)> =
            h.source_files.iter().map(|e| (e.id.as_str(), e.checksums.iter().map(|(a, v)| (a.as_str(), v.as_str())).collect())).collect();
        assert_eq!(
            stated,
            [
                ("digits", vec![("MS:1000569", "0000000000000000000000000000000000000123")]),
                (
                    "hex",
                    vec![("MS:1000569", "71be39fb2700ab2f3c8b2234b91274968b6899b1"), ("MS:1000568", "00000000000000000000000000000456"), ("MS:1003151", "1e63")]
                ),
                ("empty", vec![("MS:1000569", "")]),
                ("none", vec![]),
            ]
        );
        assert_eq!(h.start_time_stamp(), Some("2009-08-11T15:59:44"));
        assert_eq!(header(r#"<mzML><run id="r" startTimeStamp=" "/></mzML>"#).start_time_stamp(), None);
        assert_eq!(header(r#"<mzML><run id="r"><spectrumList/></run></mzML>"#).start_time_stamp(), None);

        // What mzdata read: the digits as an integer, the hex as a string, the empty one as empty.
        use mzdata::params::{Param, Value};
        let sha1 = |v: Value| Param::builder().name("SHA-1").curie(mzdata::curie!(MS:1000569)).value(v).build();
        let mut meta = FileMetadataConfig::default();
        for (id, v) in [("digits", Value::Int(123)), ("hex", Value::String("71be39fb2700ab2f3c8b2234b91274968b6899b1".into())), ("empty", Value::Empty)] {
            meta.file_description_mut().source_files.push(SourceFile { id: id.into(), params: vec![sha1(v)], ..Default::default() });
        }
        // …and the MD5 as an integer, the SHA-256 as a float: digests too, restored like the SHA-1.
        let hex = &mut meta.file_description_mut().source_files[1];
        hex.params.push(Param::builder().name("MD5").curie(mzdata::curie!(MS:1000568)).value(Value::Int(456)).build());
        hex.params.push(Param::builder().name("SHA-256").curie(mzdata::curie!(MS:1003151)).value(Value::Float(1e63)).build());
        assert_eq!(h.restore_checksums(&mut meta), 3);
        let values: Vec<&Value> = meta.file_description().source_files.iter().map(|sf| &sf.params[0].value).collect();
        assert_eq!(
            values,
            [&Value::String("0000000000000000000000000000000000000123".into()), &Value::String("71be39fb2700ab2f3c8b2234b91274968b6899b1".into()), &Value::Empty]
        );
        let others: Vec<&Value> = meta.file_description().source_files[1].params[1..].iter().map(|p| &p.value).collect();
        assert_eq!(others, [&Value::String("00000000000000000000000000000456".into()), &Value::String("1e63".into())]);
        assert_eq!(h.restore_checksums(&mut meta), 0, "nothing left to restore");
    }

    /// Each `<spectrum>`'s `sourceFileRef`, by spectrum id; a scan's or a precursor's attribute of
    /// the same name (an external spectrum's file) is not the spectrum's. One the source lists
    /// becomes the spectrum's parameter, a string whatever it spells; one it does not is dropped.
    #[test]
    fn a_spectrum_s_source_file_reference_becomes_its_parameter() {
        let doc = br#"<mzML><run><spectrumList>
            <spectrum index="0" id="File=0Scan=1" defaultArrayLength="0" sourceFileRef="sf1"><scanList><scan sourceFileRef="sfX" externalSpectrumID="scan=3"/></scanList></spectrum>
            <spectrum index="1" id="File=1Scan=1" defaultArrayLength="0"><precursorList><precursor sourceFileRef="sfY" externalSpectrumID="scan=4"/></precursorList></spectrum>
            <spectrum index="2" id="a&amp;b" sourceFileRef="007"/>
            <spectrum index="3" id="lost" sourceFileRef="ghost"></spectrum>
        </spectrumList></run></mzML>"#;
        let found = spectrum_source_files_from(&doc[..]).unwrap();
        assert_eq!(
            found,
            HashMap::from([("File=0Scan=1".to_string(), "sf1".to_string()), ("a&b".to_string(), "007".to_string()), ("lost".to_string(), "ghost".to_string())])
        );

        let mut refs = DanglingRefs { files: HashSet::from(["sf1".to_string(), "007".to_string()]), spectrum_files: found, ..Default::default() };
        let stored = |refs: &mut DanglingRefs, id: &str| {
            let mut d = SpectrumDescription { id: id.into(), ..Default::default() };
            refs.check_spectrum(&mut d);
            d.params.iter().find(|p| p.name == SOURCE_FILE_REF).map(|p| p.value.clone())
        };
        use mzdata::params::Value;
        assert_eq!(stored(&mut refs, "File=0Scan=1"), Some(Value::String("sf1".into())));
        assert_eq!(stored(&mut refs, "a&b"), Some(Value::String("007".into())), "an id of digits stays a string");
        assert_eq!(stored(&mut refs, "File=1Scan=1"), None);
        assert_eq!(refs.summary(), None);
        assert_eq!(stored(&mut refs, "lost"), None);
        assert_eq!(refs.summary().unwrap(), "1 sourceFileRef (ghost)");
        assert_eq!(refs.transformation(), Some(DROPPED));
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

    /// An array naming a processing the list does not hold is written without the reference: the
    /// dropped list default, which mzdata hands every array, without a second count; an id of the
    /// array's own counted; one that resolves kept.
    #[test]
    fn an_array_s_dangling_processing_reference_is_cleared() {
        use mzdata::spectrum::{ArrayType, BinaryArrayMap, BinaryDataArrayType, DataArray};
        let mut meta = FileMetadataConfig::default();
        meta.data_processings_mut().push(DataProcessing { id: "dp".into(), methods: Vec::new() });
        meta.run_description_mut().unwrap().default_data_processing_id = Some("dp1".into());
        let mut refs = DanglingRefs::check_metadata(&mut meta, None);
        assert_eq!(refs.summary().unwrap(), "1 defaultDataProcessingRef (dp1)");
        let mut arrays = BinaryArrayMap::new();
        for (name, reference) in [(ArrayType::MZArray, "dp1"), (ArrayType::IntensityArray, "dp"), (ArrayType::ChargeArray, "ghost")] {
            let mut array = DataArray::wrap(&name, BinaryDataArrayType::Float64, Vec::new());
            array.set_data_processing_reference(Some(reference.into()));
            arrays.add(array);
        }
        refs.check_arrays(&mut arrays);
        let named = |name: &ArrayType| arrays.get(name).unwrap().data_processing_reference().map(str::to_string);
        assert_eq!([named(&ArrayType::MZArray), named(&ArrayType::IntensityArray), named(&ArrayType::ChargeArray)], [None, Some("dp".to_string()), None]);
        assert_eq!(refs.summary().unwrap(), "1 dataProcessingRef (ghost), 1 defaultDataProcessingRef (dp1)");
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
        let found = scan_references(&scans[..], &numbered).unwrap();
        assert_eq!(
            found,
            [(("s1".to_string(), 0), "IC9".to_string()), (("s2".to_string(), 0), "IC2".to_string())],
            "in document order; a self-closing <scan/> is not counted, as mzdata skips it"
        );
        refs.first_references = Some(found.into_iter().collect());

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

    /// A lane that writes its header before its scans numbers the configurations only scans name
    /// ahead of them: the source's scans name `IC9` (nothing) first, then `IC2` (stated, self-closing),
    /// so mzdata will number them 1 and 2 — IC2 is in the list before a scan is read, IC9 is known by
    /// name, and the scans then check as they do on the archive lane.
    #[test]
    fn configurations_only_scans_name_are_numbered_ahead_of_the_scans() {
        let source = r#"<mzML><instrumentConfigurationList><instrumentConfiguration id="IC1"><cvParam/></instrumentConfiguration>
            <instrumentConfiguration id="IC2"/></instrumentConfigurationList><run id="r" defaultInstrumentConfigurationRef="IC1" startTimeStamp="2009-08-11T15:59:44"><spectrumList>
            <spectrum id="s1"><scanList><scan instrumentConfigurationRef="IC1"></scan><scan instrumentConfigurationRef="IC9"></scan></scanList></spectrum>
            <spectrum id="s2"><scanList><scan instrumentConfigurationRef="IC2"></scan></scanList></spectrum></spectrumList></run></mzML>"#;
        let path = std::env::temp_dir().join(format!("mzpc-refs-ahead-{}.mzML", std::process::id()));
        std::fs::write(&path, source).unwrap();
        // What mzdata read from the header: IC1 as configuration 0, the run's default.
        let mut meta = FileMetadataConfig::default();
        meta.instrument_configurations_mut().insert(0, InstrumentConfiguration { id: 0, ..Default::default() });
        meta.run_description_mut().unwrap().default_instrument_id = Some(0);
        let mut refs = DanglingRefs::check(&path, &mut meta);
        assert_eq!(refs.start_time_stamp(), Some("2009-08-11T15:59:44"));
        let listed = |meta: &FileMetadataConfig| {
            let mut n: Vec<u32> = meta.instrument_configurations().keys().copied().collect();
            n.sort();
            n
        };
        assert_eq!(listed(&meta), [0], "IC2 has no number yet");

        refs.number_scans_ahead(&mut meta);
        assert_eq!(listed(&meta), [0, 2], "IC9 will be 1, IC2 2");
        let _ = std::fs::remove_file(&path);

        let spectrum = |id: &str, ns: &[u32]| {
            let mut d = SpectrumDescription { id: id.into(), ..Default::default() };
            d.acquisition.scans = ns.iter().map(|&n| mzdata::spectrum::ScanEvent { instrument_configuration_id: n, ..Default::default() }).collect();
            d
        };
        let (mut s1, mut s2) = (spectrum("s1", &[0, 1]), spectrum("s2", &[2]));
        refs.check_scans(&mut s1);
        refs.check_scans(&mut s2);
        let ns = |d: &SpectrumDescription| d.acquisition.scans.iter().map(|s| s.instrument_configuration_id).collect::<Vec<_>>();
        assert_eq!((ns(&s1), ns(&s2)), (vec![0, NO_INSTRUMENT_CONFIGURATION], vec![2]));
        assert_eq!(refs.summary().unwrap(), "1 instrumentConfigurationRef (IC9)");

        // Nothing pending: the source is not read (it is gone), and nothing changes.
        let mut plain = DanglingRefs::check_metadata(&mut meta, None);
        plain.source = Some(path);
        plain.number_scans_ahead(&mut meta);
        assert_eq!(listed(&meta), [0, 2]);
    }
}

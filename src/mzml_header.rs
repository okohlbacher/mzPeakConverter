//! mzML export: the parts of the header mzdata's writer cannot write.
//!
//! mzdata 0.67's mzML writer
//!
//! * writes a fixed `<cvList count="2">` of MS and UO, whatever vocabularies its params name: the
//!   export of an imaging run wrote every pixel position as `cvRef="IMS"` against a list that does
//!   not declare `IMS` (18 such params in the export of a 6-pixel imzML, HUPO-PSI/mzPeak-specification#23);
//! * counts the `<scanSettingsList>` by the number of SAMPLES (`count="0"` over one entry);
//! * writes a scan settings' source file reference as text, `<sourceFileRef>id</sourceFileRef>`,
//!   where the schema has an attribute, `<sourceFileRef ref="id"/>`;
//! * writes `<softwareRef ref=""/>` for an instrument configuration that names no software and
//!   `<componentList count="0">` for one without components, where the schema has an `xs:IDREF`
//!   (never empty) and a list of at least one source, analyzer and detector, and makes both
//!   elements optional;
//! * states the run as `<run id="1">` with the LOWEST-numbered instrument configuration and the
//!   FIRST source file as its defaults and no `startTimeStamp`, whatever its run description holds
//!   (`MRM Neg C5`, acquired 2006-09-10T02:11:56Z from `MSScan.bin`, came out as run `1` of
//!   `acqmethod.xml`, undated).
//!
//! [`HeaderFixes`] sits under that writer, like [`crate::mzml_isolation`] and
//! [`crate::mzml_wavelength`], holds the header (everything up to the end of the `<run …>` start
//! tag) and rewrites those: the first three and the two empty elements from the text itself, the
//! run from a [`Run`] the lane hands over through a [`RunCell`] once its metadata is complete (the
//! sink is built before the writer that owns it). Unlike the two other sinks it cannot keep the
//! length: a `<cv/>` element has to be ADDED. So it also shifts every byte offset the writer records
//! at the end of the document — each `<offset idRef="…">` of the `<indexList>` and the
//! `<indexListOffset>` — by the number of bytes the header grew. Everything between passes through
//! untouched. A header with nothing to fix (no vocabulary to add, no scan settings, no empty
//! element, no [`Run`]) leaves the stream byte-identical.
//!
//! The `<fileChecksum>` is not this sink's: [`crate::mzml_index::IndexFixes`], the last sink, writes
//! it over the bytes as they are stored.

use std::cell::RefCell;
use std::io::{self, Write};
use std::rc::Rc;

use crate::mzml_isolation::find;

/// One `<cv/>` of an mzML `<cvList>`.
#[derive(Debug, Clone, PartialEq)]
pub struct Cv {
    pub id: String,
    pub full_name: String,
    pub uri: String,
    pub version: Option<String>,
}

impl Cv {
    /// The imaging vocabulary as this converter's archives declare it: pinned to a commit (the
    /// imaging profile requires that; `imagingMS.obo` publishes no releases).
    pub fn ims() -> Self {
        mzpeak_prototyping::param::ControlledVocabularyEntry::from(mzdata::params::ControlledVocabulary::IMS).into()
    }

    /// The entry with this id of an archive's `cv_list` index block.
    pub fn from_cv_list(cv_list: Option<&serde_json::Value>, id: &str) -> Option<Self> {
        let entry = cv_list?.as_array()?.iter().find(|cv| cv["id"] == id)?;
        Some(Self {
            id: id.to_string(),
            full_name: entry["full_name"].as_str()?.to_string(),
            uri: entry["uri"].as_str()?.to_string(),
            version: entry["version"].as_str().map(str::to_string),
        })
    }

    fn element(&self) -> String {
        let mut e = format!("<cv id=\"{}\" fullName=\"{}\" URI=\"{}\"", escape(&self.id), escape(&self.full_name), escape(&self.uri));
        if let Some(v) = &self.version {
            e.push_str(&format!(" version=\"{}\"", escape(v)));
        }
        e.push_str("/>");
        e
    }
}

impl From<mzpeak_prototyping::param::ControlledVocabularyEntry> for Cv {
    fn from(e: mzpeak_prototyping::param::ControlledVocabularyEntry) -> Self {
        Self { id: e.id, full_name: e.full_name, uri: e.uri, version: e.version }
    }
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// What an export's `<run>` start tag states. Each id is the one the header declares, already an
/// XML name ([`crate::pwiz_id::encode`]).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Run {
    /// `id`: the run's own, an `xs:ID`.
    pub id: String,
    /// `defaultInstrumentConfigurationRef`; `None` keeps the one mzdata wrote.
    pub default_instrument_configuration: Option<String>,
    /// `startTimeStamp`, an `xs:dateTime` — with an offset, or without one for a clock whose zone
    /// the source does not state; `None` writes no attribute.
    pub start_time_stamp: Option<String>,
    /// `defaultSourceFileRef`; `None` keeps the one mzdata wrote.
    pub default_source_file: Option<String>,
}

/// Where a lane puts its [`Run`] for the sink to find when the header arrives: the sink is boxed
/// into the writer before the writer's metadata is complete. Empty, the run tag stays as written.
pub type RunCell = Rc<RefCell<Option<Run>>>;

/// The header ends where the run starts.
const RUN: &[u8] = b"<run ";
const INDEX_LIST: &[u8] = b"<indexList";
const OFFSET_END: &[u8] = b"</offset>";
const LIST_OFFSET_END: &[u8] = b"</indexListOffset>";

enum State {
    /// Holding the header, until `<run `.
    Header,
    /// Passing spectra and chromatograms through, until `<indexList`.
    Body,
    /// Shifting the offsets of the index, until `</indexListOffset>`.
    Index,
    /// Nothing left to change.
    Through,
}

/// A byte sink for `MzMLWriter` that declares `cv` in the `<cvList>` when it is not there, counts
/// the `<scanSettingsList>`, writes `<sourceFileRef>` as the schema has it, leaves out an empty
/// `<softwareRef>` or `<componentList>` and states the run as `run` has it (see the module docs).
pub struct HeaderFixes<W: Write> {
    inner: W,
    cv: Option<Cv>,
    run: RunCell,
    state: State,
    /// Bytes not passed on yet: the header, a tail that may still become `<indexList`, or an index
    /// element that is still incomplete.
    held: Vec<u8>,
    /// How many bytes the header grew by.
    shift: i64,
}

impl<W: Write> HeaderFixes<W> {
    /// Without a [`Run`]: the run tag stays as mzdata writes it.
    #[cfg(test)]
    pub fn new(inner: W, cv: Option<Cv>) -> Self {
        Self::with_run(inner, cv, RunCell::default())
    }

    /// `run` is read when the header is complete, so the lane may fill it any time before the
    /// writer's first spectrum.
    pub fn with_run(inner: W, cv: Option<Cv>, run: RunCell) -> Self {
        Self { inner, cv, run, state: State::Header, held: Vec::new(), shift: 0 }
    }

    /// Pass on what is complete in `held`; with `end`, everything.
    fn advance(&mut self, end: bool) -> io::Result<()> {
        loop {
            match self.state {
                State::Header => {
                    let Some(run) = find(&self.held, RUN) else {
                        break;
                    };
                    // The header ends with the run's start tag, which is rewritten too.
                    let Some(end) = tag_end(&self.held[run..]).map(|len| run + len) else {
                        break;
                    };
                    let mut header = fix_header(&self.held[..run], self.cv.as_ref());
                    header.extend(fix_run(&self.held[run..end], self.run.borrow().as_ref()));
                    self.shift = header.len() as i64 - end as i64;
                    self.inner.write_all(&header)?;
                    self.held.drain(..end);
                    self.state = if self.shift == 0 { State::Through } else { State::Body };
                }
                State::Body => match find(&self.held, INDEX_LIST) {
                    Some(at) => {
                        self.inner.write_all(&self.held[..at])?;
                        self.held.drain(..at);
                        self.state = State::Index;
                    }
                    None => {
                        let keep = (1..INDEX_LIST.len()).rev().find(|&k| self.held.ends_with(&INDEX_LIST[..k])).unwrap_or(0);
                        let done = self.held.len() - keep;
                        self.inner.write_all(&self.held[..done])?;
                        self.held.drain(..done);
                        break;
                    }
                },
                State::Index => {
                    // Up to the last complete element; the `<indexListOffset>` is the last of them.
                    let last = find(&self.held, LIST_OFFSET_END).map(|at| (at + LIST_OFFSET_END.len(), true)).or_else(|| rfind(&self.held, OFFSET_END).map(|at| (at + OFFSET_END.len(), false)));
                    let Some((done, finished)) = last else {
                        break;
                    };
                    let shifted = shift_offsets(&self.held[..done], self.shift);
                    self.inner.write_all(&shifted)?;
                    self.held.drain(..done);
                    if !finished {
                        break;
                    }
                    self.state = State::Through;
                }
                State::Through => break,
            }
        }
        if end || matches!(self.state, State::Through) {
            self.inner.write_all(&self.held)?;
            self.held.clear();
        }
        Ok(())
    }
}

impl<W: Write> Write for HeaderFixes<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.held.extend_from_slice(buf);
        self.advance(false)?;
        Ok(buf.len())
    }

    /// Only what is complete goes out: a flush in mid-document must not let a header, an
    /// `<indexList` or an offset through in two halves, unfixed. mzdata flushes once, after the
    /// document is closed, and by then nothing is held: the index ends this sink's work. What a
    /// truncated document leaves here goes out when the sink is dropped.
    fn flush(&mut self) -> io::Result<()> {
        self.advance(false)?;
        self.inner.flush()
    }
}

impl<W: Write> Drop for HeaderFixes<W> {
    fn drop(&mut self) {
        if !self.held.is_empty() {
            let _ = self.advance(true);
        }
    }
}

fn rfind(hay: &[u8], needle: &[u8]) -> Option<usize> {
    (0..=hay.len().checked_sub(needle.len())?).rev().find(|&i| hay[i..].starts_with(needle))
}

/// The header as it should read. Not UTF-8 (mzdata writes UTF-8), or nothing to fix: as it is.
fn fix_header(header: &[u8], cv: Option<&Cv>) -> Vec<u8> {
    let Ok(text) = std::str::from_utf8(header) else {
        return header.to_vec();
    };
    let mut text = text.to_string();
    if let Some(cv) = cv {
        declare_cv(&mut text, cv);
    }
    count_scan_settings(&mut text);
    source_file_refs(&mut text);
    drop_empty_elements(&mut text);
    text.into_bytes()
}

/// The length of the start tag `tag` begins with, its `>` included: the first `>` outside a quoted
/// attribute value. `None` while the tag is incomplete.
fn tag_end(tag: &[u8]) -> Option<usize> {
    let mut quote = None;
    for (i, &b) in tag.iter().enumerate() {
        match (quote, b) {
            (None, b'"' | b'\'') => quote = Some(b),
            (Some(q), _) if q == b => quote = None,
            (None, b'>') => return Some(i + 1),
            _ => {}
        }
    }
    None
}

/// The value of attribute `name` of the start tag `tag`, as written (escaped).
fn attribute<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let open = format!(" {name}=\"");
    let from = tag.find(&open)? + open.len();
    Some(&tag[from..from + tag[from..].find('"')?])
}

/// The `<run …>` start tag as `run` states it: its id, its default instrument configuration, its
/// start time when one is known, its default source file — in the order ProteoWizard writes them.
/// A default `run` does not state stays as mzdata wrote it. Without `run`, or for a tag that is not
/// UTF-8, the tag as it is.
fn fix_run(tag: &[u8], run: Option<&Run>) -> Vec<u8> {
    let (Some(run), Ok(text)) = (run, std::str::from_utf8(tag)) else {
        return tag.to_vec();
    };
    let written = |name: &str| attribute(text, name).map(str::to_string);
    let mut out = format!("<run id=\"{}\"", escape(&run.id));
    let mut push = |name: &str, value: Option<String>| {
        if let Some(v) = value {
            out.push_str(&format!(" {name}=\"{v}\""));
        }
    };
    let stated = |v: &Option<String>| v.as_deref().map(escape);
    push("defaultInstrumentConfigurationRef", stated(&run.default_instrument_configuration).or_else(|| written("defaultInstrumentConfigurationRef")));
    push("startTimeStamp", stated(&run.start_time_stamp));
    push("defaultSourceFileRef", stated(&run.default_source_file).or_else(|| written("defaultSourceFileRef")));
    out.push('>');
    out.into_bytes()
}

/// Leave out each `<softwareRef ref=""/>` (an instrument configuration that names no software) and
/// each `<componentList count="0">…</componentList>` (one without components), with the line it
/// stands on: the schema makes both elements optional, and takes neither an empty `xs:IDREF` nor a
/// component list without a source, an analyzer and a detector.
fn drop_empty_elements(text: &mut String) {
    const SOFTWARE_REF: &str = "<softwareRef ref=\"\"/>";
    const COMPONENTS: &str = "<componentList count=\"0\">";
    const COMPONENTS_END: &str = "</componentList>";
    while let Some(at) = text.find(SOFTWARE_REF) {
        remove_with_line(text, at, at + SOFTWARE_REF.len());
    }
    let mut from = 0;
    while let Some(at) = text[from..].find(COMPONENTS).map(|i| from + i) {
        let inside = at + COMPONENTS.len();
        let blank = text[inside..].len() - text[inside..].trim_start().len();
        if text[inside + blank..].starts_with(COMPONENTS_END) {
            remove_with_line(text, at, inside + blank + COMPONENTS_END.len());
        } else {
            from = inside;
        }
    }
}

/// Remove `from..to` of `text`, and with it the line it stands on when nothing else does: the
/// indentation before it and the line break after it.
fn remove_with_line(text: &mut String, from: usize, to: usize) {
    let line = text[..from].rfind('\n').map_or(0, |i| i + 1);
    let alone = text[line..from].chars().all(|c| c == ' ' || c == '\t') && text[to..].starts_with('\n');
    if alone {
        text.replace_range(line..to + 1, "");
    } else {
        text.replace_range(from..to, "");
    }
}

/// The value of the first `<{element} count="…">`, as a range of `text`.
fn count_attribute(text: &str, element: &str) -> Option<std::ops::Range<usize>> {
    let open = format!("<{element} count=\"");
    let from = text.find(&open)? + open.len();
    Some(from..from + text[from..].find('"')?)
}

/// Add `cv` as the last entry of the `<cvList>`, on a line of its own indented like its siblings,
/// and count it. Nothing when the list already declares the id.
fn declare_cv(text: &mut String, cv: &Cv) {
    let Some(close) = text.find("</cvList>") else { return };
    if text[..close].contains(&format!("<cv id=\"{}\"", escape(&cv.id))) {
        return;
    }
    let line = text[..close].rfind('\n').map_or(0, |i| i + 1);
    let indent = &text[line..close];
    let entry = if indent.chars().all(|c| c == ' ' || c == '\t') && line > 0 {
        (line, format!("{indent}  {}\n", cv.element()))
    } else {
        (close, cv.element())
    };
    text.insert_str(entry.0, &entry.1);
    if let Some(count) = count_attribute(text, "cvList") {
        if let Ok(n) = text[count.clone()].parse::<usize>() {
            text.replace_range(count, &(n + 1).to_string());
        }
    }
}

/// `<scanSettingsList count>` is the number of `<scanSettings>` it holds.
fn count_scan_settings(text: &mut String) {
    if let Some(count) = count_attribute(text, "scanSettingsList") {
        let n = text.matches("<scanSettings ").count();
        text.replace_range(count, &n.to_string());
    }
}

/// `<sourceFileRef>id</sourceFileRef>` → `<sourceFileRef ref="id"/>`. The id is text mzdata
/// already escaped (quotes included); a quote left as it is would end the attribute.
fn source_file_refs(text: &mut String) {
    const OPEN: &str = "<sourceFileRef>";
    const CLOSE: &str = "</sourceFileRef>";
    let mut from = 0;
    while let Some(open) = text[from..].find(OPEN).map(|i| from + i) {
        let Some(close) = text[open..].find(CLOSE).map(|i| open + i) else { break };
        let id = text[open + OPEN.len()..close].trim().replace('"', "&quot;");
        let element = format!("<sourceFileRef ref=\"{id}\"/>");
        text.replace_range(open..close + CLOSE.len(), &element);
        from = open + element.len();
    }
}

/// Add `shift` to the number that ends at each `</offset>` and `</indexListOffset>` of `index`.
fn shift_offsets(index: &[u8], shift: i64) -> Vec<u8> {
    let mut out = Vec::with_capacity(index.len() + 16);
    let mut done = 0;
    while let Some(lt) = find(&index[done..], b"</").map(|i| done + i) {
        let tail = &index[lt..];
        if !(tail.starts_with(OFFSET_END) || tail.starts_with(LIST_OFFSET_END)) {
            out.extend_from_slice(&index[done..lt + 2]);
            done = lt + 2;
            continue;
        }
        let digits = index[done..lt].iter().rev().take_while(|b| b.is_ascii_digit()).count();
        let number = std::str::from_utf8(&index[lt - digits..lt]).ok().and_then(|n| n.parse::<i64>().ok());
        match number.and_then(|n| n.checked_add(shift)).filter(|n| *n >= 0) {
            Some(n) => {
                out.extend_from_slice(&index[done..lt - digits]);
                out.extend_from_slice(n.to_string().as_bytes());
            }
            None => out.extend_from_slice(&index[done..lt]),
        }
        out.extend_from_slice(b"</");
        done = lt + 2;
    }
    out.extend_from_slice(&index[done..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use mzdata::params::Param;
    use mzdata::prelude::*;

    /// A sink the test can read after the writer that owns it is dropped.
    #[derive(Clone, Default)]
    struct Shared(std::rc::Rc<std::cell::RefCell<Vec<u8>>>);
    impl Write for Shared {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.borrow_mut().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// An mzML of `n` spectra through mzdata's writer and `sink`, each scan stating a pixel position,
    /// with one scan settings entry (a pixel count, a source file reference) unless `plain`.
    fn document(sink: Box<dyn Write>, n: usize, plain: bool) {
        let mut w = mzdata::io::mzml::MzMLWriter::new(sink);
        if !plain {
            let mut settings = mzdata::meta::ScanSettings { id: "grid".into(), ..Default::default() };
            settings.add_param(Param::builder().name("max count of pixels x").curie(mzdata::curie!(IMS:1000042)).value(3).build());
            settings.source_file_refs.push("sf \"1\"".into());
            w.scan_settings.push(settings);
        }
        w.set_spectrum_count(n as u64);
        for i in 0..n {
            let mut spec = mzdata::spectrum::MultiLayerSpectrum::<mzpeaks::CentroidPeak, mzpeaks::DeconvolutedPeak>::default();
            spec.description_mut().id = format!("scan={}", i + 1);
            spec.description_mut().index = i;
            spec.description_mut().ms_level = 1;
            let mut scan = mzdata::spectrum::ScanEvent::default();
            if !plain {
                scan.add_param(Param::builder().name("position x").curie(mzdata::curie!(IMS:1000050)).value(i as i64 + 1).build());
            }
            spec.description_mut().acquisition.scans.push(scan);
            w.write(&spec).unwrap();
        }
        w.close().unwrap();
    }

    fn written(cv: Option<Cv>, n: usize, plain: bool) -> String {
        let out = Shared::default();
        document(Box::new(HeaderFixes::new(out.clone(), cv)), n, plain);
        String::from_utf8(out.0.take()).unwrap()
    }

    /// Every `<offset idRef>` of the index and its byte offset.
    fn offsets(doc: &str) -> Vec<(String, usize)> {
        doc.match_indices("<offset idRef=\"")
            .map(|(at, open)| {
                let rest = &doc[at + open.len()..];
                let (id, rest) = rest.split_once("\">").unwrap();
                (id.to_string(), rest[..rest.find('<').unwrap()].parse().unwrap())
            })
            .collect()
    }

    /// The header mzdata writes for an imaging export — `IMS` params against a list of MS and UO, a
    /// scan settings list counted by the samples, a source file reference as text — comes out
    /// declaring `IMS`, counted, with the reference as an attribute; and the index still points at
    /// its elements, which a header that grew would otherwise have moved from under it.
    #[test]
    fn the_header_is_fixed_and_the_index_follows() {
        let raw = {
            let out = Shared::default();
            document(Box::new(out.clone()), 12, false);
            String::from_utf8(out.0.take()).unwrap()
        };
        assert!(raw.contains("<cvList count=\"2\">") && !raw.contains("<cv id=\"IMS\""), "what mzdata writes");
        assert!(raw.contains("<scanSettingsList count=\"0\">") && raw.contains("<sourceFileRef>sf &quot;1&quot;</sourceFileRef>"), "{}", &raw[..raw.find("<run ").unwrap()]);

        let doc = written(Some(Cv::ims()), 12, false);
        let header = &doc[..doc.find("<run ").unwrap()];
        assert!(header.contains("<cvList count=\"3\">"), "{header}");
        let ims = "      <cv id=\"IMS\" fullName=\"Imaging Mass Spectrometry Ontology\" URI=\"https://raw.githubusercontent.com/imzML/imzML/2c28b05ca297430303627d8c7d192cac1a2b1374/imagingMS.obo\" version=\"1.1.0\"/>\n    </cvList>";
        assert!(header.contains(ims), "the pinned vocabulary, as the last entry, indented like the others: {header}");
        assert_eq!(header.matches("<cv id=").count(), 3);
        assert!(header.contains("<scanSettingsList count=\"1\">"), "{header}");
        assert!(header.contains("<sourceFileRef ref=\"sf &quot;1&quot;\"/>") && !header.contains("</sourceFileRef>"), "{header}");

        // Nothing but the header and the offsets changed…
        let grew = doc.len() - raw.len();
        assert!(grew > 150, "{grew}");
        let (body, raw_body) = (&doc[doc.find("<run ").unwrap()..doc.find("<indexList").unwrap()], &raw[raw.find("<run ").unwrap()..raw.find("<indexList").unwrap()]);
        assert_eq!(body, raw_body);
        // …and every offset is where its element starts: mzdata records the position before the
        // element's line break and indentation.
        let index = offsets(&doc);
        assert_eq!(index.len(), 12 + 2, "12 spectra, TIC and BPC");
        for (id, at) in &index {
            assert!(doc[*at..].starts_with('\n'), "{id} at {at}: {:?}", &doc[*at..*at + 40]);
            let element = doc[*at..].trim_start();
            assert!(element.starts_with("<spectrum ") || element.starts_with("<chromatogram "), "{id} at {at}: {:?}", &element[..40]);
            assert!(element[..element.find('>').unwrap()].contains(&format!("id=\"{id}\"")), "{id} at {at}");
        }
        let list_offset: usize = doc.split_once("<indexListOffset>").unwrap().1.split_once('<').unwrap().0.parse().unwrap();
        assert!(doc[list_offset..].trim_start().starts_with("<indexList "), "{:?}", &doc[list_offset..list_offset + 20]);
        // The unfixed document's own offsets were right too: the shift is the header's growth.
        for ((id, at), (raw_id, raw_at)) in index.iter().zip(offsets(&raw)) {
            assert_eq!((id, *at), (&raw_id, raw_at + grew));
        }
        assert!(doc.ends_with("</indexedmzML>"), "{:?}", &doc[doc.len() - 40..]);
    }

    /// Nothing to fix, nothing changed: no vocabulary to add and no scan settings is every export
    /// of non-imaging data, which must stay as it was byte for byte. And a vocabulary the list
    /// already declares is not declared twice.
    #[test]
    fn a_header_with_nothing_to_fix_passes_unchanged() {
        let raw = {
            let out = Shared::default();
            document(Box::new(out.clone()), 3, true);
            String::from_utf8(out.0.take()).unwrap()
        };
        assert_eq!(written(None, 3, true), raw);
        let ms = Cv { id: "MS".into(), full_name: "x".into(), uri: "y".into(), version: None };
        assert_eq!(written(Some(ms), 3, true), raw, "already declared");
        // A vocabulary without a version, and characters an attribute cannot hold as they are.
        let odd = Cv { id: "X".into(), full_name: "a \"b\" & <c>".into(), uri: "u".into(), version: None };
        let doc = written(Some(odd), 3, true);
        assert!(doc.contains("<cv id=\"X\" fullName=\"a &quot;b&quot; &amp; &lt;c&gt;\" URI=\"u\"/>\n    </cvList>"), "{}", &doc[..600]);
        assert!(doc.contains("<cvList count=\"3\">"));
    }

    /// The writer hands the sink its bytes in blocks of its own choosing: whatever the split —
    /// through `<run `, `<indexList` or an offset's digits — the result is the same.
    #[test]
    fn the_result_does_not_depend_on_how_the_bytes_arrive() {
        let raw = {
            let out = Shared::default();
            document(Box::new(out.clone()), 12, false);
            out.0.take()
        };
        let whole = {
            let out = Shared::default();
            let mut sink = HeaderFixes::new(out.clone(), Some(Cv::ims()));
            sink.write_all(&raw).unwrap();
            sink.flush().unwrap();
            out.0.take()
        };
        assert_eq!(String::from_utf8(whole.clone()).unwrap(), written(Some(Cv::ims()), 12, false));
        for block in [1, 2, 7, 64, 4096] {
            let out = Shared::default();
            let mut sink = HeaderFixes::new(out.clone(), Some(Cv::ims()));
            for chunk in raw.chunks(block) {
                sink.write_all(chunk).unwrap();
                sink.flush().unwrap(); // a flush anywhere holds back what is incomplete
            }
            assert!(*out.0.borrow() == whole, "blocks of {block}: complete once the index is through");
            drop(sink);
            assert!(out.0.take() == whole, "blocks of {block}");
        }
        // A document cut off in its header, or in its index: what was held goes out on drop, as it is.
        let in_index = rfind(&raw, b"<offset idRef").unwrap() + 20;
        for cut in [raw.len() / 20, in_index] {
            let out = Shared::default();
            let mut sink = HeaderFixes::new(out.clone(), Some(Cv::ims()));
            sink.write_all(&raw[..cut]).unwrap();
            sink.flush().unwrap();
            let before = out.0.borrow().len();
            drop(sink);
            let written = out.0.take();
            assert!(written.len() > before && written.ends_with(&raw[cut - 10..cut]), "cut at {cut}");
        }
    }

    /// A document whose header holds what mzdata writes wrongly about the run and an instrument
    /// configuration: two source files, a configuration without components or software (and one
    /// with), written through `sink`.
    fn run_document(sink: Box<dyn Write>, n: usize) {
        use mzdata::meta::{Component, ComponentType, InstrumentConfiguration, Software, SourceFile};
        let mut w = mzdata::io::mzml::MzMLWriter::new(sink);
        for id in ["first", "second"] {
            w.file_description_mut().source_files.push(SourceFile { id: id.into(), name: format!("{id}.raw"), ..Default::default() });
        }
        w.softwares_mut().push(Software::new("acq".into(), "1".into(), Vec::new()));
        w.instrument_configurations_mut().insert(0, InstrumentConfiguration { id: 0, ..Default::default() });
        let component = |kind, order| Component { component_type: kind, order, ..Default::default() };
        let components = vec![component(ComponentType::IonSource, 1), component(ComponentType::Analyzer, 2), component(ComponentType::Detector, 3)];
        w.instrument_configurations_mut().insert(1, InstrumentConfiguration { id: 1, components, software_reference: "acq".into(), ..Default::default() });
        w.set_spectrum_count(n as u64);
        for i in 0..n {
            let mut spec = mzdata::spectrum::MultiLayerSpectrum::<mzpeaks::CentroidPeak, mzpeaks::DeconvolutedPeak>::default();
            spec.description_mut().id = format!("scan={}", i + 1);
            spec.description_mut().index = i;
            spec.description_mut().acquisition.scans.push(Default::default());
            w.write(&spec).unwrap();
        }
        w.close().unwrap();
    }

    /// mzdata states the run as `<run id="1">` with the lowest configuration and the first source
    /// file, undated, and an empty configuration with an empty `<componentList>` and `<softwareRef>`.
    /// The sink states the run as handed over — its id, its default configuration and source file,
    /// its start time — and leaves the two empty elements out, lines and all; the configuration
    /// that has components and software keeps both; and the index still points at its elements.
    #[test]
    fn the_run_is_stated_as_handed_over_and_empty_elements_are_left_out() {
        let raw = {
            let out = Shared::default();
            run_document(Box::new(out.clone()), 5);
            String::from_utf8(out.0.take()).unwrap()
        };
        assert!(raw.contains("<run id=\"1\" defaultInstrumentConfigurationRef=\"IC1\" defaultSourceFileRef=\"first\">"), "what mzdata writes");
        assert!(raw.contains("<componentList count=\"0\">") && raw.contains("<softwareRef ref=\"\"/>"), "what mzdata writes");

        let run = RunCell::default();
        let out = Shared::default();
        let sink = HeaderFixes::with_run(out.clone(), None, run.clone());
        // Filled after the sink went into the writer, as the lanes do.
        *run.borrow_mut() = Some(Run {
            id: "MRM_x0020_Neg \"&\" C5".into(),
            default_instrument_configuration: Some("IC2".into()),
            start_time_stamp: Some("2006-09-10T02:11:56Z".into()),
            default_source_file: Some("second".into()),
        });
        run_document(Box::new(sink), 5);
        let doc = String::from_utf8(out.0.take()).unwrap();
        let tag = "<run id=\"MRM_x0020_Neg &quot;&amp;&quot; C5\" defaultInstrumentConfigurationRef=\"IC2\" startTimeStamp=\"2006-09-10T02:11:56Z\" defaultSourceFileRef=\"second\">";
        assert!(doc.contains(tag), "{}", &doc[doc.find("<run ").unwrap()..][..200]);
        assert_eq!(doc.matches("<run ").count(), 1);
        assert!(!doc.contains("<componentList count=\"0\">") && !doc.contains("<softwareRef ref=\"\"/>"), "{doc}");
        let empty = "      <instrumentConfiguration id=\"IC1\">\n      </instrumentConfiguration>\n";
        assert!(doc.contains(empty), "the empty configuration, without blank lines: {}", &doc[doc.find("<instrumentConfigurationList").unwrap()..][..400]);
        assert!(doc.contains("<componentList count=\"3\">") && doc.contains("<softwareRef ref=\"acq\"/>"), "the stated ones stay");
        assert_eq!(doc.matches("</componentList>").count(), 1);

        let index = offsets(&doc);
        assert_eq!(index.len(), 5 + 2, "5 spectra, TIC and BPC");
        for (id, at) in &index {
            let element = doc[*at..].trim_start();
            assert!(element.starts_with("<spectrum ") || element.starts_with("<chromatogram "), "{id} at {at}: {:?}", &element[..40]);
            assert!(element[..element.find('>').unwrap()].contains(&format!("id=\"{id}\"")), "{id} at {at}");
        }
        let list_offset: usize = doc.split_once("<indexListOffset>").unwrap().1.split_once('<').unwrap().0.parse().unwrap();
        assert!(doc[list_offset..].trim_start().starts_with("<indexList "));

        // A run that states only its id keeps mzdata's defaults and gains no start time; and the
        // result does not depend on where the writer's blocks end — inside the run tag included.
        let partial = Run { id: "r".into(), ..Default::default() };
        let whole = {
            let out = Shared::default();
            let mut sink = HeaderFixes::with_run(out.clone(), None, Rc::new(RefCell::new(Some(partial.clone()))));
            sink.write_all(raw.as_bytes()).unwrap();
            sink.flush().unwrap();
            out.0.take()
        };
        let text = String::from_utf8(whole.clone()).unwrap();
        assert!(text.contains("<run id=\"r\" defaultInstrumentConfigurationRef=\"IC1\" defaultSourceFileRef=\"first\">"), "{}", &text[text.find("<run ").unwrap()..][..120]);
        for block in [1, 3, 64] {
            let out = Shared::default();
            let mut sink = HeaderFixes::with_run(out.clone(), None, Rc::new(RefCell::new(Some(partial.clone()))));
            for chunk in raw.as_bytes().chunks(block) {
                sink.write_all(chunk).unwrap();
                sink.flush().unwrap();
            }
            drop(sink);
            assert!(out.0.take() == whole, "blocks of {block}");
        }
    }

    #[test]
    fn a_start_tag_ends_at_the_first_angle_outside_a_value() {
        assert_eq!(tag_end(b"<run id=\"a>b\" x='>'>rest"), Some(20));
        assert_eq!(tag_end(b"<run id=\"a>b\""), None, "incomplete");
        assert_eq!(attribute("<run id=\"1\" defaultSourceFileRef=\"a b\">", "defaultSourceFileRef"), Some("a b"));
        assert_eq!(attribute("<run id=\"1\">", "startTimeStamp"), None);
    }

    #[test]
    fn the_archives_own_declaration_is_used() {
        let cv_list = serde_json::json!([
            {"id": "MS", "full_name": "PSI-MS", "uri": "https://example.org/ms.obo", "version": "4.1.258"},
            {"id": "IMS", "full_name": "Imaging Mass Spectrometry Ontology", "uri": "https://example.org/pinned/imagingMS.obo"},
        ]);
        let ims = Cv::from_cv_list(Some(&cv_list), "IMS").unwrap();
        assert_eq!(ims.element(), "<cv id=\"IMS\" fullName=\"Imaging Mass Spectrometry Ontology\" URI=\"https://example.org/pinned/imagingMS.obo\"/>");
        assert_eq!(Cv::from_cv_list(Some(&cv_list), "NCIT"), None);
        assert_eq!(Cv::from_cv_list(None, "IMS"), None);
        assert!(Cv::ims().uri.contains("/2c28b05ca297430303627d8c7d192cac1a2b1374/"), "pinned to a commit: {}", Cv::ims().uri);
    }

    #[test]
    fn offsets_are_shifted_where_they_are_offsets() {
        let index = b"<index name=\"spectrum\">\n<offset idRef=\"scan=1 a</b\">100</offset>\n<offset idRef=\"x\">  7</offset></index>\n<indexListOffset>4000</indexListOffset>";
        assert_eq!(
            String::from_utf8(shift_offsets(index, 23)).unwrap(),
            "<index name=\"spectrum\">\n<offset idRef=\"scan=1 a</b\">123</offset>\n<offset idRef=\"x\">  30</offset></index>\n<indexListOffset>4023</indexListOffset>"
        );
        assert_eq!(shift_offsets(b"<offset idRef=\"x\">9</offset>", -8), b"<offset idRef=\"x\">1</offset>");
        assert_eq!(shift_offsets(b"<offset idRef=\"x\"></offset>", 5), b"<offset idRef=\"x\"></offset>", "no number, no change");
    }
}

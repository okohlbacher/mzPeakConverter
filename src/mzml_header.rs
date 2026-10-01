//! mzML export: the parts of the header mzdata's writer cannot write.
//!
//! mzdata 0.67's mzML writer
//!
//! * writes a fixed `<cvList count="2">` of MS and UO, whatever vocabularies its params name: the
//!   export of an imaging run wrote every pixel position as `cvRef="IMS"` against a list that does
//!   not declare `IMS` (18 such params in the export of a 6-pixel imzML, HUPO-PSI/mzPeak-specification#23);
//! * counts the `<scanSettingsList>` by the number of SAMPLES (`count="0"` over one entry);
//! * writes a scan settings' source file reference as text, `<sourceFileRef>id</sourceFileRef>`,
//!   where the schema has an attribute, `<sourceFileRef ref="id"/>`.
//!
//! [`HeaderFixes`] sits under that writer, like [`crate::mzml_isolation`] and
//! [`crate::mzml_wavelength`], holds the header (everything before `<run `) and rewrites those three.
//! Unlike the two other sinks it cannot keep the length: a `<cv/>` element has to be ADDED. So it
//! also shifts every byte offset the writer records at the end of the document — each
//! `<offset idRef="…">` of the `<indexList>` and the `<indexListOffset>` — by the number of bytes
//! the header grew. Everything between passes through untouched. A header with nothing to fix (no
//! vocabulary to add, no scan settings: every export of non-imaging data without scan settings)
//! leaves the stream byte-identical.
//!
//! The `<fileChecksum>` is left as mzdata wrote it: mzdata takes that digest before flushing its own
//! buffer, so it did not match the file's bytes before this sink existed either.

use std::io::{self, Write};

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
/// the `<scanSettingsList>` and writes `<sourceFileRef>` as the schema has it (see the module docs).
pub struct HeaderFixes<W: Write> {
    inner: W,
    cv: Option<Cv>,
    state: State,
    /// Bytes not passed on yet: the header, a tail that may still become `<indexList`, or an index
    /// element that is still incomplete.
    held: Vec<u8>,
    /// How many bytes the header grew by.
    shift: i64,
}

impl<W: Write> HeaderFixes<W> {
    pub fn new(inner: W, cv: Option<Cv>) -> Self {
        Self { inner, cv, state: State::Header, held: Vec::new(), shift: 0 }
    }

    /// Pass on what is complete in `held`; with `end`, everything.
    fn advance(&mut self, end: bool) -> io::Result<()> {
        loop {
            match self.state {
                State::Header => {
                    let Some(run) = find(&self.held, RUN) else {
                        break;
                    };
                    let header = fix_header(&self.held[..run], self.cv.as_ref());
                    self.shift = header.len() as i64 - run as i64;
                    self.inner.write_all(&header)?;
                    self.held.drain(..run);
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

    /// mzdata flushes once, after the document is closed, so nothing held here is mid-element.
    fn flush(&mut self) -> io::Result<()> {
        self.advance(true)?;
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
    text.into_bytes()
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
            }
            drop(sink); // no flush: what is still held goes out when the sink is dropped
            assert!(out.0.take() == whole, "blocks of {block}");
        }
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

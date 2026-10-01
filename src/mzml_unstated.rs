//! mzML export: what mzdata's writer states although neither the source nor the archive does.
//!
//! mzdata 0.67's spectrum model holds a scan's start time and ion injection time, a selected ion's
//! intensity and an activation's collision energy as plain numbers, 0 where nothing states one, and
//! a polarity that may be `Unknown`. Its mzML writer writes all five whatever they hold:
//! `scan start time` 0 and `ion injection time` 0 on every scan, `peak intensity` 0 and
//! `collision energy` 0 on every selected ion and activation, and `positive scan` for an unknown
//! polarity, with a warning per spectrum. Through 0.17.0-rc.1 every export therefore stated values
//! nobody had measured, on both routes: `positive scan` on all 1,196 spectra of a negative-mode
//! imaging run whose imzML and archive state no polarity (`180817_NEG_Thaliana_Leaf_bottom_1_0841`)
//! and a `scan start time` of 0 on each of them, though the imzML states no time at all;
//! `ion injection time` 0 on the 201 spectra and `peak intensity` 0 on the 186 selected ions of
//! ProteoWizard's `swath.api-sample-centroid.mzML`, which states neither; and the three zeros on the
//! 9,600 SRM spectra of `LD401_001fmol_r1.raw`, whose archive holds nulls. A reader cannot tell such
//! a 0 from a measured one.
//!
//! The writer also prints `<precursorList count="0">` on every spectrum without a precursor and
//! `<selectedIonList count="0">` in every precursor without a selected ion, where the mzML 1.1.0
//! schema wants at least one member in a list; wraps a chromatogram's precursor and product in
//! `<precursorList>` / `<productList>`, which the schema has for spectra only (a chromatogram holds
//! `<precursor>` and `<product>` themselves); and closes every run with a `<chromatogramList>`,
//! `count="0"` and empty when there is no chromatogram to write, where the schema lets a run go
//! without the list but not the list without a member.
//!
//! Two halves. The lane MARKS what is not stated in the model before it hands a spectrum or a
//! chromatogram to the writer ([`mark_spectrum`], [`mark_start_times`], [`mark_precursors`]): a
//! number becomes NaN, an unknown polarity becomes `Positive` (so the writer does not warn) beside
//! a marker `userParam`. [`UnstatedTerms`] sits under the writer, like [`crate::mzml_isolation`]
//! and [`crate::mzml_wavelength`], and blanks what the marks stand for — and the list defects —
//! with spaces of the same length, so every byte offset the writer records in `<indexList>` stays
//! true. It counts the spectra without a polarity and says so once, when the document is done.
//!
//! What "not stated" means is the lane's to say ([`Zeros`]). An archive stores a 0 injection time,
//! selected-ion intensity or collision energy as null (`writer/visitor.rs`), and no vendor reader
//! has a way to say "measured, and 0": there a 0 of those three is not stated. A start time is the
//! other way round: a vendor reader gives every scan its time and an archive stores one for every
//! spectrum, so a 0 is the scan's time there ([`Zeros::TIME`]) — except in an archive whose imaging
//! marker says its source stated no time. An mzML can state any of the four in its own text, or
//! leave it out (2 of the corpus's 153 mzML/imzML sources state `collision energy` 0; the imzML
//! of the Thaliana run, `Test_P15_r2` and `Example_Continuous` state no `scan start time` at all),
//! so the direct mzML lane reads which spectra and chromatograms state which 0 ([`StatedZeros`])
//! and keeps those.

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::ops::Range;
use std::path::Path;

use mzdata::params::Param;
use mzdata::prelude::*;
use mzdata::spectrum::{Precursor, ScanPolarity, SpectrumDescription};

use crate::mzml_isolation::find;

/// The marker [`mark_spectrum`] leaves beside the `positive scan` of a spectrum whose polarity
/// nothing states; [`UnstatedTerms`] blanks both.
pub const POLARITY_NOT_STATED: &str = "mzpeak-convert: polarity not stated";

/// The terms mzdata's model holds as a plain number, 0 when absent: (accession, the name mzdata's
/// reader takes the term by, its bit in [`Zeros`]). The writer prints the first [`WRITTEN`];
/// `activation energy` is read into the collision energy too.
const TERMS: [(&str, &str, u8); 5] = [
    ("MS:1000016", "scan start time", Zeros::START_TIME),
    ("MS:1000927", "ion injection time", Zeros::INJECTION_TIME),
    ("MS:1000042", "peak intensity", Zeros::PEAK_INTENSITY),
    ("MS:1000045", "collision energy", Zeros::COLLISION_ENERGY),
    ("MS:1000509", "activation energy", Zeros::COLLISION_ENERGY),
];
const WRITTEN: usize = 4;

/// Which of the four terms a 0 is a stated value for, in one spectrum or chromatogram.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Zeros(u8);

impl Zeros {
    const INJECTION_TIME: u8 = 1;
    const PEAK_INTENSITY: u8 = 2;
    const COLLISION_ENERGY: u8 = 4;
    const START_TIME: u8 = 8;
    /// No 0 is stated, a start time of 0 neither: a spectrum of an mzML that writes none of them
    /// itself, and one of an archive whose source stated no time.
    pub const NONE: Self = Self(0);
    /// A start time of 0 is the scan's time; no other 0 is stated. A vendor reader, which gives
    /// every scan its time, and an archive, which stores one for every spectrum.
    pub const TIME: Self = Self(Self::START_TIME);
    /// Every 0 counts as stated: nothing is blanked.
    pub const ALL: Self = Self(15);

    fn states(self, bit: u8) -> bool {
        self.0 & bit != 0
    }
}

/// Mark what `descr` holds without anything having stated it, for [`UnstatedTerms`] to blank: an
/// unknown polarity, and each 0 among the scans' start and injection times and the precursors'
/// selected-ion intensities and collision energies that `stated` does not vouch for. A spectrum's
/// start time is not a number afterwards: take it first where it is needed.
pub fn mark_spectrum(descr: &mut SpectrumDescription, stated: Zeros) {
    if descr.polarity == ScanPolarity::Unknown {
        descr.polarity = ScanPolarity::Positive;
        descr.add_param(Param::new_key_value(POLARITY_NOT_STATED, ""));
    }
    mark_scans(descr, stated, true);
    mark_precursors(descr.precursor.iter_mut(), stated);
}

/// [`mark_spectrum`] for the scans' start times alone: a wavelength spectrum's, whose polarity and
/// injection time [`crate::mzml_wavelength`] blanks whatever they hold.
pub fn mark_start_times(descr: &mut SpectrumDescription, stated: Zeros) {
    mark_scans(descr, stated, false);
}

fn mark_scans(descr: &mut SpectrumDescription, stated: Zeros, injection_times: bool) {
    for scan in descr.acquisition.scans.iter_mut() {
        // A scan that only refers to another spectrum, with both times 0: the writer states neither
        // time (`write_scan_list`), and a NaN here would make it state both.
        if scan.spectrum_reference.is_some() && scan.start_time == 0.0 && scan.injection_time == 0.0 {
            continue;
        }
        if scan.start_time == 0.0 && !stated.states(Zeros::START_TIME) {
            scan.start_time = f64::NAN;
        }
        if injection_times && scan.injection_time == 0.0 && !stated.states(Zeros::INJECTION_TIME) {
            scan.injection_time = f32::NAN;
        }
    }
}

/// [`mark_spectrum`] for the precursors alone: a chromatogram's (its polarity is written only when
/// known, by `write_source_chromatograms_mzml`).
pub fn mark_precursors<'a>(precursors: impl Iterator<Item = &'a mut Precursor>, stated: Zeros) {
    for precursor in precursors {
        if precursor.activation.energy == 0.0 && !stated.states(Zeros::COLLISION_ENERGY) {
            precursor.activation.energy = f32::NAN;
        }
        for ion in precursor.ions.iter_mut() {
            if ion.intensity == 0.0 && !stated.states(Zeros::PEAK_INTENSITY) {
                ion.intensity = f32::NAN;
            }
        }
    }
}

/// The zeros an mzML or imzML states in its own text, by the id of the spectrum or chromatogram
/// that states them — in a `cvParam` of its own or of a `referenceableParamGroup` it refers to
/// (an imzML states most of a spectrum through two or three of those). One streamed pass over the
/// file's tags; the binary payloads hold no `<`.
///
/// Per element, not per scan or precursor: a spectrum that states one `collision energy` of 0
/// keeps every 0 of that term.
#[derive(Debug, Default)]
pub struct StatedZeros {
    spectra: HashMap<String, Zeros>,
    chromatograms: HashMap<String, Zeros>,
    /// What each `referenceableParamGroup` states; the list precedes the run.
    groups: HashMap<String, Zeros>,
    /// The file could not be read: every 0 counts as stated.
    all: bool,
}

/// The start tags the pass reads, by element name: a name counts when XML whitespace follows it
/// (`<spectrum` then a line break is a spectrum, `<spectrumList` is not).
const TAGS: [&[u8]; 5] =
    [b"<spectrum", b"<chromatogram", b"<referenceableParamGroup", b"<cvParam", b"<referenceableParamGroupRef"];
const CHROMATOGRAM: usize = 1;
const GROUP: usize = 2;
const CV_PARAM: usize = 3;
const GROUP_REF: usize = 4;
/// The end tags after which a `cvParam` is no spectrum's, chromatogram's or group's.
const ENDS: [&[u8]; 3] = [b"</spectrum", b"</chromatogram", b"</referenceableParamGroup"];

/// What the bytes at a `<` begin.
enum Tag {
    Start(usize),
    End,
    /// Too few bytes to tell: wait for the next block.
    Cut,
    Other,
}

fn classify(rest: &[u8]) -> Tag {
    let named = |names: &[&[u8]], after: &[u8]| -> Option<Option<usize>> {
        let mut cut = false;
        for (k, name) in names.iter().enumerate() {
            match rest.get(name.len()) {
                Some(b) if rest.starts_with(name) && after.contains(b) => return Some(Some(k)),
                None if name.starts_with(rest) => cut = true,
                _ => {}
            }
        }
        cut.then_some(None)
    };
    match (named(&TAGS, b" \t\r\n"), named(&ENDS, b"> \t\r\n")) {
        (Some(Some(kind)), _) => Tag::Start(kind),
        (_, Some(Some(_))) => Tag::End,
        (Some(None), _) | (_, Some(None)) => Tag::Cut,
        _ => Tag::Other,
    }
}

impl StatedZeros {
    /// For a source whose text could not be searched: nothing is blanked.
    pub fn everything() -> Self {
        Self { all: true, ..Default::default() }
    }

    pub fn read(path: &Path) -> io::Result<Self> {
        let mut file = std::fs::File::open(path)?;
        let mut stated = Self::default();
        let mut current: Option<(usize, String)> = None;
        let mut held: Vec<u8> = Vec::new();
        let mut block = vec![0u8; 1 << 20];
        loop {
            let n = file.read(&mut block)?;
            if n == 0 {
                return Ok(stated);
            }
            held.extend_from_slice(&block[..n]);
            let keep = stated.scan(&held, &mut current);
            held.drain(..keep);
        }
    }

    /// Read the tags of `held`; how many of its bytes are done with — up to the first tag the block
    /// cuts short, which waits for the next block.
    fn scan(&mut self, held: &[u8], current: &mut Option<(usize, String)>) -> usize {
        let mut at = 0;
        loop {
            let Some(lt) = held[at..].iter().position(|&b| b == b'<').map(|i| at + i) else {
                return held.len();
            };
            let rest = &held[lt..];
            match classify(rest) {
                Tag::Cut => return lt,
                Tag::Other => at = lt + 1,
                Tag::End => {
                    *current = None;
                    at = lt + 1;
                }
                Tag::Start(kind) => {
                    let Some(end) = tag_end(rest) else {
                        return lt;
                    };
                    if let Ok(tag) = std::str::from_utf8(&rest[..end]) {
                        self.tag(kind, tag, current);
                    }
                    at = lt + end;
                }
            }
        }
    }

    fn tag(&mut self, kind: usize, tag: &str, current: &mut Option<(usize, String)>) {
        let unescaped = |v: &str| quick_xml::escape::unescape(v).map_or_else(|_| v.to_string(), |v| v.into_owned());
        if kind < CV_PARAM {
            *current = attribute(tag, "id").map(|id| (kind, unescaped(id)));
            return;
        }
        let Some((element, id)) = current.as_ref() else { return };
        let bits = if kind == GROUP_REF {
            attribute(tag, "ref").and_then(|group| self.groups.get(&unescaped(group))).map_or(0, |z| z.0)
        } else {
            let (accession, name) = (attribute(tag, "accession"), attribute(tag, "name"));
            let term = TERMS.iter().find(|(a, n, _)| accession == Some(a) || name == Some(n));
            let zero = attribute(tag, "value").and_then(|v| v.trim().parse::<f64>().ok()) == Some(0.0);
            term.filter(|_| zero).map_or(0, |&(_, _, bit)| bit)
        };
        if bits != 0 {
            let map = match *element {
                CHROMATOGRAM => &mut self.chromatograms,
                GROUP => &mut self.groups,
                _ => &mut self.spectra,
            };
            map.entry(id.clone()).or_default().0 |= bits;
        }
    }

    pub fn spectrum(&self, id: &str) -> Zeros {
        if self.all { Zeros::ALL } else { self.spectra.get(id).copied().unwrap_or_default() }
    }

    pub fn chromatogram(&self, id: &str) -> Zeros {
        if self.all { Zeros::ALL } else { self.chromatograms.get(id).copied().unwrap_or_default() }
    }

    /// How many spectra and chromatograms state a 0 of their own.
    pub fn count(&self) -> usize {
        self.spectra.len() + self.chromatograms.len()
    }
}

/// Where the start tag that begins `tag` ends (past its `>`), a `>` inside a quoted attribute value
/// not counting; `None` when the bytes end first.
fn tag_end(tag: &[u8]) -> Option<usize> {
    let mut quote = None;
    for (i, &b) in tag.iter().enumerate() {
        match (quote, b) {
            (None, b'"' | b'\'') => quote = Some(b),
            (Some(q), _) if b == q => quote = None,
            (None, b'>') => return Some(i + 1),
            _ => {}
        }
    }
    None
}

/// The value of attribute `key` of a start tag, as written (not unescaped), in either quote.
fn attribute<'a>(tag: &'a str, key: &str) -> Option<&'a str> {
    let mut from = 0;
    while let Some(at) = tag[from..].find(key).map(|i| from + i) {
        let rest = &tag[at + key.len()..];
        from = at + key.len();
        if !tag[..at].ends_with([' ', '\t', '\n', '\r']) || !rest.starts_with("=\"") && !rest.starts_with("='") {
            continue;
        }
        let quote = rest.as_bytes()[1] as char;
        return rest[2..].find(quote).map(|end| &rest[2..2 + end]);
    }
    None
}

/// What the sink looks for: a spectrum, a chromatogram, and the list the writer prints for a run
/// without a chromatogram.
const OPENS: [&[u8]; 3] = [b"<spectrum ", b"<chromatogram ", b"<chromatogramList count=\"0\""];
const CHROMATOGRAM_OPEN: usize = 1;
const EMPTY_LIST_OPEN: usize = 2;
const EMPTY_LIST_END: &[u8] = b"</chromatogramList>";
/// Everything the writer states about a spectrum or a chromatogram comes before its arrays.
const HEAD_END: &[u8] = b"<binaryDataArrayList";

/// A byte sink for `MzMLWriter` that blanks what [`mark_spectrum`] and [`mark_precursors`] marked,
/// an empty `<precursorList>` or `<selectedIonList>`, the lists around a chromatogram's precursor
/// and product, and a `<chromatogramList>` without a chromatogram.
pub struct UnstatedTerms<W: Write> {
    inner: W,
    /// Bytes not passed on yet: a `<spectrum>` or `<chromatogram>` element whose head is still
    /// incomplete, or a tail that may still become its start tag.
    held: Vec<u8>,
    spectra: usize,
    no_polarity: usize,
}

impl<W: Write> UnstatedTerms<W> {
    pub fn new(inner: W) -> Self {
        Self { inner, held: Vec::new(), spectra: 0, no_polarity: 0 }
    }
}

impl<W: Write> Write for UnstatedTerms<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.held.extend_from_slice(buf);
        let mut done = 0;
        loop {
            let Some((open, kind)) = find_open(&self.held[done..]).map(|(i, k)| (done + i, k)) else {
                done = self.held.len() - partial_start_tag(&self.held[done..]);
                break;
            };
            if kind == EMPTY_LIST_OPEN {
                // The writer prints the list's two tags in one go when the document is closed.
                let Some(end) = find(&self.held[open..], EMPTY_LIST_END).map(|i| open + i + EMPTY_LIST_END.len()) else {
                    done = open;
                    break;
                };
                self.held[open..end].fill(b' ');
                done = end;
                continue;
            }
            let chromatogram = kind == CHROMATOGRAM_OPEN;
            let Some(head_end) = find(&self.held[open..], HEAD_END).map(|i| open + i) else {
                done = open;
                break;
            };
            if let Ok(head) = std::str::from_utf8(&self.held[open..head_end]) {
                let (spans, no_polarity) = unstated(head, chromatogram);
                for span in spans {
                    self.held[open + span.start..open + span.end].fill(b' ');
                }
                self.spectra += usize::from(!chromatogram);
                self.no_polarity += usize::from(no_polarity);
            }
            done = head_end;
        }
        self.inner.write_all(&self.held[..done])?;
        self.held.drain(..done);
        Ok(buf.len())
    }

    /// mzdata flushes once, after the document is closed, so nothing held here is mid-element.
    fn flush(&mut self) -> io::Result<()> {
        self.inner.write_all(&self.held)?;
        self.held.clear();
        self.inner.flush()
    }
}

impl<W: Write> Drop for UnstatedTerms<W> {
    fn drop(&mut self) {
        if !self.held.is_empty() {
            let _ = self.inner.write_all(&self.held);
        }
        // One line for the run, where mzdata's writer warned once per spectrum and wrote
        // `positive scan` all the same.
        if self.no_polarity > 0 {
            log::warn!(
                "{} of {} spectra state no polarity: the mzML states none for them",
                self.no_polarity,
                self.spectra
            );
        }
    }
}

/// The first of [`OPENS`] in `hay`, and which.
fn find_open(hay: &[u8]) -> Option<(usize, usize)> {
    let mut from = 0;
    while let Some(i) = hay[from..].iter().position(|&b| b == b'<') {
        let at = from + i;
        if let Some(k) = OPENS.iter().position(|open| hay[at..].starts_with(open)) {
            return Some((at, k));
        }
        from = at + 1;
    }
    None
}

/// How many trailing bytes of `tail` could still grow into one of the start tags on the next write.
fn partial_start_tag(tail: &[u8]) -> usize {
    OPENS.iter().filter_map(|open| (1..open.len()).rev().find(|&k| tail.ends_with(&open[..k]))).max().unwrap_or(0)
}

/// The spans of each `<{open} …/>` element of `head`.
fn elements<'a>(head: &'a str, open: &'a str) -> impl Iterator<Item = Range<usize>> + 'a {
    head.match_indices(open).filter_map(move |(start, _)| Some(start..start + head[start..].find("/>")? + 2))
}

/// The spans, in the text of one `<spectrum>` or `<chromatogram>` element up to its arrays, of what
/// neither the source nor the archive states, and whether the element is a spectrum without a
/// polarity:
///
/// * each `scan start time`, `ion injection time`, `peak intensity` and `collision energy` whose
///   value is NaN;
/// * the polarity marker and, with it, the spectrum's `positive scan`;
/// * a `<precursorList count="0">` and a `<selectedIonList count="0">` up to their end tags (the
///   schema wants a member in each; a precursor may go without its selected ions);
/// * in a chromatogram, the start and end tags of the lists around its precursor and product,
///   whatever they count.
fn unstated(head: &str, chromatogram: bool) -> (Vec<Range<usize>>, bool) {
    let mut spans = Vec::new();
    let scan_list = head.find("<scanList").unwrap_or(head.len());
    let marker = elements(head, "<userParam ")
        .find(|span| span.start < scan_list && attribute(&head[span.clone()], "name") == Some(POLARITY_NOT_STATED));
    for span in elements(head, "<cvParam ") {
        let param = &head[span.clone()];
        let Some(accession) = attribute(param, "accession") else { continue };
        let nan = attribute(param, "value") == Some("NaN") && TERMS[..WRITTEN].iter().any(|(a, _, _)| *a == accession);
        let polarity = marker.is_some() && accession == "MS:1000130" && span.start < scan_list;
        if nan || polarity {
            spans.push(span);
        }
    }
    for (empty, end) in [("<precursorList count=\"0\">", "</precursorList>"), ("<selectedIonList count=\"0\">", "</selectedIonList>")] {
        for (open, _) in head.match_indices(empty) {
            let rest = head[open + empty.len()..].trim_start();
            if rest.starts_with(end) {
                spans.push(open..head.len() - rest.len() + end.len());
            }
        }
    }
    if chromatogram {
        for tag in ["<precursorList ", "</precursorList>", "<productList ", "</productList>"] {
            spans.extend(head.match_indices(tag).filter_map(|(at, _)| Some(at..at + head[at..].find('>')? + 1)));
        }
    }
    let no_polarity = marker.is_some();
    spans.extend(marker);
    (spans, no_polarity)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mzdata::spectrum::{MultiLayerSpectrum, ScanEvent, SelectedIon};

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

    fn precursor(intensity: f32, energy: f32) -> Precursor {
        let mut p = Precursor::default();
        p.ions.push(SelectedIon { mz: 445.3, intensity, ..Default::default() });
        p.activation.energy = energy;
        p
    }

    /// Three spectra as the lanes hand them over: an MS1 of unknown polarity with no time of either
    /// kind, an MS2 that states everything, an MS2 of negative polarity that states its start time
    /// and nothing else.
    fn spectra() -> Vec<MultiLayerSpectrum> {
        let spectrum = |i: usize, level: u8, polarity, start_time: f64, injection_time: f32, precursor: Option<Precursor>| {
            let mut spec = MultiLayerSpectrum::<mzpeaks::CentroidPeak, mzpeaks::DeconvolutedPeak>::default();
            let d = spec.description_mut();
            d.id = format!("scan={}", i + 1);
            d.index = i;
            d.ms_level = level;
            d.polarity = polarity;
            let mut scan = ScanEvent::default();
            scan.start_time = start_time;
            scan.injection_time = injection_time;
            d.acquisition.scans.push(scan);
            d.precursor.extend(precursor);
            spec
        };
        vec![
            spectrum(0, 1, ScanPolarity::Unknown, 0.0, 0.0, None),
            spectrum(1, 2, ScanPolarity::Positive, 0.5, 12.5, Some(precursor(1000.0, 35.0))),
            spectrum(2, 2, ScanPolarity::Negative, 0.5, 0.0, Some(precursor(0.0, 0.0))),
        ]
    }

    /// The document mzdata's writer prints for [`spectra`], marked or not, through `sink`.
    fn document(sink: Box<dyn Write>, marked: Option<Zeros>) {
        let mut w = mzdata::io::mzml::MzMLWriter::new(sink);
        w.set_spectrum_count(3);
        for mut spec in spectra() {
            if let Some(stated) = marked {
                mark_spectrum(spec.description_mut(), stated);
            }
            w.write(&spec).unwrap();
        }
        let mut chrom = mzdata::spectrum::Chromatogram::default();
        chrom.description_mut().id = "srm".into();
        chrom.description_mut().precursor.push(precursor(0.0, 0.0));
        chrom.description_mut().products.push(Default::default());
        let mut no_ion = mzdata::spectrum::Chromatogram::default();
        no_ion.description_mut().id = "no ion".into();
        no_ion.description_mut().precursor.push(Precursor::default());
        w.chromatogram_count = 2;
        for chrom in [&mut chrom, &mut no_ion] {
            if let Some(stated) = marked {
                mark_precursors(chrom.description_mut().precursor.iter_mut(), stated);
            }
            for kind in [mzdata::spectrum::ArrayType::TimeArray, mzdata::spectrum::ArrayType::IntensityArray] {
                if !chrom.arrays.has_array(&kind) {
                    chrom.arrays.add(mzdata::spectrum::DataArray::wrap(&kind, mzdata::spectrum::BinaryDataArrayType::Float64, Vec::new()));
                }
            }
            w.write_chromatogram(chrom).unwrap();
        }
        w.wrote_summaries = true;
        w.close().unwrap();
    }

    fn written(marked: Option<Zeros>, sink: bool) -> String {
        let out = Shared::default();
        if sink {
            document(Box::new(UnstatedTerms::new(out.clone())), marked);
        } else {
            document(Box::new(out.clone()), marked);
        }
        String::from_utf8(out.0.take()).unwrap()
    }

    fn count(doc: &str, needle: &str) -> usize {
        doc.matches(needle).count()
    }

    /// What mzdata's writer states for the three spectra and the chromatogram, and what is left of
    /// it: the unknown polarity, the four zeros nobody stated, the empty precursor list and the
    /// chromatogram's two lists are gone; everything stated stays; no byte moved.
    #[test]
    fn what_nobody_stated_is_not_written() {
        let raw = written(None, false);
        assert_eq!(count(&raw, "MS:1000130"), 2, "positive scan on the unknown one too");
        assert_eq!(count(&raw, "\"scan start time\" value=\"0\""), 1);
        assert_eq!(count(&raw, "\"ion injection time\" value=\"0\""), 2);
        assert_eq!(count(&raw, "\"peak intensity\" value=\"0\""), 2, "a spectrum's and the chromatogram's");
        assert_eq!(count(&raw, "\"collision energy\" value=\"0\""), 3, "a spectrum's and the two chromatograms'");
        assert_eq!(count(&raw, "<precursorList count=\"0\">"), 1);
        assert_eq!(count(&raw, "<precursorList count=\"1\">"), 4);
        assert_eq!(count(&raw, "<selectedIonList count=\"0\">"), 1, "the chromatogram whose precursor has no ion");
        assert_eq!(count(&raw, "<productList"), 1);

        let doc = written(Some(Zeros::NONE), true);
        assert_eq!(doc.len(), written(Some(Zeros::NONE), false).len(), "blanked in place, so the <indexList> offsets stay true");
        assert_eq!(count(&doc, "MS:1000130"), 1, "the stated one: {doc}");
        assert_eq!(count(&doc, "MS:1000129"), 1);
        assert!(!doc.contains("NaN") && !doc.contains(POLARITY_NOT_STATED), "{doc}");
        assert_eq!(count(&doc, "MS:1000927"), 1, "{doc}");
        assert!(doc.contains("\"ion injection time\" value=\"12.5\""), "{doc}");
        assert_eq!(count(&doc, "MS:1000042"), 1, "{doc}");
        assert!(doc.contains("\"peak intensity\" value=\"1000\""), "{doc}");
        assert_eq!(count(&doc, "MS:1000045"), 1, "{doc}");
        assert!(doc.contains("\"collision energy\" value=\"35\""), "{doc}");
        assert_eq!(count(&doc, "MS:1000016"), 2, "{doc}");
        assert_eq!(count(&doc, "\"scan start time\" value=\"0.5\""), 2, "{doc}");
        // The lists: none is empty, and the chromatogram holds its precursor and product themselves.
        assert_eq!(count(&doc, "<precursorList count=\"0\">") + count(&doc, "<selectedIonList count=\"0\">"), 0);
        assert_eq!((count(&doc, "<selectedIonList count=\"1\">"), count(&doc, "</selectedIonList>")), (3, 3), "{doc}");
        assert_eq!((count(&doc, "<precursorList count=\"1\">"), count(&doc, "</precursorList>")), (2, 2), "the two MS2 spectra's");
        assert_eq!(count(&doc, "<productList") + count(&doc, "</productList>"), 0);
        let chromatogram = &doc[doc.find("<chromatogram ").unwrap()..doc.find("</chromatogram>").unwrap()];
        assert!(chromatogram.contains("<precursor>") && chromatogram.contains("<product>"), "{chromatogram}");
        assert!(!chromatogram.contains("<precursorList") && !chromatogram.contains("<productList"), "{chromatogram}");
        // Every element is still where the index says, and mzdata reads the document back.
        for (at, open) in doc.match_indices("<offset idRef=\"") {
            let rest = &doc[at + open.len()..];
            let (id, rest) = rest.split_once("\">").unwrap();
            let offset: usize = rest[..rest.find('<').unwrap()].parse().unwrap();
            let element = doc[offset..].trim_start();
            assert!(element[..element.find('>').unwrap()].contains(&format!("id=\"{id}\"")), "{id} at {offset}");
        }
        let mut reader = mzdata::io::mzml::MzMLReader::new_indexed(std::io::Cursor::new(doc.into_bytes()));
        let read: Vec<_> = reader.iter().collect();
        assert_eq!(read.iter().map(|s| s.description().polarity).collect::<Vec<_>>(), [ScanPolarity::Unknown, ScanPolarity::Positive, ScanPolarity::Negative]);
        assert_eq!(read[1].description().precursor[0].activation.energy, 35.0);
        assert_eq!(read.iter().map(|s| s.start_time()).collect::<Vec<_>>(), [0.0, 0.5, 0.5], "no time reads as mzdata's default");
        let chrom = reader.get_chromatogram_by_id("srm").expect("the chromatogram, read by its offset");
        assert!(chrom.precursor().is_some() && chrom.product().is_some(), "read without their lists");
    }

    /// A reader that gives every scan its time (a vendor's, an archive's): a start time of 0 is the
    /// scan's, and the other zeros are still nobody's.
    #[test]
    fn a_start_time_of_zero_stays_where_every_scan_has_its_time() {
        let doc = written(Some(Zeros::TIME), true);
        assert_eq!(count(&doc, "\"scan start time\" value=\"0\""), 1, "{doc}");
        assert_eq!(count(&doc, "MS:1000016"), 3);
        assert_eq!(count(&doc, "MS:1000927") + count(&doc, "MS:1000042") + count(&doc, "MS:1000045"), 3, "the stated three: {doc}");
        assert!(!doc.contains("NaN"), "{doc}");
    }

    /// A wavelength spectrum's scans are marked for their start times alone.
    #[test]
    fn start_times_are_marked_alone() {
        let mut spec = spectra().remove(0);
        mark_start_times(spec.description_mut(), Zeros::NONE);
        let scan = &spec.description().acquisition.scans[0];
        assert!(scan.start_time.is_nan() && scan.injection_time == 0.0);
        assert_eq!(spec.description().polarity, ScanPolarity::Unknown);
        let mut spec = spectra().remove(0);
        mark_start_times(spec.description_mut(), Zeros::TIME);
        assert_eq!(spec.description().acquisition.scans[0].start_time, 0.0);
    }

    /// The lists around a chromatogram's precursors and products go whatever they count (mzdata's
    /// model lets a chromatogram hold several); a spectrum keeps its own.
    #[test]
    fn the_lists_of_a_chromatogram_go_whatever_they_count() {
        let head = "<chromatogram id=\"x\" index=\"0\">\n<precursorList count=\"2\">\n<precursor><selectedIonList count=\"1\"><selectedIon/></selectedIonList></precursor>\n<precursor/>\n</precursorList>\n<productList count=\"12\"><product/></productList>\n";
        let blanked = |head: &str, chromatogram: bool| {
            let mut out = head.as_bytes().to_vec();
            for span in unstated(head, chromatogram).0 {
                out[span].fill(b' ');
            }
            String::from_utf8(out).unwrap()
        };
        let out = blanked(head, true);
        assert_eq!(out.len(), head.len());
        assert!(!out.contains("precursorList") && !out.contains("productList"), "{out}");
        assert_eq!((count(&out, "<precursor"), count(&out, "<product"), count(&out, "selectedIonList")), (2, 1, 2), "{out}");
        let spectrum = head.replace("<chromatogram ", "<spectrum ");
        assert_eq!(blanked(&spectrum, false), spectrum);
    }

    /// A run without a chromatogram: the writer closes it with an empty `<chromatogramList>`, which
    /// goes, in place and however the bytes arrive. A list that holds a chromatogram stays.
    #[test]
    fn a_chromatogram_list_without_a_chromatogram_is_not_written() {
        let document = |sink: Box<dyn Write>| {
            let mut w = mzdata::io::mzml::MzMLWriter::new(sink);
            w.set_spectrum_count(1);
            w.write(&spectra()[1]).unwrap();
            w.chromatogram_count = 0;
            w.wrote_summaries = true;
            w.close().unwrap();
        };
        let raw = Shared::default();
        document(Box::new(raw.clone()));
        let raw = raw.0.take();
        let text = String::from_utf8(raw.clone()).unwrap();
        assert_eq!((count(&text, "<chromatogramList count=\"0\""), count(&text, "</chromatogramList>")), (1, 1), "{text}");
        let through = |block: usize| {
            let out = Shared::default();
            let mut sink = UnstatedTerms::new(out.clone());
            for chunk in raw.chunks(block) {
                sink.write_all(chunk).unwrap();
            }
            sink.flush().unwrap();
            drop(sink);
            String::from_utf8(out.0.take()).unwrap()
        };
        let doc = through(raw.len());
        assert_eq!(doc.len(), raw.len());
        assert!(!doc.contains("chromatogramList"), "{doc}");
        let after_spectra = &doc[doc.find("</spectrumList>").unwrap() + "</spectrumList>".len()..];
        assert!(after_spectra.trim_start().starts_with("</run>"), "{after_spectra}");
        for block in [1, 7, 64] {
            assert!(through(block) == doc, "blocks of {block}");
        }
        assert_eq!(count(&written(Some(Zeros::NONE), true), "<chromatogramList count=\"2\""), 1);
    }

    /// A 0 the source states itself stays, term by term.
    #[test]
    fn a_stated_zero_stays() {
        let doc = written(Some(Zeros::ALL), true);
        assert_eq!(count(&doc, "\"scan start time\" value=\"0\""), 1, "{doc}");
        assert_eq!(count(&doc, "\"ion injection time\" value=\"0\""), 2, "{doc}");
        assert_eq!(count(&doc, "\"peak intensity\" value=\"0\""), 2);
        assert_eq!(count(&doc, "\"collision energy\" value=\"0\""), 3);
        assert_eq!(count(&doc, "MS:1000130"), 1, "the polarity is not a number: unknown is unknown");
        let doc = written(Some(Zeros(Zeros::COLLISION_ENERGY)), true);
        assert_eq!(count(&doc, "\"collision energy\" value=\"0\""), 3);
        let gone = ["scan start time", "peak intensity", "ion injection time"];
        assert_eq!(gone.iter().map(|term| count(&doc, &format!("\"{term}\" value=\"0\""))).sum::<usize>(), 0, "{doc}");
    }

    /// A scan that only refers to another spectrum states no time of either kind, marked or not.
    #[test]
    fn a_reference_only_scan_stays_without_times() {
        let mut descr = SpectrumDescription::default();
        let mut scan = ScanEvent::default();
        scan.spectrum_reference = Some("scan=1".into());
        descr.acquisition.scans.push(scan);
        mark_spectrum(&mut descr, Zeros::NONE);
        assert_eq!((descr.acquisition.scans[0].start_time, descr.acquisition.scans[0].injection_time), (0.0, 0.0));
        // With an injection time the writer states both times, so a start time nobody stated is marked.
        descr.acquisition.scans[0].injection_time = 5.0;
        mark_spectrum(&mut descr, Zeros::NONE);
        assert!(descr.acquisition.scans[0].start_time.is_nan());
        assert_eq!(descr.acquisition.scans[0].injection_time, 5.0);
    }

    #[test]
    fn a_write_split_anywhere_gives_the_same_bytes() {
        let raw = written(Some(Zeros::NONE), false).into_bytes();
        let through = |chunks: &mut dyn Iterator<Item = &[u8]>| {
            let out = Shared::default();
            let mut sink = UnstatedTerms::new(out.clone());
            for chunk in chunks {
                sink.write_all(chunk).unwrap();
            }
            sink.flush().unwrap();
            drop(sink);
            out.0.take()
        };
        let whole = through(&mut std::iter::once(&raw[..]));
        assert_eq!(String::from_utf8(whole.clone()).unwrap(), written(Some(Zeros::NONE), true));
        assert_ne!(whole, raw);
        for block in [1, 2, 7, 64, 1000] {
            assert!(through(&mut raw.chunks(block)) == whole, "blocks of {block}");
        }
        for k in (0..raw.len()).step_by(37) {
            assert!(through(&mut [&raw[..k], &raw[k..]].into_iter()) == whole, "split at byte {k}");
        }
    }

    /// The zeros a source states itself are found by element, whatever the attribute order, the
    /// quotes and the white space after the element's name, across the read blocks, and through the
    /// param groups an element refers to; a 0 outside a spectrum or chromatogram is nobody's.
    #[test]
    fn stated_zeros_are_read_from_the_source_text() {
        let dir = std::env::temp_dir().join(format!("mzpc-stated-zeros-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("stated.mzML");
        let mut doc = String::from("<mzML><cvParam accession=\"MS:1000045\" name=\"collision energy\" value=\"0\"/>\n");
        doc.push_str("<referenceableParamGroupList count=\"2\"><referenceableParamGroup id=\"scan&amp;1\"><cvParam accession=\"MS:1000016\" name=\"scan start time\" value=\"0\"/></referenceableParamGroup>");
        doc.push_str("<referenceableParamGroup\n id=\"timed\"><cvParam accession=\"MS:1000016\" value=\"1.5\"/><cvParam accession=\"MS:1000927\" value=\"0\"/></referenceableParamGroup></referenceableParamGroupList>\n");
        // After a group has ended, a 0 is not the group's: `late` refers to `timed` and gains nothing from this one.
        doc.push_str("<instrumentConfiguration id=\"IC\"><cvParam accession=\"MS:1000042\" value=\"0\"/></instrumentConfiguration><spectrumList>\n");
        doc.push_str("<spectrum index=\"0\" id=\"scan=1 a&gt;b\" defaultArrayLength=\"0\"><cvParam cvRef=\"MS\" accession=\"MS:1000927\" name=\"ion injection time\" value=\"0.0\"/>");
        // Longer than one read block of binary, so the next spectrum's tags straddle a boundary.
        doc.push_str(&format!("<binary>{}</binary></spectrum>\n", "A".repeat((1 << 20) + 5)));
        doc.push_str("<spectrum id='scan=2' index='1'><cvParam value='0' name='collision energy' accession='MS:1000045'/><cvParam accession=\"MS:1000042\" value=\"12\"/></spectrum>\n");
        // Between two spectra a 0 is neither's.
        doc.push_str("<cvParam accession=\"MS:1000042\" value=\"0\"/>\n");
        doc.push_str("<spectrum id=\"scan=3\"><cvParam accession=\"MS:1000045\" value=\"35\"/><userParam name=\"x\" value=\"0\"/></spectrum>\n");
        doc.push_str("<spectrum\r\n\tid=\"scan=4\"><scanList><scan><referenceableParamGroupRef ref=\"scan&amp;1\"/></scan></scanList></spectrum>\n");
        doc.push_str("<spectrum id=\"late\"><referenceableParamGroupRef\n ref=\"timed\"/><referenceableParamGroupRef ref=\"absent\"/><cvParam\n accession=\"MS:1000045\"\n value=\"0\"/></spectrum></spectrumList>\n");
        doc.push_str("<chromatogramList><chromatogram\tid=\"srm\"><cvParam accession=\"MS:1000509\" name=\"activation energy\" value=\"0e0\"/><cvParam name=\"peak intensity\" value=\"-0\"/></chromatogram></chromatogramList></mzML>");
        std::fs::write(&path, &doc).unwrap();
        let stated = StatedZeros::read(&path).unwrap();
        assert_eq!(stated.count(), 5, "{stated:?}");
        assert_eq!(stated.spectrum("scan=1 a>b"), Zeros(Zeros::INJECTION_TIME));
        assert_eq!(stated.spectrum("scan=2"), Zeros(Zeros::COLLISION_ENERGY));
        assert_eq!(stated.spectrum("scan=3"), Zeros::NONE);
        assert_eq!(stated.spectrum("scan=4"), Zeros::TIME, "through its scan's param group");
        assert_eq!(stated.spectrum("late"), Zeros(Zeros::INJECTION_TIME | Zeros::COLLISION_ENERGY), "a group's 0 and its own");
        assert_eq!(stated.chromatogram("srm"), Zeros(Zeros::COLLISION_ENERGY | Zeros::PEAK_INTENSITY));
        assert_eq!(stated.chromatogram("scan=2"), Zeros::NONE, "a spectrum's id is not a chromatogram's");
        assert_eq!(StatedZeros::everything().spectrum("anything"), Zeros::ALL);
        assert!(StatedZeros::read(&dir.join("absent.mzML")).is_err());
        // The same text in blocks that end anywhere: a tag name cut in two is read whole.
        let small: String = doc.replace(&"A".repeat((1 << 20) + 5), "AAAA");
        for cut in 0..small.len() {
            let mut stated = StatedZeros::default();
            let mut current = None;
            let mut held = Vec::new();
            for block in [&small.as_bytes()[..cut], &small.as_bytes()[cut..]] {
                held.extend_from_slice(block);
                let keep = stated.scan(&held, &mut current);
                held.drain(..keep);
            }
            assert_eq!((stated.count(), stated.spectrum("scan=4"), stated.spectrum("late").0, stated.chromatogram("srm").0), (5, Zeros::TIME, 5, 6), "cut at {cut}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}

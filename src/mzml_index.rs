//! mzML export: the index and the checksum at the end of the document.
//!
//! mzdata 0.67's mzML writer records an element's offset before quick-xml writes the line break and
//! the indentation in front of it, so every `<offset>` of the `<indexList>` points at the newline 9
//! bytes before its `<spectrum` or `<chromatogram`, and `<indexListOffset>` 3 bytes before
//! `<indexList` (0 of 146,005 spectrum offsets in the exports of the corpus were the element's
//! own position). A reader that skips whitespace finds the element; mzML defines the offset as the
//! element's. It takes the `<fileChecksum>` before flushing its buffer, and the sinks under it
//! ([`crate::mzml_header`], [`crate::mzml_isolation`], [`crate::mzml_wavelength`],
//! [`crate::mzml_unstated`]) change bytes after it hashed them, so the checksum was the SHA-1 of
//! some shorter, different prefix of the file (0 of ~500 exports matched).
//!
//! [`IndexFixes`] is the last sink before the file (or the gzip encoder): it sees the bytes as they
//! are stored. It notes where every `<spectrum `, `<chromatogram ` and the `<indexList ` starts,
//! hashes what passes, and holds the index — the tail of the document — to write each offset as the
//! position of the element that follows it and the checksum as mzML defines it: the SHA-1 of the
//! file up to and including the `<fileChecksum>` start tag.

use std::io::{self, Write};

use sha1::{Digest, Sha1};

const ELEMENTS: [&[u8]; 2] = [b"<spectrum ", b"<chromatogram "];
const INDEX_LIST: &[u8] = b"<indexList ";
const CHECKSUM: &str = "<fileChecksum>";
/// An offset is moved to an element start at most this far behind it: mzdata's are 9 bytes short
/// (a line break and 8 spaces), and one that is further from any element is left as it is.
const REACH: u64 = 64;

/// A byte sink for `MzMLWriter` that makes the index's offsets and the file checksum true (see the
/// module docs).
pub struct IndexFixes<W: Write> {
    inner: W,
    sha: Sha1,
    /// Bytes passed on, and hashed, so far.
    written: u64,
    /// Where each `<spectrum ` and `<chromatogram ` starts, ascending.
    elements: Vec<u64>,
    /// Where `<indexList ` starts, once seen: from there on everything is held.
    index_list: Option<u64>,
    /// Bytes not passed on yet: a tail that may still become a start tag, or the index.
    held: Vec<u8>,
}

impl<W: Write> IndexFixes<W> {
    pub fn new(inner: W) -> Self {
        Self { inner, sha: Sha1::new(), written: 0, elements: Vec::new(), index_list: None, held: Vec::new() }
    }

    fn pass(&mut self, n: usize) -> io::Result<()> {
        self.inner.write_all(&self.held[..n])?;
        self.sha.update(&self.held[..n]);
        self.written += n as u64;
        self.held.drain(..n);
        Ok(())
    }

    /// Note the element starts in what is held and pass it on, up to the index or a tail that may
    /// still become one of the start tags.
    fn advance(&mut self) -> io::Result<()> {
        if self.index_list.is_some() {
            return Ok(());
        }
        let mut from = 0;
        let done = loop {
            let Some(lt) = self.held[from..].iter().position(|&b| b == b'<').map(|i| from + i) else {
                break self.held.len();
            };
            let rest = &self.held[lt..];
            if rest.starts_with(INDEX_LIST) {
                self.index_list = Some(self.written + lt as u64);
                break lt;
            }
            if ELEMENTS.iter().any(|e| rest.starts_with(e)) {
                self.elements.push(self.written + lt as u64);
            } else if ELEMENTS.iter().chain([&INDEX_LIST]).any(|e| e.starts_with(rest)) {
                break lt;
            }
            from = lt + 1;
        };
        self.pass(done)
    }

    /// The document is complete: write the index with true offsets and the checksum.
    fn finish(&mut self) -> io::Result<()> {
        if let (Some(index_list), Ok(index)) = (self.index_list, std::str::from_utf8(&self.held)) {
            let fixed = fix_index(index, index_list, &self.elements, self.sha.clone());
            self.held = fixed.into_bytes();
        }
        let n = self.held.len();
        self.pass(n)
    }
}

impl<W: Write> Write for IndexFixes<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.held.extend_from_slice(buf);
        self.advance()?;
        Ok(buf.len())
    }

    /// mzdata flushes once, after the document is closed: the index is complete then. A flush in
    /// mid-document passes on nothing that is held.
    fn flush(&mut self) -> io::Result<()> {
        if self.index_list.is_some() && self.held.trim_ascii_end().ends_with(b"</indexedmzML>") {
            self.finish()?;
        }
        self.inner.flush()
    }
}

impl<W: Write> Drop for IndexFixes<W> {
    fn drop(&mut self) {
        if !self.held.is_empty() {
            let _ = self.finish();
        }
    }
}

/// The text from `<indexList ` to the end of the document with each `<offset>` moved to the
/// element start that follows it within [`REACH`], `<indexListOffset>` set to `index_list`, and
/// the `<fileChecksum>` the SHA-1 of everything before the index (`sha` so far) and of this text
/// up to and including the `<fileChecksum>` start tag.
fn fix_index(index: &str, index_list: u64, elements: &[u64], mut sha: Sha1) -> String {
    let mut out = String::with_capacity(index.len() + 64);
    let mut rest = index;
    while let Some(end) = rest.find("</") {
        let (before, tail) = rest.split_at(end);
        let digits = before.bytes().rev().take_while(u8::is_ascii_digit).count();
        let number = before[before.len() - digits..].parse::<u64>().ok();
        let moved = match number {
            Some(n) if tail.starts_with("</offset>") => {
                let next = elements[elements.partition_point(|&at| at < n)..].first().copied();
                next.filter(|at| at - n <= REACH)
            }
            Some(_) if tail.starts_with("</indexListOffset>") => Some(index_list),
            _ => None,
        };
        match moved {
            Some(at) => {
                out.push_str(&before[..before.len() - digits]);
                out.push_str(&at.to_string());
            }
            None => out.push_str(before),
        }
        out.push_str("</");
        rest = &tail[2..];
    }
    out.push_str(rest);
    if let Some(open) = out.find(CHECKSUM).map(|at| at + CHECKSUM.len()) {
        if let Some(close) = out[open..].find("</fileChecksum>").map(|i| open + i) {
            sha.update(&out.as_bytes()[..open]);
            out.replace_range(open..close, &hex(&sha.finalize()));
        }
    }
    out
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
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

    /// An mzML of `n` spectra and the writer's two summary chromatograms, through `sink`.
    fn document(sink: Box<dyn Write>, n: usize) {
        let mut w = mzdata::io::mzml::MzMLWriter::new(sink);
        w.set_spectrum_count(n as u64);
        for i in 0..n {
            let mut spec = mzdata::spectrum::MultiLayerSpectrum::<mzpeaks::CentroidPeak, mzpeaks::DeconvolutedPeak>::default();
            // An id that holds a tag's first bytes and a number before `</`, as text.
            spec.description_mut().id = format!("scan={} <spectrum 7</b", i + 1);
            spec.description_mut().index = i;
            spec.description_mut().ms_level = 1;
            spec.description_mut().acquisition.scans.push(Default::default());
            w.write(&spec).unwrap();
        }
        w.close().unwrap();
    }

    fn raw(n: usize) -> Vec<u8> {
        let out = Shared::default();
        document(Box::new(out.clone()), n);
        out.0.take()
    }

    fn fixed(n: usize) -> String {
        let out = Shared::default();
        document(Box::new(IndexFixes::new(out.clone())), n);
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

    fn checksum(doc: &str) -> (String, String) {
        let open = doc.find(CHECKSUM).unwrap() + CHECKSUM.len();
        let stated = doc[open..open + doc[open..].find('<').unwrap()].to_string();
        (stated, hex(&Sha1::digest(&doc.as_bytes()[..open])))
    }

    /// What mzdata writes — offsets at the line break before each element, a checksum of something
    /// else — and what leaves the sink: each offset at its element, the index list's at
    /// `<indexList`, the checksum that of the bytes before it. Nothing else changes.
    #[test]
    fn the_offsets_and_the_checksum_are_true() {
        let raw = String::from_utf8(raw(12)).unwrap();
        for (id, at) in offsets(&raw) {
            assert!(raw[at..].starts_with('\n'), "mzdata's offset of {id}: {:?}", &raw[at..at + 20]);
        }
        let (stated, real) = checksum(&raw);
        assert_ne!(stated, real, "mzdata's checksum is not the file's");

        let doc = fixed(12);
        let index = offsets(&doc);
        assert_eq!(index.len(), 12 + 2, "12 spectra, TIC and BIC");
        for (id, at) in &index {
            let element = &doc[*at..];
            assert!(element.starts_with("<spectrum ") || element.starts_with("<chromatogram "), "{id} at {at}: {:?}", &element[..30]);
            assert!(element[..element.find('>').unwrap()].contains(&format!("id=\"{id}\"")), "{id} at {at}");
        }
        let list_offset: usize = doc.split_once("<indexListOffset>").unwrap().1.split_once('<').unwrap().0.parse().unwrap();
        assert!(doc[list_offset..].starts_with("<indexList "), "{:?}", &doc[list_offset..list_offset + 20]);
        let (stated, real) = checksum(&doc);
        assert_eq!(stated, real, "the SHA-1 of the file up to and including <fileChecksum>");
        // Up to the index nothing changed, and mzdata still reads every spectrum by its offset.
        let body = raw.find("<indexList ").unwrap();
        assert_eq!(doc[..body], raw[..body]);
        assert!(doc.ends_with("</indexedmzML>"), "{:?}", &doc[doc.len() - 40..]);
        let mut reader = mzdata::io::mzml::MzMLReader::new_indexed(std::io::Cursor::new(doc.into_bytes()));
        assert_eq!(reader.len(), 12);
        assert_eq!(reader.get_spectrum_by_index(7).unwrap().description().index, 7);
        assert!(reader.get_chromatogram_by_id("TIC").is_some());
    }

    /// The writer hands the sink its bytes in blocks of its own choosing, and may flush between.
    #[test]
    fn the_result_does_not_depend_on_how_the_bytes_arrive() {
        let raw = raw(5);
        let whole = fixed(5).into_bytes();
        for block in [1, 2, 7, 64, 4096] {
            let out = Shared::default();
            let mut sink = IndexFixes::new(out.clone());
            for chunk in raw.chunks(block) {
                sink.write_all(chunk).unwrap();
                sink.flush().unwrap();
            }
            assert!(*out.0.borrow() == whole, "blocks of {block}: complete once the document is closed");
            drop(sink);
            assert!(out.0.take() == whole, "blocks of {block}");
        }
        // A document cut off before or inside its index goes out as it is when the sink is dropped.
        for cut in [raw.len() / 3, raw.len() - 60] {
            let out = Shared::default();
            let mut sink = IndexFixes::new(out.clone());
            sink.write_all(&raw[..cut]).unwrap();
            sink.flush().unwrap();
            drop(sink);
            let written = out.0.take();
            assert_eq!(written.len(), cut, "cut at {cut}");
            assert!(written.ends_with(&raw[cut - 10..cut]), "cut at {cut}");
        }
    }

    #[test]
    fn an_offset_far_from_any_element_is_left() {
        let index = "<indexList count=\"1\"><index name=\"spectrum\"><offset idRef=\"a\">100</offset><offset idRef=\"b\">500</offset><offset idRef=\"c\"></offset></index></indexList><indexListOffset>7</indexListOffset><fileChecksum>0</fileChecksum>";
        let fixed = fix_index(index, 900, &[109, 700], Sha1::new());
        assert!(fixed.contains("<offset idRef=\"a\">109</offset><offset idRef=\"b\">500</offset><offset idRef=\"c\"></offset>"), "{fixed}");
        assert!(fixed.contains("<indexListOffset>900</indexListOffset>"), "{fixed}");
        assert_eq!(checksum(&fixed).0, checksum(&fixed).1);
    }
}

//! mzML export of an isolation window whose width is unknown.
//!
//! Every lane holds such a window as its target with both bounds 0: the Waters DDA set mass, a TSF
//! trigger mass without a width, mzdata's own mzML reader on a target-only window. The vendored
//! mzPeak writer stores that as null offsets. mzdata 0.66's mzML writer has no such case:
//! `write_isolation_window` always writes `target − lower_bound` and `upper_bound − target`, so the
//! same window left `--to mzml` as lower offset +target, upper offset −target, a window from 0 to
//! twice the precursor m/z (`tests/fixtures/target_only_window.mzML` exported 445.3 / −445.3).
//!
//! [`TargetOnlyWindows`] sits under that writer and blanks exactly that pair with spaces of the same
//! length. The element is left target-only, which is what ProteoWizard writes when it knows no
//! width, and every byte offset the writer records in `<indexList>` stays true. The
//! `<fileChecksum>` is left as mzdata wrote it: mzdata takes that digest before flushing its own
//! buffer, so it did not match the file's bytes before this sink existed either.

use std::io::{self, Write};
use std::ops::Range;

const OPEN: &[u8] = b"<isolationWindow>";
const CLOSE: &[u8] = b"</isolationWindow>";

/// A byte sink for `MzMLWriter` that leaves an unknown-width isolation window target-only.
pub struct TargetOnlyWindows<W: Write> {
    inner: W,
    /// Bytes not passed on yet: an `<isolationWindow>` element that is still open, or a tail that
    /// may still become its start tag.
    held: Vec<u8>,
}

impl<W: Write> TargetOnlyWindows<W> {
    pub fn new(inner: W) -> Self {
        Self { inner, held: Vec::new() }
    }
}

impl<W: Write> Write for TargetOnlyWindows<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.held.extend_from_slice(buf);
        let mut done = 0;
        loop {
            let Some(open) = find(&self.held[done..], OPEN).map(|i| done + i) else {
                done = self.held.len() - partial_start_tag(&self.held[done..]);
                break;
            };
            let Some(close) = find(&self.held[open..], CLOSE).map(|i| open + i + CLOSE.len()) else {
                done = open;
                break;
            };
            if let Some(spans) = std::str::from_utf8(&self.held[open..close]).ok().and_then(unknown_offsets) {
                for span in spans {
                    self.held[open + span.start..open + span.end].fill(b' ');
                }
            }
            done = close;
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

impl<W: Write> Drop for TargetOnlyWindows<W> {
    fn drop(&mut self) {
        if !self.held.is_empty() {
            let _ = self.inner.write_all(&self.held);
        }
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    let mut from = 0;
    while let Some(i) = hay[from..].iter().position(|&b| b == needle[0]) {
        let at = from + i;
        if hay[at..].starts_with(needle) {
            return Some(at);
        }
        from = at + 1;
    }
    None
}

/// How many trailing bytes of `tail` could still grow into `<isolationWindow>` on the next write.
fn partial_start_tag(tail: &[u8]) -> usize {
    (1..OPEN.len()).rev().find(|&k| tail.ends_with(&OPEN[..k])).unwrap_or(0)
}

/// The spans of the lower and upper offset `<cvParam/>` of one `<isolationWindow>` element when
/// they are mzdata's rendering of an unknown width: the lower offset is the target's own text and
/// the upper offset the same with a minus sign. Stated bounds cannot produce that pair, because it
/// means a window from 0 to 0 around a non-zero target.
fn unknown_offsets(element: &str) -> Option<[Range<usize>; 2]> {
    let params: Vec<(Range<usize>, &str, &str)> = element
        .match_indices("<cvParam ")
        .filter_map(|(start, _)| {
            let end = start + element[start..].find("/>")? + 2;
            let param = &element[start..end];
            Some((start..end, attribute(param, " name=\"")?, attribute(param, " value=\"")?))
        })
        .collect();
    let named = |name: &str| params.iter().find(|p| p.1 == name);
    let target = named("isolation window target m/z")?.2;
    let lower = named("isolation window lower offset")?;
    let upper = named("isolation window upper offset")?;
    (target != "0" && lower.2 == target && upper.2.strip_prefix('-') == Some(target))
        .then(|| [lower.0.clone(), upper.0.clone()])
}

fn attribute<'a>(param: &'a str, key: &str) -> Option<&'a str> {
    let from = param.find(key)? + key.len();
    Some(&param[from..from + param[from..].find('"')?])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(target: &str, lower: &str, upper: &str) -> String {
        let param = |acc: &str, name: &str, value: &str| {
            format!(
                "\n                <cvParam accession=\"{acc}\" cvRef=\"MS\" name=\"{name}\" value=\"{value}\" \
                 unitCvRef=\"MS\" unitAccession=\"MS:1000040\" unitName=\"m/z\"/>"
            )
        };
        format!(
            "\n              <isolationWindow>{}{}{}\n              </isolationWindow>",
            param("MS:1000827", "isolation window target m/z", target),
            param("MS:1000828", "isolation window lower offset", lower),
            param("MS:1000829", "isolation window upper offset", upper),
        )
    }

    fn through_sink(chunks: &[&[u8]]) -> Vec<u8> {
        let mut out = Vec::new();
        {
            let mut sink = TargetOnlyWindows::new(&mut out);
            for chunk in chunks {
                sink.write_all(chunk).unwrap();
            }
            sink.flush().unwrap();
        }
        out
    }

    /// What mzdata's writer printed for target 445.3 with both bounds 0 (the export of
    /// tests/fixtures/target_only_window.mzML before this sink).
    #[test]
    fn an_unknown_width_leaves_as_a_target_only_window() {
        let unknown = window("445.3", "445.3", "-445.3");
        let out = String::from_utf8(through_sink(&[unknown.as_bytes()])).unwrap();
        assert_eq!(out.len(), unknown.len(), "blanked in place, so the <indexList> offsets stay true");
        assert!(out.contains("name=\"isolation window target m/z\" value=\"445.3\""), "{out}");
        assert!(!out.contains("lower offset") && !out.contains("upper offset"), "{out}");
    }

    #[test]
    fn a_stated_window_passes_through() {
        for stated in [
            window("445.3", "1", "1"),
            // Equal to the target, but not a window from 0 to 0: stated numbers.
            window("445.3", "445.3", "1"),
            window("445.3", "1", "-445.3"),
            window("0", "0", "-0"),
        ] {
            assert_eq!(through_sink(&[stated.as_bytes()]), stated.as_bytes());
        }
    }

    #[test]
    fn a_write_split_anywhere_gives_the_same_bytes() {
        let unknown = window("445.3", "445.3", "-445.3");
        let doc = format!("<precursor>{}</precursor><binary>QUJD</binary>{unknown}<", window("600.2", "1", "1"));
        let whole = through_sink(&[doc.as_bytes()]);
        assert_ne!(whole, doc.as_bytes(), "the unknown window was blanked");
        let bytes = doc.as_bytes();
        for k in 0..=bytes.len() {
            assert_eq!(through_sink(&[&bytes[..k], &bytes[k..]]), whole, "split at byte {k}");
        }
        let singles: Vec<&[u8]> = bytes.chunks(1).collect();
        assert_eq!(through_sink(&singles), whole, "one byte per write");
    }
}

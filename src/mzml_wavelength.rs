//! mzML export of a wavelength (UV/PDA) spectrum.
//!
//! mzdata 0.66's mzML writer is written for mass spectra. For a spectrum measured over wavelength it
//! states `ms level` 0 and `positive scan`, neither of which such a spectrum has (pyOpenMS reads the
//! polarity back as positive, where ProteoWizard's own file of the run states none); it writes the
//! spectrum-type term a second time; and it gives the scan an `ion injection time` of 0.
//!
//! [`WavelengthSpectra`] sits under that writer and blanks exactly those with spaces of the same
//! length, as [`crate::mzml_isolation`] does for unknown isolation windows, so every byte offset the
//! writer records in `<indexList>` stays true. `defaultArrayLength="0"` stays: a longer number does
//! not fit in its place, and each array's own `arrayLength` is right. A mass spectrum passes through.
//! The `<fileChecksum>` is left as mzdata wrote it: mzdata takes that digest before flushing its own
//! buffer, so it did not match the file's bytes before this sink existed either.
//!
//! The sink cannot tell a term mzdata invented from one a source stated, so a wavelength spectrum
//! also loses a stated `positive scan` or `ion injection time` of 0. No source in the corpus states
//! either on one; `ms level` is blanked only where it is 0, which no source states.

use std::io::{self, Write};
use std::ops::Range;

use crate::mzml_isolation::find;

const OPEN: &[u8] = b"<spectrum ";
/// Everything the writer states about a spectrum comes before its arrays.
const HEAD_END: &[u8] = b"<binaryDataArrayList";
/// PSI-MS's spectrum types measured over wavelength: PDA, electromagnetic radiation, emission,
/// absorption (mzdata's `SpectrumType::default_main_axis` is the wavelength array for these four).
const WAVELENGTH_TYPES: [&str; 4] = ["MS:1000620", "MS:1000804", "MS:1000805", "MS:1000806"];

/// A byte sink for `MzMLWriter` that blanks what the writer invents for a wavelength spectrum.
pub struct WavelengthSpectra<W: Write> {
    inner: W,
    /// Bytes not passed on yet: a `<spectrum>` element whose head is still incomplete, or a tail that
    /// may still become its start tag.
    held: Vec<u8>,
}

impl<W: Write> WavelengthSpectra<W> {
    pub fn new(inner: W) -> Self {
        Self { inner, held: Vec::new() }
    }
}

impl<W: Write> Write for WavelengthSpectra<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.held.extend_from_slice(buf);
        let mut done = 0;
        loop {
            let Some(open) = find(&self.held[done..], OPEN).map(|i| done + i) else {
                done = self.held.len() - partial_start_tag(&self.held[done..]);
                break;
            };
            let Some(head_end) = find(&self.held[open..], HEAD_END).map(|i| open + i) else {
                done = open;
                break;
            };
            if let Ok(head) = std::str::from_utf8(&self.held[open..head_end]) {
                for span in invented(head) {
                    self.held[open + span.start..open + span.end].fill(b' ');
                }
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

impl<W: Write> Drop for WavelengthSpectra<W> {
    fn drop(&mut self) {
        if !self.held.is_empty() {
            let _ = self.inner.write_all(&self.held);
        }
    }
}

/// How many trailing bytes of `tail` could still grow into `<spectrum ` on the next write.
fn partial_start_tag(tail: &[u8]) -> usize {
    (1..OPEN.len()).rev().find(|&k| tail.ends_with(&OPEN[..k])).unwrap_or(0)
}

/// The spans, in the text of one `<spectrum>` element up to its arrays, of what the writer states for
/// a wavelength spectrum that the spectrum does not have: `ms level` 0, `positive scan`, an
/// `ion injection time` of 0, and each spectrum-level parameter that repeats an earlier one exactly,
/// value included (a differing value is a statement of its own). None for a mass spectrum, whose type
/// term the writer puts first.
fn invented(head: &str) -> Vec<Range<usize>> {
    let params: Vec<(Range<usize>, &str, &str)> = head
        .match_indices("<cvParam ")
        .filter_map(|(start, _)| {
            let end = start + head[start..].find("/>")? + 2;
            let param = &head[start..end];
            Some((start..end, attribute(param, " accession=\"")?, attribute(param, " value=\"").unwrap_or("")))
        })
        .collect();
    if !params.first().is_some_and(|p| WAVELENGTH_TYPES.contains(&p.1)) {
        return Vec::new();
    }
    let scan_list = head.find("<scanList").unwrap_or(head.len());
    let mut seen: Vec<(&str, &str)> = Vec::new();
    params
        .into_iter()
        .filter_map(|(span, accession, value)| {
            let spectrum_level = span.start < scan_list;
            let repeated = spectrum_level && seen.contains(&(accession, value));
            if spectrum_level {
                seen.push((accession, value));
            }
            let blank = repeated
                || (accession == "MS:1000511" && value == "0")
                || accession == "MS:1000130"
                || (accession == "MS:1000927" && value.parse::<f64>().is_ok_and(|v| v == 0.0));
            blank.then_some(span)
        })
        .collect()
}

fn attribute<'a>(param: &'a str, key: &str) -> Option<&'a str> {
    let from = param.find(key)? + key.len();
    Some(&param[from..from + param[from..].find('"')?])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn param(accession: &str, name: &str, value: &str) -> String {
        format!("\n          <cvParam accession=\"{accession}\" cvRef=\"MS\" name=\"{name}\" value=\"{value}\"/>")
    }

    /// A spectrum element as mzdata's writer prints it, up to and including its array list's start.
    fn spectrum(kind: (&str, &str), spectrum_params: &[String], injection_time: &str) -> String {
        format!(
            "\n        <spectrum id=\"s\" index=\"0\" defaultArrayLength=\"0\">{}{}\n          <scanList count=\"1\">{}\n            <scan instrumentConfigurationRef=\"IC1\">{}{}\n            </scan>\n          </scanList>\n          <binaryDataArrayList count=\"2\">",
            param(kind.0, kind.1, ""),
            spectrum_params.concat(),
            param("MS:1000795", "no combination", ""),
            param("MS:1000016", "scan start time", "0.5"),
            param("MS:1000927", "ion injection time", injection_time),
        )
    }

    fn through_sink(chunks: &[&[u8]]) -> Vec<u8> {
        let mut out = Vec::new();
        {
            let mut sink = WavelengthSpectra::new(&mut out);
            for chunk in chunks {
                sink.write_all(chunk).unwrap();
            }
            sink.flush().unwrap();
        }
        out
    }

    /// What mzdata's writer printed for a UV spectrum of `tests/fixtures/pda_uv.pwiz.mzML` read back
    /// from its archive.
    fn uv() -> String {
        let emr = ("MS:1000804", "electromagnetic radiation spectrum");
        spectrum(
            emr,
            &[
                param("MS:1000511", "ms level", "0"),
                param("MS:1000130", "positive scan", ""),
                param("MS:1000128", "profile spectrum", ""),
                param(emr.0, emr.1, ""),
                param("MS:1000619", "lowest observed wavelength", "209.953"),
                param("MS:1000619", "lowest observed wavelength", "209.953"),
            ],
            "0",
        )
    }

    #[test]
    fn a_wavelength_spectrum_loses_what_the_writer_invents() {
        let uv = uv();
        let out = String::from_utf8(through_sink(&[uv.as_bytes()])).unwrap();
        assert_eq!(out.len(), uv.len(), "blanked in place, so the <indexList> offsets stay true");
        assert_eq!(out.matches("MS:1000804").count(), 1, "{out}");
        assert_eq!(out.matches("MS:1000619").count(), 1, "{out}");
        for gone in ["ms level", "positive scan", "ion injection time"] {
            assert!(!out.contains(gone), "{gone} survived: {out}");
        }
        for kept in ["profile spectrum", "no combination", "scan start time", "<binaryDataArrayList"] {
            assert!(out.contains(kept), "{kept} was blanked: {out}");
        }
    }

    /// A repeat with a different value, and an `ms level` other than 0, are statements, not the writer's.
    #[test]
    fn a_differing_repeat_and_a_stated_ms_level_stay() {
        let emr = ("MS:1000804", "electromagnetic radiation spectrum");
        let head = spectrum(
            emr,
            &[
                param("MS:1000511", "ms level", "1"),
                param("MS:1000619", "lowest observed wavelength", "209.953"),
                param("MS:1000619", "lowest observed wavelength", "190"),
            ],
            "0",
        );
        let out = String::from_utf8(through_sink(&[head.as_bytes()])).unwrap();
        assert!(out.contains(r#"value="1""#) && out.contains(r#"value="190""#) && out.contains(r#"value="209.953""#), "{out}");
    }

    #[test]
    fn a_mass_spectrum_and_a_stated_injection_time_pass_through() {
        let ms1 = spectrum(
            ("MS:1000579", "MS1 spectrum"),
            &[param("MS:1000511", "ms level", "1"), param("MS:1000130", "positive scan", "")],
            "0",
        );
        assert_eq!(through_sink(&[ms1.as_bytes()]), ms1.as_bytes());
        let stated = spectrum(("MS:1000804", "electromagnetic radiation spectrum"), &[], "12.5");
        assert!(String::from_utf8(through_sink(&[stated.as_bytes()])).unwrap().contains("value=\"12.5\""));
    }

    #[test]
    fn a_write_split_anywhere_gives_the_same_bytes() {
        let doc = format!("<spectrumList count=\"2\">{}<binary>QUJD</binary>{}<", uv(), spectrum(("MS:1000579", "MS1 spectrum"), &[], "0"));
        let whole = through_sink(&[doc.as_bytes()]);
        assert_ne!(whole, doc.as_bytes(), "the UV spectrum was blanked");
        let bytes = doc.as_bytes();
        for k in 0..=bytes.len() {
            assert_eq!(through_sink(&[&bytes[..k], &bytes[k..]]), whole, "split at byte {k}");
        }
        let singles: Vec<&[u8]> = bytes.chunks(1).collect();
        assert_eq!(through_sink(&singles), whole, "one byte per write");
    }
}

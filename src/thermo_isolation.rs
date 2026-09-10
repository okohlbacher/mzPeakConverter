//! Thermo isolation windows the reader library computed without a stated width (D3, interim).
//!
//! thermorawfilereader's .NET core (`librawfilereader/Lib.cs`, `ExtractPrecursorAndTrailerMetadata`,
//! the same in the 0.7.3 and 0.8.0 bundles) takes a precursor's isolation width from the scan's
//! `MS<n> Isolation Width` trailer. A scan without that trailer reads as width 0, and the fallback
//! then takes the scan filter's width and halves it, and the `IsolationWindow` constructor halves it
//! again: the window is a quarter of the filter's width, and inverted when the filter reports a
//! negative one. mzdata copies those bounds as `Complete`. The published archives carry the result:
//! ec04479's 13,004 MS3 windows at −0.25/−0.25, and 2013_30_Amrutha's 15,265 MS2 windows at ±0.25
//! where the run's method says 2.00 and ProteoWizard says ±1.0.
//!
//! Until upstream stops computing them, [`UnstatedWidthGuard`] keeps such a window's target and
//! zeroes both bounds, the form every lane uses for "width unknown": the mzPeak writer stores null
//! offsets and the mzML export writes the target only (`mzml_isolation`). A window stays as the
//! library built it only when the scan's own trailer states a positive width and the window is
//! neither empty nor inverted.

use std::path::Path;

use mzdata::spectrum::{IsolationWindow, SpectrumDescription};
use thermorawfilereader::RawFileReader;

/// One scan's `MS<ms_level> Isolation Width` trailer as a number: the label the library looks up
/// (both it and `get_raw_trailers_for` strip the trailing colon). `None` when the scan has no such
/// trailer or its value is not a number.
fn stated_width<'a>(trailers: impl IntoIterator<Item = (&'a str, &'a str)>, ms_level: u8) -> Option<f64> {
    let label = format!("MS{ms_level} Isolation Width");
    trailers.into_iter().find(|(l, _)| *l == label)?.1.trim().parse().ok()
}

/// Whether the library's window rests on a width the vendor stated.
fn is_stated(width: Option<f64>, window: &IsolationWindow) -> bool {
    width.is_some_and(|w| w > 0.0) && window.lower_bound < window.upper_bound
}

/// Applies the rule to every spectrum of one Thermo run. mzdata keeps its `RawFileReader` private,
/// so the trailers are read through the converter's own handle, as the trailer facets are.
pub struct UnstatedWidthGuard {
    handle: Option<RawFileReader>,
    rewritten: usize,
}

impl UnstatedWidthGuard {
    /// A run whose trailers cannot be read states no width for any scan, so every MSn window is
    /// then written target-only.
    pub fn open(path: &Path) -> Self {
        let handle = match RawFileReader::open(path) {
            Ok(handle) => Some(handle),
            Err(e) => {
                log::warn!(
                    "cannot read the Thermo scan trailers ({e}): every MSn isolation window is written \
                     target-only"
                );
                None
            }
        };
        Self { handle, rewritten: 0 }
    }

    pub fn apply(&mut self, descr: &mut SpectrumDescription) {
        if descr.precursor.is_empty() {
            return;
        }
        let trailers = self.handle.as_ref().and_then(|h| h.get_raw_trailers_for(descr.index));
        let width = trailers
            .as_ref()
            .and_then(|t| stated_width(t.iter().map(|kv| (kv.label, kv.value)), descr.ms_level));
        for precursor in descr.precursor.iter_mut() {
            let window = &mut precursor.isolation_window;
            if !is_stated(width, window) {
                window.lower_bound = 0.0;
                window.upper_bound = 0.0;
                self.rewritten += 1;
            }
        }
    }

    /// How many windows [`apply`](Self::apply) left target-only.
    pub fn rewritten(&self) -> usize {
        self.rewritten
    }

    /// The run's one warning, when a window was rewritten.
    pub fn report(&self) {
        if self.rewritten > 0 {
            log::warn!(
                "{} Thermo precursor isolation window(s) had no stated width (no positive 'MS<n> \
                 Isolation Width' trailer on the scan, or an empty or inverted window): written \
                 target-only, because thermorawfilereader computes a quarter-width or inverted window \
                 for them",
                self.rewritten
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mzdata::spectrum::{IsolationWindowState, Precursor};

    fn window(target: f32, lower_bound: f32, upper_bound: f32) -> IsolationWindow {
        IsolationWindow { target, lower_bound, upper_bound, flags: IsolationWindowState::Complete }
    }

    #[test]
    fn the_width_is_the_trailer_of_the_scans_own_ms_level() {
        // ec04479's MS3 scans: an MS2 width, no MS3 one, which is the key the library reads.
        let ms3 = [("Monoisotopic M/Z", "0.0000"), ("MS2 Isolation Width", "1.20")];
        assert_eq!(stated_width(ms3, 2), Some(1.2));
        assert_eq!(stated_width(ms3, 3), None);
        assert_eq!(stated_width([("MS2 Isolation Width", " 2.00 ")], 2), Some(2.0));
        assert_eq!(stated_width([("MS2 Isolation Width", "")], 2), None);
    }

    #[test]
    fn only_a_positive_trailer_and_a_proper_window_keep_the_library_numbers() {
        // small.RAW: trailer 2.0 and 810.79 ± 1.0, as ProteoWizard says.
        assert!(is_stated(Some(2.0), &window(810.79, 809.79, 811.79)));
        // 2013_30_Amrutha, SZB8102938: no trailer, ± 0.25.
        assert!(!is_stated(None, &window(371.14, 370.89, 371.39)));
        // A trailer of 0 takes the same fallback; a negative one is no width either.
        assert!(!is_stated(Some(0.0), &window(371.14, 370.89, 371.39)));
        assert!(!is_stated(Some(-1.0), &window(371.14, 370.89, 371.39)));
        assert!(!is_stated(Some(f64::NAN), &window(371.14, 370.89, 371.39)));
        // ec04479 MS3 (−0.25/−0.25), or an empty window, whatever the trailer says.
        assert!(!is_stated(Some(1.2), &window(677.45, 677.70, 677.20)));
        assert!(!is_stated(Some(1.2), &window(677.45, 677.45, 677.45)));
    }

    #[test]
    fn a_computed_window_keeps_its_target_and_loses_its_bounds() {
        // No trailers to read: no scan states a width.
        let mut guard = UnstatedWidthGuard { handle: None, rewritten: 0 };
        let mut ms3 = SpectrumDescription { ms_level: 3, ..Default::default() };
        let mut precursor = Precursor::default();
        precursor.isolation_window = window(677.45, 677.70, 677.20);
        ms3.precursor.push(precursor);
        let mut ms1 = SpectrumDescription { ms_level: 1, ..Default::default() };
        guard.apply(&mut ms3);
        guard.apply(&mut ms1);
        let w = &ms3.precursor[0].isolation_window;
        assert_eq!((w.target, w.lower_bound, w.upper_bound), (677.45, 0.0, 0.0));
        assert_eq!(guard.rewritten(), 1, "an MS1 spectrum has no window to rewrite");
    }
}

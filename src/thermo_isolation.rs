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
//!
//! The same guard drops a **precursor reference the library made up**. mzdata's Thermo reader sets
//! `precursor_id` to the native id of the library's parent index unconditionally, and the library
//! reports index 0 for a scan without a parent: on a run with no MS1 (PXD057269's SRM
//! `LD401_001fmol_r1.raw`) every one of 9,600 MS2 spectra named scan 1 — itself an MS2 spectrum,
//! scan 1 included — as the spectrum its precursor was selected from. A reference is cleared when
//! it names the spectrum itself or a spectrum whose MS level is not below the child's
//! ([`INVALID_PARENT`]); `precursor_index` and `precursor_id` are then null, which the spec asks
//! for when the parent is not in the archive.

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

/// The `transformations` entry for a precursor reference that was cleared.
pub const INVALID_PARENT: &str = "thermo:invalid-precursor-reference-dropped";

/// The MS level of each spectrum seen so far, by Thermo scan number (a parent precedes its
/// children), and how many precursor references were cleared.
#[derive(Debug, Default)]
struct Parents {
    levels: Vec<u8>,
    dropped: usize,
}

/// The scan number of a Thermo native id (`controllerType=0 controllerNumber=1 scan=17`).
fn scan_number(id: &str) -> Option<usize> {
    id.rsplit_once("scan=")?.1.trim().parse().ok()
}

impl Parents {
    /// Clear each precursor reference of `descr` that names `descr` itself or a spectrum already
    /// seen whose MS level is not below `descr`'s, then record `descr`'s level. A reference to a
    /// spectrum not seen, or to an id that is not a Thermo scan, stays as it is.
    fn apply(&mut self, descr: &mut SpectrumDescription) {
        let own = scan_number(&descr.id);
        for precursor in descr.precursor.iter_mut() {
            let Some(id) = precursor.precursor_id.as_deref() else { continue };
            let parent = scan_number(id);
            let itself = id == descr.id;
            let not_below = parent.and_then(|n| self.levels.get(n).copied()).is_some_and(|level| level != 0 && level >= descr.ms_level);
            if itself || not_below {
                precursor.precursor_id = None;
                self.dropped += 1;
            }
        }
        if let Some(n) = own {
            if self.levels.len() <= n {
                self.levels.resize(n + 1, 0);
            }
            self.levels[n] = descr.ms_level;
        }
    }
}

/// Applies both rules to every spectrum of one Thermo run. mzdata keeps its `RawFileReader` private,
/// so the trailers are read through the converter's own handle, as the trailer facets are.
pub struct UnstatedWidthGuard {
    handle: Option<RawFileReader>,
    rewritten: usize,
    parents: Parents,
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
        Self { handle, rewritten: 0, parents: Parents::default() }
    }

    pub fn apply(&mut self, descr: &mut SpectrumDescription) {
        self.parents.apply(descr);
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

    /// How many precursor references [`apply`](Self::apply) cleared ([`INVALID_PARENT`]).
    pub fn invalid_parents(&self) -> usize {
        self.parents.dropped
    }

    /// The run's warnings: one when a window was rewritten, one when a precursor reference was cleared.
    pub fn report(&self) {
        if self.parents.dropped > 0 {
            log::warn!(
                "{} Thermo precursor reference(s) named the spectrum itself or a spectrum whose MS level \
                 is not below its own (the reader library names scan 1 for a scan without a parent): \
                 not written",
                self.parents.dropped
            );
        }
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
        IsolationWindow::new(target, lower_bound, upper_bound, IsolationWindowState::Complete)
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
        let mut guard = UnstatedWidthGuard { handle: None, rewritten: 0, parents: Parents::default() };
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

    /// PXD057269's SRM run has no MS1: the library reports parent index 0 for every scan, and
    /// mzdata names scan 1 on all of them, scan 1 included. A reference to the spectrum itself or to
    /// one of the same or a higher MS level is cleared; a real parent stays.
    #[test]
    fn a_precursor_reference_to_itself_or_to_no_lower_level_is_cleared() {
        let id = |n: usize| format!("controllerType=0 controllerNumber=1 scan={n}");
        let spectrum = |n: usize, ms_level: u8, parent: Option<usize>| {
            let mut d = SpectrumDescription { id: id(n), index: n - 1, ms_level, ..Default::default() };
            if ms_level > 1 {
                let mut p = Precursor::default();
                p.precursor_id = parent.map(id);
                d.precursor.push(p);
            }
            d
        };
        let parent_of = |d: &SpectrumDescription| d.precursor.first().and_then(|p| p.precursor_id.clone());
        assert_eq!(scan_number(&id(17)), Some(17));
        assert_eq!(scan_number("scan=1 "), Some(1));
        assert_eq!(scan_number("index=3"), None);

        // The SRM run: MS2 only, each naming scan 1.
        let mut srm = Parents::default();
        let mut scans: Vec<SpectrumDescription> = (1..=4).map(|n| spectrum(n, 2, Some(1))).collect();
        scans.iter_mut().for_each(|d| srm.apply(d));
        assert!(scans.iter().all(|d| parent_of(d).is_none()), "scan 1 names itself, the others an MS2 spectrum");
        assert_eq!(srm.dropped, 4);

        // A data-dependent run: MS1, its MS2, that MS2's MS3; and an MS2 naming another MS2.
        let mut dda = Parents::default();
        let mut scans = [spectrum(1, 1, None), spectrum(2, 2, Some(1)), spectrum(3, 3, Some(2)), spectrum(4, 2, Some(2)), spectrum(5, 2, Some(9))];
        scans.iter_mut().for_each(|d| dda.apply(d));
        assert_eq!(parent_of(&scans[1]), Some(id(1)));
        assert_eq!(parent_of(&scans[2]), Some(id(2)), "an MS3 spectrum's parent is an MS2 spectrum");
        assert_eq!(parent_of(&scans[3]), None, "an MS2 spectrum is no parent of an MS2 spectrum");
        assert_eq!(parent_of(&scans[4]), Some(id(9)), "a spectrum not seen yet is not judged");
        assert_eq!(dda.dropped, 1);

        // Through the guard, as the lanes call it.
        let mut guard = UnstatedWidthGuard { handle: None, rewritten: 0, parents: Parents::default() };
        let mut first = spectrum(1, 2, Some(1));
        guard.apply(&mut first);
        assert_eq!((parent_of(&first), guard.invalid_parents()), (None, 1));
    }
}

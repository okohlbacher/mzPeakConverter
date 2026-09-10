//! What the native SciEX lane decides from what the glue reports: which runs it refuses to convert
//! (from the run-level counts) and which precursor a spectrum states (from its metadata).
//!
//! Host-independent ON PURPOSE (same reasoning as `pwiz_layout`): `sciex.rs` is `#[cfg(windows)]`,
//! so a decision inside it can be neither compiled nor tested here — and the one it held ran on the
//! mzPeak lane only, while `--to mzml` streamed the same file unchecked. The decisions and their
//! messages live here with tests that run on every host; the glue calls stay gated in
//! `SciexReader::open_run` and `SciexReader::spectrum`, which both lanes use.
#![cfg_attr(not(windows), allow(dead_code))]

use std::path::Path;

use mzdata::meta::DissociationMethodTerm;
use mzdata::params::{Param, ParamDescribed, Unit};
use mzdata::spectrum::{Activation, IsolationWindow, IsolationWindowState, Precursor, SelectedIon};

/// Run-level counts the glue gathered while indexing the file.
#[derive(Debug, Clone, Copy, Default)]
pub struct SciexRunInfo {
    pub samples: i32,
    pub unreadable_samples: i32,
    pub dwell_experiments: i32,
    pub scan_experiments: i32,
    pub total_experiments: i32,
}

/// Why `path` must not be converted, or `None`. Decided BEFORE any spectrum is written:
///
/// * **MRM / SIM experiments.** Clearcore2 hands a dwell out as a one-point "spectrum" whose
///   m/z is the transition ORDINAL — the two published corpus archives built this way carried
///   154,520 and 2,215 such rows with no Q1/Q3, dwell or compound identity (BACKLOG). They are
///   transition chromatograms; msconvert writes them as SRM chromatograms with their identity,
///   so the harness's fallback is the right lane (the message is what
///   `tools/box_convert_remote.ps1` classifies on). A MIXED run is refused as a whole: dropping
///   the dwell experiments would lose channels msconvert keeps.
/// * **Unreadable samples.** A partial file must not publish as a complete one.
/// * **Several samples without `--sample`.** The archive is one run; concatenating N samples
///   under one run id is not a conversion of any of them (En_PPY: 116 of 117 samples in one
///   archive, the mzML lane keeps only the last).
/// * **`--sample` outside `1..=samples`.**
///
/// `types` and `sample_names` are the glue's run strings 0 and 5 (names U+001F-joined),
/// `last_error` its last error.
pub fn refusal(
    path: &Path,
    info: &SciexRunInfo,
    sample: Option<u32>,
    types: &str,
    sample_names: &str,
    last_error: &str,
) -> Option<String> {
    if info.dwell_experiments > 0 {
        return Some(format!(
            "{}: MRM/SIM dwell data only handled by the msconvert lane — {} of {} experiments are \
             MRM/SIM dwells and {} are scans (types: {}); the native reader would store each dwell \
             as a one-point spectrum without its transition. Use --via-msconvert, which writes SRM \
             chromatograms.",
            path.display(),
            info.dwell_experiments,
            info.total_experiments,
            info.scan_experiments,
            types
        ));
    }
    if info.unreadable_samples > 0 {
        return Some(format!(
            "{}: {} of {} samples (or their experiments) could not be read by Clearcore2; refusing \
             to write a partial archive. Last glue error: {}",
            path.display(),
            info.unreadable_samples,
            info.samples,
            last_error
        ));
    }
    if info.samples > 1 && sample.is_none() {
        return Some(format!(
            "{}: the WIFF holds {} samples and an archive is ONE run; pass --sample <1..{}> to \
             choose which to convert (sample names: {})",
            path.display(),
            info.samples,
            info.samples,
            sample_names.replace('\u{1F}', " | ")
        ));
    }
    match sample {
        // i64: `n as i32` wraps a huge --sample negative and let it past the bound.
        Some(n) if n == 0 || i64::from(n) > i64::from(info.samples.max(1)) => Some(format!(
            "{}: --sample {n} is out of range (the WIFF holds {} samples)",
            path.display(),
            info.samples
        )),
        _ => None,
    }
}

/// Clearcore2 `ExperimentType` values; ProteoWizard's `WiffFile.hpp` casts the enum straight to them.
const PRODUCT_EXPERIMENT: i32 = 1;
const PRECURSOR_ION_EXPERIMENT: i32 = 2;

/// What the glue reads about one spectrum's precursor (`SciexSpectrumMetaV2`); 0 = not stated.
#[derive(Debug, Clone, Copy, Default)]
pub struct PrecursorFacts {
    /// `ExperimentDetails.ExperimentType`: MS 0, Product 1, Precursor 2, NeutralGainOrLoss 3, SIM 4,
    /// MRM 5; -1 when unreadable.
    pub experiment_type: i32,
    /// `MassSpectrumInfo.ParentMZ` of a product spectrum.
    pub parent_mz: f64,
    /// `MassSpectrumInfo.ParentChargeState`.
    pub charge: i32,
    /// The experiment's `FragmentBasedScanMassRange.IsolationWindow`, full width.
    pub isolation_width: f64,
    /// The experiment's `Parameters["CE"]` as (Start, Stop), eV as stored.
    pub collision_energy: (f64, f64),
    /// The instrument can fragment only in its collision cell ([`collision_only_instrument`]).
    pub collision_only_instrument: bool,
}

/// Whether the instrument Clearcore2 names (`Sample.Details.InstrumentName`) can fragment ONLY by
/// collision, so that beam-type CID is not a guess about the method. A ZenoTOF (7600, 8600) can also
/// fragment by EAD, which Clearcore2 does not report per experiment: ProteoWizard reads the mode only
/// through the `.wiff2` API, and its own `7600ZenoTOFMSMS_EAD_TestData.wiff2` states MS:1003294 on
/// every precursor. An empty or "Unknown" name states nothing. Normalised the way pwiz's
/// `WiffFileImpl::getInstrumentModel` does: upper case, no spaces, no "API".
pub fn collision_only_instrument(instrument: &str) -> bool {
    let model = instrument.to_uppercase().replace(' ', "").replace("API", "");
    !model.is_empty() && model != "UNKNOWN" && !model.contains("ZENOTOF")
}

/// The precursor one spectrum states, or `None`. Read where ProteoWizard's ABI reader reads it
/// (`WiffFile.cpp`, `SpectrumList_ABI.cpp`); a value the file does not state stays unset:
///
/// * **Selected ion**: the product spectrum's parent m/z, with its charge when positive. No parent
///   m/z, no precursor — pwiz's gate too.
/// * **Precursor-ion scans** state their fixed mass as a PRODUCT (pwiz writes a `<product>`). mzdata
///   has no product list, so nothing is written rather than a product ion passed off as a precursor.
/// * **Isolation window**, on a Product experiment only (DDA, MRM-HR's Q1, and every SWATH window —
///   each variable window is its own experiment): parent m/z ± half the experiment's width. When no
///   width is stated the window is target-only (null offsets in the archive), where pwiz writes
///   offsets of 0.
/// * **Collision energy**, as a magnitude (a negative-polarity method stores it negative), when the
///   file states ONE value: Start and Stop equal, or one of them 0. A genuine ramp is kept as
///   `collision energy ramp start` / `end` (MS:1002013 / MS:1002014, as the Waters lane writes one)
///   with no single energy; pwiz writes the midpoint, which the file does not state.
/// * **Dissociation method**: beam-type CID, pwiz's assumption for WIFF instruments (QqTOF and
///   QqLIT collision cells), but only on an instrument that cannot fragment any other way.
///   Clearcore2 states no fragmentation mode and a ZenoTOF can also do EAD, so its precursors carry
///   no method where pwiz's `.wiff` reader writes CID.
pub fn precursor(f: &PrecursorFacts) -> Option<Precursor> {
    if f.experiment_type == PRECURSOR_ION_EXPERIMENT || !(f.parent_mz.is_finite() && f.parent_mz > 0.0) {
        return None;
    }
    let target = f.parent_mz as f32;
    let half = (f.isolation_width / 2.0) as f32;
    let isolation_window = match f.experiment_type {
        PRODUCT_EXPERIMENT if half.is_finite() && half > 0.0 => IsolationWindow {
            target,
            lower_bound: target - half,
            upper_bound: target + half,
            flags: IsolationWindowState::Complete,
        },
        PRODUCT_EXPERIMENT => {
            IsolationWindow { target, flags: IsolationWindowState::Complete, ..Default::default() }
        }
        _ => IsolationWindow::default(),
    };
    let mut activation = Activation::default();
    if f.collision_only_instrument {
        activation.methods_mut().push(DissociationMethodTerm::BeamTypeCollisionInducedDissociation);
    }
    let magnitude = |v: f64| if v.is_finite() { v.abs() } else { 0.0 };
    let (start, stop) = (magnitude(f.collision_energy.0), magnitude(f.collision_energy.1));
    if start == stop || start == 0.0 || stop == 0.0 {
        // 0 when neither is stated; the writer omits a zero energy.
        activation.energy = start.max(stop) as f32;
    } else {
        activation.add_param(
            Param::builder()
                .name("collision energy ramp start")
                .curie(mzdata::curie!(MS:1002013))
                .value(start)
                .unit(Unit::Electronvolt)
                .build(),
        );
        activation.add_param(
            Param::builder()
                .name("collision energy ramp end")
                .curie(mzdata::curie!(MS:1002014))
                .value(stop)
                .unit(Unit::Electronvolt)
                .build(),
        );
    }
    Some(Precursor {
        ions: vec![SelectedIon {
            mz: f.parent_mz,
            charge: (f.charge > 0).then_some(f.charge),
            ..Default::default()
        }],
        isolation_window,
        activation,
        ..Default::default()
    })
}

/// What the glue changed in spectrum arrays on their way out (`SpectrumDataV2`), summed over the
/// spectra a lane wrote. Clearcore2 returns intensities as f64 and the glue narrows them to the
/// schema's f32: NaN becomes 0, a value beyond ±`f32::MAX` (±Inf included) is clamped to it, and an
/// m/z / intensity pair of unequal length is cut to the shorter one.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GlueValueChanges {
    /// Intensity points mapped from NaN to 0.
    pub nan_to_zero: u64,
    /// Intensity points clamped to ±`f32::MAX`.
    pub clamped_to_f32: u64,
    /// Points dropped by cutting the longer array.
    pub truncated_points: u64,
}

impl GlueValueChanges {
    /// Add one spectrum's counts as the glue reports them; a negative count (a glue bug) adds nothing.
    pub fn add(&mut self, nan_to_zero: i64, clamped_to_f32: i64, truncated_points: i64) {
        let count = |v: i64| u64::try_from(v).unwrap_or(0);
        self.nan_to_zero = self.nan_to_zero.saturating_add(count(nan_to_zero));
        self.clamped_to_f32 = self.clamped_to_f32.saturating_add(count(clamped_to_f32));
        self.truncated_points = self.truncated_points.saturating_add(count(truncated_points));
    }

    /// The `transformations` entries for the kinds of change that happened, in a fixed order.
    pub fn transformations(&self) -> Vec<String> {
        [
            (self.nan_to_zero, "sciex:nan-intensity-to-zero"),
            (self.clamped_to_f32, "sciex:clamp-intensity-to-f32"),
            (self.truncated_points, "sciex:truncate-unequal-arrays"),
        ]
        .into_iter()
        .filter(|(n, _)| *n > 0)
        .map(|(_, entry)| entry.to_string())
        .collect()
    }

    /// The warning both SciEX lanes log when anything changed.
    pub fn warning(&self) -> Option<String> {
        (*self != Self::default()).then(|| {
            format!(
                "SciEX glue changed values on their way out of Clearcore2: {} NaN intensities set to 0, \
                 {} intensities clamped to ±f32::MAX, {} points dropped from m/z / intensity arrays of \
                 unequal length",
                self.nan_to_zero, self.clamped_to_f32, self.truncated_points
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{collision_only_instrument, precursor, refusal, GlueValueChanges, PrecursorFacts, SciexRunInfo};
    use mzdata::meta::DissociationMethodTerm;
    use mzdata::params::{ParamDescribed, Unit};
    use mzdata::spectrum::IsolationWindowState;
    use std::path::Path;

    fn refuse(info: SciexRunInfo, sample: Option<u32>) -> Option<String> {
        refusal(Path::new("x.wiff"), &info, sample, "MRM", "a\u{1F}b", "boom")
    }

    fn run(samples: i32) -> SciexRunInfo {
        SciexRunInfo { samples, scan_experiments: 1, total_experiments: 1, ..Default::default() }
    }

    /// The box harness routes a refused dwell run to msconvert by matching `MRM/SIM dwell`, and a
    /// mixed run is refused whole — a valid `--sample` does not rescue it.
    #[test]
    fn dwell_experiments_are_refused() {
        let msg = refuse(SciexRunInfo { dwell_experiments: 2, total_experiments: 3, ..run(1) }, Some(1))
            .expect("a dwell run is refused");
        assert!(msg.starts_with("x.wiff: MRM/SIM dwell data only handled by the msconvert lane"), "{msg}");
        assert!(msg.contains("2 of 3 experiments are MRM/SIM dwells and 1 are scans (types: MRM)"), "{msg}");
    }

    #[test]
    fn unreadable_samples_are_refused() {
        let msg = refuse(SciexRunInfo { unreadable_samples: 1, ..run(3) }, Some(1))
            .expect("a partial file is refused");
        assert_eq!(
            msg,
            "x.wiff: 1 of 3 samples (or their experiments) could not be read by Clearcore2; refusing \
             to write a partial archive. Last glue error: boom"
        );
    }

    #[test]
    fn several_samples_need_a_choice() {
        assert_eq!(
            refuse(run(117), None).as_deref(),
            Some(
                "x.wiff: the WIFF holds 117 samples and an archive is ONE run; pass --sample <1..117> \
                 to choose which to convert (sample names: a | b)"
            )
        );
        assert_eq!(refuse(run(117), Some(117)), None, "a chosen sample converts");
        assert_eq!(refuse(run(1), None), None, "a single-sample file needs no choice");
    }

    #[test]
    fn sample_must_be_in_range() {
        for n in [0, 4, u32::MAX] {
            assert_eq!(
                refuse(run(3), Some(n)),
                Some(format!("x.wiff: --sample {n} is out of range (the WIFF holds 3 samples)"))
            );
        }
    }

    fn product(parent_mz: f64, isolation_width: f64, collision_energy: (f64, f64)) -> PrecursorFacts {
        PrecursorFacts {
            experiment_type: 1,
            parent_mz,
            charge: 0,
            isolation_width,
            collision_energy,
            collision_only_instrument: true,
        }
    }

    /// Beam-type CID only where the instrument cannot fragment any other way: a ZenoTOF can also do
    /// EAD, which Clearcore2 does not report, and an unnamed instrument states nothing.
    #[test]
    fn the_dissociation_method_is_stated_only_for_collision_only_instruments() {
        for name in ["TripleTOF 5600", "TripleTOF 6600+", "API 4000 QTRAP", "X500R QTOF"] {
            assert!(collision_only_instrument(name), "{name}");
        }
        for name in ["ZenoTOF 7600", "SCIEX ZenoTOF 8600", "zenotof7600", "", "  ", "Unknown"] {
            assert!(!collision_only_instrument(name), "{name:?}");
        }
        let zeno = PrecursorFacts { collision_only_instrument: false, ..product(829.5, 0.0, (12.0, 12.0)) };
        let p = precursor(&zeno).expect("the precursor itself is stated");
        assert!(p.activation.methods().is_empty(), "no method where EAD is possible");
        assert_eq!((p.ions[0].mz, p.activation.energy), (829.5, 12.0), "the stated ion and energy are still written");
    }

    /// A SWATH window: the selected ion and the window come from the product spectrum's parent m/z
    /// and the experiment's width, activated by beam-type CID at the one stated energy.
    #[test]
    fn a_product_spectrum_states_its_ion_window_and_energy() {
        let p = precursor(&product(412.5, 25.0, (35.0, 35.0))).expect("a product spectrum states a precursor");
        assert_eq!(p.ions.len(), 1);
        assert_eq!((p.ions[0].mz, p.ions[0].charge), (412.5, None));
        let w = &p.isolation_window;
        assert_eq!((w.target, w.lower_bound, w.upper_bound), (412.5, 400.0, 425.0));
        assert!(matches!(w.flags, IsolationWindowState::Complete));
        assert!(matches!(p.activation.methods(), [DissociationMethodTerm::BeamTypeCollisionInducedDissociation]));
        assert_eq!(p.activation.energy, 35.0);
        assert!(p.activation.params().is_empty());
        assert_eq!(p.precursor_id, None);
    }

    #[test]
    fn a_charge_is_kept_only_when_stated() {
        for (charge, want) in [(2, Some(2)), (0, None), (-1, None)] {
            let p = precursor(&PrecursorFacts { charge, ..product(500.0, 1.0, (0.0, 0.0)) }).unwrap();
            assert_eq!(p.ions[0].charge, want, "ParentChargeState {charge}");
        }
    }

    /// pwiz's gate: no parent m/z, no precursor. A precursor-ion scan's fixed mass is a product, and
    /// mzdata has no product list, so that scan states none either.
    #[test]
    fn no_parent_mz_or_a_precursor_ion_scan_states_no_precursor() {
        for parent_mz in [0.0, -1.0, f64::NAN] {
            assert!(precursor(&product(parent_mz, 25.0, (35.0, 35.0))).is_none(), "parent m/z {parent_mz}");
        }
        let precursor_ion_scan = PrecursorFacts { experiment_type: 2, ..product(500.0, 1.0, (30.0, 30.0)) };
        assert!(precursor(&precursor_ion_scan).is_none());
    }

    /// Only a Product experiment isolates around the parent in Q1. Without a stated width the window
    /// is target-only — the writer stores null offsets — never a width of 0 or of 2·target.
    #[test]
    fn the_window_follows_the_experiment_and_its_stated_width() {
        let w = precursor(&product(500.0, 0.0, (0.0, 0.0))).unwrap().isolation_window;
        assert_eq!((w.target, w.lower_bound, w.upper_bound), (500.0, 0.0, 0.0));
        assert!(matches!(w.flags, IsolationWindowState::Complete));
        for experiment_type in [0, 3, -1] {
            let p = precursor(&PrecursorFacts { experiment_type, ..product(500.0, 1.0, (0.0, 0.0)) })
                .expect("the parent m/z is still the selected ion");
            assert_eq!(p.ions[0].mz, 500.0);
            assert!(
                matches!(p.isolation_window.flags, IsolationWindowState::Unknown),
                "experiment type {experiment_type} states no isolation window"
            );
        }
    }

    /// One stated value is the energy, as a magnitude; a ramp is kept as its two ends and never
    /// collapsed to a midpoint the file does not state.
    #[test]
    fn collision_energy_is_a_stated_value_or_a_ramp_never_a_midpoint() {
        for (ce, want) in [((35.0, 35.0), 35.0), ((0.0, -40.0), 40.0), ((-30.0, 0.0), 30.0), ((0.0, 0.0), 0.0)] {
            let a = precursor(&product(500.0, 1.0, ce)).unwrap().activation;
            assert_eq!(a.energy, want, "CE {ce:?}");
            assert!(a.params().is_empty(), "CE {ce:?} is at most one value");
        }
        let a = precursor(&product(500.0, 1.0, (20.0, -50.0))).unwrap().activation;
        assert_eq!(a.energy, 0.0, "a ramp has no single energy (the writer omits 0)");
        let ramp: Vec<(&str, f64, bool)> = a
            .params()
            .iter()
            .map(|p| (p.name.as_str(), p.value.to_f64().unwrap(), p.unit == Unit::Electronvolt))
            .collect();
        assert_eq!(ramp, [("collision energy ramp start", 20.0, true), ("collision energy ramp end", 50.0, true)]);
        assert_eq!(a.params().iter().map(|p| p.accession).collect::<Vec<_>>(), [Some(1002013), Some(1002014)]);
    }

    /// Only a kind of change that happened is declared, and a spectrum the glue passed through
    /// untouched declares nothing.
    #[test]
    fn only_glue_value_changes_that_happened_are_declared() {
        let mut changes = GlueValueChanges::default();
        changes.add(0, 0, 0);
        assert!(changes.transformations().is_empty(), "untouched spectra declare nothing");
        assert_eq!(changes.warning(), None);
        changes.add(2, 0, 0);
        changes.add(1, 0, -5);
        assert_eq!(changes.transformations(), ["sciex:nan-intensity-to-zero"], "a negative count adds nothing");
        changes.add(0, 1, 3);
        assert_eq!(
            changes.transformations(),
            ["sciex:nan-intensity-to-zero", "sciex:clamp-intensity-to-f32", "sciex:truncate-unequal-arrays"]
        );
        assert_eq!(changes, GlueValueChanges { nan_to_zero: 3, clamped_to_f32: 1, truncated_points: 3 });
        assert_eq!(
            changes.warning().as_deref(),
            Some(
                "SciEX glue changed values on their way out of Clearcore2: 3 NaN intensities set to 0, \
                 1 intensities clamped to ±f32::MAX, 3 points dropped from m/z / intensity arrays of \
                 unequal length"
            )
        );
    }
}

//! The vendor's centroids for Thermo profile scans (`--representation`, `--representation-levels`).
//!
//! A Thermo `.raw` stores, for an FT or Astral profile scan, the instrument's own centroid list
//! beside the profile (the "label data", RawFileReader's centroid stream). mzdata reads one or the
//! other for the whole run; this reads the centroids through the converter's own `RawFileReader`
//! handle, as `thermo_isolation` reads the trailers, and attaches them to each profile spectrum the
//! way the Shimadzu reader does (`shimadzu.rs`, `spectrum`):
//!
//! * `both` (the default): the profile stays in `spectra_data` and the centroids ride along as the
//!   spectrum's peak list, so the writer fills `spectra_peaks` and `number_of_peaks` too;
//! * `centroid`: the centroids replace the profile, and the spectrum is labelled centroid;
//! * `profile`: nothing is read.
//!
//! The choice is per spectrum, by MS level ([`crate::representation_for`]). A profile scan the
//! library returns no centroid list for keeps its profile, correctly labelled, with one warning: the
//! Shimadzu reader's rule for a requested representation the file does not store.

use std::path::Path;

use mzdata::prelude::*;
use mzdata::spectrum::{MultiLayerSpectrum, SignalContinuity};
use mzpeaks::{CentroidPeak, PeakSet};
use thermorawfilereader::RawFileReader;

use crate::RepresentationArg;

/// `transformations` entry: profile spectra carry the vendor's centroid list beside the profile.
pub const VENDOR_CENTROIDS: &str = "vendor-centroids";

/// `transformations` entry: profile spectra are stored as the vendor's centroid list only, their
/// profile left out. Changes the stored signal (`fidelity::SIGNAL_TRANSFORMATIONS`).
pub const VENDOR_CENTROIDS_ONLY: &str = "vendor-centroids:profile-dropped";

pub struct VendorCentroids {
    handle: RawFileReader,
    /// Profile spectra written with their centroids beside them.
    beside: usize,
    /// Profile spectra written as their centroids only.
    replaced: usize,
    /// Profile spectra the library gave no centroid list for.
    missing: usize,
}

impl VendorCentroids {
    /// `None` when no level asks for centroids, or (with a warning) when the file cannot be opened
    /// a second time: the profile is then written as before.
    pub fn open(path: &Path) -> Option<Self> {
        if !crate::representation_may_centroid() {
            return None;
        }
        match RawFileReader::open(path) {
            Ok(handle) => Some(Self { handle, beside: 0, replaced: 0, missing: 0 }),
            Err(e) => {
                log::warn!("cannot read the Thermo centroid lists ({e}): profile spectra are written without them");
                None
            }
        }
    }

    pub fn apply(&mut self, spec: &mut MultiLayerSpectrum) {
        if spec.signal_continuity() != SignalContinuity::Profile {
            return;
        }
        let representation = crate::representation_for(spec.ms_level());
        if representation == RepresentationArg::Profile {
            return;
        }
        let profile_mz = spec.arrays.as_ref().and_then(|a| a.mzs().ok().map(|m| m.to_vec())).unwrap_or_default();
        let Some(data) = self.handle.get_spectrum_data(spec.index(), true) else { return };
        let (mz, intensity) = (data.mz(), data.intensity());
        // No list, or the library handed the profile back unchanged: nothing to attach.
        if mz.is_empty() || *mz == *profile_mz {
            if !profile_mz.is_empty() {
                if self.missing == 0 {
                    log::warn!(
                        "Thermo spectrum {}: the file stores no centroid list for this profile scan; its profile is \
                         written, correctly labelled",
                        spec.id()
                    );
                }
                self.missing += 1;
            }
            return;
        }
        let peaks: PeakSet = PeakSet::new(
            mz.iter().zip(intensity.iter()).enumerate().map(|(k, (m, i))| CentroidPeak::new(*m, *i, k as u32)).collect(),
        );
        spec.peaks = Some(peaks);
        if representation == RepresentationArg::Centroid {
            spec.arrays = None;
            spec.description.signal_continuity = SignalContinuity::Centroid;
            self.replaced += 1;
        } else {
            self.beside += 1;
        }
    }

    pub fn report(&self) {
        if self.beside + self.replaced > 0 {
            log::info!(
                "Thermo vendor centroids: {} profile spectra with their centroid list beside the profile, {} as the \
                 centroid list only",
                self.beside,
                self.replaced
            );
        }
        if self.missing > 0 {
            log::warn!("{} Thermo profile spectra have no vendor centroid list; written as profile only", self.missing);
        }
    }

    /// The `transformations` entries for what was applied.
    pub fn transformations(&self) -> Vec<String> {
        [(self.beside > 0, VENDOR_CENTROIDS), (self.replaced > 0, VENDOR_CENTROIDS_ONLY)]
            .into_iter()
            .filter_map(|(applied, entry)| applied.then(|| entry.to_string()))
            .collect()
    }
}

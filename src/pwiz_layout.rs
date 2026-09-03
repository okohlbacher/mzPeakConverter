//! Where a ProteoWizard install keeps its vendor assemblies.
//!
//! Host-independent ON PURPOSE. The Agilent lanes that consume this are `#[cfg(windows)]`, and a
//! helper inside a gated module can be neither compiled nor TESTED on a non-Windows host: a syntax
//! error in one shipped in v0.9.9, and a unit test for this very function silently reported
//! `0 passed; 78 filtered out`. Pure path logic lives here; only the FFI stays gated.
//!
//! Consequence: every item here is called only from `#[cfg(windows)]` code, so off Windows the
//! module is dead by construction and says so once, here, rather than item by item. The tests below
//! run everywhere and are what actually keeps it honest.
#![cfg_attr(not(windows), allow(dead_code))]

/// Directory holding the Agilent MHDAC/MIDAC assemblies inside a ProteoWizard install.
///
/// ProteoWizard lays these out TWO ways and both are in the wild on the same machine: the installer
/// build (`ProteoWizard 3.0.x`) FLATTENS every vendor DLL beside `msconvert.exe`, while some bundled
/// builds (the FLASHApp/OpenMS third-party tree) keep the documented `vendor_api/Agilent`
/// subdirectory. Hard-coding the subdirectory pinned this lane to the bundled layout — and that in
/// turn pinned the whole box to a pwiz whose Shimadzu library (3.8.4.6016) misaligns centroid
/// intensities on profile-less `.lcd` files. Probe both, preferring the subdirectory, so one clean
/// pwiz install serves every lane.
pub fn agilent_dll_dir(pwiz_dir: &std::path::Path) -> std::path::PathBuf {
    let sub = pwiz_dir.join("vendor_api").join("Agilent");
    if sub.join("MassSpecDataReader.dll").is_file() {
        return sub;
    }
    if pwiz_dir.join("MassSpecDataReader.dll").is_file() {
        return pwiz_dir.to_path_buf();
    }
    sub // neither present: keep the documented path so the error names it
}

/// First `Shimadzu.LabSolutions.IO.IoModule` version that returns centroid intensities ALIGNED
/// with their m/z on profile-less `.lcd` files. ProteoWizard 3.0.26151+ ships it.
const SHIMADZU_FIRST_GOOD_MAJOR: u32 = 5;

/// What we know about the loaded `Shimadzu.LabSolutions.IO.IoModule`, as THREE states.
///
/// 3.8.4.6016 returns centroid intensities rotated against their m/z (shifted 1-7 positions, last
/// peak dropped) for spectra with no profile signal; 5.0.0.0 returns the SAME file correctly
/// (measured on DIA_Hela_20ng.lcd: msconvert driving 5.0.0.0 reproduces the LabSolutions export
/// exactly, max |dintensity| = 0). So the defect belongs to the LIBRARY, not to the file — a
/// warning keyed on "this file stores no profile signal" fires on good data and teaches users to
/// ignore it when it is real.
///
/// Lives here rather than in `shimadzu.rs` because that module is `#[cfg(windows)]` and a test
/// inside it can never run on this host — see the module note above.
///
/// The third one matters. A plain boolean collapses "this library is fine" and "we could not read
/// its version" into the same silence, which turns the old FALSE ALARM into a silent FALSE
/// NEGATIVE: a stale 3.8.4.6016 whose version string we failed to read would store rotated
/// centroids with no warning at all — the exact outcome the check exists to prevent. Unknown must
/// therefore be reported differently from both, and the caller says so out loud.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShimadzuLibrary {
    /// Predates 5.0.0.0: returns rotated centroid intensities on profile-less `.lcd` files.
    KnownBad,
    /// 5.0.0.0 or newer: reads the same files correctly.
    KnownGood,
    /// No version reported, or one we cannot parse. Neither reassure nor accuse.
    Unknown,
}

/// Classify the loaded library version. `None` (no `LibraryVersion` reported), empty and
/// unparseable strings all land on [`ShimadzuLibrary::Unknown`].
pub fn shimadzu_library_status(version: Option<&str>) -> ShimadzuLibrary {
    let Some(version) = version else { return ShimadzuLibrary::Unknown };
    let major = version
        .trim()
        .split(['.', ',', ' ', '-', '+'])
        .next()
        .unwrap_or("")
        .parse::<u32>();
    match major {
        Ok(m) if m < SHIMADZU_FIRST_GOOD_MAJOR => ShimadzuLibrary::KnownBad,
        Ok(_) => ShimadzuLibrary::KnownGood,
        Err(_) => ShimadzuLibrary::Unknown,
    }
}

#[cfg(test)]
mod tests {
    /// Both ProteoWizard layouts resolve, and the documented one wins when both exist. Observed on
    /// one machine: the FLASHApp/OpenMS bundle keeps `vendor_api/Agilent`, the 3.0.26151 installer
    /// flattens the same DLLs beside `msconvert.exe`.
    #[test]
    fn resolves_either_proteowizard_layout() {
        let root = std::env::temp_dir().join(format!("mzpc-agdir-{}", std::process::id()));
        let sub = root.join("sub/vendor_api/Agilent");
        let flat = root.join("flat");
        let both = root.join("both/vendor_api/Agilent");
        for d in [&sub, &flat, &both] {
            std::fs::create_dir_all(d).unwrap();
            std::fs::write(d.join("MassSpecDataReader.dll"), b"stub").unwrap();
        }
        std::fs::write(root.join("both").join("MassSpecDataReader.dll"), b"stub").unwrap();
        let empty = root.join("empty");
        std::fs::create_dir_all(&empty).unwrap();

        assert_eq!(super::agilent_dll_dir(&root.join("sub")), sub, "vendor_api layout");
        assert_eq!(super::agilent_dll_dir(&flat), flat, "flattened layout");
        assert_eq!(super::agilent_dll_dir(&root.join("both")), both, "subdir wins when both exist");
        assert_eq!(
            super::agilent_dll_dir(&empty),
            empty.join("vendor_api").join("Agilent"),
            "neither present: keep the documented path so the error names it"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The centroid-rotation warning must fire on the library versions that HAVE the defect and
    /// stay silent on the ones that do not, because a false alarm on good data is what made the
    /// previous file-based predicate worthless. An UNPARSEABLE version is neither: it is not
    /// accused, but it is not cleared either — it reaches the file probe and gets its own
    /// "could not be checked" wording.
    #[test]
    fn shimadzu_defect_is_keyed_on_the_library_version() {
        for bad in ["3.8.4.6016", "3.8.4", "3", "4.9.9.9", "0.0.0.1", " 3.8.4.6016 "] {
            assert!(
                super::shimadzu_library_status(Some(bad)) == super::ShimadzuLibrary::KnownBad,
                "{bad} predates 5.0.0.0 and rotates centroids"
            );
        }
        for good in ["5.0.0.0", "5.0", "5", "6.1.0.0", "10.0.0.0"] {
            assert!(
                super::shimadzu_library_status(Some(good)) != super::ShimadzuLibrary::KnownBad,
                "{good} reads profile-less .lcd correctly"
            );
        }
        for unknown in ["", "   ", "unknown", "v3.8.4.6016"] {
            assert!(
                super::shimadzu_library_status(Some(unknown)) != super::ShimadzuLibrary::KnownBad,
                "{unknown:?} is not a version we can judge; do not cry wolf"
            );
        }
    }

    /// UNKNOWN is its own answer, distinct from "good". Collapsing the two is how a warning that
    /// exists to catch a stale library goes silent precisely when it cannot see the library: the
    /// caller must be able to say "could not check" instead of saying nothing.
    #[test]
    fn an_unreadable_library_version_is_unknown_not_good() {
        use super::ShimadzuLibrary::*;
        assert_eq!(super::shimadzu_library_status(Some("3.8.4.6016")), KnownBad);
        assert_eq!(super::shimadzu_library_status(Some("5.0.0.0")), KnownGood);
        for u in [None, Some(""), Some("   "), Some("unknown"), Some("v3.8.4.6016")] {
            assert_eq!(
                super::shimadzu_library_status(u),
                Unknown,
                "{u:?} must be Unknown, never KnownGood — silence there hides a stale library"
            );
        }
    }
}

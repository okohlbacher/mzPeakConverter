//! Where a ProteoWizard install keeps its vendor assemblies.
//!
//! Host-independent ON PURPOSE. The Agilent lanes that consume this are `#[cfg(windows)]`, and a
//! helper inside a gated module can be neither compiled nor TESTED on a non-Windows host: a syntax
//! error in one shipped in v0.9.9, and a unit test for this very function silently reported
//! `0 passed; 78 filtered out`. Pure path logic lives here; only the FFI stays gated.

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
}

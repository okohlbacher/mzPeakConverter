//! Run metadata of an Agilent MassHunter `.d`, read from its `AcqData` XML side files on ANY host.
//!
//! The MHDAC lane (Windows) reads spectra through the vendor library, but the run's identity is
//! plain XML that every platform can read and that ProteoWizard itself reads the same way
//! (`XmlMetadataParser` in `MassHunterData.cpp`): `Devices.xml` names the mass spectrometer, its
//! model number and serial; `Contents.xml` carries the acquisition time WITH its UTC offset and the
//! acquisition software version; `sample_info.xml` the sample name. Measured on five corpus units
//! (two `Devices.xsd` generations, `Contents` v3/v4, one unit without `AcqSoftwareVersion`, one with
//! a purely numeric serial) — every read below is optional.
//!
//! Not read here: per-scan precursors (`MSScan.bin`, whose `MzOfInterest` is mode-dependent — the
//! MHDAC host owns that), and the LC device traces (`.cg`/`.cd`, layout unknown).

use std::path::Path;

use mzdata::meta::{Component, ComponentType, InstrumentConfiguration, Sample, Software};

use crate::run_metadata::{
    self, parse_vendor_time, read_text_lossy, source_files_from_members, term, term_str, xml_blocks, xml_text,
    MemberPolicy, Members, VendorRunMetadata,
};

/// Device `Type` codes of the MassHunter `Devices.xml` (the `DeviceType` enumeration of the MHDAC
/// API, which pwiz mirrors): the mass spectrometers among them, with the analyzers each one
/// STATES by being that type. No ion source or detector is asserted — the file does not say.
fn analyzers_for_type(t: u32) -> Option<Vec<Component>> {
    let quad = || term(1000081, "quadrupole");
    let tof = || term(1000084, "time-of-flight");
    let comps: Vec<mzdata::params::Param> = match t {
        2 => vec![quad()],              // Quadrupole (single quad)
        4 => vec![tof()],               // TOF
        5 => vec![quad(), quad(), quad()], // TandemQuadrupole
        6 => vec![quad(), tof()],       // QTOF
        _ => return None,
    };
    Some(
        comps
            .into_iter()
            .enumerate()
            .map(|(i, p)| Component { component_type: ComponentType::Analyzer, order: (i + 1) as u8, params: vec![p] })
            .collect(),
    )
}

/// `Some` when `dot_d/AcqData` carries at least one of the XML side files.
pub(crate) fn read(dot_d: &Path) -> Option<VendorRunMetadata> {
    let acq = dot_d.join("AcqData");
    if !acq.is_dir() {
        return None;
    }
    let mut meta = VendorRunMetadata::default();
    let mut any = false;

    // Devices.xml — the MS device: model name, model number, serial, analyzers.
    if let Ok(doc) = read_text_lossy(&acq.join("Devices.xml")) {
        any = true;
        let devices = xml_blocks(&doc, "Device").into_iter().map(|b| b.to_string()).collect::<Vec<_>>();
        let devices: Vec<&str> = devices.iter().map(String::as_str).collect();
        let ms = devices
            .iter()
            .find(|d| xml_text(d, "Type").and_then(|t| t.parse::<u32>().ok()).is_some_and(|t| matches!(t, 2 | 4 | 5 | 6)))
            .or_else(|| devices.iter().find(|d| xml_text(d, "SerialNumber").is_some_and(|s| !s.trim().is_empty())));
        if let Some(d) = ms {
            let mut cfg = InstrumentConfiguration { id: 0, ..Default::default() };
            cfg.params.push(term(1000490, "Agilent instrument model"));
            if let Some(name) = xml_text(d, "Name").filter(|s| !s.is_empty()) {
                cfg.params.push(term_str(1000031, "instrument model", &name));
            }
            if let Some(model) = xml_text(d, "ModelNumber").filter(|s| !s.is_empty()) {
                cfg.params.push(mzdata::params::Param::new_key_value("model number", model));
            }
            if let Some(serial) = xml_text(d, "SerialNumber").filter(|s| !s.is_empty()) {
                cfg.params.push(term_str(1000529, "instrument serial number", &serial));
            }
            if let Some(comps) = xml_text(d, "Type").and_then(|t| t.parse::<u32>().ok()).and_then(analyzers_for_type) {
                cfg.components = comps;
            }
            meta.instrument = Some(cfg);
        }
    }

    // Contents.xml — acquisition time (with its stated offset) and the acquisition software.
    if let Ok(doc) = read_text_lossy(&acq.join("Contents.xml")) {
        any = true;
        if let Some(t) = xml_text(&doc, "AcquiredTime") {
            match parse_vendor_time(&t, "Agilent Contents.xml AcquiredTime") {
                Ok(at) => meta.start_time = Some(at),
                Err(e) => log::warn!("{e}"),
            }
        }
        let version = xml_text(&doc, "AcqSoftwareVersion").filter(|v| !v.is_empty()).unwrap_or_else(|| "unknown".to_string());
        meta.acquisition_software = Some(Software::new(
            "MassHunter".to_string(),
            version,
            vec![term(1000678, "MassHunter Data Acquisition")],
        ));
        if let Some(cfg) = meta.instrument.as_mut() {
            if let Some(iname) = xml_text(&doc, "InstrumentName").filter(|s| !s.is_empty()) {
                cfg.params.push(mzdata::params::Param::new_key_value("instrument name", iname));
            }
        }
    }

    // sample_info.xml — `<Field><Name>Sample Name</Name><Value>…</Value></Field>`.
    if let Ok(doc) = read_text_lossy(&acq.join("sample_info.xml")) {
        any = true;
        let name = xml_blocks(&doc, "Field")
            .into_iter()
            .find(|f| xml_text(f, "Name").as_deref() == Some("Sample Name"))
            .and_then(|f| xml_text(f, "Value"))
            .filter(|v| !v.is_empty());
        if let Some(name) = name {
            meta.samples.push(Sample::new("sample_1".to_string(), Some(name), vec![]));
        }
    }

    // Source members: the AcqData files, non-recursive, as ProteoWizard lists them (it skips the
    // method directory and any exported text formats sitting beside the binaries).
    let (files, default) = source_files_from_members(
        dot_d,
        &MemberPolicy {
            members: Members::Walk { subdir: Some("AcqData"), skip_ext: &["mzxml", "mzdata", "mgf", "ms2", "txt"] },
            file_format: Some(term(1001509, "Agilent MassHunter format")),
            id_format: Some(term(1001508, "Agilent MassHunter nativeID format")),
            default_member: Some("MSScan.bin"),
        },
    );
    if !files.is_empty() {
        any = true;
        meta.source_files = files;
        meta.default_source_file = default;
    }
    any.then_some(meta)
}

#[allow(dead_code)]
fn _uses(_: &run_metadata::VendorRunMetadata) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run_metadata::AcquisitionTime;

    fn corpus(rel: &str) -> Option<std::path::PathBuf> {
        let p = dirs_home().join("Claude/mzpeak-example-data/data").join(rel);
        p.is_dir().then_some(p)
    }
    fn dirs_home() -> std::path::PathBuf {
        std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default())
    }

    #[test]
    fn blank1_gc_ms_states_model_serial_time_and_members() {
        let Some(d) = corpus("general-ms/MTBLS11742/blank1.D") else { return };
        let m = read(&d).expect("AcqData present");
        let cfg = m.instrument.expect("instrument");
        let serial = cfg.params.iter().find(|p| p.curie() == Some(mzdata::curie!(MS:1000529))).expect("serial");
        assert_eq!(serial.value.to_string(), "US1930M034");
        assert_eq!(cfg.components.len(), 1, "a Type-2 device is a single quadrupole");
        match m.start_time.expect("time") {
            AcquisitionTime::Stated(t) => assert_eq!(t.to_rfc3339(), "2022-11-01T13:11:27.729717400-04:00"),
            other => panic!("{other:?}"),
        }
        assert_eq!(m.acquisition_software.unwrap().id, "MassHunter");
        let names: Vec<&str> = m.source_files.iter().map(|s| s.name.as_str()).collect();
        assert!(names.iter().any(|n| n.eq_ignore_ascii_case("MSScan.bin")), "{names:?}");
        assert!(names.iter().all(|n| !n.starts_with('.')), "AppleDouble and dot files never become sources: {names:?}");
        assert!(m.source_files.iter().all(|s| s.params.iter().any(|p| p.curie() == Some(mzdata::curie!(MS:1000569)))), "every member digested");
        assert_eq!(m.default_source_file.as_deref(), Some("MSScan.bin"));
    }

    #[test]
    fn numeric_serial_stays_a_string() {
        let Some(d) = corpus("general-ms/MTBLS243/03_D24062013T1259_1399CBU_01QC_A3.d") else { return };
        let m = read(&d).expect("AcqData present");
        let cfg = m.instrument.expect("instrument");
        let serial = cfg.params.iter().find(|p| p.curie() == Some(mzdata::curie!(MS:1000529))).expect("serial");
        assert!(matches!(serial.value, mzdata::params::Value::String(_)), "{:?}", serial.value);
        assert_eq!(serial.value.to_string(), "67108922");
    }
}

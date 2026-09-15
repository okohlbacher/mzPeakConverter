//! Run metadata of a Shimadzu LabSolutions `.lcd`, read from the file itself on ANY host.
//!
//! An `.lcd` is an OLE2 compound file. Its root stream `File Property` (and the run-end snapshot
//! `File Property Original`) is a 4-byte header followed by concatenated `<?xml …?>` documents:
//! `FileProperty` (generated/modified FILETIMEs with the writing PC's GMT offset as
//! `szLocGMTDiffGenDateTime`, e.g. `+01'00'`), `DataFileProperty` (`szVersion` = the LabSolutions
//! version), `SampleInfo` (`smpl_name`, `smpl_id`, `vial_num`, `operator_name`, `inj_vol`, and the
//! acquisition start as a FILETIME split into `dwLowDateTime`/`dwHighDateTime` printed as SIGNED
//! decimals). Strings are `@StoX@<hex>` in the file's code page (1252), floats `@FtoX@<hex>` (IEEE f32).
//!
//! Measured on `Blind_P1_pos_012.lcd` (2026-09-09, byte search + three independent encodings): the
//! FILETIMEs are UTC (they match the OLE2 directory FILETIMEs to tens of ms, and the MS-CAB local
//! stamps inside the same file sit exactly the declared +1 h above them), so the acquisition start is
//! a fully ZONED instant — `2024-02-15T11:47:18.756+01:00` = `10:47:18.756Z`. The vendor DLL exposes the
//! same value as `SampleInfo.AnalysisDate` (empty in our .NET 8 host; ProteoWizard then shifts it by
//! the CONVERTING host's current offset and wrote `08:47:18Z`, two hours early). Reading the file is
//! both host-independent and more faithful.

use std::io::Read;
use std::path::Path;

use chrono::{DateTime, FixedOffset, Utc};
use mzdata::meta::{InstrumentConfiguration, Sample, Software};
use mzdata::params::Param;

use crate::run_metadata::{term, term_str, xml_text, AcquisitionTime, VendorRunMetadata};

/// The XML documents of the `File Property` stream, as `(root tag, document text)`.
fn property_documents(lcd: &Path) -> Option<Vec<(String, String)>> {
    let mut comp = cfb::open(lcd).ok()?;
    let mut bytes = Vec::new();
    for name in ["/File Property", "/File Property Original"] {
        if let Ok(mut s) = comp.open_stream(name) {
            bytes.clear();
            if s.read_to_end(&mut bytes).is_ok() && bytes.len() > 4 {
                break;
            }
        }
    }
    if bytes.len() <= 4 {
        return None;
    }
    // Latin-1 keeps every byte addressable; the payload strings are hex-encoded anyway.
    let text: String = bytes[4..].iter().map(|&b| b as char).collect();
    let docs = text
        .split("<?xml")
        .filter_map(|d| {
            let body = d.split_once("?>").map(|(_, b)| b).unwrap_or(d);
            let root = body.trim_start().strip_prefix('<')?;
            let root = root.split(['>', ' ']).next()?.to_string();
            Some((root, body.to_string()))
        })
        .collect::<Vec<_>>();
    (!docs.is_empty()).then_some(docs)
}

/// `@StoX@53797374656D` → `System`; anything else verbatim.
fn stox(v: &str) -> String {
    match v.strip_prefix("@StoX@") {
        Some(hex) => (0..hex.len() / 2)
            .filter_map(|i| u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).ok())
            .map(|b| b as char) // code page 1252 ≈ Latin-1 for everything the fields contain
            .collect(),
        None => v.to_string(),
    }
}

/// `@FtoX@40a00000` → 5.0 (IEEE single, hex big-endian); a bare number parses as itself.
fn ftox(v: &str) -> Option<f32> {
    match v.strip_prefix("@FtoX@") {
        Some(hex) => u32::from_str_radix(hex, 16).ok().map(f32::from_bits),
        None => v.trim().parse().ok(),
    }
}

/// A FILETIME split into two DWORDs printed as signed decimals; `0` means unset.
fn filetime(doc: &str, low_tag: &str, high_tag: &str) -> Option<DateTime<Utc>> {
    let lo = xml_text(doc, low_tag)?.trim().parse::<i64>().ok()? & 0xFFFF_FFFF;
    let hi = xml_text(doc, high_tag)?.trim().parse::<i64>().ok()? & 0xFFFF_FFFF;
    let ft = (hi << 32) | lo;
    if ft == 0 {
        return None;
    }
    const EPOCH_DIFF_S: i64 = 11_644_473_600; // 1601-01-01 → 1970-01-01
    DateTime::<Utc>::from_timestamp(ft / 10_000_000 - EPOCH_DIFF_S, ((ft % 10_000_000) * 100) as u32)
}

/// `+01'00'` → the writing PC's offset.
fn gmt_diff(v: &str) -> Option<FixedOffset> {
    let v = v.trim();
    let sign = match v.chars().next()? {
        '+' => 1,
        '-' => -1,
        _ => return None,
    };
    let digits: Vec<u32> = v[1..].split(['\'', ':']).filter(|s| !s.is_empty()).filter_map(|s| s.parse().ok()).collect();
    let (h, m) = (*digits.first()?, digits.get(1).copied().unwrap_or(0));
    FixedOffset::east_opt(sign * ((h * 3600 + m * 60) as i32))
}

/// The mass spectrometer's model and serial number from the `GUMM_Information/GUMMSubStg/SystemInformation`
/// stream: a UTF-16 `<GUD Type="SI">` document listing every unit the system configuration knows, one
/// `<GUM IT="…">` each with its own XML escaped inside. The MS unit is the one typed `<IT>LCMS</IT>` — for
/// an LCMS-9030 `<IN>LCMS-9030</IN>` with `<USBSN>` its serial number, `<USBPN>` its USB product name —
/// and its serial is exactly what LabSolutions' own mzML export states as `instrument serial number`
/// (Blind_P1_pos_012: `O12035900220JA`; the DIA_Hela runs: `O12035600067JA`). The vendor library exposes
/// neither: `SystemName()` is the operator's name for the whole system (`neo-ms`, `LCMS-9030 wo PDA`).
fn system_information(lcd: &Path) -> Option<(String, String)> {
    let mut comp = cfb::open(lcd).ok()?;
    let mut bytes = Vec::new();
    comp.open_stream("/GUMM_Information/GUMMSubStg/SystemInformation").ok()?.read_to_end(&mut bytes).ok()?;
    let text = if bytes.len() >= 2 && bytes[1] == 0 {
        String::from_utf16_lossy(&bytes.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect::<Vec<_>>())
    } else {
        bytes.iter().map(|&b| b as char).collect()
    };
    let unescape = |s: &str| s.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&amp;", "&");
    let mut rest = text.as_str();
    while let Some(start) = rest.find("<GUM IT=\"") {
        let after = &rest[start..];
        let Some(end) = after.find("</GUM>") else { break };
        let unit = unescape(&after[..end]);
        rest = &after[end + "</GUM>".len()..];
        if xml_text(&unit, "IT").as_deref() != Some("LCMS") {
            continue;
        }
        let serial = xml_text(&unit, "USBSN").map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
        let model = xml_text(&unit, "IN").or_else(|| xml_text(&unit, "USBPN")).map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
        if let (Some(model), Some(serial)) = (model, serial) {
            return Some((model, serial));
        }
    }
    None
}

/// `Some` when the file carries a `File Property` stream or a system configuration with anything usable.
pub(crate) fn read(lcd: &Path) -> Option<VendorRunMetadata> {
    let docs = property_documents(lcd).unwrap_or_default();
    let doc = |root: &str| docs.iter().find(|(r, _)| r == root).map(|(_, d)| d.as_str());
    let mut meta = VendorRunMetadata::default();
    let mut any = false;

    if let Some((model, serial)) = system_information(lcd) {
        meta.instrument = Some(InstrumentConfiguration {
            id: 0,
            params: vec![
                term_str(1000031, "instrument model", &model),
                term_str(1000529, "instrument serial number", &serial),
            ],
            ..Default::default()
        });
        any = true;
    }

    let offset = doc("FileProperty")
        .and_then(|d| xml_text(d, "szLocGMTDiffGenDateTime"))
        .and_then(|v| gmt_diff(&stox(&v)));

    if let Some(si) = doc("SampleInfo") {
        // The acquisition start: a UTC FILETIME, presented in the writer's own offset when it states one.
        if let Some(utc) = filetime(si, "dwLowDateTime", "dwHighDateTime") {
            let stated = match offset {
                Some(off) => utc.with_timezone(&off),
                None => utc.with_timezone(&FixedOffset::east_opt(0).unwrap()),
            };
            meta.start_time = Some(AcquisitionTime::Stated(stated));
            any = true;
        }
        let field = |tag: &str| xml_text(si, tag).map(|v| stox(&v)).filter(|v| !v.is_empty());
        if let Some(name) = field("smpl_name") {
            let mut params = Vec::new();
            for (tag, label) in [("smpl_id", "sample id"), ("vial_num", "vial number"), ("operator_name", "operator name"), ("smpl_type", "sample type")] {
                if let Some(v) = field(tag) {
                    params.push(Param::new_key_value(label, v));
                }
            }
            if let Some(v) = xml_text(si, "inj_vol").and_then(|v| ftox(&v)).filter(|v| *v > 0.0) {
                params.push(Param::new_key_value("injection volume (uL)", v.to_string()));
            }
            meta.samples.push(Sample::new("sample_1".to_string(), Some(name), params));
            any = true;
        }
    }

    // LabSolutions version: the data file's own format/software stamp.
    if let Some(v) = doc("DataFileProperty").and_then(|d| xml_text(d, "szVersion")).map(|v| stox(&v)).filter(|v| !v.is_empty()) {
        meta.acquisition_software = Some(Software::new(
            "LabSolutions".to_string(),
            v,
            vec![term(1001557, "Shimadzu Corporation software")],
        ));
        any = true;
    }
    any.then_some(meta)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stox_ftox_and_gmt_diff_decode() {
        assert_eq!(stox("@StoX@53797374656D2041646D696E6973747261746F72"), "System Administrator");
        assert_eq!(stox("plain"), "plain");
        assert_eq!(ftox("@FtoX@40a00000"), Some(5.0));
        assert_eq!(gmt_diff("+01'00'").unwrap().local_minus_utc(), 3600);
        assert_eq!(gmt_diff("-05'30'").unwrap().local_minus_utc(), -(5 * 3600 + 1800));
        let d = "<x><dwLowDateTime>1490313960</dwLowDateTime><dwHighDateTime>31088636</dwHighDateTime></x>";
        assert_eq!(filetime(d, "dwLowDateTime", "dwHighDateTime").unwrap().to_rfc3339(), "2024-02-15T10:47:18.756221600+00:00");
        assert!(filetime("<x><dwLowDateTime>0</dwLowDateTime><dwHighDateTime>0</dwHighDateTime></x>", "dwLowDateTime", "dwHighDateTime").is_none());
    }

    #[test]
    fn blind_states_a_zoned_start_sample_and_labsolutions_version() {
        // The primary stream `read` prefers — the root `File Property` of MetaboLights MTBLS13204
        // Blind_P1_pos_012.lcd, 8 KB of the 55 MB file — re-wrapped in a compound file here. It used
        // to read the corpus copy from $HOME and return silently wherever that was absent.
        let stream = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/blind_file_property.bin")).unwrap();
        let lcd = std::env::temp_dir().join(format!("mzpc-blind-{}.lcd", std::process::id()));
        {
            let mut comp = cfb::create(&lcd).unwrap();
            let mut s = comp.create_stream("/File Property").unwrap();
            std::io::Write::write_all(&mut s, &stream).unwrap();
            std::io::Write::flush(&mut s).unwrap();
            drop(s);
            comp.flush().unwrap();
        }
        let m = read(&lcd).expect("File Property stream");
        let _ = std::fs::remove_file(&lcd);
        match m.start_time.expect("start time") {
            AcquisitionTime::Stated(t) => assert_eq!(t.to_rfc3339(), "2024-02-15T11:47:18.756221600+01:00"),
            other => panic!("{other:?}"),
        }
        assert_eq!(m.samples[0].name.as_deref(), Some("Blind"));
        assert!(m.samples[0].params.iter().any(|p| p.name == "sample id" && p.value.to_string() == "P1_pos"));
        assert!(m.samples[0].params.iter().any(|p| p.name == "injection volume (uL)" && p.value.to_string() == "5"));
        let sw = m.acquisition_software.expect("software");
        assert_eq!((sw.id.as_str(), sw.version.as_str()), ("LabSolutions", "5.114"));
        assert!(m.instrument.is_none(), "no system configuration stream in this fixture");
    }

    #[test]
    fn blind_states_its_mass_spectrometer_model_and_serial() {
        // Blind_P1_pos_012.lcd's `GUMM_Information/GUMMSubStg/SystemInformation` stream (21 KB), re-wrapped
        // in a compound file with the same storage path; the value it must yield is the serial LabSolutions'
        // own mzML export of the run states.
        let stream = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/blind_system_information.bin")).unwrap();
        let lcd = std::env::temp_dir().join(format!("mzpc-blind-si-{}.lcd", std::process::id()));
        {
            let mut comp = cfb::create(&lcd).unwrap();
            comp.create_storage("/GUMM_Information").unwrap();
            comp.create_storage("/GUMM_Information/GUMMSubStg").unwrap();
            let mut s = comp.create_stream("/GUMM_Information/GUMMSubStg/SystemInformation").unwrap();
            std::io::Write::write_all(&mut s, &stream).unwrap();
            std::io::Write::flush(&mut s).unwrap();
            drop(s);
            comp.flush().unwrap();
        }
        assert_eq!(system_information(&lcd), Some(("LCMS-9030".to_string(), "O12035900220JA".to_string())));
        let m = read(&lcd).expect("the system configuration alone is worth a record");
        let _ = std::fs::remove_file(&lcd);
        let cfg = m.instrument.expect("instrument");
        let value = |c: mzdata::params::CURIE| cfg.params.iter().find(|p| p.curie() == Some(c)).map(|p| p.value.to_string());
        assert_eq!(value(mzdata::curie!(MS:1000031)).as_deref(), Some("LCMS-9030"));
        assert_eq!(value(mzdata::curie!(MS:1000529)).as_deref(), Some("O12035900220JA"));
        assert!(m.start_time.is_none() && m.samples.is_empty(), "nothing else is invented");
    }
}

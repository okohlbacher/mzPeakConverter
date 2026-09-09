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
use mzdata::meta::{Sample, Software};
use mzdata::params::Param;

use crate::run_metadata::{term, xml_text, AcquisitionTime, VendorRunMetadata};

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

/// `Some` when the file carries a `File Property` stream with anything usable in it.
pub(crate) fn read(lcd: &Path) -> Option<VendorRunMetadata> {
    let docs = property_documents(lcd)?;
    let doc = |root: &str| docs.iter().find(|(r, _)| r == root).map(|(_, d)| d.as_str());
    let mut meta = VendorRunMetadata::default();
    let mut any = false;

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
        let lcd = std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default())
            .join("Claude/mzpeak-example-data/data/general-ms/shimadzu-lcms-9030-qtof/Blind_P1_pos_012.lcd");
        if !lcd.is_file() {
            return;
        }
        let m = read(&lcd).expect("File Property stream");
        match m.start_time.expect("start time") {
            AcquisitionTime::Stated(t) => assert_eq!(t.to_rfc3339(), "2024-02-15T11:47:18.756221600+01:00"),
            other => panic!("{other:?}"),
        }
        assert_eq!(m.samples[0].name.as_deref(), Some("Blind"));
        assert!(m.samples[0].params.iter().any(|p| p.name == "sample id" && p.value.to_string() == "P1_pos"));
        assert!(m.samples[0].params.iter().any(|p| p.name == "injection volume (uL)" && p.value.to_string() == "5"));
        let sw = m.acquisition_software.expect("software");
        assert_eq!((sw.id.as_str(), sw.version.as_str()), ("LabSolutions", "5.114"));
    }
}

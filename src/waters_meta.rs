//! Run metadata of a Waters MassLynx `.raw` directory, read from its TEXT side files on any host.
//!
//! The MassLynx SDK (Windows) yields spectra; the run's identity is plain text that ProteoWizard
//! also parses as text: `_HEADER.TXT` (`$$ Instrument: MODEL#SERIAL`, `$$ Acquired Date/Time`,
//! `$$ Acquired Name`, `$$ Sample Description`) and `_extern.inf` (`Created by 4.1 SCN916`, the
//! MassLynx version). Latin-1 encoded; keys are `$$ Name: value` lines.
//!
//! The acquisition time here is a wall clock WITHOUT a zone (Waters never states one), so it is
//! preserved as `Naive` and never becomes `run.start_time` — see `run_metadata`.
//!
//! Deliberately NOT read here: `_FUNCTNS.INF` / `_FUNCnnn.STS` / `_FUNCnnn.IDX` (function types,
//! precursor set masses, retention times). Their layouts are reverse-engineered, undocumented and
//! MassLynx-version-dependent; the adversarial review (2026-09-08) refused to let them decide
//! published precursors before a side-by-side with the SDK on the Windows box.

use std::collections::BTreeMap;
use std::path::Path;

use chrono::NaiveDateTime;
use mzdata::meta::{InstrumentConfiguration, Sample, Software, SourceFile};
use mzdata::params::{Param, ParamDescribed};

use crate::run_metadata::{read_text_lossy, term, term_str, AcquisitionTime, VendorRunMetadata};

/// `$$ Key: value` lines → map (first occurrence wins; values trimmed).
fn header_fields(text: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for line in text.lines() {
        let Some(rest) = line.trim().strip_prefix("$$") else { continue };
        let Some((k, v)) = rest.split_once(':') else { continue };
        out.entry(k.trim().to_string()).or_insert_with(|| v.trim().to_string());
    }
    out
}

fn sha1_param(hex: String) -> Param {
    Param::builder().name("SHA-1").curie(mzdata::curie!(MS:1000569)).value(mzdata::params::Value::String(hex)).build()
}

/// `Some` when `raw/_HEADER.TXT` exists.
pub(crate) fn read(raw: &Path) -> Option<VendorRunMetadata> {
    let header = read_text_lossy(&raw.join("_HEADER.TXT")).ok()?;
    let f = header_fields(&header);
    let mut meta = VendorRunMetadata::default();

    // Instrument: `MODEL#SERIAL`; `#NotSet` means the serial was never configured.
    if let Some(inst) = f.get("Instrument").filter(|s| !s.is_empty()) {
        let (model, serial) = match inst.split_once('#') {
            Some((m, s)) => (m.trim(), Some(s.trim())),
            None => (inst.as_str(), None),
        };
        let mut cfg = InstrumentConfiguration { id: 0, ..Default::default() };
        cfg.params.push(term(1000126, "Waters instrument model"));
        if !model.is_empty() {
            cfg.params.push(term_str(1000031, "instrument model", model));
        }
        if let Some(s) = serial.filter(|s| !s.is_empty() && !s.eq_ignore_ascii_case("NotSet")) {
            cfg.params.push(term_str(1000529, "instrument serial number", s));
        }
        meta.instrument = Some(cfg);
    }

    // Time: `03-Dec-2018` + `22:39:33`, no zone.
    if let (Some(d), Some(t)) = (f.get("Acquired Date"), f.get("Acquired Time")) {
        match NaiveDateTime::parse_from_str(&format!("{d} {t}"), "%d-%b-%Y %H:%M:%S") {
            Ok(n) => meta.start_time = Some(AcquisitionTime::Naive { wall_clock: n, source: "Waters _HEADER.TXT" }),
            Err(e) => log::warn!("Waters _HEADER.TXT: unparsed acquired date/time {d:?} {t:?}: {e}"),
        }
    }

    // Sample: the acquired name, with the free-text descriptors as user params.
    if let Some(name) = f.get("Acquired Name").filter(|s| !s.is_empty()) {
        let mut params = Vec::new();
        for (key, label) in [("Sample Description", "sample description"), ("SampleID", "sample id"), ("Job Code", "job code"), ("Bottle Number", "bottle number")] {
            if let Some(v) = f.get(key).filter(|s| !s.is_empty()) {
                params.push(Param::new_key_value(label, v.clone()));
            }
        }
        meta.samples.push(Sample::new("sample_1".to_string(), Some(name.clone()), params));
    }

    // Software: `Created by 4.1 SCN916` in _extern.inf.
    if let Ok(ext) = read_text_lossy(&raw.join("_extern.inf")) {
        let version = ext
            .lines()
            .find_map(|l| l.trim().strip_prefix("Created by").map(|v| v.trim().to_string()))
            .filter(|v| !v.is_empty());
        if let Some(v) = version {
            meta.acquisition_software = Some(Software::new("MassLynx".to_string(), v, vec![term(1000534, "MassLynx")]));
        }
        // The tune page's quadrupole settings, verbatim and unitless: the only statement the file
        // makes about the quadrupole's pass band (MSe runs it non-resolving; DDA widths derive from
        // LM/HM Resolution through the instrument's tune, never stated in Da). A reader can bound the
        // real RF-only pass band from these; the lane never converts them.
        if let Some(cfg) = meta.instrument.as_mut() {
            for key in ["LM Resolution", "HM Resolution", "MS Profile Type", "MSProfileMass1", "MSProfileMass2", "MSProfileMass3", "MSProfileDwellTime1", "MSProfileDwellTime2", "MSProfileRampTime1", "MSProfileRampTime2"] {
                let value = ext.lines().find_map(|l| {
                    let l = l.trim();
                    let rest = l.strip_prefix(key)?;
                    rest.starts_with(|c: char| c == '\t' || c == ' ').then(|| rest.trim().to_string()).filter(|v| !v.is_empty())
                });
                if let Some(v) = value {
                    cfg.params.push(Param::new_key_value(format!("MassLynx tune {key}"), v));
                }
            }
        }
    }

    // Source members, as ProteoWizard lists them: the `_FUNCnnn.DAT` files in numeric order carry
    // the spectra (Waters nativeID format), then every other regular file (no nativeID), minus the
    // vendor's lock file and any dot/AppleDouble name.
    let mut dats: Vec<(u32, String, std::path::PathBuf)> = Vec::new();
    let mut others: Vec<(String, std::path::PathBuf)> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(raw) {
        for e in rd.flatten() {
            if !e.file_type().map(|t| t.is_file()).unwrap_or(false) {
                continue;
            }
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with('.') || name.eq_ignore_ascii_case("lmgt.inf") {
                continue;
            }
            let up = name.to_ascii_uppercase();
            if let Some(num) = up.strip_prefix("_FUNC").and_then(|s| s.strip_suffix(".DAT")).and_then(|n| n.parse::<u32>().ok()) {
                dats.push((num, name, e.path()));
            } else {
                others.push((up, e.path()));
            }
        }
    }
    dats.sort_by_key(|(n, _, _)| *n);
    others.sort_by(|a, b| a.0.cmp(&b.0));
    let mut push = |p: &Path, spectra: bool| {
        let name = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        let mut sf = SourceFile {
            name: name.clone(),
            location: "file://".to_string(),
            id: name,
            file_format: Some(term(1000526, "Waters raw format")),
            id_format: Some(if spectra { term(1000769, "Waters nativeID format") } else { term(1000824, "no nativeID format") }),
            params: Vec::new(),
        };
        match crate::embed_aux::sha1_hex(p) {
            Ok(hex) => sf.add_param(sha1_param(hex)),
            Err(e) => log::warn!("could not digest {}: {e}", p.display()),
        }
        meta.source_files.push(sf);
    };
    for (_, _, p) in &dats {
        push(p, true);
    }
    for (_, p) in &others {
        push(p, false);
    }
    meta.default_source_file = dats.first().map(|(_, n, _)| n.clone()).or_else(|| meta.source_files.first().map(|s| s.id.clone()));
    Some(meta)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_fields_take_the_first_occurrence() {
        let f = header_fields("$$ Instrument: SYNAPTG2-Si#NotSet\r\n$$ Acquired Date: 03-Dec-2018\r\n$$ Instrument: other\r\n");
        assert_eq!(f["Instrument"], "SYNAPTG2-Si#NotSet");
        assert_eq!(f["Acquired Date"], "03-Dec-2018");
    }

    #[test]
    fn capan2_states_model_time_and_members() {
        let raw = std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default())
            .join("Claude/mzpeak-example-data/data/general-ms/waters-synapt-g2si-hdmse/20181203_Capan2_1.raw");
        if !raw.is_dir() {
            return;
        }
        let m = read(&raw).expect("_HEADER.TXT");
        let cfg = m.instrument.expect("instrument");
        assert!(cfg.params.iter().any(|p| p.curie() == Some(mzdata::curie!(MS:1000126))));
        assert!(cfg.params.iter().all(|p| p.curie() != Some(mzdata::curie!(MS:1000529))), "#NotSet is not a serial");
        assert!(matches!(m.start_time, Some(AcquisitionTime::Naive { .. })), "Waters states no zone");
        assert_eq!(m.samples[0].name.as_deref(), Some("20181203_Capan2_1"));
        assert_eq!(m.acquisition_software.as_ref().map(|s| s.version.as_str()), Some("4.1 SCN916"));
        let names: Vec<&str> = m.source_files.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names[0].to_ascii_uppercase(), "_FUNC001.DAT", "{names:?}");
        assert!(names.iter().any(|n| n.eq_ignore_ascii_case("_HEADER.TXT")));
        assert_eq!(m.default_source_file.as_deref().map(str::to_ascii_uppercase), Some("_FUNC001.DAT".to_string()));
    }
}

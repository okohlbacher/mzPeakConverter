//! What a vendor file states about the RUN beyond its spectra — sample, acquisition time,
//! instrument identity, acquisition software, the source members with their digests — and how
//! that is merged into an archive's metadata.
//!
//! Every native lane builds its archive by hand, so every one of these fields had to be plumbed
//! separately and, measured on four lane pairs (`tests/lane_metadata_parity.rs`), none of them
//! was: the mzML lane inherits ProteoWizard's finished model, the native lanes carried a
//! synthesised single source file and a device-type string. This module is the one seam: a lane
//! (or the generic `fixup_run_metadata`, for the vendor directories it can recognise) fills a
//! [`VendorRunMetadata`] and [`apply`] merges it **field by field, idempotently, never overriding
//! what an earlier reader already stated** — mzdata's Thermo and TDF readers pre-fill parts of
//! the model, a lane's own hints may have set the instrument, and `apply` must be safe to call
//! after either.
//!
//! Time policy (adversarial review, 2026-09-08): a vendor time WITH a stated offset is carried
//! verbatim (`run.start_time` is a `DateTime<FixedOffset>`, the schema accepts any offset). A
//! NAIVE wall-clock time (Waters `_HEADER.TXT`, SCIEX `AcquisitionDateTime`, Shimadzu
//! `AnalysisDate`) is NOT labelled with an offset it does not have — RFC 3339 has no "zone
//! unknown" form and chrono renders a zero offset as `Z`, i.e. a false claim of UTC that no
//! consumer could tell apart from a true one. It stays `null` and the wall clock is preserved
//! verbatim in an `acquisition_time` index block, where the reader sees exactly what the vendor
//! stated and nothing more. (ProteoWizard shifts such times by the CONVERTING host's zone, which
//! is why the mzML-lane corpus archives disagree with the vendor files by whole hours.)

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, FixedOffset, NaiveDateTime};
use mzdata::meta::{InstrumentConfiguration, MSDataFileMetadata, Sample, Software, SourceFile};
use mzdata::params::{ControlledVocabulary, Param, ParamDescribed, Value};
use mzdata::curie;

/// An acquisition timestamp as the vendor states it.
#[derive(Debug, Clone)]
pub(crate) enum AcquisitionTime {
    /// Wall clock plus a stated UTC offset (Agilent `Contents.xml`, Bruker `GlobalMetadata`).
    Stated(DateTime<FixedOffset>),
    /// Wall clock only; the vendor says nothing about the zone. Kept verbatim, never given an offset.
    Naive { wall_clock: NaiveDateTime, source: &'static str },
}

/// What one vendor file states about its run. Empty fields mean "the vendor does not say".
#[derive(Debug, Default)]
pub(crate) struct VendorRunMetadata {
    pub samples: Vec<Sample>,
    pub start_time: Option<AcquisitionTime>,
    /// Model/serial params and components for configuration 0. Merged, not replaced.
    pub instrument: Option<InstrumentConfiguration>,
    /// The acquisition software; inserted ahead of the converter's own entry and referenced from
    /// the instrument configuration when that reference is still empty.
    pub acquisition_software: Option<Software>,
    /// The members of the source (a directory input has several), each with its MS:1000569 SHA-1.
    pub source_files: Vec<SourceFile>,
    /// Which member `run.default_source_file_id` should name (an id from `source_files`).
    pub default_source_file: Option<String>,
}

fn same_name(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

fn has_curie(params: &[Param], c: mzdata::params::CURIE) -> bool {
    params.iter().any(|p| p.curie() == Some(c))
}

/// Merge `meta` into `target`. Returns the `acquisition_time` index block when the vendor stated
/// only a naive wall clock (the caller adds it to `mzpeak_index.json`); `None` otherwise.
pub(crate) fn apply(
    target: &mut impl MSDataFileMetadata,
    meta: VendorRunMetadata,
) -> Option<(String, serde_json::Value)> {
    // Samples: by name, then by id.
    for s in meta.samples {
        let dup = target.samples().iter().any(|e| {
            same_name(&e.id, &s.id) || matches!((&e.name, &s.name), (Some(a), Some(b)) if same_name(a, b))
        });
        if !dup {
            target.samples_mut().push(s);
        }
    }

    // Source files: by member name; an existing entry without a digest gains the digest.
    for sf in meta.source_files {
        let existing = target.file_description_mut().source_files.iter_mut().find(|e| same_name(&e.name, &sf.name));
        match existing {
            Some(e) => {
                if !has_curie(&e.params, curie!(MS:1000569)) {
                    if let Some(p) = sf.params.iter().find(|p| p.curie() == Some(curie!(MS:1000569))) {
                        e.add_param(p.clone());
                    }
                }
                if e.file_format.is_none() {
                    e.file_format = sf.file_format.clone();
                }
                if e.id_format.is_none() {
                    e.id_format = sf.id_format.clone();
                }
            }
            None => target.file_description_mut().source_files.push(sf),
        }
    }
    if let Some(want) = meta.default_source_file {
        if target.file_description().source_files.iter().any(|sf| sf.id == want) {
            if let Some(run) = target.run_description_mut() {
                if run.default_source_file_id.is_none() {
                    run.default_source_file_id = Some(want);
                }
            }
        }
    }

    // Acquisition software: by id or name; goes first so `default_data_processing` stays ours.
    let mut software_ref: Option<String> = None;
    if let Some(sw) = meta.acquisition_software {
        let existing = target.softwares().iter().find(|e| same_name(&e.id, &sw.id)).map(|e| e.id.clone());
        match existing {
            Some(id) => software_ref = Some(id),
            None => {
                software_ref = Some(sw.id.clone());
                target.softwares_mut().insert(0, sw);
            }
        }
    }

    // Instrument: configuration 0 gains whatever it lacks; nothing it has is touched.
    if let Some(vendor_cfg) = meta.instrument {
        let cfgs = target.instrument_configurations_mut();
        let first = cfgs.keys().copied().min();
        let cfg = match first {
            Some(id) => cfgs.get_mut(&id).expect("key from keys()"),
            None => {
                cfgs.insert(0, InstrumentConfiguration { id: 0, ..Default::default() });
                cfgs.get_mut(&0).expect("just inserted")
            }
        };
        for p in vendor_cfg.params {
            let present = match p.curie() {
                Some(c) => has_curie(&cfg.params, c),
                None => cfg.params.iter().any(|e| same_name(&e.name, &p.name)),
            };
            if !present {
                cfg.params.push(p);
            }
        }
        if cfg.components.is_empty() {
            cfg.components = vendor_cfg.components;
        }
        if cfg.software_reference.is_empty() {
            if let Some(r) = software_ref.clone().or(Some(vendor_cfg.software_reference).filter(|s| !s.is_empty())) {
                cfg.software_reference = r;
            }
        }
    } else if let Some(r) = software_ref {
        if let Some(cfg) = target.instrument_configurations_mut().values_mut().next() {
            if cfg.software_reference.is_empty() {
                cfg.software_reference = r;
            }
        }
    }

    // Time.
    let mut block = None;
    match meta.start_time {
        Some(AcquisitionTime::Stated(t)) => {
            if let Some(run) = target.run_description_mut() {
                if run.start_time.is_none() {
                    run.start_time = Some(t);
                }
            }
        }
        Some(AcquisitionTime::Naive { wall_clock, source }) => {
            let already = target.run_description().and_then(|r| r.start_time).is_some();
            if !already {
                log::info!(
                    "{source}: acquisition time {wall_clock} carries no time zone; run.start_time stays null and \
                     the wall clock is recorded verbatim in the `acquisition_time` index block"
                );
                block = Some((
                    "acquisition_time".to_string(),
                    serde_json::json!({
                        "wall_clock": wall_clock.format("%Y-%m-%dT%H:%M:%S%.f").to_string(),
                        "zone": "unstated",
                        "source": source,
                        "note": "the vendor file records a local wall-clock time without a UTC offset; \
                                 run.start_time is null because RFC 3339 cannot say 'zone unknown'"
                    }),
                ));
            }
        }
        None => {}
    }
    block
}

// ---------------------------------------------------------------------------------------------
// Source members
// ---------------------------------------------------------------------------------------------

/// Which files of a directory input are its source members.
pub(crate) enum Members<'a> {
    /// Named members, looked up case-insensitively; absent ones are skipped.
    Explicit(&'a [&'a str]),
    /// Every regular file directly under `subdir` of the input (never recursive), minus dot-files,
    /// AppleDouble `._*` siblings and the listed extensions.
    Walk { subdir: Option<&'a str>, skip_ext: &'a [&'a str] },
}

pub(crate) struct MemberPolicy<'a> {
    pub members: Members<'a>,
    pub file_format: Option<Param>,
    /// The nativeID format for members that carry spectra; `None` for auxiliary members.
    pub id_format: Option<Param>,
    /// Member name that becomes `run.default_source_file_id` (case-insensitive).
    pub default_member: Option<&'a str>,
}

/// The `MS:1000569` SHA-1 param msconvert records on a source file. The one builder: every lane's
/// digest goes through it, so the name, accession and string value cannot drift apart per lane.
pub(crate) fn sha1_param(hex: String) -> Param {
    Param::builder()
        .name("SHA-1")
        .curie(curie!(MS:1000569))
        .value(Value::String(hex))
        .build()
}

fn list_dir_files(dir: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = match std::fs::read_dir(dir) {
        Ok(rd) => rd
            .flatten()
            .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
            .map(|e| e.path())
            .collect(),
        Err(_) => Vec::new(),
    };
    out.sort_by_key(|p| p.file_name().map(|n| n.to_string_lossy().to_ascii_uppercase()).unwrap_or_default());
    out
}

/// Enumerate and digest the members of `root` under `policy`. Names are the on-disk names,
/// `location` is the bare `file://` authority (never the converting machine's path), the id is the
/// member name. Returns `(source_files, default_source_file_id)`.
pub(crate) fn source_files_from_members(root: &Path, policy: &MemberPolicy) -> (Vec<SourceFile>, Option<String>) {
    let paths: Vec<PathBuf> = match &policy.members {
        Members::Explicit(names) => {
            let present = list_dir_files(root);
            names
                .iter()
                .filter_map(|want| present.iter().find(|p| p.file_name().is_some_and(|n| same_name(&n.to_string_lossy(), want))).cloned())
                .collect()
        }
        Members::Walk { subdir, skip_ext } => {
            let dir = match subdir {
                Some(s) => root.join(s),
                None => root.to_path_buf(),
            };
            list_dir_files(&dir)
                .into_iter()
                .filter(|p| {
                    let name = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                    if name.starts_with('.') {
                        return false; // .DS_Store, AppleDouble `._*`
                    }
                    let ext = p.extension().map(|e| e.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
                    !skip_ext.iter().any(|s| s.eq_ignore_ascii_case(&ext))
                })
                .collect()
        }
    };
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut out = Vec::new();
    let mut default_id = None;
    for p in paths {
        let name = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        if !seen.insert(name.to_ascii_lowercase()) {
            continue;
        }
        let mut sf = SourceFile {
            name: name.clone(),
            location: "file://".to_string(),
            id: name.clone(),
            file_format: policy.file_format.clone(),
            id_format: policy.id_format.clone(),
            params: Vec::new(),
        };
        match crate::embed_aux::sha1_hex(&p) {
            Ok(hex) => sf.add_param(sha1_param(hex)),
            Err(e) => log::warn!("could not digest {}: {e}", p.display()),
        }
        if policy.default_member.is_some_and(|d| same_name(d, &name)) {
            default_id = Some(sf.id.clone());
        }
        out.push(sf);
    }
    if default_id.is_none() {
        default_id = out.first().map(|sf| sf.id.clone());
    }
    (out, default_id)
}

/// A CV param without a value (a format or model term).
pub(crate) fn term(accession: u32, name: &str) -> Param {
    Param::builder().name(name).curie(mzdata::params::CURIE::new(ControlledVocabulary::MS, accession)).build()
}

/// A CV param carrying a string VALUE (a serial, a model name). Forced to `Value::String`: an
/// `Into<Value>` on `&str` auto-types numeric-looking text to Float, which would drop leading zeros
/// and re-render the value (MTBLS243's Agilent serial is `67108922`).
pub(crate) fn term_str(accession: u32, name: &str, value: &str) -> Param {
    Param::builder()
        .name(name)
        .curie(mzdata::params::CURIE::new(ControlledVocabulary::MS, accession))
        .value(Value::String(value.to_string()))
        .build()
}

/// Parse an ISO-8601 / RFC 3339 timestamp as the vendor wrote it. A stated offset (or `Z`) is kept
/// verbatim; a bare wall clock becomes `Naive`.
pub(crate) fn parse_vendor_time(text: &str, source: &'static str) -> Result<AcquisitionTime> {
    let t = text.trim();
    if let Ok(dt) = DateTime::parse_from_rfc3339(t) {
        return Ok(AcquisitionTime::Stated(dt));
    }
    for fmt in ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%dT%H:%M:%S", "%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%d %H:%M:%S"] {
        if let Ok(n) = NaiveDateTime::parse_from_str(t, fmt) {
            return Ok(AcquisitionTime::Naive { wall_clock: n, source });
        }
    }
    anyhow::bail!("unrecognised timestamp {t:?} in {source}")
}

/// The minimal XML reader these vendor side files need: the text of the FIRST `<tag>…</tag>`
/// (no attributes, no nesting of the same tag), entity-decoded. The Agilent `AcqData` XMLs and the
/// Bruker `SampleInfo.xml` are flat element/value documents; a full parser would be a new
/// dependency for a dozen scalar reads.
pub(crate) fn xml_text<'a>(doc: &'a str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = doc.find(&open)? + open.len();
    let end = doc[start..].find(&close)? + start;
    Some(xml_unescape(doc[start..end].trim()))
}

/// Every `<tag>…</tag>` block of `doc`, in order.
pub(crate) fn xml_blocks<'a>(doc: &'a str, tag: &str) -> Vec<&'a str> {
    let open = format!("<{tag}");
    let close = format!("</{tag}>");
    let mut out = Vec::new();
    let mut from = 0usize;
    while let Some(s) = doc[from..].find(&open) {
        let s = from + s;
        // `<Device` must not match `<Devices`: the name ends at `>` or whitespace.
        let after = doc[s + open.len()..].chars().next();
        if !matches!(after, Some('>') | Some(' ') | Some('\t') | Some('\n') | Some('\r') | Some('/')) {
            from = s + open.len();
            continue;
        }
        let Some(e) = doc[s..].find(&close) else { break };
        let e = s + e + close.len();
        out.push(&doc[s..e]);
        from = e;
    }
    out
}

fn xml_unescape(s: &str) -> String {
    s.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'").replace("&amp;", "&")
}

/// Read a small text file as UTF-8, tolerating a BOM and Latin-1 bytes (Waters headers).
pub(crate) fn read_text_lossy(path: &Path) -> Result<String> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(&bytes);
    Ok(match std::str::from_utf8(bytes) {
        Ok(s) => s.to_string(),
        Err(_) => bytes.iter().map(|&b| b as char).collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stated_offsets_are_kept_and_naive_times_are_not_given_one() {
        match parse_vendor_time("2022-11-01T13:11:27.7297174-04:00", "t").unwrap() {
            AcquisitionTime::Stated(dt) => {
                assert_eq!(dt.offset().local_minus_utc(), -4 * 3600);
                assert_eq!(dt.to_rfc3339(), "2022-11-01T13:11:27.729717400-04:00");
            }
            other => panic!("{other:?}"),
        }
        match parse_vendor_time("2024-07-26T23:15:09.130+02:00", "t").unwrap() {
            AcquisitionTime::Stated(dt) => assert_eq!(dt.offset().local_minus_utc(), 2 * 3600),
            other => panic!("{other:?}"),
        }
        assert!(matches!(parse_vendor_time("2018-12-03T22:39:33", "t").unwrap(), AcquisitionTime::Naive { .. }));
        assert!(parse_vendor_time("yesterday", "t").is_err());
    }

    #[test]
    fn xml_helpers_read_flat_documents() {
        let doc = "\u{feff}<?xml version=\"1.0\"?><Devices><Device DeviceID=\"1\"><Name>SingleQuadrupole</Name><SerialNumber>US19&amp;30</SerialNumber></Device><Device DeviceID=\"2\"><Name>Pump</Name></Device></Devices>";
        assert_eq!(xml_text(doc, "Name").as_deref(), Some("SingleQuadrupole"));
        assert_eq!(xml_text(doc, "SerialNumber").as_deref(), Some("US19&30"));
        let devs = xml_blocks(doc, "Device");
        assert_eq!(devs.len(), 2);
        assert_eq!(xml_text(devs[1], "Name").as_deref(), Some("Pump"));
        assert_eq!(xml_text(doc, "Missing"), None);
    }

    #[test]
    fn apply_is_idempotent_and_never_overrides() {
        use mzdata::meta::MassSpectrometryRun;
        // A minimal MSDataFileMetadata: the vendored writer implements it, but a plain struct with
        // the trait's fields is enough here.
        #[derive(Default)]
        struct M {
            fd: mzdata::meta::FileDescription,
            ic: std::collections::HashMap<u32, InstrumentConfiguration>,
            sw: Vec<Software>,
            sa: Vec<Sample>,
            dp: Vec<mzdata::meta::DataProcessing>,
            run: MassSpectrometryRun,
        }
        impl MSDataFileMetadata for M {
            fn data_processings(&self) -> &Vec<mzdata::meta::DataProcessing> { &self.dp }
            fn instrument_configurations(&self) -> &std::collections::HashMap<u32, InstrumentConfiguration> { &self.ic }
            fn file_description(&self) -> &mzdata::meta::FileDescription { &self.fd }
            fn softwares(&self) -> &Vec<Software> { &self.sw }
            fn samples(&self) -> &Vec<Sample> { &self.sa }
            fn data_processings_mut(&mut self) -> &mut Vec<mzdata::meta::DataProcessing> { &mut self.dp }
            fn instrument_configurations_mut(&mut self) -> &mut std::collections::HashMap<u32, InstrumentConfiguration> { &mut self.ic }
            fn file_description_mut(&mut self) -> &mut mzdata::meta::FileDescription { &mut self.fd }
            fn softwares_mut(&mut self) -> &mut Vec<Software> { &mut self.sw }
            fn samples_mut(&mut self) -> &mut Vec<Sample> { &mut self.sa }
            fn run_description(&self) -> Option<&MassSpectrometryRun> { Some(&self.run) }
            fn run_description_mut(&mut self) -> Option<&mut MassSpectrometryRun> { Some(&mut self.run) }
        }
        let meta = || VendorRunMetadata {
            samples: vec![Sample::new("sample_1".into(), Some("Urine".into()), vec![])],
            start_time: Some(AcquisitionTime::Naive { wall_clock: NaiveDateTime::parse_from_str("2018-12-03T22:39:33", "%Y-%m-%dT%H:%M:%S").unwrap(), source: "t" }),
            instrument: Some(InstrumentConfiguration { id: 0, params: vec![term_str(1000529, "instrument serial number", "007")], ..Default::default() }),
            acquisition_software: Some(Software::new("MassLynx".into(), "4.1".into(), vec![])),
            source_files: vec![SourceFile { name: "_FUNC001.DAT".into(), location: "file://".into(), id: "_FUNC001.DAT".into(), params: vec![sha1_param("ab".into())], ..Default::default() }],
            default_source_file: Some("_FUNC001.DAT".into()),
        };
        let mut m = M::default();
        // Pre-existing state the merge must respect: a serial already stated, one source without digest.
        m.ic.insert(0, InstrumentConfiguration { id: 0, params: vec![term_str(1000529, "instrument serial number", "PRE")], ..Default::default() });
        m.fd.source_files.push(SourceFile { name: "_func001.dat".into(), location: "file://".into(), id: "sourceFile".into(), ..Default::default() });
        let block = apply(&mut m, meta());
        let block2 = apply(&mut m, meta());
        assert!(block.is_some() && block2.is_some(), "the naive-time block is reported on every call; callers add it once");
        assert_eq!(m.sa.len(), 1);
        assert_eq!(m.sw.len(), 1);
        assert_eq!(m.fd.source_files.len(), 1, "case-insensitive member match, no duplicate");
        assert!(m.fd.source_files[0].params.iter().any(|p| p.curie() == Some(curie!(MS:1000569))), "digest added to the existing entry");
        let serial = m.ic[&0].params.iter().find(|p| p.curie() == Some(curie!(MS:1000529))).unwrap();
        assert_eq!(serial.value.to_string(), "PRE", "an earlier reader's serial is never overridden");
        assert_eq!(m.ic[&0].software_reference, "MassLynx");
        assert!(m.run.start_time.is_none(), "a naive time never becomes an instant");
        assert_eq!(block.unwrap().1["zone"], "unstated");
    }
}

//! Vendor aux-file policy + stream-embedding into the mzPeak archive (PLAN P4).
//!
//! For a Bruker `.d` input, after the standard facets are written we walk the source directory and,
//! per a glob→action YAML policy (ported in spirit from BRFP's `aux_config.rs`), either DROP a file
//! (redundant raw signal / transient journals) or EMBED it under `vendor/` as a STORED ZIP member —
//! streamed in fixed chunks (never buffering the whole file), gzip-compressed on the fly for
//! compressible types. The mzPeak STORED-member requirement is preserved: gzip is applied to the
//! *content*, and the `.gz` member itself is STORED (opaque, read whole — never range-accessed).
//!
//! Policy is **preserve-by-default**: anything without a matching DROP rule is embedded. Every
//! decision (embed/drop, gzip, bytes) is recorded in the `vendor_files` index block, so a DROP is
//! always visible, never silent. Run-level `GlobalMetadata` (TSF/TDF SQLite) is captured into a
//! `vendor_metadata` index block.

use std::collections::HashSet;
use std::fs::File;
use std::io::BufReader;
use std::path::Path;

use anyhow::{Context, Result};
use flate2::Compression;
use flate2::read::GzEncoder;
use rusqlite::Connection;
use serde::Deserialize;

use mzpeak_prototyping::archive::{DataKind, EntityType, FileEntry, ZipArchiveWriter};

#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    Drop,
    Embed,
}

#[derive(Debug, Clone, Copy, PartialEq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Gzip {
    #[default]
    Auto,
    Always,
    Never,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Rule {
    #[serde(rename = "match")]
    pat: String,
    action: Action,
    #[serde(default)]
    gzip: Gzip,
}

#[derive(Debug, Clone, Deserialize)]
pub struct VendorPolicy {
    rules: Vec<Rule>,
}

impl VendorPolicy {
    /// Built-in **preserve-by-default** policy: embed every side-file (gzip compressible types).
    /// Nothing is dropped by default — dropping is opt-in via `--aux glob=drop` or a YAML policy.
    /// Rationale: for the LOSSY paths (mzdata f64 m/z, or the Bruker SDK) `analysis.tdf_bin` is the
    /// only exact copy of the signal, so it must be preserved; and SQLite rollback journals can be
    /// needed to recover a DB snapshot. The converter's job is to ADD the mzPeak facets, not to
    /// decide the raw data is disposable.
    ///
    /// The LOSSLESS ims-compact path is different: it encodes the exact integer-TOF + intensity
    /// signal into the Parquet peak facet, so the raw `*_bin` bulk file is fully redundant (it was
    /// ~39% of the archive, a verbatim copy). That path uses [`load_lossless`](Self::load_lossless),
    /// which drops `*_bin` by default. To force-keep it: `--aux 'analysis.tdf_bin=embed'`.
    pub fn builtin() -> Self {
        VendorPolicy { rules: vec![Rule { pat: "*".to_string(), action: Action::Embed, gzip: Gzip::Auto }] }
    }

    /// Load from a YAML file (`rules: [{match, action, gzip}]`), falling back to the built-in
    /// policy when no path is given. `overrides` are `glob=action` strings (highest precedence).
    pub fn load(path: Option<&Path>, overrides: &[String]) -> Result<Self> {
        let mut policy = match path {
            Some(p) => {
                let text = std::fs::read_to_string(p).with_context(|| format!("reading {}", p.display()))?;
                serde_yaml::from_str(&text).with_context(|| format!("parsing {}", p.display()))?
            }
            None => Self::builtin(),
        };
        // Prepend overrides so they win.
        let mut front = Vec::new();
        for o in overrides {
            let (pat, act) = o.split_once('=').with_context(|| format!("--aux must be glob=action, got {o}"))?;
            let action = match act.trim().to_ascii_lowercase().as_str() {
                "drop" => Action::Drop,
                "embed" => Action::Embed,
                other => anyhow::bail!("--aux action must be drop|embed, got {other}"),
            };
            front.push(Rule { pat: pat.trim().to_string(), action, gzip: Gzip::Auto });
        }
        front.extend(policy.rules);
        policy.rules = front;
        Ok(policy)
    }

    /// Like [`load`](Self::load), but for the **lossless** ims-compact facet: the exact integer-TOF
    /// signal already lives in the Parquet peak facet, so the raw `*_bin` bulk binary
    /// (`analysis.tdf_bin` / `analysis.tsf_bin`) is redundant and defaults to DROP. The drop rule is
    /// inserted right after the user `--aux` overrides (which `load` prepends) and before the base
    /// policy, so an explicit `--aux 'analysis.tdf_bin=embed'` still wins. The drop is recorded in
    /// the `vendor_files` manifest, so it is visible, never silent.
    pub fn load_lossless(path: Option<&Path>, overrides: &[String]) -> Result<Self> {
        let mut policy = Self::load(path, overrides)?;
        let drop_bin = Rule { pat: "*_bin".to_string(), action: Action::Drop, gzip: Gzip::Auto };
        // `load` prepends exactly one rule per override, so index == overrides.len() lands the drop
        // immediately after them and ahead of the base catch-all.
        policy.rules.insert(overrides.len(), drop_bin);
        Ok(policy)
    }

    fn resolve(&self, filename: &str) -> (Action, Gzip) {
        for r in &self.rules {
            if glob_match(&r.pat, filename) {
                return (r.action, r.gzip);
            }
        }
        (Action::Embed, Gzip::Auto) // preserve-by-default fallback
    }
}

/// One recorded vendor-file decision for the index `vendor_files` block. `bytes` is the original
/// (uncompressed source) size; `content_encoding` is `gzip` (member holds gzip-compressed bytes,
/// read whole) or `identity`.
fn entry(path: &str, action: &str, content_encoding: &str, bytes: u64) -> serde_json::Value {
    serde_json::json!({ "path": path, "action": action, "content_encoding": content_encoding, "bytes": bytes })
}

/// Walk the `.d` directory and embed/drop per policy, recording every decision. Then attach the
/// `vendor_files` manifest and `vendor_metadata` (GlobalMetadata) to the archive index.
pub fn embed_into_archive(
    zip: &mut ZipArchiveWriter<File>,
    dot_d: &Path,
    policy: &VendorPolicy,
) -> Result<()> {
    let mut manifest: Vec<serde_json::Value> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut files = Vec::new();
    collect_files(dot_d, dot_d, &mut files)?;
    files.sort();

    for rel in files {
        let abs = dot_d.join(&rel);
        let name = abs.file_name().and_then(|s| s.to_str()).unwrap_or("");
        let (action, gzip) = policy.resolve(name);
        let src_bytes = std::fs::metadata(&abs).map(|m| m.len()).unwrap_or(0);
        if action == Action::Drop {
            log::debug!("vendor: drop {rel} ({src_bytes} bytes)");
            manifest.push(entry(&rel, "drop", "identity", src_bytes));
            continue;
        }
        let do_gzip = match gzip {
            Gzip::Always => true,
            Gzip::Never => false,
            Gzip::Auto => is_compressible(name),
        };
        // Gzipped members carry the `.gz` suffix because the mzPeakViewer consumer keys its
        // gunzip-on-download feature on that suffix (per the compliance handoff) — interop with the
        // real reader beats the theoretical no-suffix choice. Rare name collisions (a real `foo.gz`
        // vs a gzipped `foo`) are detected and skipped rather than silently overwriting.
        let member = if do_gzip { format!("vendor/{rel}.gz") } else { format!("vendor/{rel}") };
        if !seen.insert(member.clone()) {
            log::warn!("vendor: skipping {rel} (member-name collision on {member})");
            manifest.push(entry(&rel, "error", "identity", src_bytes));
            continue;
        }
        // Open is non-fatal (missing/unreadable → recorded + skipped, no member started). A failure
        // DURING streaming would leave a truncated member in a finished archive, so it is FATAL
        // (propagated) — the outer convert path then never renames the temp output into place.
        let f = match File::open(&abs) {
            Ok(f) => f,
            Err(e) => {
                log::warn!("vendor: skipping {rel} (open failed: {e})");
                manifest.push(entry(&rel, "error", "identity", src_bytes));
                continue;
            }
        };
        let encoding = if do_gzip { "gzip" } else { "identity" };
        // Declared as a `proprietary` FileEntry so it lands in the index `files[]` — the viewer
        // surfaces proprietary members in its Structure inspector and the validator skips them
        // (they are not parsed as Parquet).
        let fe = FileEntry::new(member.clone(), EntityType::Other("vendor".into()), DataKind::Proprietary);
        if do_gzip {
            let mut enc = GzEncoder::new(BufReader::new(f), Compression::default());
            zip.add_file_from_read(&mut enc, None::<&String>, Some(fe))
                .with_context(|| format!("streaming {member} (gzip) — archive may be partial"))?;
        } else {
            let mut r = BufReader::new(f);
            zip.add_file_from_read(&mut r, None::<&String>, Some(fe))
                .with_context(|| format!("streaming {member} — archive may be partial"))?;
        }
        log::debug!("vendor: embed {member} ({encoding})");
        manifest.push(entry(&member, "embed", encoding, src_bytes));
    }

    zip.add_index_metadata("vendor_files", &manifest).context("writing vendor_files index")?;

    if let Some(meta) = read_global_metadata(dot_d) {
        zip.add_index_metadata("vendor_metadata", &meta).context("writing vendor_metadata index")?;
    }
    Ok(())
}

/// The SQLite that describes this `.d`: `analysis.tdf` when it holds data, else `analysis.tsf`.
///
/// Bruker writes an EMPTY `analysis.tsf` beside every `analysis.tdf` (and 0-byte `analysis.tdf`
/// stubs turned up in the corpus next to real BAF/TSF data). The old rule — the first name that
/// EXISTS — picked the stub, every query failed silently, and PXD076703 was published with a null
/// start time, an empty instrument and no `vendor_metadata` block although its `.tdf` states the
/// model, serial and a zoned timestamp. Non-empty, in the order the converter routes the lanes.
pub(crate) fn bruker_sqlite(dot_d: &Path) -> Option<std::path::PathBuf> {
    ["analysis.tdf", "analysis.tsf"]
        .iter()
        .map(|n| dot_d.join(n))
        .find(|p| std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.len() > 0))
}

/// What a Bruker `.d` states about its run, from `GlobalMetadata` (TDF/TSF) — shared by every
/// Bruker lane. Everything is optional; nothing the file does not state is asserted (no ion source,
/// no detector: `InstrumentSourceType` is an opaque code whose legend differs between files).
pub(crate) fn bruker_run_metadata(dot_d: &Path) -> Option<crate::run_metadata::VendorRunMetadata> {
    use crate::run_metadata::{parse_vendor_time, source_files_from_members, term, term_str, MemberPolicy, Members, VendorRunMetadata};
    use mzdata::meta::{Component, ComponentType, InstrumentConfiguration, Sample, Software};

    let sql = bruker_sqlite(dot_d)?;
    let meta = read_global_metadata(dot_d)?;
    let get = |k: &str| meta.get(k).and_then(|v| v.as_str()).map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned);
    let mut out = VendorRunMetadata::default();

    if let Some(model) = get("InstrumentName") {
        let mut cfg = InstrumentConfiguration { id: 0, ..Default::default() };
        // The series term ProteoWizard uses for every timsTOF, plus the vendor's own model string.
        if model.to_ascii_lowercase().contains("timstof") {
            cfg.params.push(term(1003123, "Bruker Daltonics timsTOF series"));
        }
        cfg.params.push(term_str(1000031, "instrument model", &model));
        if let Some(serial) = get("InstrumentSerialNumber") {
            cfg.params.push(term_str(1000529, "instrument serial number", &serial));
        }
        // A timsTOF is a TOF: the one component that is not in doubt (the existing rule).
        cfg.components.push(Component {
            component_type: ComponentType::Analyzer,
            order: 1,
            params: vec![term(1000084, "time-of-flight")],
        });
        out.instrument = Some(cfg);
    }
    if let Some(t) = get("AcquisitionDateTime") {
        match parse_vendor_time(&t, "Bruker GlobalMetadata AcquisitionDateTime") {
            Ok(at) => out.start_time = Some(at),
            Err(e) => log::warn!("{e}"),
        }
    }
    if let Some(name) = get("AcquisitionSoftware") {
        let version = get("AcquisitionSoftwareVersion").unwrap_or_else(|| "unknown".to_string());
        out.acquisition_software = Some(Software::new(name, version, vec![term(1000692, "Bruker software")]));
    }
    if let Some(sample) = get("SampleName") {
        out.samples.push(Sample::new("sample_1".to_string(), Some(sample), vec![]));
    }
    let is_tdf = sql.file_name().is_some_and(|n| n == "analysis.tdf");
    let (members, fmt, idfmt): (&[&str], _, _) = if is_tdf {
        (&["analysis.tdf", "analysis.tdf_bin"], term(1002817, "Bruker TDF format"), term(1002818, "Bruker TDF nativeID format"))
    } else {
        (&["analysis.tsf", "analysis.tsf_bin"], term(1003282, "Bruker TSF format"), term(1003283, "Bruker TSF nativeID format"))
    };
    let (files, default) = source_files_from_members(
        dot_d,
        &MemberPolicy { members: Members::Explicit(members), file_format: Some(fmt), id_format: Some(idfmt), default_member: Some(members[0]) },
    );
    out.source_files = files;
    out.default_source_file = default;
    Some(out)
}

/// The source members of a Bruker BAF `.d` (`analysis.baf` with its `_idx` and `_xtr` siblings),
/// each with its MS:1000569 SHA-1 — readable on any host, although the BAF reader itself is not.
/// `None` when the directory holds no non-empty `analysis.baf`. Through 0.11.5 the BAF lane named
/// only the `.d` and carried no digest.
pub(crate) fn bruker_baf_members(dot_d: &Path) -> Option<crate::run_metadata::VendorRunMetadata> {
    use crate::run_metadata::{source_files_from_members, term, MemberPolicy, Members, VendorRunMetadata};

    if !std::fs::metadata(dot_d.join("analysis.baf")).is_ok_and(|m| m.is_file() && m.len() > 0) {
        return None;
    }
    const MEMBERS: &[&str] = &["analysis.baf", "analysis.baf_idx", "analysis.baf_xtr"];
    let (files, default) = source_files_from_members(
        dot_d,
        &MemberPolicy {
            members: Members::Explicit(MEMBERS),
            file_format: Some(term(1000815, "Bruker BAF format")),
            id_format: Some(term(1000772, "Bruker BAF nativeID format")),
            default_member: Some(MEMBERS[0]),
        },
    );
    Some(VendorRunMetadata { source_files: files, default_source_file: default, ..Default::default() })
}

/// What a baf2sql cache's `Properties` table (key → value) states about the run: the instrument,
/// its serial, the acquisition software and the acquisition time. The keys are the ones
/// ProteoWizard reads (`Baf2Sql.cpp`). The model is the PSI-MS series term ProteoWizard's
/// `translateAsInstrumentSeries` gives the vendor's `InstrumentFamily` code (`CompassDataEnums.hpp`),
/// and the generic Bruker model term for a code it does not list — nothing is inferred from a method
/// or file name. An empty table states nothing, and the configuration is left as it is.
// Its caller, the BAF reader, builds on Windows and Linux only; the tests here run everywhere.
#[cfg_attr(not(any(windows, target_os = "linux")), allow(dead_code))]
pub(crate) fn baf_properties_metadata(
    props: &std::collections::BTreeMap<String, String>,
) -> crate::run_metadata::VendorRunMetadata {
    use crate::run_metadata::{parse_vendor_time, term, term_str, VendorRunMetadata};
    use mzdata::meta::{InstrumentConfiguration, Software};

    let get = |k: &str| props.get(k).map(|v| v.trim()).filter(|v| !v.is_empty());
    let mut out = VendorRunMetadata::default();
    if props.is_empty() {
        return out;
    }
    // Every BAF file is a Bruker acquisition, so the generic term is a fact even without a family;
    // a serial must never stand alone either, or the configuration would carry no model term at all.
    let (accession, name) = match get("InstrumentFamily").and_then(|v| v.parse::<i64>().ok()) {
        Some(0) => (1000697, "Bruker Daltonics HCT Series"),
        Some(1 | 2) => (1001536, "Bruker Daltonics micrOTOF series"),
        Some(3 | 4) => (1001535, "Bruker Daltonics BioTOF series"),
        Some(5) => (1001534, "Bruker Daltonics flex series"),
        Some(6) => (1001556, "Bruker Daltonics apex series"),
        Some(7 | 90 | 91) => (1001547, "Bruker Daltonics maXis series"),
        Some(9) => (1003123, "Bruker Daltonics timsTOF series"),
        Some(92) => (1001548, "Bruker Daltonics solarix series"),
        _ => (1000122, "Bruker Daltonics instrument model"),
    };
    let mut cfg = InstrumentConfiguration { id: 0, ..Default::default() };
    cfg.params.push(term(accession, name));
    if let Some(serial) = get("InstrumentSerialNumber") {
        cfg.params.push(term_str(1000529, "instrument serial number", serial));
    }
    out.instrument = Some(cfg);
    if let Some(name) = get("AcquisitionSoftware") {
        let version = get("AcquisitionSoftwareVersion").unwrap_or("unknown");
        out.acquisition_software = Some(Software::new(name.to_string(), version.to_string(), vec![term(1000692, "Bruker software")]));
    }
    if let Some(t) = get("AcquisitionDateTime") {
        match parse_vendor_time(t, "Bruker BAF Properties AcquisitionDateTime") {
            Ok(at) => out.start_time = Some(at),
            Err(e) => log::warn!("{e}"),
        }
    }
    out
}

/// Read run-level `GlobalMetadata` (key/value) from a TSF or TDF SQLite, as a JSON object.
pub(crate) fn read_global_metadata(dot_d: &Path) -> Option<serde_json::Value> {
    let sql = bruker_sqlite(dot_d)?;
    let conn = Connection::open_with_flags(&sql, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).ok()?;
    let mut stmt = conn.prepare("SELECT Key, Value FROM GlobalMetadata").ok()?;
    let rows = stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        .ok()?;
    let mut map = serde_json::Map::new();
    for row in rows.flatten() {
        map.insert(row.0, serde_json::Value::String(row.1));
    }
    if map.is_empty() { None } else { Some(serde_json::Value::Object(map)) }
}

/// Recursively collect REGULAR-file paths relative to `root`, NOT following symlinks (a symlink in
/// the `.d` could otherwise pull in files outside the tree). Each path is validated to be a safe
/// relative member name (only normal components — no `..`, root, prefix, or control/NUL chars);
/// unsafe entries are skipped with a warning.
fn collect_files(root: &Path, dir: &Path, out: &mut Vec<String>) -> Result<()> {
    for e in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let e = e?;
        let p = e.path();
        // symlink_metadata does NOT traverse symlinks → a symlinked dir/file is treated as a link
        // (neither recursed nor embedded).
        let md = std::fs::symlink_metadata(&p)?;
        if md.file_type().is_symlink() {
            log::warn!("vendor: skipping symlink {}", p.display());
            continue;
        }
        if md.is_dir() {
            collect_files(root, &p, out)?;
        } else if md.is_file() {
            let Ok(rel) = p.strip_prefix(root) else { continue };
            match safe_relative_member(rel) {
                Some(s) => out.push(s),
                None => log::warn!("vendor: skipping unsafe path {}", rel.display()),
            }
        }
    }
    Ok(())
}

/// Validate `rel` as a safe ZIP member suffix: every component must be `Normal`, UTF-8, and free of
/// path separators / control chars. Returns the `/`-joined name, or `None` if unsafe.
fn safe_relative_member(rel: &Path) -> Option<String> {
    use std::path::Component;
    let mut parts = Vec::new();
    for c in rel.components() {
        match c {
            Component::Normal(os) => {
                let s = os.to_str()?; // reject non-UTF-8 (avoids lossy collisions)
                if s.is_empty() || s.contains(['/', '\\']) || s.chars().any(|ch| ch.is_control()) {
                    return None;
                }
                parts.push(s.to_string());
            }
            _ => return None, // ParentDir / RootDir / Prefix / CurDir → reject
        }
    }
    if parts.is_empty() { None } else { Some(parts.join("/")) }
}

fn is_compressible(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    [".xml", ".txt", ".method", ".tsf", ".tdf", ".json", ".csv", ".sqlite", ".cfg", ".ini", ".log"]
        .iter()
        .any(|ext| lower.ends_with(ext))
}

/// Case-insensitive glob (`*` any run, `?` single char). Iterative two-pointer with backtracking —
/// O(pattern × name), so a user-supplied pattern with many `*` cannot cause exponential blow-up.
fn glob_match(pat: &str, name: &str) -> bool {
    let p: Vec<char> = pat.to_ascii_lowercase().chars().collect();
    let s: Vec<char> = name.to_ascii_lowercase().chars().collect();
    let (mut pi, mut si) = (0usize, 0usize);
    let (mut star, mut mark) = (None::<usize>, 0usize);
    while si < s.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == s[si]) {
            pi += 1;
            si += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = si;
            pi += 1;
        } else if let Some(st) = star {
            pi = st + 1;
            mark += 1;
            si = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn glob_and_policy() {
        assert!(glob_match("*_bin", "analysis.tsf_bin"));
        assert!(glob_match("*.method", "Hystar.Method"));
        assert!(!glob_match("*_bin", "analysis.tsf"));
        assert!(glob_match("a*b*c", "axxbyyc")); // multi-star, no blow-up
        assert!(!glob_match("a*b*c", "axxbyy"));
        // preserve-by-default: nothing dropped unless asked
        let pol = VendorPolicy::builtin();
        assert_eq!(pol.resolve("analysis.tdf_bin").0, Action::Embed);
        assert_eq!(pol.resolve("analysis.tsf").0, Action::Embed);
        // override opts into a drop and wins over the default
        let pol = VendorPolicy::load(None, &["*_bin=drop".to_string()]).unwrap();
        assert_eq!(pol.resolve("analysis.tsf_bin").0, Action::Drop);
        assert_eq!(pol.resolve("analysis.tsf").0, Action::Embed);
    }

    #[test]
    fn lossless_drops_bulk_bin_by_default() {
        // ims-compact path: the raw `*_bin` is redundant with the Parquet facet → DROP by default,
        // but the SQLite metadata and other side-files are still embedded.
        let pol = VendorPolicy::load_lossless(None, &[]).unwrap();
        assert_eq!(pol.resolve("analysis.tdf_bin").0, Action::Drop);
        assert_eq!(pol.resolve("analysis.tsf_bin").0, Action::Drop);
        assert_eq!(pol.resolve("analysis.tdf").0, Action::Embed);
        assert_eq!(pol.resolve("Hystar.Method").0, Action::Embed);
        // explicit user override to keep the bulk binary still wins over the lossless default
        let pol = VendorPolicy::load_lossless(None, &["analysis.tdf_bin=embed".to_string()]).unwrap();
        assert_eq!(pol.resolve("analysis.tdf_bin").0, Action::Embed);
    }

    #[test]
    fn rejects_unsafe_paths() {
        use std::path::Path;
        assert!(safe_relative_member(Path::new("808.m/Maldi.method")).is_some());
        assert!(safe_relative_member(Path::new("../escape")).is_none());
        assert!(safe_relative_member(Path::new("/abs/path")).is_none());
    }

    #[test]
    fn baf_directory_members_are_digested() {
        let dir = std::env::temp_dir().join(format!("mzpc-baf-members-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // A 0-byte `analysis.baf` is no BAF run (NreB_PAS_DECONV.d holds a 0-byte `analysis.tdf`).
        std::fs::write(dir.join("analysis.baf"), b"").unwrap();
        assert!(bruker_baf_members(&dir).is_none());
        for (name, body) in [
            ("analysis.baf", &b"baf"[..]),
            ("analysis.baf_idx", b"idx"),
            ("analysis.baf_xtr", b"xtr"),
            ("SampleInfo.xml", b"<SampleTable/>"),
        ] {
            std::fs::write(dir.join(name), body).unwrap();
        }
        let m = bruker_baf_members(&dir).expect("a non-empty analysis.baf");
        let names: Vec<&str> = m.source_files.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["analysis.baf", "analysis.baf_idx", "analysis.baf_xtr"], "the three members, nothing else");
        for sf in &m.source_files {
            let sha = sf.params.iter().find(|p| p.accession == Some(1000569)).expect("every member digested");
            assert_eq!(sha.value.to_string(), crate::embed_aux::sha1_hex(&dir.join(&sf.name)).unwrap());
            assert_eq!(sf.file_format.as_ref().map(|p| (p.accession, p.name.as_str())), Some((Some(1000815), "Bruker BAF format")));
            assert_eq!(sf.id_format.as_ref().map(|p| (p.accession, p.name.as_str())), Some((Some(1000772), "Bruker BAF nativeID format")));
        }
        assert_eq!(m.default_source_file.as_deref(), Some("analysis.baf"));
        assert!(m.instrument.is_none() && m.start_time.is_none(), "the directory states no run facts; the baf2sql cache does");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn baf_properties_state_the_series_their_family_code_names() {
        use crate::run_metadata::{AcquisitionTime, VendorRunMetadata};
        let props = |pairs: &[(&str, &str)]| -> std::collections::BTreeMap<String, String> {
            pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
        };
        let model = |m: &VendorRunMetadata| {
            m.instrument.as_ref().map(|c| c.params.iter().map(|p| (p.accession, p.name.clone())).collect::<Vec<_>>())
        };
        // An impact II (family 90) is ProteoWizard's maXis series.
        let m = baf_properties_metadata(&props(&[
            ("InstrumentFamily", "90"),
            ("InstrumentSerialNumber", "1825265.10252"),
            ("AcquisitionSoftware", "otofControl"),
            ("AcquisitionSoftwareVersion", "5.2.109"),
            ("AcquisitionDateTime", "2024-10-09T09:09:26.123-03:00"),
        ]));
        assert_eq!(
            model(&m),
            Some(vec![
                (Some(1001547), "Bruker Daltonics maXis series".to_string()),
                (Some(1000529), "instrument serial number".to_string()),
            ])
        );
        assert_eq!(m.instrument.as_ref().unwrap().params[1].value.to_string(), "1825265.10252");
        let sw = m.acquisition_software.as_ref().unwrap();
        assert_eq!((sw.id.as_str(), sw.version.as_str()), ("otofControl", "5.2.109"));
        assert!(matches!(&m.start_time, Some(AcquisitionTime::Stated(t)) if t.offset().local_minus_utc() == -3 * 3600));
        // solariX and timsTOF have their own series; an unlisted code, or a serial with no family,
        // gets the generic Bruker model term — never a configuration without a model term.
        let first = |pairs: &[(&str, &str)]| model(&baf_properties_metadata(&props(pairs))).unwrap()[0].0;
        assert_eq!(first(&[("InstrumentFamily", "92")]), Some(1001548));
        assert_eq!(first(&[("InstrumentFamily", "9")]), Some(1003123));
        assert_eq!(first(&[("InstrumentFamily", "42")]), Some(1000122));
        assert_eq!(first(&[("InstrumentSerialNumber", "7")]), Some(1000122));
        // An unzoned clock stays naive; an empty table states nothing.
        let naive = baf_properties_metadata(&props(&[("AcquisitionDateTime", "2024-10-09T09:09:26")]));
        assert!(matches!(naive.start_time, Some(AcquisitionTime::Naive { .. })));
        let empty = baf_properties_metadata(&props(&[]));
        assert!(empty.instrument.is_none() && empty.start_time.is_none() && empty.acquisition_software.is_none());
    }
}

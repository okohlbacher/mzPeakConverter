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
    /// Built-in **preserve-by-default** policy: embed every side-file (gzip compressible types) but the
    /// files below, each dropped by its name in any letter case and recorded in `vendor_files`. An
    /// `--aux glob=action` rule (or a YAML policy) comes first, so `--aux '<glob>=embed'` keeps one.
    ///
    /// * **The raw signal files of BAF, Agilent MassHunter and Waters MassLynx directories.** They are
    ///   nearly all of such a directory and several times its archive (FM_1-1: `analysis.baf` is
    ///   714 MB beside a 109 MB archive; Capan2: 1.1 GB of `_FUNC*.DAT` and `_func*.cdt` beside
    ///   531 MB). Through 0.11.5 no default archive of those lanes embedded them, `--agilent-grid`
    ///   aside, which stores that profile signal itself; one embed rule for every vendor directory
    ///   must not grow them by the vendor file's size. What they hold beyond the archive — the BAF
    ///   profile unless `--representation profile`, the MassHunter representation a lane did not
    ///   read, the Waters functions not written as spectra — stays with the original directory, or
    ///   in the archive on request. What describes the run is embedded: the Agilent scan records
    ///   (`MSScan.bin`, whose MSn precursor fields no lane decodes yet) and mass calibration, device
    ///   traces, DataAnalysis `.mcf` result containers, the Waters scan statistics (`_FUNC*.STS`),
    ///   analog traces (`_CHRO*`) and `_mob/` projections.
    /// * **baf2sql's `analysis.sqlite`**: the BAF reader has the library materialize that cache next
    ///   to `analysis.baf`, inside the `.d`, when it opens the run (`bruker_baf.rs`), so it is this
    ///   converter's by-product, not a file the vendor wrote.
    ///
    /// The timsTOF `*_bin` is embedded here, as through 0.11.5: for the f64 paths (mzdata's TDF
    /// reader, the TSF reader, the Bruker SDK) it is, beside the embedded `analysis.tdf` or
    /// `analysis.tsf`, the exact copy of a signal they store as calibrated f64 m/z, in a format open
    /// readers decode (timsrust a TDF, this converter a TSF). The LOSSLESS ims-compact path encodes that
    /// exact integer-TOF + intensity signal into the Parquet peak facet, so the raw `*_bin` bulk file
    /// is fully redundant there (it was ~39% of the archive, a verbatim copy); that path uses
    /// [`load_lossless`](Self::load_lossless), which drops it too. SQLite rollback journals stay: they
    /// can be needed to recover a database snapshot.
    pub fn builtin() -> Self {
        const DROP: &[&str] = &[
            "analysis.sqlite",
            // Bruker BAF: the signal and its two indexes, DataAnalysis's cached views, FTMS transients.
            "analysis.baf",
            "analysis.baf_idx",
            "analysis.baf_xtr",
            "*.ami",
            "ser",
            "fid",
            // Agilent MassHunter: the profile, centroid and ion-mobility frame signal.
            "MSProfile.bin",
            "MSPeak.bin",
            "IMSFrame.bin",
            // Waters MassLynx: each function's scans and their index, and the compressed ion-mobility
            // data (`_func001.cdt` beside `_FUNC001.DAT` in one directory; matching ignores case).
            "_FUNC*.DAT",
            "_FUNC*.IDX",
            "_FUNC*.CDT",
            "_FUNC*.IND",
        ];
        let rule = |pat: &str, action| Rule { pat: pat.to_string(), action, gzip: Gzip::Auto };
        let mut rules: Vec<Rule> = DROP.iter().map(|pat| rule(pat, Action::Drop)).collect();
        rules.push(rule("*", Action::Embed));
        VendorPolicy { rules }
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

    /// The first rule whose glob matches the member at `rel`, its `/`-joined path inside the
    /// directory: by its file name, or by that path. Rules were matched against the file name alone,
    /// so `--aux 'AcqData/MSProfile.bin=drop'`, the spelling the manual gave, matched nothing.
    fn resolve(&self, rel: &str) -> (Action, Gzip) {
        let name = rel.rsplit('/').next().unwrap_or(rel);
        for r in &self.rules {
            if glob_match(&r.pat, name) || glob_match(&r.pat, rel) {
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
        let (action, gzip) = policy.resolve(&rel);
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
/// ProteoWizard reads (`Baf2Sql.cpp`). The model is the PSI-MS series term ProteoWizard arrives at
/// for the raw `InstrumentFamily` code: `translateInstrumentFamily` (`Baf2Sql.cpp`) turns the code
/// into a family, then `translateAsInstrumentSeries` (`Reader_Bruker_Detail.cpp`) the family into a
/// series. Any other code, or none, gets the generic Bruker model term — nothing is inferred from a
/// method or file name.
// Its caller, the BAF reader, builds on Windows and Linux only; the tests here run everywhere.
#[cfg_attr(not(any(windows, target_os = "linux")), allow(dead_code))]
pub(crate) fn baf_properties_metadata(
    props: &std::collections::BTreeMap<String, String>,
) -> crate::run_metadata::VendorRunMetadata {
    use crate::run_metadata::{parse_vendor_time, term, term_str, VendorRunMetadata};
    use mzdata::meta::{InstrumentConfiguration, Software};

    let get = |k: &str| props.get(k).map(|v| v.trim()).filter(|v| !v.is_empty());
    let mut out = VendorRunMetadata::default();
    // Every BAF file is a Bruker acquisition, so the generic term is a fact even for an empty or
    // unreadable table: a configuration without a model term gets the writer's valueless MS:1000031.
    // A serial never stands alone either. The raw code is not a `CompassDataEnums` value, so it goes
    // through `translateInstrumentFamily`'s cases first; every code that function does not list is
    // `InstrumentFamily_Unknown`, which `translateAsInstrumentSeries` makes the generic term.
    let (accession, name) = match get("InstrumentFamily").and_then(|v| v.parse::<i64>().ok()) {
        Some(1 | 2) => (1001536, "Bruker Daltonics micrOTOF series"), // OTOF, OTOFQ
        Some(6..=8) => (1001547, "Bruker Daltonics maXis series"),    // maXis, impact, compact
        Some(512) => (1001556, "Bruker Daltonics apex series"),       // FTMS
        Some(513) => (1001548, "Bruker Daltonics solarix series"),    // solariX
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

    /// The raw signal files of a BAF, Agilent MassHunter or Waters MassLynx directory, and the
    /// baf2sql cache the BAF reader writes into the `.d`, are dropped by default: matched on the file
    /// name in any letter case (one Waters `.raw` holds `_FUNC001.DAT` beside `_func001.cdt`), wherever
    /// the file sits. What describes the run stays, the Waters analog traces and the Agilent scan
    /// records among it. An `--aux` rule wins, spelt as the file name or as the path in the directory.
    #[test]
    fn vendor_signal_files_are_dropped_unless_asked_for() {
        for pol in [VendorPolicy::load(None, &[]).unwrap(), VendorPolicy::load_lossless(None, &[]).unwrap()] {
            for dropped in [
                "analysis.sqlite",
                "analysis.baf",
                "analysis.baf_idx",
                "analysis.baf_xtr",
                "BackgroundProfNeg.ami",
                "ser",
                "fid",
                "AcqData/MSProfile.bin",
                "AcqData/MSPeak.bin",
                "AcqData/IMSFrame.bin",
                "_FUNC001.DAT",
                "_FUNC001.IDX",
                "_func001.cdt",
                "_func001.ind",
                "_FUNC010.CDT",
            ] {
                assert_eq!(pol.resolve(dropped).0, Action::Drop, "{dropped}");
            }
            for kept in [
                "analysis.tdf",
                "analysis.tsf",
                "SampleInfo.xml",
                "037df41c-54d6-4ec4-a91a-893bcb5caf81_1.mcf",
                "AcqData/MSScan.bin",
                "AcqData/MSMassCal.bin",
                "AcqData/BinPump1.cg",
                "_FUNC001.STS",
                "_CHRO001.DAT",
                "_CHROMS.INF",
                "_FUNCTNS.INF",
                "_mob/729441462.1dMZ",
                "Hystar.Method",
            ] {
                assert_eq!(pol.resolve(kept).0, Action::Embed, "{kept}");
            }
        }
        let rules = ["analysis.sqlite=embed", "MSProfile.bin=embed", "AcqData/MSPeak.bin=embed", "AcqData/MSScan.bin=drop"];
        let asked = VendorPolicy::load(None, &rules.map(String::from)).unwrap();
        assert_eq!(asked.resolve("analysis.sqlite").0, Action::Embed);
        assert_eq!(asked.resolve("AcqData/MSProfile.bin").0, Action::Embed, "a file-name rule");
        assert_eq!(asked.resolve("AcqData/MSPeak.bin").0, Action::Embed, "a path rule");
        assert_eq!(asked.resolve("AcqData/MSScan.bin").0, Action::Drop);
        assert_eq!(asked.resolve("Other/MSScan.bin").0, Action::Embed, "a path rule matches that path only");
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
        // An impact II (raw family 7, FM_1-1's instrument) is ProteoWizard's maXis series.
        let m = baf_properties_metadata(&props(&[
            ("InstrumentFamily", "7"),
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
        // The raw code goes through `translateInstrumentFamily` before the series table: 6 is a
        // maXis (not an FTMS), 8 a compact, 1 and 2 the micrOTOF line, 512 an FTMS (apex series),
        // 513 a solariX. A code it does not list (0, 9, 90, 92, …), or a serial with no family, is
        // `InstrumentFamily_Unknown`: the generic Bruker model term, never no model term at all.
        let first = |pairs: &[(&str, &str)]| model(&baf_properties_metadata(&props(pairs))).unwrap()[0].0;
        assert_eq!(first(&[("InstrumentFamily", "6")]), Some(1001547));
        assert_eq!(first(&[("InstrumentFamily", "8")]), Some(1001547));
        assert_eq!(first(&[("InstrumentFamily", "1")]), Some(1001536));
        assert_eq!(first(&[("InstrumentFamily", "2")]), Some(1001536));
        assert_eq!(first(&[("InstrumentFamily", "512")]), Some(1001556));
        assert_eq!(first(&[("InstrumentFamily", "513")]), Some(1001548));
        for unlisted in ["0", "3", "5", "9", "42", "90", "92"] {
            assert_eq!(first(&[("InstrumentFamily", unlisted)]), Some(1000122), "family {unlisted}");
        }
        assert_eq!(first(&[("InstrumentSerialNumber", "7")]), Some(1000122));
        // An unzoned clock stays naive. An empty (or unreadable) table states only what every BAF
        // file is, a Bruker instrument: no software, no time.
        let naive = baf_properties_metadata(&props(&[("AcquisitionDateTime", "2024-10-09T09:09:26")]));
        assert!(matches!(naive.start_time, Some(AcquisitionTime::Naive { .. })));
        let empty = baf_properties_metadata(&props(&[]));
        assert_eq!(model(&empty), Some(vec![(Some(1000122), "Bruker Daltonics instrument model".to_string())]));
        assert!(empty.start_time.is_none() && empty.acquisition_software.is_none());
    }
}

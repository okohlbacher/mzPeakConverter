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
    /// Rationale (codex review): the TDF path still reads via mzdata (lossy m/z/intensity) until
    /// ims-compact is the in-archive facet, so silently dropping `analysis.tdf_bin` would lose the
    /// only exact signal; and SQLite rollback journals can be needed to recover a DB snapshot. The
    /// converter's job is to ADD the mzPeak facets, not to decide the raw data is disposable.
    /// (To shrink archives once a path is proven lossless: `--aux 'analysis.tsf_bin=drop'`.)
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

/// Read run-level `GlobalMetadata` (key/value) from a TSF or TDF SQLite, as a JSON object.
fn read_global_metadata(dot_d: &Path) -> Option<serde_json::Value> {
    let sql = ["analysis.tsf", "analysis.tdf"]
        .iter()
        .map(|n| dot_d.join(n))
        .find(|p| p.exists())?;
    let conn = Connection::open(&sql).ok()?;
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
    fn rejects_unsafe_paths() {
        use std::path::Path;
        assert!(safe_relative_member(Path::new("808.m/Maldi.method")).is_some());
        assert!(safe_relative_member(Path::new("../escape")).is_none());
        assert!(safe_relative_member(Path::new("/abs/path")).is_none());
    }
}

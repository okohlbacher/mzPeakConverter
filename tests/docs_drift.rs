//! The user manual against what it documents: every long option the binary's `--help` prints is in
//! §4, every `FileConfig` key in §5's example, and every `"MZPC_…"` variable name quoted in `src/`,
//! the vendored writer or the .NET glue in §10. The manual was regenerated from the binary once, in
//! 0.9.13; by 0.11.5 §10 lacked two Waters levers and claimed a variable count that no longer held,
//! and nothing noticed.

use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;

fn root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

/// `## <n>. …` of docs/USER_MANUAL.md, up to the next `## ` heading.
fn manual_section(n: u32) -> String {
    let text = std::fs::read_to_string(root().join("docs/USER_MANUAL.md")).unwrap();
    let start = text
        .find(&format!("\n## {n}. "))
        .map(|i| i + 1)
        .unwrap_or_else(|| panic!("docs/USER_MANUAL.md has no `## {n}.` section"));
    let end = text[start..].find("\n## ").map_or(text.len(), |e| start + e);
    text[start..end].to_string()
}

/// `word` occurs in `text` with no identifier character on either side, so `--to` is not found
/// inside `--tof-grid`, nor a variable inside a longer one.
fn mentions(text: &str, word: &str) -> bool {
    let ident = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '-';
    text.match_indices(word).any(|(i, _)| {
        !text[..i].chars().next_back().is_some_and(ident)
            && !text[i + word.len()..].chars().next().is_some_and(ident)
    })
}

#[test]
fn every_long_option_of_help_is_in_section_4() {
    let out = Command::new(env!("CARGO_BIN_EXE_mzpeak-convert")).arg("--help").output().unwrap();
    assert!(out.status.success(), "--help failed");
    let help = String::from_utf8(out.stdout).unwrap();
    // clap starts an option line within six columns (`  -o, --output`, `      --layout`); the
    // description lines below it are indented further. A counted flag prints as `--verbose...`.
    let flags: BTreeSet<&str> = help
        .lines()
        .filter(|l| l.len() - l.trim_start().len() <= 6 && l.trim_start().starts_with('-'))
        .flat_map(|l| l.split(|c: char| c.is_whitespace() || c == ','))
        .filter(|t| t.starts_with("--") && t.len() > 2)
        .map(|t| &t[..t[2..].find(|c: char| !(c.is_ascii_alphanumeric() || c == '-')).map_or(t.len(), |e| e + 2)])
        .collect();
    assert!(flags.contains("--output") && flags.contains("--zstd-level"), "no options parsed from --help:\n{help}");
    let section = manual_section(4);
    let missing: Vec<_> = flags.iter().filter(|f| !mentions(&section, f)).collect();
    assert!(missing.is_empty(), "options `--help` prints but USER_MANUAL.md §4 does not: {missing:?}");
}

#[test]
fn every_config_key_is_in_section_5() {
    let main = std::fs::read_to_string(root().join("src/main.rs")).unwrap();
    let body = main.split_once("struct FileConfig {").expect("src/main.rs has no `struct FileConfig`").1;
    let body = &body[..body.find("\n}").unwrap()];
    let keys: Vec<&str> = body
        .lines()
        .map(str::trim)
        .filter(|l| !l.starts_with("//") && !l.starts_with('#'))
        .filter_map(|l| l.split_once(':').map(|(k, _)| k))
        .filter(|k| !k.is_empty() && k.chars().all(|c| c.is_ascii_lowercase() || c == '_'))
        .collect();
    assert!(keys.contains(&"zstd_level"), "no fields parsed from FileConfig");
    let section = manual_section(5);
    let missing: Vec<_> = keys
        .iter()
        .filter(|k| !section.lines().any(|l| l.trim_start().starts_with(&format!("{k}:"))))
        .collect();
    assert!(missing.is_empty(), "FileConfig keys missing from USER_MANUAL.md §5's example: {missing:?}");
}

/// Every `"MZPC_…"` name quoted under `dir`. Build output is skipped, and so are `MZPC_TEST_…`
/// names, which unit tests set on their own process.
fn quoted_mzpc_names(dir: &Path, names: &mut BTreeSet<String>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            if !matches!(path.file_name().and_then(|n| n.to_str()), Some("bin" | "obj" | "target")) {
                quoted_mzpc_names(&path, names);
            }
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        let mut rest = text.as_str();
        while let Some(i) = rest.find("\"MZPC_") {
            let tail = &rest[i + 1..];
            let len = tail
                .find(|c: char| !(c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_'))
                .unwrap_or(tail.len());
            if tail[len..].starts_with('"') && !tail.starts_with("MZPC_TEST_") {
                names.insert(tail[..len].to_string());
            }
            rest = &tail[len..];
        }
    }
}

#[test]
fn every_quoted_mzpc_variable_is_in_section_10() {
    let mut names = BTreeSet::new();
    for dir in ["src", "vendor", "glue"] {
        quoted_mzpc_names(&root().join(dir), &mut names);
    }
    assert!(names.contains("MZPC_PWIZ_DIR"), "no MZPC_ names found under src/, vendor/ and glue/");
    let section = manual_section(10);
    let missing: Vec<_> = names.iter().filter(|n| !mentions(&section, n)).collect();
    assert!(missing.is_empty(), "MZPC_ variables the tree reads but USER_MANUAL.md §10 does not list: {missing:?}");
}

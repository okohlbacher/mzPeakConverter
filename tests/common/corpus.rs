//! Where the reference corpus lives, and what a test does when a fixture is not there.
//!
//! One implementation for both kinds of test. The crate has no library target, so the unit tests in
//! `src/` and the integration tests in `tests/` cannot import each other's items; each pulls this
//! file in with `#[path]` instead. Before it existed the gates disagreed — some read
//! `MZPEAK_CORPUS`, some only `$HOME`, some returned without a word — so a run pointed at a corpus
//! could still pass tests that had asserted nothing.
//!
//! * The root is `MZPEAK_CORPUS`, else `~/Claude/mzpeak-example-data/data`.
//! * A missing fixture SKIPS. The line goes straight to the process's stderr, not through
//!   `eprintln!`, so it reaches the log even when libtest captures a passing test's output.
//! * With `MZPC_REQUIRE_CORPUS=1` a missing fixture PANICS, so a run that is meant to have the
//!   corpus cannot go green on skips.
#![allow(dead_code)]

use std::io::Write;
use std::path::PathBuf;

pub fn corpus_root() -> PathBuf {
    std::env::var_os("MZPEAK_CORPUS").map(PathBuf::from).unwrap_or_else(|| {
        std::env::home_dir().unwrap_or_default().join("Claude/mzpeak-example-data/data")
    })
}

/// `corpus_root()/rel` when it exists; otherwise skip, or panic under `MZPC_REQUIRE_CORPUS=1`.
#[track_caller]
pub fn corpus_path(rel: &str) -> Option<PathBuf> {
    let p = corpus_root().join(rel);
    if p.exists() {
        return Some(p);
    }
    let at = std::panic::Location::caller();
    if std::env::var_os("MZPC_REQUIRE_CORPUS").is_some_and(|v| v == "1") {
        panic!("MZPC_REQUIRE_CORPUS=1 but the corpus fixture is missing: {} ({at})", p.display());
    }
    let _ = writeln!(std::io::stderr(), "SKIPPED {at}: corpus fixture {} not present", p.display());
    None
}

/// The path in `var`, for pins whose input no corpus holds (a Bruker TSF acquisition, lane pairs built
/// on the Windows box). Unset: skip, loudly, whatever `MZPC_REQUIRE_CORPUS` says — the corpus cannot
/// supply these. Set to a path that does not exist: panic, so a typo cannot pass as a skip.
#[track_caller]
pub fn env_path(var: &str) -> Option<PathBuf> {
    let at = std::panic::Location::caller();
    let Some(v) = std::env::var_os(var) else {
        let _ = writeln!(std::io::stderr(), "SKIPPED {at}: {var} is not set");
        return None;
    };
    let p = PathBuf::from(v);
    assert!(p.exists(), "{var}={} does not exist ({at})", p.display());
    Some(p)
}

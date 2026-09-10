//! SciEX glue pins: the Rust loader boots CoreCLR once per process, and it and the C# glue describe
//! ONE contract (fixture-free, host-independent; runs everywhere `cargo test` runs).
//!
//! WHY THIS EXISTS. `src/sciex.rs` compiles only on Windows, the glue only runs beside Clearcore2, and
//! nothing on a macOS/Linux host builds either half against the other. The pins read the SOURCE with
//! `include_str!` and compare it with plain string operations — no C# parser, no fixture, no Windows —
//! the pattern of `tests/shimadzu_abi_pin.rs`. They cannot see whether the glue was rebuilt or whether
//! Windows accepts the boot; they can see whether the code still has the shape that makes both work.

const RUST_RAW: &str = include_str!("../src/sciex.rs");

/// CRLF folded to LF: the Windows box checks this repo out with `core.autocrlf=true`, and every
/// `\n`-anchored match would otherwise miss there (see `tests/shimadzu_abi_pin.rs`).
fn rust() -> &'static str {
    static S: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    S.get_or_init(|| RUST_RAW.replace("\r\n", "\n"))
}

/// Strip a trailing `// …` comment and surrounding whitespace.
fn code_part(line: &str) -> &str {
    line.split("//").next().unwrap_or("").trim()
}

/// `src/sciex.rs` without comments, one trimmed line per source line.
fn rust_code() -> String {
    rust().lines().map(code_part).collect::<Vec<_>>().join("\n")
}

/// Every open booted CoreCLR, and hostfxr cannot be initialised again once the first handle has been
/// freed; `-v` opens the reader for the inspection report, drops it and opens it again, so every
/// verbose native SciEX conversion failed on Windows (the failure Shimadzu showed before 0446ea3).
/// Only a Windows run can show the boot in motion; this pins the shape that prevents the second one:
/// one process-wide cache, `::load(` called only inside `GlueApi::shared`, and the reader opening
/// through `shared`.
#[test]
fn the_glue_is_booted_once_per_process() {
    let code = rust_code();
    assert!(
        code.contains("static GLUE: OnceLock<Mutex<Option<GlueApi>>> = OnceLock::new();"),
        "sciex.rs: no process-wide `static GLUE` holding the loaded glue"
    );
    let shared = code.find("fn shared(").expect("sciex.rs: no `GlueApi::shared`");
    let shared_end = shared + 1 + code[shared + 1..].find("\nfn ").expect("sciex.rs: nothing follows `fn shared`");
    let loads: Vec<usize> = code.match_indices("::load(").map(|(i, _)| i).collect();
    assert_eq!(loads.len(), 1, "sciex.rs: `::load(` is called {} times; only `GlueApi::shared` may load the glue", loads.len());
    assert!(
        (shared..shared_end).contains(&loads[0]),
        "sciex.rs: `::load(` is called outside `GlueApi::shared`, so that caller boots CoreCLR again"
    );
    let open = code.find("pub fn open(").expect("sciex.rs: no `SciexReader::open`");
    let open_end = open + 1 + code[open + 1..].find("\npub fn ").expect("sciex.rs: nothing follows `SciexReader::open`");
    assert!(
        code[open..open_end].contains("GlueApi::shared("),
        "sciex.rs: `SciexReader::open` does not take the glue from `GlueApi::shared`"
    );
}

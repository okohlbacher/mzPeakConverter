//! Shimadzu glue ABI pin: the Rust loader and the C# glue must describe ONE contract (fixture-free,
//! host-independent; runs everywhere `cargo test` runs).
//!
//! WHY THIS EXISTS. `src/shimadzu.rs` only compiles on Windows, the glue DLL only builds with the
//! LabSolutions SDK, and the two meet at a hand-mirrored C ABI: a version literal on each side, two
//! `#[repr(C)]` / `[StructLayout]` struct twins, and a dozen `[UnmanagedCallersOnly]` exports the
//! Rust side resolves by name. Nothing in the build checks that the two files still agree — the
//! `const _: () = assert!(size_of…)` guards catch a Rust struct that drifts from ITSELF, and the C#
//! static ctor catches a C# struct that drifts from ITSELF, but a field renamed or reordered on one
//! side only, or an entry point renamed on one side only, or the ABI literal bumped on one side only,
//! ships without a single test going red on a macOS/Linux host. That drift class shipped three
//! broken releases (review M26, invariant 8).
//!
//! The pin reads BOTH source files with `include_str!` and compares them with plain string
//! operations — no C# parser, no fixture, no Windows. It is deliberately a SOURCE pin: the failure
//! messages name the field or export that drifted so the fix is a one-line edit on the side that
//! moved. It cannot see whether the glue was rebuilt (that is the release checklist's job); it can
//! see whether the two halves that MUST be rebuilt together still agree.

const RUST_RAW: &str = include_str!("../src/shimadzu.rs");
const GLUE_RAW: &str = include_str!("../glue/shimadzu/Glue.cs");

/// Both sources with CRLF folded to LF. The Windows box checks this repo out with
/// `core.autocrlf=true`, so `include_str!` hands back `\r\n` there and every `\n`-anchored match in
/// this file returns `None` — three of these pins were red on the box and green on macOS for
/// exactly that reason (the same trap `tests/contract_strings.rs` hit a day earlier).
fn rust() -> &'static str {
    static S: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    S.get_or_init(|| RUST_RAW.replace("\r\n", "\n"))
}
fn glue() -> &'static str {
    static S: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    S.get_or_init(|| GLUE_RAW.replace("\r\n", "\n"))
}

/// Both sides spell the same field `n_points` / `NPoints`, `ms_level` / `MsLevel`: lowercase and
/// drop underscores, and the two spellings meet in the middle without a PascalCase→snake_case
/// heuristic that would have to guess where `Mz` ends.
fn norm(name: &str) -> String {
    name.chars().filter(|c| *c != '_').flat_map(char::to_lowercase).collect()
}

/// Text between `open` and the first line that is exactly `}` — the body of a struct declared
/// `struct NAME {` (Rust) or `public struct NAME\n{` (C#). Both files close their structs at column 0.
fn block_after<'a>(src: &'a str, open: &str, what: &str) -> &'a str {
    let start = src.find(open).unwrap_or_else(|| panic!("{what}: `{open}` not found"));
    let body = &src[start + open.len()..];
    let end = body.find("\n}").unwrap_or_else(|| panic!("{what}: unterminated block after `{open}`"));
    &body[..end]
}

/// Strip a trailing `// …` comment and surrounding whitespace.
fn code_part(line: &str) -> &str {
    line.split("//").next().unwrap_or("").trim()
}

/// Ordered field names of a Rust struct: every `name: Type,` line in its body.
fn rust_fields(name: &str) -> Vec<String> {
    let body = block_after(rust(), &format!("struct {name} {{"), "shimadzu.rs");
    body.lines()
        .map(code_part)
        .filter(|l| !l.is_empty())
        .filter_map(|l| l.split_once(':').map(|(f, _)| f.trim().to_string()))
        .collect()
}

/// Ordered field names of a C# struct: every `public <type> <Name>;` line in its body. The body
/// starts at the `{` on the line after the declaration.
fn cs_fields(name: &str) -> Vec<String> {
    let body = block_after(glue(), &format!("public struct {name}\n{{"), "Glue.cs");
    body.lines()
        .map(code_part)
        .filter(|l| l.starts_with("public ") && l.ends_with(';'))
        .map(|l| {
            let l = l.trim_end_matches(';');
            l.rsplit(char::is_whitespace).next().unwrap().to_string()
        })
        .collect()
}

/// Every literal the Rust loader resolves through `pdcstr!("…")`, minus the managed type name
/// (`"ShimadzuGlue.Api, ShimadzuGlue"` — not an export; anything that is not a bare identifier).
fn rust_resolved_exports() -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = rust();
    while let Some(i) = rest.find("pdcstr!(\"") {
        let after = &rest[i + "pdcstr!(\"".len()..];
        let end = after.find('"').expect("unterminated pdcstr! literal");
        let lit = &after[..end];
        if lit.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            out.push(lit.to_string());
        }
        rest = &after[end..];
    }
    out
}

/// Every `[UnmanagedCallersOnly(EntryPoint = "…")]` in the glue, with the parameter count of the
/// method declared on the next non-empty line.
fn cs_exports() -> Vec<(String, usize)> {
    let mut out = Vec::new();
    let mut lines = glue().lines().peekable();
    while let Some(line) = lines.next() {
        let Some(rest) = line.trim().strip_prefix("[UnmanagedCallersOnly(EntryPoint = \"") else {
            continue;
        };
        let name = rest.split('"').next().unwrap().to_string();
        let sig = lines
            .find(|l| !l.trim().is_empty())
            .unwrap_or_else(|| panic!("Glue.cs: export {name} has no method after its attribute"));
        assert!(
            sig.contains(&format!(" {name}(")),
            "Glue.cs: export {name}: method on the next line is not named {name}: `{}`",
            sig.trim()
        );
        out.push((name, param_count(sig)));
    }
    out
}

/// Number of comma-separated entries between the first `(` and its matching `)`.
fn param_count(sig: &str) -> usize {
    let open = sig.find('(').unwrap_or_else(|| panic!("no `(` in `{}`", sig.trim()));
    let inner = &sig[open + 1..];
    let close = inner.find(')').unwrap_or_else(|| panic!("no `)` in `{}`", sig.trim()));
    let inner = inner[..close].trim();
    if inner.is_empty() { 0 } else { inner.split(',').count() }
}

/// The Rust `extern "system" fn(…)` parameter count behind an export: the loader resolves each
/// name through `get_function_with_unmanaged_callers_only::<ShimX>(ty, pdcstr!("Name"))`, and
/// `type ShimX = extern "system" fn(A, B) -> R;` says how many arguments it will pass.
fn rust_param_count(export: &str) -> usize {
    let call = format!("pdcstr!(\"{export}\")");
    let at = rust().find(&call).unwrap();
    let before = &rust()[..at];
    let turbofish = before
        .rfind("get_function_with_unmanaged_callers_only::<")
        .unwrap_or_else(|| panic!("shimadzu.rs: {export} is not resolved through the typed loader"));
    let alias = &before[turbofish + "get_function_with_unmanaged_callers_only::<".len()..];
    let alias = alias.split('>').next().unwrap().trim();
    let decl = rust()
        .find(&format!("type {alias} ="))
        .unwrap_or_else(|| panic!("shimadzu.rs: no `type {alias} =` for export {export}"));
    let sig = &rust()[decl..];
    let sig = &sig[..sig.find(';').unwrap()];
    assert!(sig.contains("extern \"system\" fn("), "shimadzu.rs: {alias} is not an extern \"system\" fn");
    param_count(&sig[sig.find("fn(").unwrap() + 2..])
}

#[test]
fn abi_version_literal_agrees() {
    let rust = rust()
        .lines()
        .map(code_part)
        .find_map(|l| l.strip_prefix("const REQUIRED_ABI_VERSION: i32 = "))
        .map(|v| v.trim_end_matches(';').trim().parse::<i32>().expect("Rust ABI literal is an i32"))
        .expect("shimadzu.rs: `const REQUIRED_ABI_VERSION: i32 = N;` not found");
    let cs = glue()
        .lines()
        .map(code_part)
        .find_map(|l| l.split_once("ShimadzuAbiVersion() => ").map(|(_, v)| v))
        .map(|v| v.trim_end_matches(';').trim().parse::<i32>().expect("C# ABI literal is an int"))
        .expect("Glue.cs: `ShimadzuAbiVersion() => N;` not found");
    assert_eq!(
        rust, cs,
        "ABI version drift: shimadzu.rs REQUIRED_ABI_VERSION = {rust}, Glue.cs ShimadzuAbiVersion() => {cs}. \
         The two are one unit — bump both, and rebuild the glue."
    );
}

#[test]
fn struct_twins_have_the_same_fields_in_the_same_order() {
    for name in ["ShimadzuSpectrumMeta", "ShimadzuSpectrumMetaV2"] {
        let rust = rust_fields(name);
        let cs = cs_fields(name);
        assert!(!rust.is_empty(), "shimadzu.rs: {name} has no fields (parser drift?)");
        assert!(!cs.is_empty(), "Glue.cs: {name} has no fields (parser drift?)");
        for (i, (r, c)) in rust.iter().zip(cs.iter()).enumerate() {
            assert_eq!(
                norm(r),
                norm(c),
                "{name} field #{i} drifted: shimadzu.rs `{r}` vs Glue.cs `{c}` (order and names must match; \
                 #[repr(C)] and LayoutKind.Sequential lay fields out in declaration order)"
            );
        }
        assert_eq!(
            rust.len(),
            cs.len(),
            "{name} field count drifted: shimadzu.rs has {} {rust:?}, Glue.cs has {} {cs:?}",
            rust.len(),
            cs.len()
        );
    }
}

/// V2 is V1 plus a suffix: the Rust `offset_of!` block and the C# static ctor both assert the prefix
/// offsets, but each only against its own twin. This holds it across the boundary.
#[test]
fn v2_struct_starts_with_the_v1_layout_on_both_sides() {
    let v1 = rust_fields("ShimadzuSpectrumMeta");
    let v2 = rust_fields("ShimadzuSpectrumMetaV2");
    assert_eq!(&v2[..v1.len()], &v1[..], "shimadzu.rs: ShimadzuSpectrumMetaV2 does not start with the V1 fields");
    let v1 = cs_fields("ShimadzuSpectrumMeta");
    let v2 = cs_fields("ShimadzuSpectrumMetaV2");
    assert_eq!(&v2[..v1.len()], &v1[..], "Glue.cs: ShimadzuSpectrumMetaV2 does not start with the V1 fields");
}

/// A `[StructLayout]` on either twin must be Sequential: `Auto` lets the CLR reorder, `Explicit`
/// moves the truth into `FieldOffset`s this pin does not read. (Absent means Sequential for a C#
/// struct, which is why V2 may omit it.)
#[test]
fn struct_layout_attributes_are_sequential() {
    for name in ["ShimadzuSpectrumMeta", "ShimadzuSpectrumMetaV2"] {
        let decl = glue().find(&format!("public struct {name}\n")).unwrap();
        let prev = glue()[..decl].trim_end().rsplit('\n').next().unwrap().trim();
        if prev.starts_with("[StructLayout(") {
            assert!(
                prev.contains("LayoutKind.Sequential") && !prev.contains("Pack"),
                "Glue.cs: {name} carries `{prev}`; shimadzu.rs assumes plain sequential #[repr(C)] layout"
            );
        }
    }
}

#[test]
fn every_export_the_loader_resolves_exists_with_the_same_arity() {
    let rust = rust_resolved_exports();
    let cs = cs_exports();
    assert!(rust.len() >= 12, "shimadzu.rs: only {} pdcstr! exports found (parser drift?): {rust:?}", rust.len());
    for name in &rust {
        let Some((_, cs_arity)) = cs.iter().find(|(n, _)| n == name) else {
            panic!(
                "shimadzu.rs resolves `{name}` but Glue.cs has no `[UnmanagedCallersOnly(EntryPoint = \"{name}\")]`; \
                 C# exports are {:?}",
                cs.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>()
            );
        };
        let rust_arity = rust_param_count(name);
        assert_eq!(
            rust_arity, *cs_arity,
            "export `{name}`: shimadzu.rs passes {rust_arity} argument(s), Glue.cs declares {cs_arity}"
        );
    }
    // The loader also checks the version through the same table, so the literal pin above is only
    // reachable if this export is one of the resolved ones.
    assert!(rust.iter().any(|n| n == "ShimadzuAbiVersion"), "shimadzu.rs no longer resolves ShimadzuAbiVersion");
}

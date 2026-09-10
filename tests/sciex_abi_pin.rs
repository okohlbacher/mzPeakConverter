//! SciEX glue pins: the Rust loader boots CoreCLR once per process, and it and the C# glue describe
//! ONE contract (fixture-free, host-independent; runs everywhere `cargo test` runs).
//!
//! WHY THIS EXISTS. `src/sciex.rs` compiles only on Windows, the glue only runs beside Clearcore2, and
//! nothing on a macOS/Linux host builds either half against the other. They meet at a hand-mirrored
//! C ABI: a version literal on each side, `#[repr(C)]` / `[StructLayout]` struct twins, and the
//! `[UnmanagedCallersOnly]` exports the Rust side resolves by name. Each side's own size asserts catch
//! a struct that drifts from ITSELF; a field renamed or reordered on one side only, an export renamed
//! on one side only, or the version bumped on one side only would ship with every test green. The
//! pins read the SOURCE of both with `include_str!` and compare it with plain string operations — no
//! C# parser, no fixture, no Windows — the pattern of `tests/shimadzu_abi_pin.rs`. They cannot see
//! whether the glue was rebuilt or whether Windows accepts the boot; they can see whether the two
//! halves that must change together still agree.

const RUST_RAW: &str = include_str!("../src/sciex.rs");
const GLUE_RAW: &str = include_str!("../glue/sciex/Glue.cs");

/// Both sources with CRLF folded to LF: the Windows box checks this repo out with
/// `core.autocrlf=true`, and every `\n`-anchored match would otherwise miss there (see
/// `tests/shimadzu_abi_pin.rs`).
fn rust() -> &'static str {
    static S: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    S.get_or_init(|| RUST_RAW.replace("\r\n", "\n"))
}
fn glue() -> &'static str {
    static S: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    S.get_or_init(|| GLUE_RAW.replace("\r\n", "\n"))
}

/// Strip a trailing `// …` comment and surrounding whitespace.
fn code_part(line: &str) -> &str {
    line.split("//").next().unwrap_or("").trim()
}

/// `src/sciex.rs` without comments, one trimmed line per source line.
fn rust_code() -> String {
    rust().lines().map(code_part).collect::<Vec<_>>().join("\n")
}

/// `parent_mz` / `ParentMz`: lowercase and drop underscores, and the two spellings meet.
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

/// Ordered `(name, type)` of a Rust struct: every `name: Type,` line in its body.
fn rust_fields(name: &str) -> Vec<(String, String)> {
    let body = block_after(rust(), &format!("struct {name} {{"), "sciex.rs");
    body.lines()
        .map(code_part)
        .filter(|l| !l.is_empty())
        .filter_map(|l| {
            l.split_once(':')
                .map(|(f, t)| (f.trim().to_string(), t.trim().trim_end_matches(',').trim().to_string()))
        })
        .collect()
}

/// Ordered `(Name, type)` of a C# struct: every `public <type> <Name>;` line in its body.
fn cs_fields(name: &str) -> Vec<(String, String)> {
    let body = block_after(glue(), &format!("public struct {name}\n{{"), "Glue.cs");
    body.lines()
        .map(code_part)
        .filter(|l| l.starts_with("public ") && l.ends_with(';'))
        .map(|l| {
            let words: Vec<&str> = l.trim_end_matches(';').split_whitespace().collect();
            assert_eq!(words.len(), 3, "Glue.cs: {name}: `{l}` is not `public <type> <Name>;` (parser drift?)");
            (words[2].to_string(), words[1].to_string())
        })
        .collect()
}

/// The blittable scalars `#[repr(C)]` and `LayoutKind.Sequential` lay out identically: Rust type, C#
/// type, size in bytes (= alignment).
const SCALARS: &[(&str, &str, usize)] = &[
    ("i32", "int", 4),
    ("u32", "uint", 4),
    ("i64", "long", 8),
    ("u64", "ulong", 8),
    ("f32", "float", 4),
    ("f64", "double", 8),
];

/// The struct twins that cross the boundary.
const STRUCTS: &[&str] = &["SciexSpectrumMeta", "SciexSpectrumMetaV2", "SciexValueChanges"];

/// The integer literal right after `needle` (`size_of::<X>() == 72` → 72).
fn literal_after(src: &str, needle: &str, what: &str) -> usize {
    let at = src.find(needle).unwrap_or_else(|| panic!("{what}: `{needle}` not found")) + needle.len();
    let digits: String = src[at..].chars().take_while(char::is_ascii_digit).collect();
    digits.parse().unwrap_or_else(|_| panic!("{what}: no integer after `{needle}`"))
}

/// Every literal the Rust loader resolves through `pdcstr!("…")`, minus the managed type name.
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
/// method that follows — its header joined across lines up to the closing parenthesis, since
/// `SpectrumData` wraps its parameters.
fn cs_exports() -> Vec<(String, usize)> {
    let mut out = Vec::new();
    let mut lines = glue().lines();
    while let Some(line) = lines.next() {
        let Some(rest) = line.trim().strip_prefix("[UnmanagedCallersOnly(EntryPoint = \"") else {
            continue;
        };
        let name = rest.split('"').next().unwrap().to_string();
        let mut sig = String::new();
        for l in lines.by_ref() {
            if sig.is_empty() && l.trim().is_empty() {
                continue;
            }
            sig.push_str(l.trim());
            sig.push(' ');
            if sig.contains(')') {
                break;
            }
        }
        assert!(
            sig.contains(&format!(" {name}(")),
            "Glue.cs: export {name}: the method after its attribute is not named {name}: `{sig}`"
        );
        out.push((name, param_count(&sig)));
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

/// The Rust `extern "system" fn(…)` parameter count behind an export: the loader resolves each name
/// through `get_function_with_unmanaged_callers_only::<Alias>(ty, pdcstr!("Name"))`, and
/// `type Alias = extern "system" fn(A, B) -> R;` says how many arguments it will pass.
fn rust_param_count(export: &str) -> usize {
    let call = format!("pdcstr!(\"{export}\")");
    let at = rust().find(&call).unwrap();
    let before = &rust()[..at];
    let turbofish = before
        .rfind("get_function_with_unmanaged_callers_only::<")
        .unwrap_or_else(|| panic!("sciex.rs: {export} is not resolved through the typed loader"));
    let alias = &before[turbofish + "get_function_with_unmanaged_callers_only::<".len()..];
    let alias = alias.split('>').next().unwrap().trim();
    let decl = rust()
        .find(&format!("type {alias} ="))
        .unwrap_or_else(|| panic!("sciex.rs: no `type {alias} =` for export {export}"));
    let sig = &rust()[decl..];
    let sig = &sig[..sig.find(';').unwrap()];
    assert!(sig.contains("extern \"system\" fn("), "sciex.rs: {alias} is not an extern \"system\" fn");
    param_count(&sig[sig.find("fn(").unwrap() + 2..])
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

#[test]
fn abi_version_literal_agrees() {
    let rust = rust()
        .lines()
        .map(code_part)
        .find_map(|l| l.strip_prefix("const REQUIRED_ABI_VERSION: i32 = "))
        .map(|v| v.trim_end_matches(';').trim().parse::<i32>().expect("Rust ABI literal is an i32"))
        .expect("sciex.rs: `const REQUIRED_ABI_VERSION: i32 = N;` not found");
    let cs = glue()
        .lines()
        .map(code_part)
        .find_map(|l| l.split_once("SciexAbiVersion() => ").map(|(_, v)| v))
        .map(|v| v.trim_end_matches(';').trim().parse::<i32>().expect("C# ABI literal is an int"))
        .expect("Glue.cs: `SciexAbiVersion() => N;` not found");
    assert!(rust >= 2, "sciex.rs: REQUIRED_ABI_VERSION {rust}: 1 is the pre-handshake glue, which the loader must refuse");
    assert_eq!(
        rust, cs,
        "ABI version drift: sciex.rs REQUIRED_ABI_VERSION = {rust}, Glue.cs SciexAbiVersion() => {cs}. \
         The two are one unit — bump both, and rebuild the glue."
    );
}

#[test]
fn struct_twins_have_the_same_fields_in_the_same_order() {
    for &name in STRUCTS {
        let rust = rust_fields(name);
        let cs = cs_fields(name);
        assert!(!rust.is_empty(), "sciex.rs: {name} has no fields (parser drift?)");
        assert!(!cs.is_empty(), "Glue.cs: {name} has no fields (parser drift?)");
        for (i, ((r, _), (c, _))) in rust.iter().zip(cs.iter()).enumerate() {
            assert_eq!(
                norm(r),
                norm(c),
                "{name} field #{i} drifted: sciex.rs `{r}` vs Glue.cs `{c}` (order and names must match; \
                 #[repr(C)] and LayoutKind.Sequential lay fields out in declaration order)"
            );
        }
        assert_eq!(rust.len(), cs.len(), "{name} field count drifted: sciex.rs has {rust:?}, Glue.cs has {cs:?}");
    }
}

/// Names alone let an `i32` face a `long`: each field's C# type must be the twin of its Rust type, and
/// the size BOTH sides assert (`size_of` in sciex.rs, `Marshal.SizeOf` in the glue's static ctor) must
/// be the sequential layout those types produce.
#[test]
fn struct_twins_have_the_same_field_types_and_size() {
    for &name in STRUCTS {
        let (rs, cs) = (rust_fields(name), cs_fields(name));
        assert_eq!(rs.len(), cs.len(), "{name}: field count drifted: {rs:?} vs {cs:?}");
        let (mut size, mut align) = (0usize, 1usize);
        for ((r, rt), (c, ct)) in rs.iter().zip(&cs) {
            let &(_, want, bytes) = SCALARS.iter().find(|(t, _, _)| t == rt).unwrap_or_else(|| {
                panic!("{name}.{r}: `{rt}` is not a blittable scalar this pin maps (extend SCALARS)")
            });
            assert_eq!(ct, want, "{name}.{r}: sciex.rs `{rt}` ({bytes} B) faces Glue.cs `{ct} {c}`");
            size = size.next_multiple_of(bytes) + bytes;
            align = align.max(bytes);
        }
        let size = size.next_multiple_of(align);
        let rust_says = literal_after(rust(), &format!("size_of::<{name}>() == "), "sciex.rs");
        let cs_says = literal_after(glue(), &format!("Marshal.SizeOf<{name}>() != "), "Glue.cs");
        assert_eq!(
            (rust_says, cs_says),
            (size, size),
            "{name}: its fields lay out to {size} B; sciex.rs asserts {rust_says}, Glue.cs {cs_says}"
        );
    }
}

/// V2 is V1 plus a suffix: the Rust `offset_of!` block and the C# static ctor assert the prefix
/// offsets, each only against its own twin. This holds it across the boundary.
#[test]
fn v2_struct_starts_with_the_v1_layout_on_both_sides() {
    let v1 = rust_fields("SciexSpectrumMeta");
    let v2 = rust_fields("SciexSpectrumMetaV2");
    assert_eq!(&v2[..v1.len()], &v1[..], "sciex.rs: SciexSpectrumMetaV2 does not start with the V1 fields");
    let v1 = cs_fields("SciexSpectrumMeta");
    let v2 = cs_fields("SciexSpectrumMetaV2");
    assert_eq!(&v2[..v1.len()], &v1[..], "Glue.cs: SciexSpectrumMetaV2 does not start with the V1 fields");
}

/// A `[StructLayout]` on a twin must be Sequential: `Auto` lets the CLR reorder, `Explicit` moves the
/// truth into `FieldOffset`s this pin does not read.
#[test]
fn struct_layout_attributes_are_sequential() {
    for &name in STRUCTS {
        let decl = glue().find(&format!("public struct {name}\n")).unwrap();
        let prev = glue()[..decl].trim_end().rsplit('\n').next().unwrap().trim();
        if prev.starts_with("[StructLayout(") {
            assert!(
                prev.contains("LayoutKind.Sequential") && !prev.contains("Pack"),
                "Glue.cs: {name} carries `{prev}`; sciex.rs assumes plain sequential #[repr(C)] layout"
            );
        }
    }
}

#[test]
fn every_export_the_loader_resolves_exists_with_the_same_arity() {
    let rust = rust_resolved_exports();
    let cs = cs_exports();
    assert!(rust.len() >= 12, "sciex.rs: only {} pdcstr! exports found (parser drift?): {rust:?}", rust.len());
    for name in &rust {
        let Some((_, cs_arity)) = cs.iter().find(|(n, _)| n == name) else {
            panic!(
                "sciex.rs resolves `{name}` but Glue.cs has no `[UnmanagedCallersOnly(EntryPoint = \"{name}\")]`; \
                 C# exports are {:?}",
                cs.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>()
            );
        };
        let rust_arity = rust_param_count(name);
        assert_eq!(rust_arity, *cs_arity, "export `{name}`: sciex.rs passes {rust_arity} argument(s), Glue.cs declares {cs_arity}");
    }
    // The version literal pin above is only meaningful if the loader checks it through this table.
    assert!(rust.iter().any(|n| n == "SciexAbiVersion"), "sciex.rs no longer resolves SciexAbiVersion");
}

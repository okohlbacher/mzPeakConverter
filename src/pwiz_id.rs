//! ProteoWizard's escaped XML ids. `encode_xml_id` (pwiz/utility/minimxml/XMLWriter.cpp) writes an
//! id as an XML name and replaces every BYTE a name may not hold with `_x00hh_`: a space is
//! `_x0020_`, a leading digit `_x0032_`, and each byte of a non-ASCII character's UTF-8 is its own
//! escape (六 is `_x00e5__x0085__x00ad_`). `tests/lane_metadata_parity.rs` includes this file by
//! `#[path]`, so the converter and that test cannot decode differently.

/// Escape `v` as ProteoWizard does, so an id decoded on the way into an archive is an XML name
/// again on the way out to mzML: every byte an NCName may not hold becomes `_x00hh_`. That is a
/// first character other than an ASCII letter or `_`, any later one other than an ASCII letter,
/// digit, `_`, `-` or `.`, and each byte of a non-ASCII character. An XML name comes back
/// unchanged, ProteoWizard's escaped form included; [`decode`] undoes the rest.
pub fn encode(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    for (i, b) in v.bytes().enumerate() {
        let name_byte = b.is_ascii_alphabetic() || b == b'_' || (i > 0 && (b.is_ascii_digit() || b == b'-' || b == b'.'));
        if name_byte {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("_x{b:04x}_"));
        }
    }
    out
}

/// Undo the escaping. A run of adjacent byte escapes is decoded as UTF-8, and left as written when
/// it is not UTF-8 (decoding each byte as a character of its own turns 六 into `å`, U+0085, U+00AD).
/// An escape above a byte is that character, as .NET's `XmlConvert.EncodeName` writes one UTF-16
/// unit; anything that is not a well-formed escape is left as written.
pub fn decode(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    let mut rest = v;
    while let Some(i) = rest.find("_x") {
        out.push_str(&rest[..i]);
        let tail = &rest[i..];
        let mut bytes = Vec::new();
        let mut after = tail;
        while let Some(b) = escape_at(after).and_then(|x| u8::try_from(x).ok()) {
            bytes.push(b);
            after = &after[7..];
        }
        if !bytes.is_empty() {
            match String::from_utf8(bytes) {
                Ok(s) => out.push_str(&s),
                Err(_) => out.push_str(&tail[..tail.len() - after.len()]),
            }
            rest = after;
        } else if let Some(c) = escape_at(tail).and_then(char::from_u32) {
            out.push(c);
            rest = &tail[7..];
        } else {
            out.push_str("_x");
            rest = &tail[2..];
        }
    }
    out.push_str(rest);
    out
}

/// The value of the `_xHHHH_` escape `s` starts with.
fn escape_at(s: &str) -> Option<u32> {
    let hex = s.strip_prefix("_x")?.get(..4)?;
    let well_formed = hex.bytes().all(|b| b.is_ascii_hexdigit()) && s[6..].starts_with('_');
    well_formed.then(|| u32::from_str_radix(hex, 16).unwrap())
}

//! ProteoWizard's escaped XML ids. `encode_xml_id` (pwiz/utility/minimxml/XMLWriter.cpp) writes an
//! id as an XML name and replaces every BYTE a name may not hold with `_x00hh_`: a space is
//! `_x0020_`, a leading digit `_x0032_`, and each byte of a non-ASCII character's UTF-8 is its own
//! escape (六 is `_x00e5__x0085__x00ad_`). `tests/lane_metadata_parity.rs` includes this file by
//! `#[path]`, so the converter and that test cannot decode differently.

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

/// The escaping itself: `id` as an XML name (`xs:ID`), which every id of an mzML header must be.
/// A character an NCName may not start with (anything but an ASCII letter or `_`) or hold (anything
/// but those, an ASCII digit, `.` and `-`) becomes `_x00hh_`, one escape per byte of its UTF-8, in
/// lower-case hex, exactly as `encode_xml_id` writes it: `MRM Neg C5` is `MRM_x0020_Neg_x0020_C5`,
/// `20181203_Capan2` is `_x0032_0181203_Capan2`. A valid name is returned as it is, so an id read
/// from a ProteoWizard mzML and never decoded passes through unchanged, and [`decode`] undoes it.
/// The one string it cannot represent is the empty one, which is not an id: returned empty.
// `tests/lane_metadata_parity.rs` includes this file for `decode` alone.
#[allow(dead_code)]
pub fn encode(id: &str) -> String {
    let start = |b: u8| b.is_ascii_alphabetic() || b == b'_';
    let mut out = String::with_capacity(id.len());
    for (i, b) in id.bytes().enumerate() {
        if start(b) || (i > 0 && (b.is_ascii_digit() || b == b'.' || b == b'-')) {
            out.push(b as char);
        } else {
            out.push_str(&format!("_x{b:04x}_"));
        }
    }
    out
}

/// The value of the `_xHHHH_` escape `s` starts with.
fn escape_at(s: &str) -> Option<u32> {
    let hex = s.strip_prefix("_x")?.get(..4)?;
    let well_formed = hex.bytes().all(|b| b.is_ascii_hexdigit()) && s[6..].starts_with('_');
    well_formed.then(|| u32::from_str_radix(hex, 16).unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What ProteoWizard writes for the ids of the corpus, and back: a space, a leading digit, a
    /// character outside ASCII (each byte of its UTF-8), punctuation a name may not hold. A valid
    /// name — an escaped one included — is left alone.
    #[test]
    fn an_id_is_escaped_as_proteowizard_escapes_it_and_decodes_back() {
        for (plain, escaped) in [
            ("MRM Neg C5", "MRM_x0020_Neg_x0020_C5"),
            ("20181203_Capan2_1", "_x0032_0181203_Capan2_1"),
            ("1", "_x0031_"),
            ("-a.b", "_x002d_a.b"),
            (".x", "_x002e_x"),
            ("Sample_1-A,1_01_985", "Sample_1-A_x002c_1_01_985"),
            ("六mix", "_x00e5__x0085__x00ad_mix"),
            ("a:b/c", "a_x003a_b_x002f_c"),
            ("SZB8102938", "SZB8102938"),
            ("pwiz_Reader_Thermo_conversion", "pwiz_Reader_Thermo_conversion"),
            ("", ""),
        ] {
            assert_eq!(encode(plain), escaped, "{plain:?}");
            assert_eq!(decode(escaped), plain, "{escaped:?}");
            assert_eq!(encode(escaped), escaped, "an escaped id is a name already: {escaped:?}");
        }
    }
}

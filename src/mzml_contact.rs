//! The `<contact>` of an mzML or imzML `<fileDescription>` (`MS:1000586` name, `MS:1000590`
//! affiliation, `MS:1000587` address, `MS:1000588` URL, `MS:1000589` e-mail). mzdata's model has no
//! contact, so the reader drops it; through 0.17.0-rc.2 nothing of it reached the archive or an
//! export. Owner decision D11 (2026-10-01): dropped by default — a published archive would spread a
//! name, an e-mail and a street address with every copy — and carried under `--keep-contact`
//! ([`crate::keep_contact`]): into the archive's `file_description.contacts` (the spec's own slot,
//! `docs/archive/file_description.md`: `contact_name`, `contact_affiliation`, `parameters`), into
//! the direct mzML export, and from the index into the export of an archive, each contact as the
//! header states it, in order, every param kept.
//!
//! [`read`] takes the contacts from the header's text, as [`crate::imaging::read_file_content`]
//! takes the file content. [`Contact::index_json`] is the archive's form, built through the
//! vendored writer's own [`mzpeak_prototyping::param::Contact`] so a reader of the index sees the
//! same param shape everywhere; [`from_index`] reads it back for the export of an archive; and
//! [`Contact::xml`] is the element the header sink writes into `<fileDescription>`
//! ([`crate::mzml_header::HeaderFixes`]), after the source files, where the schema has it.

use std::io::BufRead;
use std::path::Path;

use anyhow::{Context, Result};
use quick_xml::events::Event;

/// An attribute's value, entities decoded (`&amp;`, `&#252;`): a contact's text is read, not
/// matched, so the decoded form is the one carried.
fn attr(e: &quick_xml::events::BytesStart, key: &[u8]) -> Option<String> {
    e.try_get_attribute(key).ok().flatten().and_then(|a| quick_xml::escape::unescape(&String::from_utf8_lossy(&a.value)).ok().map(|v| v.into_owned()))
}

/// `MS:1000586`, "contact name".
pub const NAME: &str = "MS:1000586";
/// `MS:1000590`, "contact affiliation" (the mzML 1.1 example files name it "contact organization").
pub const AFFILIATION: &str = "MS:1000590";

/// One param of a contact: a `cvParam` (with `cv_ref` and `accession`) or a `userParam` (neither).
#[derive(Debug, Clone, PartialEq)]
pub struct ContactParam {
    pub cv_ref: Option<String>,
    pub accession: Option<String>,
    pub name: String,
    pub value: String,
}

/// One `<contact>`: its params in the header's order.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Contact {
    pub params: Vec<ContactParam>,
}

impl Contact {
    /// The value of the param with accession `accession`, if any.
    pub fn value_of(&self, accession: &str) -> Option<&str> {
        self.params.iter().find(|p| p.accession.as_deref() == Some(accession)).map(|p| p.value.as_str())
    }

    /// A one-line account for the log: the name, else the first value.
    pub fn summary(&self) -> String {
        self.value_of(NAME).or_else(|| self.params.first().map(|p| p.value.as_str())).unwrap_or("(no value)").to_string()
    }

    /// The archive's form: the spec's `contact` object, through the vendored writer's own type.
    pub fn index_json(&self) -> serde_json::Value {
        let parameters = self
            .params
            .iter()
            .map(|p| {
                let b = mzdata::params::Param::builder().name(p.name.clone()).value(mzdata::params::Value::String(p.value.clone()));
                let b = match p.accession.as_deref().and_then(|a| a.parse::<mzdata::params::CURIE>().ok()) {
                    Some(curie) => b.curie(curie),
                    None => b,
                };
                mzpeak_prototyping::param::MetaParam::from(b.build())
            })
            .collect();
        let contact = mzpeak_prototyping::param::Contact {
            contact_name: self.value_of(NAME).map(str::to_string),
            contact_affiliation: self.value_of(AFFILIATION).map(str::to_string),
            parameters,
        };
        serde_json::to_value(contact).expect("a contact serializes")
    }

    /// The `<contact>` element for an mzML header, each param as stated (escaped), indented as
    /// mzdata indents the children of `<fileDescription>` (six spaces, their params eight).
    pub fn xml(&self) -> String {
        let esc = crate::mzml_header::escape;
        let mut s = String::from("      <contact>\n");
        for p in &self.params {
            match (&p.cv_ref, &p.accession) {
                (Some(cv), Some(acc)) => s.push_str(&format!(
                    "        <cvParam cvRef=\"{}\" accession=\"{}\" name=\"{}\" value=\"{}\"/>\n",
                    esc(cv),
                    esc(acc),
                    esc(&p.name),
                    esc(&p.value)
                )),
                _ => s.push_str(&format!("        <userParam name=\"{}\" value=\"{}\"/>\n", esc(&p.name), esc(&p.value))),
            }
        }
        s.push_str("      </contact>\n");
        s
    }
}

/// The `<contact>` elements of an mzML or imzML header, in order, params as stated. A header
/// without one gives an empty list.
pub fn read(path: &Path) -> Result<Vec<Contact>> {
    let file = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    read_from(std::io::BufReader::new(file))
}

pub fn read_from(input: impl BufRead) -> Result<Vec<Contact>> {
    let mut reader = quick_xml::Reader::from_reader(input);
    let mut buf = Vec::new();
    let mut contacts = Vec::new();
    let mut current: Option<Contact> = None;
    loop {
        let ev = reader.read_event_into(&mut buf).context("parsing the mzML header")?;
        match &ev {
            Event::Start(e) | Event::Empty(e) => {
                let empty = matches!(ev, Event::Empty(_));
                match e.local_name().as_ref() {
                    // The header is over: the lists after the file description hold no contact.
                    b"referenceableParamGroupList" | b"sampleList" | b"softwareList" | b"run" => break,
                    b"contact" if !empty => current = Some(Contact::default()),
                    b"cvParam" | b"userParam" => {
                        if let Some(c) = current.as_mut() {
                            let cv = e.local_name().as_ref() == b"cvParam";
                            c.params.push(ContactParam {
                                cv_ref: cv.then(|| attr(e, b"cvRef")).flatten(),
                                accession: cv.then(|| attr(e, b"accession")).flatten(),
                                name: attr(e, b"name").unwrap_or_default(),
                                value: attr(e, b"value").unwrap_or_default(),
                            });
                        }
                    }
                    _ => {}
                }
            }
            Event::End(e) => match e.local_name().as_ref() {
                b"contact" => contacts.extend(current.take()),
                b"fileDescription" => break,
                _ => {}
            },
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    Ok(contacts)
}

/// The contacts an archive's `file_description` index block holds ([`Contact::index_json`]), for
/// the export of the archive; none when the block has no `contacts`.
pub fn from_index(file_description: Option<&serde_json::Value>) -> Vec<Contact> {
    let Some(list) = file_description.and_then(|fd| fd.get("contacts")).and_then(serde_json::Value::as_array) else {
        return Vec::new();
    };
    list.iter()
        .map(|c| Contact {
            params: c["parameters"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|p| {
                    let accession = p["accession"].as_str().map(str::to_string);
                    ContactParam {
                        cv_ref: accession.as_deref().and_then(|a| a.split_once(':')).map(|(cv, _)| cv.to_string()),
                        accession,
                        name: p["name"].as_str().unwrap_or_default().to_string(),
                        value: match &p["value"] {
                            serde_json::Value::String(s) => s.clone(),
                            serde_json::Value::Null => String::new(),
                            other => other.to_string(),
                        },
                    }
                })
                .collect(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEADER: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<mzML xmlns="http://psi.hupo.org/ms/mzml" version="1.1">
  <cvList count="1"><cv id="MS" fullName="x" URI="y"/></cvList>
  <fileDescription>
    <fileContent><cvParam cvRef="MS" accession="MS:1000579" name="MS1 spectrum" value=""/></fileContent>
    <sourceFileList count="1"><sourceFile id="sf1" name="a.raw" location="file://"><cvParam cvRef="MS" accession="MS:1000563" name="Thermo RAW file" value=""/></sourceFile></sourceFileList>
    <contact>
      <cvParam cvRef="MS" accession="MS:1000586" name="contact name" value="Thorsten Schramm"/>
      <cvParam cvRef="MS" accession="MS:1000590" name="contact organization" value="Institut f&#252;r Anorganische &amp; Analytische Chemie"/>
      <cvParam cvRef="MS" accession="MS:1000587" name="contact address" value="Schubertstra&#223;e 60"/>
      <cvParam cvRef="MS" accession="MS:1000589" name="contact email" value="a@b.de"/>
      <userParam name="role" value="PI"/>
    </contact>
    <contact><cvParam cvRef="MS" accession="MS:1000586" name="contact name" value="Second One"/><cvParam cvRef="MS" accession="MS:1000590" name="contact affiliation" value="JLU"/></contact>
  </fileDescription>
  <referenceableParamGroupList count="0"/>
  <run id="r"><spectrumList count="0"/></run>
</mzML>"#;

    /// Both contacts, params in order and decoded, the user param included; the archive form names
    /// the two the spec singles out and keeps every param; the XML form escapes again.
    #[test]
    fn contacts_are_read_as_stated_and_carried_in_both_forms() {
        let contacts = read_from(HEADER.as_bytes()).unwrap();
        assert_eq!(contacts.len(), 2, "{contacts:#?}");
        let first = &contacts[0];
        assert_eq!(first.params.len(), 5);
        assert_eq!(first.value_of(NAME), Some("Thorsten Schramm"));
        assert_eq!(first.value_of(AFFILIATION), Some("Institut für Anorganische & Analytische Chemie"));
        assert_eq!(first.params[4], ContactParam { cv_ref: None, accession: None, name: "role".into(), value: "PI".into() });
        assert_eq!(first.summary(), "Thorsten Schramm");

        let json = first.index_json();
        assert_eq!(json["contact_name"], "Thorsten Schramm");
        assert_eq!(json["contact_affiliation"], "Institut für Anorganische & Analytische Chemie");
        let params = json["parameters"].as_array().unwrap();
        assert_eq!(params.len(), 5, "{json:#}");
        assert_eq!((params[2]["accession"].as_str(), params[2]["value"].as_str()), (Some("MS:1000587"), Some("Schubertstraße 60")));
        assert_eq!((params[4].get("accession").and_then(|a| a.as_str()), params[4]["name"].as_str()), (None, Some("role")), "{:#}", params[4]);

        // Back from the index as it was, and the XML as the header states it.
        let fd = serde_json::json!({"contents": [], "source_files": [], "contacts": contacts.iter().map(Contact::index_json).collect::<Vec<_>>()});
        let back = from_index(Some(&fd));
        assert_eq!(back, contacts);
        assert!(from_index(Some(&serde_json::json!({"contents": []}))).is_empty() && from_index(None).is_empty());
        let xml = first.xml();
        assert!(xml.starts_with("      <contact>\n        <cvParam cvRef=\"MS\" accession=\"MS:1000586\" name=\"contact name\" value=\"Thorsten Schramm\"/>\n"), "{xml}");
        assert!(xml.contains("value=\"Institut für Anorganische &amp; Analytische Chemie\"") && xml.contains("<userParam name=\"role\" value=\"PI\"/>") && xml.ends_with("      </contact>\n"), "{xml}");
    }

    /// A header without a contact, and one whose text ends before the file description does.
    #[test]
    fn a_header_without_a_contact_gives_none() {
        let none = HEADER.replace(&HEADER[HEADER.find("    <contact>").unwrap()..HEADER.find("  </fileDescription>").unwrap()], "");
        assert!(read_from(none.as_bytes()).unwrap().is_empty());
        assert!(read_from(&HEADER.as_bytes()[..HEADER.find("<fileDescription>").unwrap()]).unwrap().is_empty());
    }
}

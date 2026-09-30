//! The cross-references an mzML or imzML source states between its own lists, checked while an
//! archive lane copies them.
//!
//! An mzML ties its parts together by id: a `<scan>` names its `instrumentConfiguration`, a
//! `processingMethod` and an `instrumentConfiguration` their `software`, the spectrum list its
//! default `dataProcessing`, the run its default configuration and source file. mzdata copies each
//! reference as it reads it, whether or not the list holds the id — a configuration reference it has
//! not seen gets a fresh number — so a source whose writer got one wrong passed it into the archive,
//! where it names nothing: the scans of a pyimzML export (GBM `Test_P15_r2`) name
//! `instrumentConfiguration0` while its list holds `IC1`, and were stored as configuration 1 of a list
//! that holds only 0. [`DanglingRefs`] finds those as the metadata and the scans are copied and drops
//! each one: a software reference becomes empty (the spec's instrument configuration requires the
//! string), a run default absent — [`crate::fixup_run_metadata`] then names the list's first entry, as
//! for a source that states none, since the spec's run requires all three — and a scan's
//! configuration null (the vendored writer's [`NO_INSTRUMENT_CONFIGURATION`]). The archive declares
//! [`DROPPED`] in `transformations`, and the run warns once, counting each kind.
//!
//! mzdata also loses entries itself. A self-closing `<software id="…" version="…"/>` is never read
//! (it acts on a `<software>` start tag only), so the method naming it looked dangling although the
//! source is whole — MALDIquantForeign's imzML export (LA-ESI `Thaliana`) is written that way.
//! [`restore_self_closing_software`] reads such entries back from the header first, so only a
//! reference the source itself cannot resolve is dropped. A self-closing `<instrumentConfiguration/>`
//! is lost the same way; its scans then name the number mzdata gives the id on first sight, which is
//! 0 when the list read back empty, and 0 is the empty configuration `fixup_run_metadata` writes for
//! such a run — so they resolve, and are kept.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::io::BufRead;
use std::path::Path;

use anyhow::{Context, Result};
use mzdata::meta::{MSDataFileMetadata, Software};
use mzdata::spectrum::SpectrumDescription;
use mzpeak_prototyping::writer::NO_INSTRUMENT_CONFIGURATION;
use quick_xml::events::Event;

use crate::imaging::attr;
use crate::pwiz_id;

/// The `transformations` entry of an archive from which a dangling reference was dropped.
pub const DROPPED: &str = "mzml:dangling-reference-dropped";

/// The ids a source could not resolve, as they were dropped. Built by [`DanglingRefs::check_metadata`]
/// and fed each spectrum through [`DanglingRefs::check_scans`].
#[derive(Debug, Default)]
pub struct DanglingRefs {
    /// The configuration numbers a scan may name: the list's, or 0 alone for an empty list, which
    /// `fixup_run_metadata` gives an empty configuration 0.
    configurations: HashSet<u32>,
    /// Per mzML attribute: how many references were dropped, and the ids they named.
    dropped: BTreeMap<&'static str, (usize, BTreeSet<String>)>,
}

impl DanglingRefs {
    /// Drop the run-level references of copied source metadata that name no entry of its lists: a
    /// `processingMethod`'s or an `instrumentConfiguration`'s `softwareRef`, and the run's
    /// `defaultInstrumentConfigurationRef`, `defaultDataProcessingRef` and `defaultSourceFileRef`.
    /// Call after [`restore_self_closing_software`] and `decode_pwiz_ids` (both sides of a software
    /// reference are decoded there), and before this conversion adds its own entries.
    pub fn check_metadata(target: &mut impl MSDataFileMetadata) -> Self {
        let mut configurations: HashSet<u32> = target.instrument_configurations().keys().copied().collect();
        if configurations.is_empty() {
            configurations.insert(0);
        }
        let mut this = Self { configurations, ..Default::default() };
        let software: HashSet<String> = target.softwares().iter().map(|s| s.id.clone()).collect();
        let drop_software = |this: &mut Self, reference: &mut String| {
            if !reference.is_empty() && !software.contains(reference.as_str()) {
                this.note("softwareRef", std::mem::take(reference));
            }
        };
        for dp in target.data_processings_mut() {
            for method in dp.methods.iter_mut() {
                drop_software(&mut this, &mut method.software_reference);
            }
        }
        for ic in target.instrument_configurations_mut().values_mut() {
            drop_software(&mut this, &mut ic.software_reference);
        }
        let processing: HashSet<String> = target.data_processings().iter().map(|dp| dp.id.clone()).collect();
        let files: HashSet<String> = target.file_description().source_files.iter().map(|sf| sf.id.clone()).collect();
        if let Some(run) = target.run_description_mut() {
            if let Some(id) = run.default_instrument_id.filter(|id| !this.configurations.contains(id)) {
                run.default_instrument_id = None;
                this.note("defaultInstrumentConfigurationRef", format!("configuration {id}"));
            }
            if let Some(id) = run.default_data_processing_id.take_if(|id| !processing.contains(id.as_str())) {
                this.note("defaultDataProcessingRef", id);
            }
            if let Some(id) = run.default_source_file_id.take_if(|id| !files.contains(id.as_str())) {
                this.note("defaultSourceFileRef", id);
            }
        }
        this
    }

    /// Null the configuration of each scan of `descr` that names none of the source's.
    pub fn check_scans(&mut self, descr: &mut SpectrumDescription) {
        for scan in descr.acquisition.scans.iter_mut() {
            let id = scan.instrument_configuration_id;
            if id != NO_INSTRUMENT_CONFIGURATION && !self.configurations.contains(&id) {
                scan.instrument_configuration_id = NO_INSTRUMENT_CONFIGURATION;
                self.note("instrumentConfigurationRef", format!("configuration {id}"));
            }
        }
    }

    fn note(&mut self, kind: &'static str, id: String) {
        let (n, ids) = self.dropped.entry(kind).or_default();
        *n += 1;
        ids.insert(id);
    }

    /// [`DROPPED`] when a reference was dropped.
    pub fn transformation(&self) -> Option<&'static str> {
        (!self.dropped.is_empty()).then_some(DROPPED)
    }

    /// The one warning of a run, counting each kind: `2826 instrumentConfigurationRef (configuration
    /// 1)`. Silent when nothing was dropped.
    pub fn warn(&self, input: &Path) {
        if let Some(what) = self.summary() {
            log::warn!(
                "{}: dropped references that name no entry of the source's lists: {what}; declared as {DROPPED}",
                input.display()
            );
        }
    }

    fn summary(&self) -> Option<String> {
        (!self.dropped.is_empty()).then(|| {
            self.dropped
                .iter()
                .map(|(kind, (n, ids))| {
                    let named: Vec<&str> = ids.iter().take(3).map(String::as_str).collect();
                    let more = if ids.len() > 3 { format!(" and {} more", ids.len() - 3) } else { String::new() };
                    format!("{n} {kind} ({}{more})", named.join(", "))
                })
                .collect::<Vec<_>>()
                .join(", ")
        })
    }
}

/// Put back the `<software/>` entries of an mzML or imzML header that mzdata skipped for being
/// self-closing (it reads a `<software>` start tag only): id (decoded as `decode_pwiz_ids` decodes
/// the others) and version, with no params — the element states none. An entry mzdata did read is
/// left alone. Reading stops at `<run>`. Returns how many were put back.
pub fn restore_self_closing_software(path: &Path, target: &mut impl MSDataFileMetadata) -> Result<usize> {
    let file = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let found = self_closing_software(std::io::BufReader::new(file))?;
    let mut restored = 0;
    for (id, version) in found {
        let id = pwiz_id::decode(&id);
        if !target.softwares().iter().any(|s| s.id == id) {
            target.softwares_mut().push(Software::new(id, version, Vec::new()));
            restored += 1;
        }
    }
    Ok(restored)
}

/// The (id, version) of each self-closing `<software/>` in `<softwareList>`, in document order.
fn self_closing_software(input: impl BufRead) -> Result<Vec<(String, String)>> {
    let mut reader = quick_xml::Reader::from_reader(input);
    let mut buf = Vec::new();
    let mut found = Vec::new();
    let mut in_list = false;
    loop {
        match reader.read_event_into(&mut buf).context("parsing the mzML header")? {
            Event::Start(e) => match e.local_name().as_ref() {
                b"softwareList" => in_list = true,
                b"run" => break,
                _ => {}
            },
            Event::Empty(e) if in_list && e.local_name().as_ref() == b"software" => {
                if let Some(id) = attr(&e, b"id") {
                    found.push((id, attr(&e, b"version").unwrap_or_default()));
                }
            }
            Event::End(e) if e.local_name().as_ref() == b"softwareList" => break,
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mzdata::meta::{DataProcessing, FileMetadataConfig, InstrumentConfiguration, ProcessingMethod, SourceFile};

    #[test]
    fn a_self_closing_software_is_read_and_one_with_content_is_not() {
        let header = br#"<mzML><softwareList count="3">
            <software id="MALDIquantForeign" version="0.12"/>
            <software id="pwiz" version="3"><cvParam accession="MS:1000615" name="ProteoWizard software"/></software>
            <software id="noversion"/>
          </softwareList><run><spectrumList><spectrum><software id="late"/></spectrum></spectrumList></run></mzML>"#;
        let found = self_closing_software(&header[..]).unwrap();
        assert_eq!(found, [("MALDIquantForeign".to_string(), "0.12".to_string()), ("noversion".to_string(), String::new())]);
    }

    /// Every run-level kind is dropped exactly when it names nothing, and a scan's configuration
    /// becomes the null sentinel; what resolves is kept.
    #[test]
    fn each_dangling_kind_is_dropped_and_counted() {
        let mut meta = FileMetadataConfig::default();
        meta.softwares_mut().push(Software::new("pwiz".into(), "3".into(), Vec::new()));
        meta.instrument_configurations_mut()
            .insert(0, InstrumentConfiguration { id: 0, software_reference: "vendor".into(), ..Default::default() });
        let method = |sw: &str| ProcessingMethod { order: 0, software_reference: sw.into(), params: Vec::new() };
        meta.data_processings_mut().push(DataProcessing { id: "dp".into(), methods: vec![method("pwiz"), method("ghost"), method("")] });
        meta.file_description_mut().source_files.push(SourceFile { id: "sf".into(), ..Default::default() });
        let run = meta.run_description_mut().unwrap();
        run.default_instrument_id = Some(3);
        run.default_data_processing_id = Some("dp1".into());
        run.default_source_file_id = Some("sf".into());

        let mut refs = DanglingRefs::check_metadata(&mut meta);
        let methods: Vec<&str> = meta.data_processings()[0].methods.iter().map(|m| m.software_reference.as_str()).collect();
        assert_eq!(methods, ["pwiz", "", ""]);
        assert_eq!(meta.instrument_configurations()[&0].software_reference, "");
        let run = meta.run_description().unwrap();
        assert_eq!((run.default_instrument_id, run.default_data_processing_id.as_deref()), (None, None));
        assert_eq!(run.default_source_file_id.as_deref(), Some("sf"), "a resolving default stays");

        let mut descr = SpectrumDescription::default();
        descr.acquisition.scans = vec![Default::default(), Default::default()];
        descr.acquisition.scans[1].instrument_configuration_id = 1;
        refs.check_scans(&mut descr);
        assert_eq!(descr.acquisition.scans[0].instrument_configuration_id, 0);
        assert_eq!(descr.acquisition.scans[1].instrument_configuration_id, NO_INSTRUMENT_CONFIGURATION);
        refs.check_scans(&mut descr);
        assert_eq!(refs.transformation(), Some(DROPPED));
        assert_eq!(
            refs.summary().unwrap(),
            "1 defaultDataProcessingRef (dp1), 1 defaultInstrumentConfigurationRef (configuration 3), \
             1 instrumentConfigurationRef (configuration 1), 2 softwareRef (ghost, vendor)"
        );
    }

    /// A run whose configuration list read back empty gets the empty configuration 0, so a scan or a
    /// default naming 0 resolves; nothing is declared for a whole source.
    #[test]
    fn an_empty_configuration_list_resolves_configuration_zero() {
        let mut meta = FileMetadataConfig::default();
        meta.run_description_mut().unwrap().default_instrument_id = Some(0);
        let mut refs = DanglingRefs::check_metadata(&mut meta);
        let mut descr = SpectrumDescription::default();
        descr.acquisition.scans = vec![Default::default()];
        refs.check_scans(&mut descr);
        assert_eq!(descr.acquisition.scans[0].instrument_configuration_id, 0);
        assert_eq!(meta.run_description().unwrap().default_instrument_id, Some(0));
        assert_eq!(refs.transformation(), None);
        assert_eq!(refs.summary(), None);
    }
}

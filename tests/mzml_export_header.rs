//! What an mzML this tool writes states about the RUN in its header: every id an XML name, every
//! reference resolving ([`mzml_meta::assert_header_contract`]); the run's own id, start time and
//! default source file; and, for an archive's export, the lists the archive's index holds.
//!
//! Through 0.17.0-rc.1
//! * an archive's export took the file content and the scan settings from the index and nothing
//!   else: no source file but the archive, no sample, no software but this tool, one EMPTY instrument
//!   configuration (`<componentList count="0">`, `<softwareRef ref=""/>`), and every scan of a
//!   second analyzer naming an `IC2` the export did not declare (976 FTMS scans of an LTQ-FT run);
//! * every export — direct or of an archive — stated the run as `<run id="1">`, undated, with the
//!   first listed source file as its default (`MRM Neg C5`, acquired 2006-09-10T02:11:56Z from
//!   `MSScan.bin`: run `1` of `acqmethod.xml`);
//! * the direct export of an mzML or imzML copied each cross-reference as mzdata read it, where the
//!   archive lane drops one that names nothing (`src/mzml_refs.rs`): the scans of a pyimzML export
//!   named `IC2` under a list of `IC1`;
//! * converting an export back wrote software `mzpeak-convert` twice, and filtering a filtered
//!   archive processing `mzpeak_convert_filter` twice.

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

#[path = "common/mzml_meta.rs"]
mod mzml_meta;

const TINY: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/tiny.pwiz.1.1.mzML");
const THERMO: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/small.RAW");
const DANGLING: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/dangling_refs.mzML");
const VERSION: &str = env!("CARGO_PKG_VERSION");

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("mzpc-mzml-header-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Run the converter and return its log (stderr).
fn convert(input: &Path, output: &Path, extra: &[&str]) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_mzpeak-convert"))
        .arg(input)
        .arg("-o")
        .arg(output)
        .arg("--force")
        .args(extra)
        .env_remove("RUST_LOG")
        .output()
        .expect("failed to run mzpeak-convert");
    let log = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(out.status.success(), "mzpeak-convert {} -o {} {extra:?} failed: {}\n{log}", input.display(), output.display(), out.status);
    log
}

fn index_metadata(archive: &Path) -> serde_json::Value {
    let mut zip = zip::ZipArchive::new(File::open(archive).unwrap()).unwrap();
    let mut text = String::new();
    zip.by_name("mzpeak_index.json").unwrap().read_to_string(&mut text).unwrap();
    serde_json::from_str::<serde_json::Value>(&text).unwrap()["metadata"].clone()
}

fn ids(list: &serde_json::Value) -> Vec<String> {
    list.as_array().unwrap().iter().map(|e| e["id"].as_str().unwrap().to_string()).collect()
}

/// The header up to the run's start tag, as text.
fn header(mzml: &Path) -> String {
    let text = std::fs::read_to_string(mzml).unwrap();
    let run = text.find("<run ").expect("a run");
    text[..run + text[run..].find('>').unwrap() + 1].to_string()
}

fn read(mzml: &Path, what: &str) -> mzml_meta::Mzml {
    let m = mzml_meta::read(mzml);
    mzml_meta::assert_processing_contract(&m, what);
    mzml_meta::assert_header_contract(&m, what);
    m
}

/// An mzML source through an archive and out again: the export states the source files (and the
/// archive, last, never the default), the sample, the software, the instrument configuration with
/// its components and its software, the scan settings with their source file reference, and the run
/// under its own id with the source's default source file — what the direct export of the same
/// source states. Its processing chain is the source's default, then the conversion that wrote the
/// archive, then the export.
#[test]
fn an_archive_export_states_the_lists_the_archive_holds() {
    let dir = scratch("archive");
    let (archive, exported, direct) = (dir.join("tiny.mzpeak"), dir.join("tiny.archive.mzML"), dir.join("tiny.direct.mzML"));
    convert(Path::new(TINY), &archive, &[]);
    convert(&archive, &exported, &[]);
    convert(Path::new(TINY), &direct, &["--to", "mzml"]);
    let (a, d) = (read(&exported, "tiny → mzPeak → mzML"), read(&direct, "tiny → mzML"));

    assert_eq!(d.source_files, ["tiny1.yep", "tiny.wiff", "sf_parameters"]);
    assert_eq!(a.source_files, ["tiny1.yep", "tiny.wiff", "sf_parameters", "mzpeak_archive"]);
    assert_eq!(a.samples, d.samples);
    assert_eq!(a.samples, ["_x0032_0090101_x0020_-_x0020_Sample_x0020_1"], "the archive holds it as stated; a name already");
    assert_eq!(a.softwares[..3], d.softwares[..3]);
    assert_eq!(a.softwares[..3], [("Bioworks".into(), "3.3.1 sp1".into()), ("pwiz".into(), "1.0".into()), ("CompassXtract".into(), "2.0.5".into())]);
    for m in [&a, &d] {
        assert_eq!(m.configurations.len(), 1);
        let c = &m.configurations[0];
        assert_eq!((c.id.as_str(), c.components, c.software_ref.as_deref()), ("IC1", Some((3, 3)), Some("CompassXtract")));
        assert_eq!(m.scan_settings, ["tiny_x0020_scan_x0020_settings"]);
        assert_eq!(m.source_file_refs, ["sf_parameters"], "the export lists the file, so the reference stays");
        assert_eq!(m.run["id"], "Experiment_x0020_1", "the archive holds `Experiment 1`");
        assert_eq!(m.run["defaultSourceFileRef"], "tiny1.yep");
        assert_eq!(m.run["defaultInstrumentConfigurationRef"], "IC1");
    }
    let h = header(&exported);
    assert!(h.contains(r#"<cvParam accession="MS:1000554" cvRef="MS" name="LCQ Deca"/>"#), "the instrument model: {h}");
    assert!(h.contains(r#"<sourceFile id="mzpeak_archive" name="tiny.mzpeak" location="file://">"#) && h.contains("MS:1000569"), "the archive, with its SHA-1");

    // The chain: the source's default (`pwiz_processing`), the archive's conversion, the export.
    let steps = |m: &mzml_meta::Mzml| -> Vec<(String, Option<i64>)> {
        let default = m.spectrum_list_default.clone().flatten().unwrap();
        let (_, methods) = m.data_processings.iter().find(|(id, _)| *id == default).unwrap();
        methods.iter().map(|meth| (meth.software_ref.clone(), meth.order)).collect()
    };
    assert_eq!(steps(&d), [("pwiz".to_string(), Some(2)), ("mzpeak-convert".to_string(), Some(3))]);
    assert_eq!(steps(&a), [("pwiz".to_string(), Some(2)), ("mzpeak-convert".to_string(), Some(3)), ("mzpeak-convert".to_string(), Some(4))]);
    let listed: Vec<&str> = a.data_processings.iter().map(|(id, _)| id.as_str()).collect();
    assert_eq!(listed, ["mzpeak_convert_to_mzml", "CompassXtract_x0020_processing", "pwiz_processing", "mzpeak_convert_conversion"]);
    // cvParams before userParams in every method, as the schema orders them.
    for method in h.split("<processingMethod").skip(1) {
        let method = &method[..method.find("</processingMethod>").unwrap()];
        let first_user = method.find("<userParam").unwrap_or(method.len());
        assert!(method.rfind("<cvParam").is_none_or(|cv| cv < first_user), "a cvParam after a userParam: {method}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// A run of two analyzers (small.RAW: an LTQ-FT, FTMS and ITMS scans): the archive's export
/// declares both configurations and every scan names the one it was acquired on, as in the direct
/// export; the run is the file's, dated.
#[test]
fn an_archive_export_declares_every_configuration_its_scans_name() {
    let dir = scratch("two-analyzers");
    let (archive, exported, direct) = (dir.join("small.mzpeak"), dir.join("small.archive.mzML"), dir.join("small.direct.mzML"));
    convert(Path::new(THERMO), &archive, &[]);
    convert(&archive, &exported, &[]);
    convert(Path::new(THERMO), &direct, &["--to", "mzml"]);
    let (a, d) = (read(&exported, "small.RAW → mzPeak → mzML"), read(&direct, "small.RAW → mzML"));
    let names = |m: &mzml_meta::Mzml| m.configurations.iter().map(|c| c.id.clone()).collect::<Vec<_>>();
    assert_eq!(names(&d), ["IC1", "IC2"]);
    assert_eq!(names(&a), ["IC1", "IC2"]);
    assert!(d.scan_configurations.iter().any(|c| c.as_deref() == Some("IC2")), "the fixture has scans of the second analyzer");
    assert_eq!(a.scan_configurations, d.scan_configurations, "each scan under the configuration it was acquired on");
    for m in [&a, &d] {
        assert_eq!(m.run["id"], "small");
        assert_eq!(m.run["defaultSourceFileRef"], "RAW1");
        assert_eq!(m.run["startTimeStamp"], "2005-07-20T14:44:22.377Z", "{:?}", m.run);
        assert!(m.configurations.iter().all(|c| c.components.is_some_and(|(n, _)| n >= 3)), "{:?}", m.configurations);
    }
    assert_eq!(a.samples, d.samples);
    assert_eq!(a.softwares[0], d.softwares[0], "the acquisition software");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A run whose name is not an XML name — it starts with a digit and holds a space — is written
/// escaped as ProteoWizard escapes it, by the direct export (the Thermo reader names the run after
/// the file) and by the archive's (which holds the name plain); converting the export back decodes it.
#[test]
fn a_run_id_that_is_not_an_xml_name_is_escaped() {
    let dir = scratch("run-id");
    let raw = dir.join("2 small.RAW");
    std::fs::copy(THERMO, &raw).unwrap();
    let (archive, exported, direct, back) = (dir.join("a.mzpeak"), dir.join("a.mzML"), dir.join("d.mzML"), dir.join("back.mzpeak"));
    let cap = ["--to", "mzml"];
    convert(&raw, &direct, &cap);
    convert(&raw, &archive, &[]);
    convert(&archive, &exported, &[]);
    assert_eq!(index_metadata(&archive)["run"]["id"], "2 small");
    for (mzml, what) in [(&direct, "direct"), (&exported, "archive")] {
        let m = read(mzml, what);
        assert_eq!(m.run["id"], "_x0032__x0020_small", "{what}");
    }
    convert(&exported, &back, &[]);
    assert_eq!(index_metadata(&back)["run"]["id"], "2 small");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A start time the source states without a zone: `xs:dateTime` has that form, so the direct export
/// writes the clock as stated (mzdata discards it: its run model holds a zoned time only), and so
/// does the export of an archive that keeps the clock in its `acquisition_time` block. A zoned
/// time is written in RFC 3339.
#[test]
fn a_start_time_is_written_zoned_or_as_the_clock_the_source_states() {
    let dir = scratch("start-time");
    let direct = dir.join("tiny.mzML");
    convert(Path::new(TINY), &direct, &["--to", "mzml"]);
    assert_eq!(read(&direct, "tiny → mzML").run["startTimeStamp"], "2007-06-27T15:23:45.00035");

    let zoned = dir.join("zoned.src.mzML");
    let text = String::from_utf8_lossy(&std::fs::read(TINY).unwrap()).replace(r#"startTimeStamp="2007-06-27T15:23:45.00035""#, r#"startTimeStamp="2007-06-27T15:23:45.5+02:00""#);
    std::fs::write(&zoned, text).unwrap();
    let (archive, out, exported) = (dir.join("zoned.mzpeak"), dir.join("zoned.mzML"), dir.join("zoned.archive.mzML"));
    convert(&zoned, &out, &["--to", "mzml"]);
    convert(&zoned, &archive, &[]);
    convert(&archive, &exported, &[]);
    for (mzml, what) in [(&out, "zoned → mzML"), (&exported, "zoned → mzPeak → mzML")] {
        assert_eq!(read(mzml, what).run["startTimeStamp"], "2007-06-27T15:23:45.500+02:00", "{what}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// The direct export of an mzML whose references dangle runs the archive lane's repair: scan=2's
/// `IC9` is written under the run's default configuration, the run's defaults name the first entry
/// of their lists, IC1's `softwareRef` to nothing is left out, the method naming software `ghost`
/// names `software_not_stated`, the self-closing `exporter` is back in the software list — and one
/// warning counts them. Through 0.17.0-rc.1 the export named `IC10` and `IC8`, declared neither,
/// and kept `acquisition` and `ghost` as references to nothing.
#[test]
fn a_direct_mzml_export_drops_the_references_that_name_nothing() {
    let dir = scratch("dangling");
    let out = dir.join("dangling.mzML");
    let log = convert(Path::new(DANGLING), &out, &["--to", "mzml"]);
    let m = read(&out, "dangling_refs → mzML");
    assert_eq!(m.configurations.len(), 1);
    assert_eq!((m.configurations[0].software_ref.as_deref(), m.configurations[0].components), (None, None), "{:?}", m.configurations);
    assert_eq!(m.scan_configurations, [Some("IC1".to_string()), Some("IC1".to_string())]);
    assert_eq!(m.run["defaultInstrumentConfigurationRef"], "IC1");
    assert_eq!(m.run["defaultSourceFileRef"], "sf1");
    let software: Vec<&str> = m.softwares.iter().map(|(id, _)| id.as_str()).collect();
    assert_eq!(software, ["pwiz", "exporter", "mzpeak-convert", "software_not_stated"]);
    let named = |id: &str| m.data_processings.iter().find(|(i, _)| i == id).unwrap().1[0].software_ref.clone();
    assert_eq!((named("export"), named("ghost_processing")), ("exporter".to_string(), "software_not_stated".to_string()));
    let warnings: Vec<&str> = log.lines().filter(|l| l.contains("dropped references")).collect();
    assert_eq!(warnings.len(), 1, "{log}");
    assert!(
        warnings[0].contains(
            "1 defaultDataProcessingRef (dp1), 1 defaultInstrumentConfigurationRef (IC7), \
             1 defaultSourceFileRef (sf9), 1 instrumentConfigurationRef (IC9), 2 softwareRef (acquisition, ghost)"
        ) && warnings[0].contains("mzML has no transformations list"),
        "{}",
        warnings[0]
    );

    // The archive of the same source, exported: the same header but for the archive's own entries.
    let (archive, exported) = (dir.join("dangling.mzpeak"), dir.join("dangling.archive.mzML"));
    convert(Path::new(DANGLING), &archive, &[]);
    let log = convert(&archive, &exported, &[]);
    assert!(!log.contains("dropped references"), "the archive holds no reference that dangles: {log}");
    let a = read(&exported, "dangling_refs → mzPeak → mzML");
    assert_eq!(a.scan_configurations, m.scan_configurations);
    assert_eq!(a.configurations[0].software_ref, None);
    assert_eq!(a.run["defaultSourceFileRef"], "sf1");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A configuration the source states as a self-closing element, which mzdata skips, and that only
/// scans name: an mzML declares its configurations before its scans, so the direct export numbers
/// it ahead of them and scan=2 keeps it; scan=1's `IC1` and a scan naming nothing are told apart.
#[test]
fn a_direct_mzml_export_declares_a_configuration_only_scans_name() {
    let dir = scratch("selfclosing");
    let mut source = std::fs::read_to_string(DANGLING).unwrap();
    for (from, to) in [
        (r#"<softwareRef ref="acquisition"/>"#, r#"<softwareRef ref="pwiz"/>"#),
        (r#"softwareRef="ghost""#, r#"softwareRef="pwiz""#),
        (r#"defaultInstrumentConfigurationRef="IC7""#, r#"defaultInstrumentConfigurationRef="IC1""#),
        (r#"defaultDataProcessingRef="dp1""#, r#"defaultDataProcessingRef="pwiz_conversion""#),
        (r#"defaultSourceFileRef="sf9""#, r#"defaultSourceFileRef="sf1""#),
        (r#"instrumentConfigurationRef="IC9""#, r#"instrumentConfigurationRef="IC2""#),
        (
            "    </instrumentConfiguration>\n  </instrumentConfigurationList>",
            "    </instrumentConfiguration>\n    <instrumentConfiguration id=\"IC2\"/>\n  </instrumentConfigurationList>",
        ),
    ] {
        assert!(source.contains(from), "{from}");
        source = source.replacen(from, to, 1);
    }
    let (src, out) = (dir.join("self_closing.mzML"), dir.join("out.mzML"));
    std::fs::write(&src, source).unwrap();
    let log = convert(&src, &out, &["--to", "mzml"]);
    let m = read(&out, "self-closing configuration → mzML");
    let names: Vec<&str> = m.configurations.iter().map(|c| c.id.as_str()).collect();
    assert_eq!(names, ["IC1", "IC2"]);
    assert_eq!(m.scan_configurations, [Some("IC1".to_string()), Some("IC2".to_string())]);
    assert!(!log.contains("dropped references") && !log.contains("does not declare"), "{log}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Round trips keep the software and processing ids unique: an export converted back records its
/// conversion beside the export's (one `mzpeak-convert` entry for the one version), exporting that
/// again and filtering a filtered archive number their steps, and each step names software of the
/// version that ran.
#[test]
fn software_and_processing_ids_stay_unique_across_round_trips() {
    let dir = scratch("round-trip");
    let p = |name: &str| dir.join(name);
    convert(Path::new(TINY), &p("a.mzpeak"), &[]);
    convert(&p("a.mzpeak"), &p("a.mzML"), &[]);
    convert(&p("a.mzML"), &p("b.mzpeak"), &[]);
    let md = index_metadata(&p("b.mzpeak"));
    let unique = |list: &serde_json::Value, what: &str| {
        let ids = ids(list);
        let mut seen = std::collections::BTreeSet::new();
        assert!(ids.iter().all(|id| seen.insert(id.clone())), "{what}: {ids:?}");
        ids
    };
    let software = unique(&md["software_list"], "software of export → archive");
    assert_eq!(software.iter().filter(|id| id.starts_with("mzpeak-convert")).count(), 1, "{software:?}");
    let processing = unique(&md["data_processing_method_list"], "processing of export → archive");
    assert!(processing.contains(&"mzpeak_convert_conversion".to_string()) && processing.contains(&"mzpeak_convert_conversion_2".to_string()), "{processing:?}");
    assert_eq!(md["run"]["id"], "Experiment 1");

    // Out again: the chain holds each step once.
    convert(&p("b.mzpeak"), &p("b.mzML"), &[]);
    let m = read(&p("b.mzML"), "mzML → mzPeak → mzML → mzPeak → mzML");
    let default = m.spectrum_list_default.clone().flatten().unwrap();
    assert_eq!(default, "mzpeak_convert_to_mzml_2");
    let chain: Vec<Option<i64>> = m.data_processings.iter().find(|(id, _)| *id == default).unwrap().1.iter().map(|meth| meth.order).collect();
    assert_eq!(chain, [Some(2), Some(3), Some(4), Some(5), Some(6)], "pwiz, conversion, export, conversion, export");
    assert_eq!(m.softwares.iter().filter(|(id, _)| id.starts_with("mzpeak-convert")).count(), 1, "{:?}", m.softwares);

    // A filter of a filter.
    convert(&p("a.mzpeak"), &p("f1.mzpeak"), &["--ms-level", "1,2"]);
    convert(&p("f1.mzpeak"), &p("f2.mzpeak"), &["--rt", "0-100000"]);
    let md = index_metadata(&p("f2.mzpeak"));
    let processing = unique(&md["data_processing_method_list"], "processing of a filter of a filter");
    assert_eq!(processing[processing.len() - 2..], ["mzpeak_convert_filter", "mzpeak_convert_filter_2"]);
    unique(&md["software_list"], "software of a filter of a filter");
    for dp in md["data_processing_method_list"].as_array().unwrap().iter().filter(|dp| dp["id"].as_str().unwrap().starts_with("mzpeak_convert_filter")) {
        let named = dp["methods"][0]["software_reference"].as_str().unwrap();
        let entry = md["software_list"].as_array().unwrap().iter().find(|s| s["id"] == named).unwrap_or_else(|| panic!("{named} is not listed"));
        assert_eq!(entry["version"], VERSION);
    }
    // And its export states both filters in its chain, after the conversion.
    convert(&p("f2.mzpeak"), &p("f2.mzML"), &[]);
    let m = read(&p("f2.mzML"), "filtered twice → mzML");
    let default = m.spectrum_list_default.clone().flatten().unwrap();
    let chain = &m.data_processings.iter().find(|(id, _)| *id == default).unwrap().1;
    assert_eq!(chain.len(), 5, "pwiz, conversion, filter, filter, export: {chain:?}");
    assert!(chain[2].accessions.contains(&"MS:1001486".to_string()) && chain[3].accessions.contains(&"MS:1001486".to_string()), "{chain:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

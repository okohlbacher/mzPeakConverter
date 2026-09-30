//! What an archive → mzML export (`filter_mzpeak_to_mzml`) states against what the direct `--to mzml`
//! lane states for the same source, element by element. Through 0.16.0:
//!
//!   * the TIC and base-peak chromatogram the mzML writer sums over the spectra came out in spectrum
//!     order, so a run whose spectra are not in time order got an unsorted time array
//!     (`tiny.pwiz.1.1`: 5.8905, 5.9905, 0.0, 0.7008) — on the direct lane, and on an export of an
//!     archive that holds no chromatogram of that kind;
//!   * a chromatogram's polarity (`negative scan` on every SRM trace of a negative-mode run) was
//!     written on neither route;
//!   * a chromatogram's precursor came back with an isolation window of target 0 (the SRM trace of
//!     `tiny.pwiz.1.1` states 456.7) and no activation: the vendored reader looked the window's
//!     columns up in an empty column mapping;
//!   * every precursor, a spectrum's or a chromatogram's, came back with no dissociation method and a
//!     collision energy of 0: the reader read the activation's `parameters` list alone, and the
//!     writer keeps both in columns of their own;
//!   * a selected ion's and a scan's 1/K0 (MS:1002815) were named `inverse reduced ion mobility drift
//!     time`, a scan stated it twice, and every spectrum stated its `scan start time` a second time,
//!     at the spectrum level.
//!
//! The corpus-gated test runs the comparison the fixtures stand in for on PXD059079 2485.d.

use std::path::{Path, PathBuf};
use std::process::Command;

use mzdata::prelude::*;
use mzdata::spectrum::{ArrayType, Chromatogram};

#[path = "common/corpus.rs"]
mod corpus;

const TINY: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/tiny.pwiz.1.1.mzML");
const DOT_D: &str = "ims-examples/PXD059079/20230830_100SPD_NCI7_0p12ng_HS_01_S1-B1_1_2485.d";

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("mzpc-export-fidelity-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Run the converter and return its log (stderr).
fn convert(input: &Path, output: &Path, extra: &[&str], envs: &[(&str, &str)]) -> String {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_mzpeak-convert"));
    cmd.arg(input).arg("-o").arg(output).arg("--force").args(extra);
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("failed to run mzpeak-convert");
    let log = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(out.status.success(), "mzpeak-convert {} -o {} {extra:?} failed: {}\n{log}", input.display(), output.display(), out.status);
    log
}

fn chromatograms(mzml: &Path) -> Vec<Chromatogram> {
    let mut reader = mzdata::io::mzml::MzMLReader::open_path(mzml).unwrap();
    let n = reader.count_chromatograms();
    (0..n).map(|i| reader.get_chromatogram_by_index(i).unwrap()).collect()
}

fn times(c: &Chromatogram) -> Vec<f64> {
    c.arrays.get(&ArrayType::TimeArray).unwrap().to_f64().unwrap().to_vec()
}

fn intensities(c: &Chromatogram) -> Vec<f32> {
    c.arrays.get(&ArrayType::IntensityArray).unwrap().to_f32().unwrap().to_vec()
}

/// Each `<tag ...>` element of an mzML's text, up to its closing tag.
fn elements<'a>(xml: &'a str, tag: &str) -> Vec<&'a str> {
    let (open, close) = (format!("<{tag}"), format!("</{tag}>"));
    xml.match_indices(&open)
        .filter(|(at, _)| xml[at + open.len()..].starts_with([' ', '>']))
        .map(|(at, _)| &xml[at..at + xml[at..].find(&close).unwrap()])
        .collect()
}

/// The `<spectrum>` with this id.
fn spectrum_element<'a>(xml: &'a str, id: &str) -> &'a str {
    let key = format!(r#"id="{id}""#);
    elements(xml, "spectrum").into_iter().find(|s| s.contains(&key)).unwrap_or_else(|| panic!("no spectrum {id}"))
}

/// The spectrum-level part of a `<spectrum>`: what precedes its lists.
fn spectrum_head(spectrum: &str) -> &str {
    &spectrum[..spectrum.find("<scanList").unwrap_or(spectrum.len())]
}

/// `(target, the chromatogram's first activation method, collision energy)` of `id`'s first precursor.
fn chromatogram_precursor(chroms: &[Chromatogram], id: &str) -> (f32, Option<String>, f32) {
    let c = chroms.iter().find(|c| c.id() == id).unwrap_or_else(|| panic!("no chromatogram {id}"));
    let p = c.precursor().unwrap_or_else(|| panic!("chromatogram {id} has no precursor"));
    (p.isolation_window.target, p.activation.method().map(|m| m.to_param().name.to_string()), p.activation.energy)
}

/// (1) `--to mzml`: tiny.pwiz.1.1 carries a TIC but no base-peak chromatogram, so the writer sums
/// one — in spectrum order, 5.8905, 5.9905, 0.0, 0.7008 min, through 0.16.0. Now in time order, each
/// point keeping its intensity (the spectra's stated base-peak intensities).
#[test]
fn the_summed_base_peak_chromatogram_is_in_time_order() {
    let dir = scratch("direct-bic");
    let mzml = dir.join("direct.mzML");
    convert(Path::new(TINY), &mzml, &[], &[]);
    let chroms = chromatograms(&mzml);
    let bic = chroms.iter().find(|c| c.id() == "BIC").expect("the writer's base-peak chromatogram");
    assert_eq!(times(bic), [0.0, 0.7008333333333333, 5.8905, 5.9905]);
    assert_eq!(intensities(bic), [0.0, 42.0, 120053.0, 23433.0]);
    let _ = std::fs::remove_dir_all(&dir);
}

/// (1) The archive route: an archive written with `--no-chromatograms` holds the source's TIC and SRM
/// trace but no base-peak chromatogram, so the export sums one, in time order; the stored two go
/// across as the archive holds them (times in minutes, as stored).
#[test]
fn an_archive_export_carries_the_stored_chromatograms_and_sorts_what_it_sums() {
    let dir = scratch("archive-bic");
    let archive = dir.join("tiny.mzpeak");
    convert(Path::new(TINY), &archive, &["--no-chromatograms"], &[]);
    let mut stored: Vec<(String, Vec<f64>, Vec<f32>)> = Vec::new();
    {
        let mut reader = mzpeak_prototyping::MzPeakReader::new(&archive).unwrap();
        for i in 0..mzdata::prelude::ChromatogramSource::count_chromatograms(&reader) {
            let c = mzdata::prelude::ChromatogramSource::get_chromatogram_by_index(&mut reader, i).unwrap();
            stored.push((c.id().to_string(), times(&c), intensities(&c)));
        }
    }
    assert_eq!(stored.iter().map(|(id, _, _)| id.as_str()).collect::<Vec<_>>(), ["tic", "sic"]);
    let mzml = dir.join("export.mzML");
    convert(&archive, &mzml, &[], &[]);
    let chroms = chromatograms(&mzml);
    assert_eq!(chroms.iter().map(|c| c.id()).collect::<Vec<_>>(), ["tic", "sic", "BIC"]);
    for (id, t, i) in &stored {
        let c = chroms.iter().find(|c| c.id() == id).unwrap();
        assert_eq!((&times(c), &intensities(c)), (t, i), "{id}: not the archive's arrays");
    }
    let bic = chroms.iter().find(|c| c.id() == "BIC").unwrap();
    assert_eq!(times(bic), [0.0, 0.7008333333333333, 5.8905, 5.9905]);
    let _ = std::fs::remove_dir_all(&dir);
}

/// (1) A chromatogram's polarity goes across: ProteoWizard states `negative scan` on every SRM trace
/// of a negative-mode run (`MRM Neg C5`), and the archive stores it (`scan_polarity` -1), but mzdata's
/// writer writes no chromatogram's polarity, so both routes dropped it. tiny.pwiz.1.1's SRM trace,
/// made negative; its TIC and the archive's base-peak trace state none, and must not gain one (the
/// vendored reader read a null polarity as whatever value slot sat under it: -1, here).
#[test]
fn a_chromatograms_polarity_goes_across() {
    let dir = scratch("polarity");
    let src = std::fs::read_to_string(TINY).unwrap();
    let sic = r#"<cvParam cvRef="MS" accession="MS:1000627" name="selected ion current chromatogram" value=""/>"#;
    assert_eq!(src.matches(sic).count(), 1, "the fixture's SRM trace moved");
    let negative = src.replacen(sic, &format!(r#"{sic}<cvParam cvRef="MS" accession="MS:1000129" name="negative scan" value=""/>"#), 1);
    let source = dir.join("negative.mzML");
    std::fs::write(&source, negative).unwrap();
    let (archive, export, direct) = (dir.join("negative.mzpeak"), dir.join("export.mzML"), dir.join("direct.mzML"));
    convert(&source, &archive, &[], &[]);
    convert(&archive, &export, &[], &[]);
    convert(&source, &direct, &[], &[]);
    for (route, mzml) in [("export", &export), ("--to mzml", &direct)] {
        let xml = std::fs::read_to_string(mzml).unwrap();
        for c in elements(&xml, "chromatogram") {
            let negative = c.contains(r#"accession="MS:1000129""#);
            assert_eq!(negative, c.contains(r#"id="sic""#), "{route}: `negative scan` on the SRM trace alone:\n{c}");
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// (2) tiny.pwiz.1.1's selected ion current chromatogram states a target-only precursor window
/// (456.7) activated by CID. The archive stores both; the export wrote a window of target 0 (and
/// offsets 0) and no activation. The direct lane is the reference.
#[test]
fn an_exported_chromatograms_precursor_keeps_its_window_and_activation() {
    let dir = scratch("sic");
    let (archive, export, direct) = (dir.join("tiny.mzpeak"), dir.join("export.mzML"), dir.join("direct.mzML"));
    convert(Path::new(TINY), &archive, &[], &[]);
    convert(&archive, &export, &[], &[]);
    convert(Path::new(TINY), &direct, &[], &[]);
    let (target, method, _) = chromatogram_precursor(&chromatograms(&export), "sic");
    assert_eq!(chromatogram_precursor(&chromatograms(&direct), "sic").0, target, "export vs --to mzml");
    assert!((target - 456.7).abs() < 1e-3, "target {target}");
    assert_eq!(method.as_deref(), Some("collision-induced dissociation"));
    // Target-only stays target-only: no offsets are written for a window of unknown width.
    let xml = std::fs::read_to_string(&export).unwrap();
    let sic = elements(&xml, "chromatogram").into_iter().find(|c| c.contains(r#"id="sic""#)).unwrap();
    assert!(!sic.contains("MS:1000828") && !sic.contains("MS:1000829"), "offsets on a target-only window:\n{sic}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// (2)/(3c) A spectrum's activation: tiny.pwiz.1.1's `scan=20` states CID at 35 eV, which the
/// archive stores in `activation.dissociation_method` / `collision_energy`; the export wrote
/// `collision energy 0` and no method.
#[test]
fn an_exported_spectrums_activation_keeps_its_method_and_energy() {
    let dir = scratch("activation");
    let (archive, export) = (dir.join("tiny.mzpeak"), dir.join("export.mzML"));
    convert(Path::new(TINY), &archive, &[], &[]);
    convert(&archive, &export, &[], &[]);
    let mut reader = mzdata::io::mzml::MzMLReader::open_path(&export).unwrap();
    let spec = reader.get_spectrum_by_id("scan=20").unwrap();
    let activation = &spec.precursor().expect("scan=20's precursor").activation;
    assert_eq!(activation.method().map(|m| m.to_param().name.to_string()).as_deref(), Some("collision-induced dissociation"));
    assert_eq!(activation.energy, 35.0);
    let _ = std::fs::remove_dir_all(&dir);
}

/// tiny.pwiz.1.1 with a diaPASEF window's 1/K0 on `scan=20`'s scan and selected ion, as ProteoWizard
/// and the `.d → mzML` lane state it. The writer keeps the scan's copy in its `ion_mobility_value`
/// column and in its `parameters`, as it does on the `--no-ims-compact` timsTOF lane.
fn tiny_with_mobility(dir: &Path) -> PathBuf {
    const K0: &str = r#"<cvParam cvRef="MS" accession="MS:1002815" name="inverse reduced ion mobility" value="1.3323874701174356" unitCvRef="MS" unitAccession="MS:1002814" unitName="volt-second per square centimeter"/>"#;
    let src = std::fs::read_to_string(TINY).unwrap();
    let scan = r#"<cvParam cvRef="MS" accession="MS:1000616" name="preset scan configuration" value="4"/>"#;
    let ion = r#"<cvParam cvRef="MS" accession="MS:1000041" name="charge state" value="2"/>"#;
    assert_eq!((src.matches(scan).count(), src.matches(ion).count()), (1, 1), "the fixture's scan=20 moved");
    let patched = src.replacen(scan, &format!("{scan}\n{K0}"), 1).replacen(ion, &format!("{ion}\n{K0}"), 1);
    let path = dir.join("tiny_k0.mzML");
    std::fs::write(&path, patched).unwrap();
    path
}

/// (3a, 3b) A selected ion's and a scan's 1/K0 go out under PSI-MS's name for MS:1002815, `inverse
/// reduced ion mobility`, once per element; and no spectrum states a `scan start time` of its own
/// beside its scan's. The export named both `… drift time` and wrote the scan's twice.
#[test]
fn an_exported_1_over_k0_is_named_as_psi_ms_names_it_and_stated_once() {
    let dir = scratch("k0");
    let source = tiny_with_mobility(&dir);
    let (archive, export, direct) = (dir.join("k0.mzpeak"), dir.join("export.mzML"), dir.join("direct.mzML"));
    convert(&source, &archive, &[], &[]);
    convert(&archive, &export, &[], &[]);
    convert(&source, &direct, &[], &[]);
    for (route, mzml) in [("export", &export), ("--to mzml", &direct)] {
        let xml = std::fs::read_to_string(mzml).unwrap();
        let spectrum = spectrum_element(&xml, "scan=20");
        let scan = elements(spectrum, "scan")[0];
        let ion = elements(spectrum, "selectedIon")[0];
        for (element, text) in [("scan", scan), ("selectedIon", ion)] {
            assert_eq!(text.matches(r#"accession="MS:1002815""#).count(), 1, "{route}: {element} states MS:1002815 once:\n{text}");
            assert!(text.contains(r#"accession="MS:1002815" cvRef="MS" name="inverse reduced ion mobility" "#), "{route}: {element}:\n{text}");
        }
        for s in elements(&xml, "spectrum") {
            assert!(!spectrum_head(s).contains("MS:1000016"), "{route}: a spectrum-level scan start time:\n{}", spectrum_head(s));
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// PXD059079 2485.d through both kinds of archive against its `--to mzml` export (40 spectra): every
/// precursor's activation (CID at the window's energy), each frame's ion injection time and scan
/// window as the `.d` states them, no 1/K0 misnamed, each `<scan>`'s once. What stays different, by
/// design: an ims-compact archive holds whole frames (no per-window scan 1/K0 or limits) and not
/// mzdata's per-window TIC/BPC pair, which repeats each frame's summed and maximum intensity once per
/// window (so its export has 28 chromatograms, the `.d`'s 30: HyStar's 25 pump traces and 3 MS
/// traces, and that pair).
#[test]
#[ignore = "needs the 142 MB 2485.d timsTOF corpus fixture (MZPEAK_CORPUS); run with --include-ignored"]
fn a_timstof_archive_exports_the_precursors_the_d_exports() {
    let Some(dot_d) = corpus::corpus_path(DOT_D) else { return };
    let dir = scratch("tdf");
    let cap = [("MZPC_MAX_SPECTRA", "40")];
    let direct = dir.join("d.mzML");
    convert(&dot_d, &direct, &["--to", "mzml"], &cap);
    let activation = |mzml: &Path| -> Vec<(String, u32, Option<String>, f32)> {
        let reader = mzdata::io::mzml::MzMLReader::open_path(mzml).unwrap();
        let mut out = Vec::new();
        for s in reader {
            let frame = s.id().split_whitespace().find(|t| t.starts_with("frame=")).unwrap().to_string();
            for p in s.precursor_iter() {
                let method = p.activation.method().map(|m| m.to_param().name.to_string());
                out.push((frame.clone(), (p.isolation_window.target * 1000.0).round() as u32, method, p.activation.energy));
            }
        }
        out.sort_by(|a, b| (&a.0, a.1).cmp(&(&b.0, b.1)));
        out
    };
    // Each frame's scan: its injection (accumulation) time and its scan windows, from the frame's first
    // spectrum.
    type Scan = (f32, Vec<(f32, f32)>);
    let scans = |mzml: &Path| -> std::collections::BTreeMap<String, Scan> {
        let reader = mzdata::io::mzml::MzMLReader::open_path(mzml).unwrap();
        let mut out = std::collections::BTreeMap::new();
        for s in reader {
            let frame = s.id().split_whitespace().find(|t| t.starts_with("frame=")).unwrap().to_string();
            let scan = s.acquisition().first_scan().unwrap();
            let windows = scan.scan_windows.iter().map(|w| (w.lower_bound, w.upper_bound)).collect();
            out.entry(frame).or_insert((scan.injection_time, windows));
        }
        out
    };
    let reference = activation(&direct);
    assert!(!reference.is_empty() && reference.iter().all(|(_, _, m, e)| m.is_some() && *e > 0.0));
    let reference_scans = scans(&direct);
    for (label, extra) in [("--no-ims-compact", &["--no-ims-compact"][..]), ("ims-compact", &[][..])] {
        let (archive, export) = (dir.join(format!("{label}.mzpeak")), dir.join(format!("{label}.mzML")));
        convert(&dot_d, &archive, extra, &cap);
        convert(&archive, &export, &[], &[]);
        let got = activation(&export);
        // The ims-compact archive's 40 spectra are 40 FRAMES: more windows than the `.d`'s 40.
        let common: Vec<_> = got.iter().filter(|g| reference.iter().any(|r| (&r.0, r.1) == (&g.0, g.1))).collect();
        assert!(!common.is_empty(), "{label}: no precursor in common");
        for g in common {
            let r = reference.iter().find(|r| (&r.0, r.1) == (&g.0, g.1)).unwrap();
            assert_eq!(g, r, "{label}: activation differs from the .d's");
        }
        // An ims-compact archive held no frame's accumulation time or the acquisition range through
        // 0.16.0: its export stated `ion injection time 0` and no scan window.
        let got_scans = scans(&export);
        let common: Vec<_> = got_scans.keys().filter(|f| reference_scans.contains_key(*f)).collect();
        assert!(!common.is_empty(), "{label}: no frame in common");
        for f in common {
            assert_eq!(got_scans[f], reference_scans[f], "{label} {f}: scan differs from the .d's");
        }
        let xml = std::fs::read_to_string(&export).unwrap();
        assert!(!xml.contains("inverse reduced ion mobility drift time"), "{label}: MS:1002815 misnamed");
        for scan in elements(&xml, "scan") {
            assert!(scan.matches(r#"accession="MS:1002815""#).count() <= 1, "{label}: {scan}");
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

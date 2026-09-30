//! What an archive → mzML export (`filter_mzpeak_to_mzml`) states against what the direct `--to mzml`
//! lane states for the same source, element by element. Through 0.16.0:
//!
//!   * the TIC and base-peak chromatogram the mzML writer sums over the spectra came out in spectrum
//!     order, so a run whose spectra are not in time order got an unsorted time array
//!     (`tiny.pwiz.1.1`: 5.8905, 5.9905, 0.0, 0.7008) — on the direct lane, and on an export of an
//!     archive that holds no chromatogram of that kind;
//!   * a chromatogram's polarity (`negative scan` on every SRM trace of a negative-mode run) was
//!     written on neither route.

use std::path::{Path, PathBuf};
use std::process::Command;

use mzdata::prelude::*;
use mzdata::spectrum::{ArrayType, Chromatogram};

const TINY: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/tiny.pwiz.1.1.mzML");

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

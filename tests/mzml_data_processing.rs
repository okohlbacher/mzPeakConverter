//! Every mzML this tool writes meets mzML 1.1's processing contract, with its own `Conversion to
//! mzML` step as the default processing ([`mzml_meta::assert_processing_contract`]); a timsTOF
//! export carries each diaPASEF window's 1/K0 limits in order, on the vendor model its mobility
//! arrays use, bracketing them exactly; and an archive exports each peak's ion mobility.
//!
//! Through 0.13.0 the export of a raw file or an archive wrote `<dataProcessingList count="0">` and no
//! `defaultDataProcessingRef`, which stock OpenMS 3.5 refuses ("Required attribute
//! 'defaultDataProcessingRef' not present!"); an mzML source's spectra moved to whatever processing
//! came first; a TDF export wrote every MS2 spectrum's `ion mobility lower limit` above its
//! `upper limit` (1.3674 / 1.1931), against which OpenSWATH's strict `lower < IM < upper`
//! precursor test matched nothing; and an archive's export dropped the mobility of every peak.
//!
//! Lanes run here: `convert_to_mzml` on an mzML (a source with processing of its own), on this
//! tool's own mzML (ids already taken), on a Thermo `.raw` (a source with none), on a file whose
//! name is not Unicode (Linux) and — corpus-gated — on a timsTOF `.d`; `filter_mzpeak_to_mzml` on
//! an archive, on one holding a mobility array per peak, and — corpus-gated — on both kinds of
//! timsTOF archive. `write_native_mzml` (TSF, BAF, the Windows vendor readers) and
//! `write_agilent_profile_mzml` need inputs no fixture holds; they share the prologue
//! (`fixup_mzml_run_metadata`) that the unit test
//! `mzml_prologue_records_the_conversion_step_on_every_source` writes and reads back.

use std::path::{Path, PathBuf};
use std::process::Command;

#[path = "common/corpus.rs"]
mod corpus;
#[path = "common/mzml_meta.rs"]
mod mzml_meta;

const TINY: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/tiny.pwiz.1.1.mzML");
const THERMO: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/small.RAW");
const PASEF: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/pasef_combineims_centroid.pwiz.mzML");
const DOT_D: &str = "ims-examples/PXD059079/20230830_100SPD_NCI7_0p12ng_HS_01_S1-B1_1_2485.d";

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("mzpc-mzml-dp-{}-{name}", std::process::id()));
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
    assert!(
        out.status.success(),
        "mzpeak-convert {} -o {} {extra:?} failed: {}\n{log}",
        input.display(),
        output.display(),
        out.status,
    );
    log
}

fn dp_ids(m: &mzml_meta::Mzml) -> Vec<&str> {
    m.data_processings.iter().map(|(id, _)| id.as_str()).collect()
}

fn methods(m: &mzml_meta::Mzml, id: &str) -> Vec<(String, Option<i64>)> {
    let (_, methods) = m.data_processings.iter().find(|(i, _)| i == id).unwrap_or_else(|| panic!("no dataProcessing {id}"));
    methods.iter().map(|meth| (meth.software_ref.clone(), meth.order)).collect()
}

/// An mzML source: the step extends the processing the source's spectra point at by default
/// (`pwiz_processing`, the SECOND entry of tiny.pwiz.1.1.mzML — 0.13.0 moved them to the first,
/// `CompassXtract_x0020_processing`) and becomes the default of both lists; the source's entries stay,
/// after it, and every element-level reference still resolves. Exporting that export again extends
/// the chain under a fresh id and reuses the software entry.
#[test]
fn an_mzml_source_s_default_processing_is_extended_by_the_step() {
    let dir = scratch("mzml");
    let once = dir.join("once.mzML");
    convert(Path::new(TINY), &once, &["--to", "mzml"], &[]);
    let m = mzml_meta::read(&once);
    let ours = mzml_meta::assert_processing_contract(&m, "tiny → mzML");
    assert_eq!(dp_ids(&m), [ours.as_str(), "CompassXtract_x0020_processing", "pwiz_processing"]);
    assert_eq!(m.spectrum_list_default, Some(Some(ours.clone())));
    assert_eq!(m.chromatogram_list_default, Some(Some(ours.clone())));
    assert_eq!(methods(&m, &ours), [("pwiz".to_string(), Some(2)), ("mzpeak-convert".to_string(), Some(3))]);
    // The source's redundant references to its own default are left out, so those arrays inherit
    // the step that extends it; its references to another entry stay.
    assert!(!m.data_processing_refs.iter().any(|(_, r)| r == "pwiz_processing"), "{:?}", m.data_processing_refs);

    let twice = dir.join("twice.mzML");
    convert(&once, &twice, &["--to", "mzml"], &[]);
    let m = mzml_meta::read(&twice);
    let again = mzml_meta::assert_processing_contract(&m, "tiny → mzML → mzML");
    assert_ne!(ours, again);
    assert_eq!(dp_ids(&m), [again.as_str(), ours.as_str(), "CompassXtract_x0020_processing", "pwiz_processing"]);
    assert_eq!(m.spectrum_list_default, Some(Some(again.clone())));
    assert_eq!(
        methods(&m, &again),
        [("pwiz".to_string(), Some(2)), ("mzpeak-convert".to_string(), Some(3)), ("mzpeak-convert".to_string(), Some(4))]
    );
    let ours_sw: Vec<_> = m.softwares.iter().filter(|(id, _)| id.starts_with("mzpeak-convert")).collect();
    assert_eq!(ours_sw.len(), 1, "one software entry per version: {:?}", m.softwares);
    let _ = std::fs::remove_dir_all(&dir);
}

/// The archive export (`filter_mzpeak_to_mzml`). The vendored archive reader restores none of the
/// lists the archive's index holds (software, processing, instruments), so 0.13.0 wrote
/// `<softwareList count="0">` and `<dataProcessingList count="0">` here too; the export's own step
/// fills both, as the only entry.
#[test]
fn an_archive_export_records_the_mzml_conversion() {
    let dir = scratch("archive");
    let archive = dir.join("tiny.mzpeak");
    let mzml = dir.join("tiny.mzML");
    convert(Path::new(TINY), &archive, &[], &[]);
    convert(&archive, &mzml, &[], &[]);
    let m = mzml_meta::read(&mzml);
    let ours = mzml_meta::assert_processing_contract(&m, "tiny → mzPeak → mzML");
    assert_eq!(dp_ids(&m), [ours.as_str()]);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Every spectrum of `path` as mzdata reads it back.
fn spectra(path: &Path) -> Vec<mzdata::spectrum::MultiLayerSpectrum> {
    use mzdata::prelude::*;
    mzdata::io::mzml::MzMLReader::open_path(path).unwrap_or_else(|e| panic!("{}: {e}", path.display())).iter().collect()
}

fn array(spec: &mzdata::spectrum::MultiLayerSpectrum, kind: &mzdata::spectrum::ArrayType) -> Option<Vec<f64>> {
    use mzdata::prelude::ByteArrayView;
    spec.arrays.as_ref()?.get(kind).map(|a| a.to_f64().unwrap().to_vec())
}

/// An archive whose peak facet holds a 1/K0 per peak (a PASEF frame combined across its mobility
/// scans, here; every timsTOF archive) exports it as MS:1003006, value for value. The reader's peak
/// list has no room for it, and through 0.13.0 the export wrote m/z and intensity only.
#[test]
fn an_archive_export_keeps_the_peak_mobility_array() {
    use mzdata::spectrum::ArrayType;
    let im = ArrayType::MeanInverseReducedIonMobilityArray;
    let src = &spectra(Path::new(PASEF))[0];
    let (want_mz, want_im) = (array(src, &ArrayType::MZArray).unwrap(), array(src, &im).unwrap());
    let dir = scratch("pasef");
    for layout in ["chunked", "point"] {
        let archive = dir.join(format!("{layout}.mzpeak"));
        let mzml = dir.join(format!("{layout}.mzML"));
        convert(Path::new(PASEF), &archive, &["--layout", layout], &[]);
        convert(&archive, &mzml, &[], &[]);
        mzml_meta::assert_processing_contract(&mzml_meta::read(&mzml), layout);
        let back = spectra(&mzml);
        assert_eq!(back.len(), 1, "{layout}");
        let got_im = array(&back[0], &im).unwrap_or_else(|| panic!("{layout}: the export has no mobility array"));
        let got_mz = array(&back[0], &ArrayType::MZArray).unwrap();
        assert_eq!(got_im.len(), want_im.len(), "{layout}: one mobility per peak");
        assert!(got_mz.windows(2).all(|w| w[0] <= w[1]), "{layout}: m/z order");
        for (i, ((gm, gi), (wm, wi))) in got_mz.iter().zip(&got_im).zip(want_mz.iter().zip(&want_im)).enumerate() {
            assert!((gm - wm).abs() < 1e-6, "{layout} peak {i}: m/z {gm} vs {wm}");
            assert_eq!(gi.to_bits(), wi.to_bits(), "{layout} peak {i}: 1/K0 {gi} vs {wi}");
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// A file whose name is not Unicode converts. `std::env::args`, which the recorded command line
/// was read through, panics on such an argument: 0.13.0 aborted every archive conversion of the
/// file, and recording the step in every mzML would have aborted those too.
#[cfg(target_os = "linux")]
#[test]
fn a_file_name_that_is_not_unicode_converts() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    let dir = scratch("latin1");
    let src = dir.join(OsStr::from_bytes(b"lat\xe9n.mzML"));
    std::fs::copy(TINY, &src).unwrap();
    let mzml = dir.join(OsStr::from_bytes(b"out\xe9.mzML"));
    convert(&src, &mzml, &["--to", "mzml"], &[]);
    let m = mzml_meta::read(&mzml);
    let ours = mzml_meta::assert_processing_contract(&m, "non-Unicode file name");
    let xml = std::fs::read(&mzml).unwrap();
    let options = String::from_utf8_lossy(&xml).contains("lat\u{FFFD}n.mzML -o out\u{FFFD}.mzML");
    assert!(options, "{ours}: the command line is recorded, lossily");
    convert(&src, &dir.join(OsStr::from_bytes(b"a\xe9.mzpeak")), &[], &[]);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A Thermo `.raw` states no processing: the step is the only entry and the default of both lists.
#[test]
fn a_raw_file_export_is_valid_mzml() {
    let dir = scratch("thermo");
    let mzml = dir.join("small.mzML");
    convert(Path::new(THERMO), &mzml, &["--to", "mzml"], &[]);
    let m = mzml_meta::read(&mzml);
    let ours = mzml_meta::assert_processing_contract(&m, "small.RAW → mzML");
    assert_eq!(dp_ids(&m), [ours.as_str()]);
    assert_eq!(m.spectrum_list_default, Some(Some(ours.clone())));
    assert_eq!(m.chromatogram_list_default, Some(Some(ours)));
    let _ = std::fs::remove_dir_all(&dir);
}

/// Frame id from mzdata's TDF spectrum id (`merged=0 frame=N startScan=..`).
fn frame_of(id: &str) -> Option<i64> {
    id.split_whitespace().find_map(|t| t.strip_prefix("frame=")).and_then(|n| n.parse().ok())
}

/// Every MS2 spectrum of a timsTOF export: the window limits in order, equal to the selected ion's
/// band, around its 1/K0, and bracketing the spectrum's own mobility array EXACTLY, with no tolerance
/// (0.12.5's limits missed 8.9 % of the peaks; an intermediate fix still missed some by 2.2e-16).
/// Returns the spectra and how many of them have a peak ON the upper limit — the window's first
/// scan, whose array value the limit must be.
fn assert_windows_bracket_their_peaks(mzml: &Path, label: &str) -> (Vec<mzml_meta::Spectrum>, usize) {
    let m = mzml_meta::read(mzml);
    mzml_meta::assert_processing_contract(&m, label);
    let arrays = spectra(mzml);
    assert_eq!(arrays.len(), m.spectra.len(), "{label}");
    let im_kind = mzdata::spectrum::ArrayType::MeanInverseReducedIonMobilityArray;
    let (mut ms2, mut on_upper) = (0, 0);
    for (s, full) in m.spectra.iter().zip(&arrays) {
        let im_array = array(full, &im_kind).unwrap_or_default();
        let peaks = array(full, &mzdata::spectrum::ArrayType::MZArray).map_or(0, |a| a.len());
        assert_eq!(im_array.len(), peaks, "{label} {}: one 1/K0 per peak", s.id);
        if s.ms_level != Some(2) {
            continue;
        }
        ms2 += 1;
        let (lo, hi) = (s.im_lower.unwrap_or_else(|| panic!("{label} {}: no lower limit", s.id)), s.im_upper.unwrap_or_else(|| panic!("{label} {}: no upper limit", s.id)));
        assert!(lo < hi, "{label} {}: window limits {lo} .. {hi} not in order", s.id);
        assert_eq!((s.band_lower, s.band_upper), (Some(lo), Some(hi)), "{label} {}: the band is the window", s.id);
        let im = s.ion_mobility.unwrap_or_else(|| panic!("{label} {}: no selected-ion 1/K0", s.id));
        assert!(lo < im && im < hi, "{label} {}: {lo} < {im} < {hi}", s.id);
        let (min, max) = im_array.iter().fold((f64::INFINITY, f64::NEG_INFINITY), |(a, b), &v| (a.min(v), b.max(v)));
        assert!(lo <= min && max <= hi, "{label} {}: peaks at 1/K0 {min} ..= {max} outside the window {lo} ..= {hi}", s.id);
        on_upper += usize::from(im_array.contains(&hi));
    }
    assert!(ms2 >= 4, "{label}: {ms2} MS2 spectra");
    (m.spectra, on_upper)
}

/// A timsTOF `.d` → mzML: valid processing, every MS2 window as [`assert_windows_bracket_their_peaks`]
/// requires, and the first window of frame 2 (m/z 1276.05) at the vendor-model values
/// `tests/tdf_ims_window_band.rs` pins for the archive lanes. `--no-tims-recalibration` is inert
/// here and says so: limits on timsrust's linear map would miss the arrays, which stay on the model.
#[test]
#[ignore = "needs the 142 MB 2485.d timsTOF corpus fixture (MZPEAK_CORPUS); run with --include-ignored"]
fn a_timstof_export_orders_its_window_limits_on_the_vendor_model() {
    let Some(dot_d) = corpus::corpus_path(DOT_D) else { return };
    let dir = scratch("tdf");
    // Frame 1 is MS1, frame 2 the first diaPASEF frame: 40 spectra hold several MS2 frames.
    let cap = [("MZPC_MAX_SPECTRA", "40")];
    for (label, extra) in [("default", &[][..]), ("--no-tims-recalibration", &["--no-tims-recalibration"][..])] {
        let mzml = dir.join(format!("{}.mzML", extra.len()));
        let log = convert(&dot_d, &mzml, &[&["--to", "mzml"][..], extra].concat(), &cap);
        if !extra.is_empty() {
            assert!(log.contains("--no-tims-recalibration is inert"), "{label}: no inert warning in {log}");
        }
        let (spectra, on_upper) = assert_windows_bracket_their_peaks(&mzml, label);
        assert!(on_upper > 0, "{label}: no window has a peak on its upper limit");
        let first = spectra
            .iter()
            .find(|s| frame_of(&s.id) == Some(2) && s.isolation_target.is_some_and(|t| (t * 1000.0).round() == 1_276_051.0))
            .unwrap_or_else(|| panic!("{label}: no frame 2 window at m/z 1276.05"));
        let (lo, hi, im) = (first.im_lower.unwrap(), first.im_upper.unwrap(), first.ion_mobility.unwrap());
        assert!((im - 1.332387).abs() < 1e-6 && (lo - 1.305615).abs() < 1e-6 && (hi - 1.359142).abs() < 1e-6, "{label}: {lo} < {im} < {hi}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// Both kinds of timsTOF archive export each peak's 1/K0 (0.13.0: none). A `--no-ims-compact`
/// archive holds one spectrum per diaPASEF window, so its export meets the `--to mzml` lane's window
/// contract; an ims-compact archive holds whole frames, which are exported as such, with a warning
/// that a reader assigning precursors by mobility window needs the `.d` exported instead.
#[test]
#[ignore = "needs the 142 MB 2485.d timsTOF corpus fixture (MZPEAK_CORPUS); run with --include-ignored"]
fn a_timstof_archive_exports_its_peak_mobility() {
    let Some(dot_d) = corpus::corpus_path(DOT_D) else { return };
    let dir = scratch("tdf-archive");
    let cap = [("MZPC_MAX_SPECTRA", "40")];

    let (archive, mzml) = (dir.join("nic.mzpeak"), dir.join("nic.mzML"));
    convert(&dot_d, &archive, &["--no-ims-compact"], &cap);
    convert(&archive, &mzml, &[], &[]);
    let (_, on_upper) = assert_windows_bracket_their_peaks(&mzml, "--no-ims-compact archive");
    assert!(on_upper > 0, "--no-ims-compact archive: no window has a peak on its upper limit");

    let (archive, mzml) = (dir.join("ic.mzpeak"), dir.join("ic.mzML"));
    convert(&dot_d, &archive, &[], &cap);
    let log = convert(&archive, &mzml, &[], &[]);
    assert!(log.contains("whole timsTOF frames"), "ims-compact archive: no whole-frame warning in {log}");
    let m = mzml_meta::read(&mzml);
    mzml_meta::assert_processing_contract(&m, "ims-compact archive");
    let im_kind = mzdata::spectrum::ArrayType::MeanInverseReducedIonMobilityArray;
    let mut with_peaks = 0;
    for (s, full) in m.spectra.iter().zip(spectra(&mzml)) {
        let peaks = array(&full, &mzdata::spectrum::ArrayType::MZArray).map_or(0, |a| a.len());
        assert_eq!(array(&full, &im_kind).map_or(0, |a| a.len()), peaks, "ims-compact archive {}: one 1/K0 per peak", s.id);
        with_peaks += usize::from(peaks > 0);
    }
    assert!(with_peaks > 0, "ims-compact archive: no spectrum with peaks");
    let _ = std::fs::remove_dir_all(&dir);
}

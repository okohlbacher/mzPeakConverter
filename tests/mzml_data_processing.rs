//! Every mzML this tool writes meets mzML 1.1's processing contract, with its own `Conversion to
//! mzML` step in it ([`mzml_meta::assert_processing_contract`]), and a timsTOF export carries each
//! diaPASEF window's 1/K0 limits in order, on the vendor model its mobility arrays use.
//!
//! Through 0.12.5 the export of a raw file or an archive wrote `<dataProcessingList count="0">` and no
//! `defaultDataProcessingRef`, which stock OpenMS 3.5 refuses ("Required attribute
//! 'defaultDataProcessingRef' not present!"), and a TDF export wrote every MS2 spectrum's
//! `ion mobility lower limit` above its `upper limit` (1.3674 / 1.1931), against which OpenSWATH's
//! strict `lower < IM < upper` precursor test matched nothing.
//!
//! Lanes run here: `convert_to_mzml` on an mzML (a source with processing of its own), on this
//! tool's own mzML (ids already taken), on a Thermo `.raw` (a source with none) and — corpus-gated
//! — on a timsTOF `.d`; `filter_mzpeak_to_mzml` on an archive. `write_native_mzml` (TSF, BAF, the
//! Windows vendor readers) and `write_agilent_profile_mzml` need inputs no fixture holds; they share
//! the prologue (`fixup_mzml_run_metadata`) that the unit test
//! `mzml_prologue_records_the_conversion_step_on_every_source` writes and reads back.

use std::path::{Path, PathBuf};
use std::process::Command;

#[path = "common/corpus.rs"]
mod corpus;
#[path = "common/mzml_meta.rs"]
mod mzml_meta;

const TINY: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/tiny.pwiz.1.1.mzML");
const THERMO: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/small.RAW");
const DOT_D: &str = "ims-examples/PXD059079/20230830_100SPD_NCI7_0p12ng_HS_01_S1-B1_1_2485.d";

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("mzpc-mzml-dp-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn convert(input: &Path, output: &Path, extra: &[&str], envs: &[(&str, &str)]) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_mzpeak-convert"));
    cmd.arg(input).arg("-o").arg(output).arg("--force").args(extra);
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("failed to run mzpeak-convert");
    assert!(
        out.status.success(),
        "mzpeak-convert {} -o {} {extra:?} failed: {}\n{}",
        input.display(),
        output.display(),
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
}

fn dp_ids(m: &mzml_meta::Mzml) -> Vec<&str> {
    m.data_processings.iter().map(|(id, _)| id.as_str()).collect()
}

/// An mzML source keeps its processing, first and so the default; the export's step follows it.
/// Exporting that export again adds a second step under a fresh id and reuses the software entry.
#[test]
fn an_mzml_source_keeps_its_processing_and_gains_the_step() {
    let dir = scratch("mzml");
    let once = dir.join("once.mzML");
    convert(Path::new(TINY), &once, &["--to", "mzml"], &[]);
    let m = mzml_meta::read(&once);
    let ours = mzml_meta::assert_processing_contract(&m, "tiny → mzML");
    assert_eq!(dp_ids(&m), ["CompassXtract_x0020_processing", "pwiz_processing", ours.as_str()]);
    assert_eq!(m.spectrum_list_default, Some(Some("CompassXtract_x0020_processing".to_string())));

    let twice = dir.join("twice.mzML");
    convert(&once, &twice, &["--to", "mzml"], &[]);
    let m = mzml_meta::read(&twice);
    let again = mzml_meta::assert_processing_contract(&m, "tiny → mzML → mzML");
    assert_eq!(dp_ids(&m), ["CompassXtract_x0020_processing", "pwiz_processing", ours.as_str(), again.as_str()]);
    assert_ne!(ours, again);
    let ours_sw: Vec<_> = m.softwares.iter().filter(|(id, _)| id.starts_with("mzpeak-convert")).collect();
    assert_eq!(ours_sw.len(), 1, "one software entry per version: {:?}", m.softwares);
    let _ = std::fs::remove_dir_all(&dir);
}

/// The archive export (`filter_mzpeak_to_mzml`). The vendored archive reader restores none of the
/// lists the archive's index holds (software, processing, instruments), so 0.12.5 wrote
/// `<softwareList count="0">` and `<dataProcessingList count="0">` here too; the export's own step
/// makes both valid, and stays last should the archive's lists come back.
#[test]
fn an_archive_export_records_the_mzml_conversion() {
    let dir = scratch("archive");
    let archive = dir.join("tiny.mzpeak");
    let mzml = dir.join("tiny.mzML");
    convert(Path::new(TINY), &archive, &[], &[]);
    convert(&archive, &mzml, &[], &[]);
    let m = mzml_meta::read(&mzml);
    let ours = mzml_meta::assert_processing_contract(&m, "tiny → mzPeak → mzML");
    assert_eq!(dp_ids(&m).last(), Some(&ours.as_str()));
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

/// A timsTOF `.d` → mzML: valid processing, and on every MS2 spectrum the window limits in order,
/// equal to the selected ion's band, around its 1/K0; the first window of frame 2 (m/z 1276.05)
/// on the vendor model — or, under `--no-tims-recalibration`, on timsrust's linear map — at the
/// values `tests/tdf_ims_window_band.rs` pins for the archive lanes.
#[test]
#[ignore = "needs the 142 MB 2485.d timsTOF corpus fixture (MZPEAK_CORPUS); run with --include-ignored"]
fn a_timstof_export_orders_its_window_limits_on_the_vendor_model() {
    let Some(dot_d) = corpus::corpus_path(DOT_D) else { return };
    let dir = scratch("tdf");
    // Frame 1 is MS1, frame 2 the first diaPASEF frame: 40 spectra hold several MS2 frames.
    let cap = [("MZPC_MAX_SPECTRA", "40")];
    for (label, extra, (im_at, lo_at, hi_at)) in [
        ("model", &[][..], (1.332387, 1.305615, 1.359142)),
        ("linear", &["--no-tims-recalibration"][..], (1.317349, 1.291012, 1.343686)),
    ] {
        let mzml = dir.join(format!("{label}.mzML"));
        convert(&dot_d, &mzml, &[&["--to", "mzml"][..], extra].concat(), &cap);
        let m = mzml_meta::read(&mzml);
        mzml_meta::assert_processing_contract(&m, label);
        let ms2: Vec<_> = m.spectra.iter().filter(|s| s.ms_level == Some(2)).collect();
        assert!(ms2.len() >= 4, "{label}: {} MS2 spectra in the first 40", ms2.len());
        for s in &ms2 {
            let (lo, hi) = (s.im_lower.unwrap_or_else(|| panic!("{label} {}: no lower limit", s.id)), s.im_upper.unwrap_or_else(|| panic!("{label} {}: no upper limit", s.id)));
            assert!(lo < hi, "{label} {}: window limits {lo} .. {hi} not in order", s.id);
            assert_eq!((s.band_lower, s.band_upper), (Some(lo), Some(hi)), "{label} {}: the band is the window", s.id);
            let im = s.ion_mobility.unwrap_or_else(|| panic!("{label} {}: no selected-ion 1/K0", s.id));
            assert!(lo < im && im < hi, "{label} {}: {lo} < {im} < {hi}", s.id);
        }
        let first = ms2
            .iter()
            .find(|s| frame_of(&s.id) == Some(2) && s.isolation_target.is_some_and(|t| (t * 1000.0).round() == 1_276_051.0))
            .unwrap_or_else(|| panic!("{label}: no frame 2 window at m/z 1276.05"));
        let (lo, hi, im) = (first.im_lower.unwrap(), first.im_upper.unwrap(), first.ion_mobility.unwrap());
        assert!((im - im_at).abs() < 1e-6 && (lo - lo_at).abs() < 1e-6 && (hi - hi_at).abs() < 1e-6, "{label}: {lo} < {im} < {hi}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

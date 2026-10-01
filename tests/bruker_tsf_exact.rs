//! The native Bruker TSF lane stores every frame's m/z exactly and no longer embeds
//! `analysis.tsf_bin` by default (owner decision D9 of 2026-10-01), through the real binary on
//! synthetic TSF runs this test writes itself (the corpus holds no TSF acquisition):
//!
//! * dense line spectra: delta chunks where no sampled chunk spans more than a factor of two, or
//!   the point layout where that is smaller — never numpress-linear; the decoded m/z are the
//!   calibration's values `(a + b·tof)²` bit for bit, `fidelity.mz_error` is empty and
//!   `transformations` names no m/z change;
//! * sparse frames whose delta chunks would be at risk: the point layout, exact — under
//!   `--no-numpress` too (inert on this lane: there is no numpress to switch off) and under
//!   `MZPC_ENCODING_PRESCAN=0`, which writes the point layout without the trial, so no path leaves
//!   the archive inexact while the bin is gone;
//! * `analysis.tsf_bin` is dropped and the drop recorded in `vendor_files`; `--aux
//!   'analysis.tsf_bin=embed'` keeps it.
//!
//! Through 0.17.0-rc.2 the lane wrote numpress-linear (bounded-lossy) and embedded the bin: on the
//! MSV000088438 MALDI run 24.2 MB of a 25.1 MB archive was that file.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use mzdata::prelude::*;
use mzpeak_prototyping::MzPeakReader;
use serde_json::Value;

fn scratch(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("mzpc-tsf-exact-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn run(input: &Path, output: &Path, args: &[&str]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_mzpeak-convert"));
    cmd.arg(input).arg("-o").arg(output).arg("-q").arg("--force").args(args);
    cmd.env_remove("MZPC_ENCODING_PRESCAN").env_remove("MZPC_MAX_SPECTRA");
    cmd.output().expect("failed to run mzpeak-convert")
}

fn convert(input: &Path, output: &Path, args: &[&str]) {
    let r = run(input, output, args);
    assert!(r.status.success(), "{args:?} failed: {}", String::from_utf8_lossy(&r.stderr));
}

fn index(archive: &Path) -> Value {
    let mut zip = zip::ZipArchive::new(std::fs::File::open(archive).unwrap()).unwrap();
    let mut e = zip.by_name("mzpeak_index.json").unwrap();
    let mut buf = Vec::new();
    e.read_to_end(&mut buf).unwrap();
    serde_json::from_slice(&buf).unwrap()
}

fn members(archive: &Path) -> Vec<String> {
    let zip = zip::ZipArchive::new(std::fs::File::open(archive).unwrap()).unwrap();
    zip.file_names().map(str::to_string).collect()
}

/// The sqrt calibration the reader applies: `m/z = (a + b·tof)²` with `a = √lower` and
/// `b = (√upper − a) / digitizer`, from `GlobalMetadata` (timsControl: no ±5 Th widening).
const LOWER: f64 = 100.0;
const UPPER: f64 = 1000.0;
const DIGITIZER: f64 = 100_000.0;

fn mz_of(tof: f64) -> f64 {
    let a = LOWER.sqrt();
    let b = (UPPER.sqrt() - a) / DIGITIZER;
    let v = a + b * tof;
    v * v
}

/// A synthetic `.d` with `analysis.tsf` (SQLite) and `analysis.tsf_bin`: `frames[i]` is frame
/// `i + 1`'s list of (tof, intensity). Returns the directory and every frame's m/z.
fn write_tsf(dir: &Path, name: &str, frames: &[Vec<(f64, f32)>]) -> (PathBuf, Vec<Vec<f64>>) {
    let dot_d = dir.join(format!("{name}.d"));
    std::fs::create_dir_all(&dot_d).unwrap();
    let mut bin = Vec::new();
    let mut rows = Vec::new();
    let mut expected = Vec::new();
    for (k, peaks) in frames.iter().enumerate() {
        let id = k as i64 + 1;
        let mut raw = Vec::new();
        for (tof, _) in peaks {
            raw.extend_from_slice(&tof.to_le_bytes());
        }
        for (_, it) in peaks {
            raw.extend_from_slice(&it.to_le_bytes());
        }
        let z = zstd::encode_all(&raw[..], 3).unwrap();
        let offset = bin.len();
        bin.extend_from_slice(&((z.len() + 8) as u32).to_le_bytes());
        bin.extend_from_slice(&(z.len() as u32).to_le_bytes());
        bin.extend_from_slice(&z);
        rows.push(format!("({id}, {}, 0, '+', {}, {offset})", id as f64 * 0.5, peaks.len()));
        expected.push(peaks.iter().map(|(tof, _)| mz_of(*tof)).collect());
    }
    std::fs::write(dot_d.join("analysis.tsf_bin"), &bin).unwrap();
    let db = rusqlite::Connection::open(dot_d.join("analysis.tsf")).unwrap();
    db.execute_batch(&format!(
        "CREATE TABLE GlobalMetadata (Key TEXT, Value TEXT);
         INSERT INTO GlobalMetadata VALUES ('MzAcqRangeLower', '{LOWER}'), ('MzAcqRangeUpper', '{UPPER}'),
                                           ('DigitizerNumSamples', '{DIGITIZER}'), ('AcquisitionSoftware', 'timsControl');
         CREATE TABLE Frames (Id INTEGER PRIMARY KEY, Time REAL, MsMsType INTEGER, Polarity TEXT, NumPeaks INTEGER, TimsId INTEGER);
         INSERT INTO Frames VALUES {};",
        rows.join(", ")
    ))
    .unwrap();
    drop(db);
    (dot_d, expected)
}

/// A spectrum's m/z as the reader hands them back: its raw arrays' when the facet decoded into
/// arrays, else its peak set's (a centroid archive's spectra come back as a peak set).
fn spectrum_mzs(s: &mzdata::spectrum::MultiLayerSpectrum) -> Vec<f64> {
    match s.raw_arrays().and_then(|a| a.mzs().ok()).filter(|v| !v.is_empty()) {
        Some(v) => v.to_vec(),
        None => s.peaks.as_ref().map(|p| p.iter().map(|p| p.mz).collect()).unwrap_or_default(),
    }
}

/// The m/z the archive decodes to, frame by frame.
fn decoded(archive: &Path, n: usize) -> Vec<Vec<f64>> {
    let mut reader = MzPeakReader::new(archive).unwrap();
    (0..n).map(|i| spectrum_mzs(&reader.get_spectrum(i).expect("spectrum"))).collect()
}

/// A deterministic generator in [0, 1).
struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> f64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (self.0 >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// Dense line spectra: 300 peaks per frame on neighbouring flight-time bins, so every chunk's m/z
/// span is far inside a factor of two.
fn dense_frames(rng: &mut Lcg, n: usize) -> Vec<Vec<(f64, f32)>> {
    (0..n)
        .map(|k| {
            let start = 40_000.0 + 50.0 * k as f64;
            (0..300).map(|i| (start + 12.0 * i as f64 + (rng.next() * 4.0).floor(), (1.0 + 5000.0 * rng.next()) as f32)).collect()
        })
        .collect()
}

/// The flight-time bin nearest to an m/z under the calibration (m/z is quadratic in the bin).
fn tof_of(mz: f64) -> f64 {
    let a = LOWER.sqrt();
    let b = (UPPER.sqrt() - a) / DIGITIZER;
    ((mz.sqrt() - a) / b).round()
}

/// Sparse frames: four peaks whose m/z more than double between neighbours (about 101, 210, 450
/// and 950 Th), in every frame, so a delta chunk holding two of them is at risk.
fn sparse_frames(rng: &mut Lcg, n: usize) -> Vec<Vec<(f64, f32)>> {
    (0..n)
        .map(|k| {
            let jitter = 0.05 * k as f64;
            [101.0, 210.0, 450.0, 950.0]
                .iter()
                .map(|mz| (tof_of(mz + jitter + rng.next() * 0.01), (100.0 + 50.0 * rng.next()) as f32))
                .collect()
        })
        .collect()
}

fn assert_exact(archive: &Path, expected: &[Vec<f64>], what: &str) {
    let m = &index(archive)["metadata"];
    let applied: Vec<&str> = m["transformations"].as_array().unwrap().iter().map(|v| v.as_str().unwrap()).collect();
    assert!(!applied.iter().any(|t| *t == "numpress-linear" || *t == "delta-ulp"), "{what}: {applied:?}");
    assert_eq!(m["fidelity"]["mz_error"], serde_json::json!([]), "{what}: {}", m["fidelity"]);
    assert_eq!(m["fidelity"]["spectra_peaks"]["source_points"], m["fidelity"]["spectra_peaks"]["stored_points"], "{what}: {}", m["fidelity"]);
    for (i, (d, e)) in decoded(archive, expected.len()).iter().zip(expected).enumerate() {
        assert_eq!(
            d, e,
            "{what}: frame {} decodes to the calibration's m/z bit for bit (decoded {} values, first {:?}; expected {} values, first {:?}; layout {})",
            i + 1, d.len(), &d[..d.len().min(3)], e.len(), &e[..e.len().min(3)], m["fidelity"]["spectra_peaks"]["layout"]
        );
    }
}

#[test]
fn tsf_mz_are_stored_exactly_as_delta_or_point_never_numpress() {
    let dir = scratch("exact");
    let mut rng = Lcg(7);

    let (dense, expected) = write_tsf(&dir, "dense", &dense_frames(&mut rng, 24));
    let out = dir.join("dense.mzpeak");
    convert(&dense, &out, &[]);
    assert_exact(&out, &expected, "dense");
    let m = index(&out)["metadata"].clone();
    let block = &m["encoding_prescan"];
    let chosen = block["chosen"]["mz"].as_str().unwrap();
    assert!(chosen == "delta" || chosen == "point", "{block:#}");
    assert_eq!(block["delta_chunks_at_risk"], 0, "{block:#}");
    // The trial counts whole facets (the point layout moves every column), filed as such.
    assert!(block["measured_bytes"].get("mz").is_none(), "whole-facet bytes are not the m/z column's: {block:#}");
    assert!(block["measured_bytes"]["facets"].get("numpress-linear").is_none(), "an exact lane offers no lossy arm: {block:#}");
    assert!(block["measured_bytes"]["facets"]["delta"].is_u64() && block["measured_bytes"]["facets"]["point"].is_u64(), "{block:#}");
    assert_eq!(m["fidelity"]["spectra_peaks"]["layout"], if chosen == "point" { "point" } else { "chunked" }, "{block:#}");

    // Sparse frames: delta has a chunk at risk in every frame, so the exact arm is the point layout.
    let (sparse, expected) = write_tsf(&dir, "sparse", &sparse_frames(&mut rng, 24));
    let out = dir.join("sparse.mzpeak");
    convert(&sparse, &out, &[]);
    assert_exact(&out, &expected, "sparse");
    let m = index(&out)["metadata"].clone();
    let block = &m["encoding_prescan"];
    assert_eq!(block["chosen"]["mz"], "point", "{block:#}");
    assert!(block["delta_chunks_at_risk"].as_u64() > Some(0), "{block:#}");
    assert_eq!(m["fidelity"]["spectra_peaks"]["layout"], "point");

    // `--no-numpress` is inert on this lane (nothing to switch off) and says so: the trial runs as
    // by default and the point layout, the exact arm, is written with its block.
    let out = dir.join("sparse-flag.mzpeak");
    let r = Command::new(env!("CARGO_BIN_EXE_mzpeak-convert"))
        .arg(&sparse).arg("-o").arg(&out).arg("--force").arg("--no-numpress")
        .env_remove("MZPC_ENCODING_PRESCAN")
        .env("RUST_LOG", "info")
        .output()
        .unwrap();
    assert!(r.status.success(), "{}", String::from_utf8_lossy(&r.stderr));
    let err = String::from_utf8_lossy(&r.stderr);
    assert!(err.contains("--no-numpress is inert on this lane"), "{err}");
    assert_exact(&out, &expected, "sparse --no-numpress");
    let m = index(&out)["metadata"].clone();
    assert_eq!((&m["fidelity"]["spectra_peaks"]["layout"], &m["encoding_prescan"]["chosen"]["mz"]), (&Value::from("point"), &Value::from("point")), "{m:#}");

    // The lever skips the trial: the point layout, exact without a check — never untested delta,
    // which on these frames would be one ulp off while the bin, the vendor's exact copy, is dropped.
    let out = dir.join("sparse-env.mzpeak");
    let r = Command::new(env!("CARGO_BIN_EXE_mzpeak-convert"))
        .arg(&sparse).arg("-o").arg(&out).arg("-q").arg("--force")
        .env("MZPC_ENCODING_PRESCAN", "0")
        .output()
        .unwrap();
    assert!(r.status.success(), "{}", String::from_utf8_lossy(&r.stderr));
    assert_exact(&out, &expected, "sparse MZPC_ENCODING_PRESCAN=0");
    let m = index(&out)["metadata"].clone();
    assert!(m.get("encoding_prescan").is_none(), "{m:#}");
    assert_eq!(m["fidelity"]["spectra_peaks"]["layout"], "point", "{m:#}");
    assert!(!members(&out).iter().any(|n| n.contains("tsf_bin")), "the bin stays dropped on an exact archive");

    // `--layout point` is the user's own choice of the exact layout: no trial, no block.
    let out = dir.join("sparse-point.mzpeak");
    convert(&sparse, &out, &["--layout", "point"]);
    assert_exact(&out, &expected, "sparse --layout point");
    let m = index(&out)["metadata"].clone();
    assert!(m.get("encoding_prescan").is_none(), "{m:#}");
    assert_eq!(m["fidelity"]["spectra_peaks"]["layout"], "point");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn tsf_bin_is_dropped_by_default_and_kept_on_request() {
    let dir = scratch("bin");
    let mut rng = Lcg(3);
    let (dot_d, _) = write_tsf(&dir, "run", &dense_frames(&mut rng, 6));
    let bin_bytes = std::fs::metadata(dot_d.join("analysis.tsf_bin")).unwrap().len();

    let out = dir.join("default.mzpeak");
    convert(&dot_d, &out, &[]);
    let names = members(&out);
    assert!(names.iter().any(|n| n == "vendor/analysis.tsf.gz"), "{names:?}");
    assert!(!names.iter().any(|n| n.contains("tsf_bin")), "the bin was embedded: {names:?}");
    let manifest = index(&out)["metadata"]["vendor_files"].clone();
    let bin = manifest.as_array().unwrap().iter().find(|e| e["path"] == "analysis.tsf_bin").unwrap_or_else(|| panic!("{manifest:#}"));
    assert_eq!((&bin["action"], &bin["bytes"]), (&Value::from("drop"), &Value::from(bin_bytes)), "{manifest:#}");

    let out = dir.join("kept.mzpeak");
    convert(&dot_d, &out, &["--aux", "analysis.tsf_bin=embed"]);
    let names = members(&out);
    assert!(names.iter().any(|n| n == "vendor/analysis.tsf_bin"), "{names:?}");
    let manifest = index(&out)["metadata"]["vendor_files"].clone();
    let bin = manifest.as_array().unwrap().iter().find(|e| e["path"] == "vendor/analysis.tsf_bin").unwrap_or_else(|| panic!("{manifest:#}"));
    assert_eq!(bin["action"], "embed", "{manifest:#}");
    let _ = std::fs::remove_dir_all(&dir);
}

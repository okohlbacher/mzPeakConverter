//! timsTOF isolation-window mobility band + lane agreement (corpus-gated; ~10 s, dominated by the
//! `--no-ims-compact` conversion itself).
//!
//! Both timsTOF lanes — the default ims-compact (native timsrust) lane and `--no-ims-compact`
//! (mzdata's TDF reader) — must:
//!   * attach the isolation window's 1/K0 band to EVERY selected ion as MZP:1000006/7
//!     (`cv/mzpeak.obo`), unit MS:1002814, with `lower <= ion_mobility_value <= upper`, and list
//!     the MZP vocabulary in the archive's `cv_list`;
//!   * agree on the selected ion's 1/K0 and band for the same window, bit for bit: both evaluate
//!     the vendor's ModelType-2 model as the mobility arrays (mzdata lane) and the mobility grid
//!     (ims-compact) are evaluated, so a window's limits are the values its boundary scans' points
//!     carry. They used to differ by 0.015 Vs/cm² (1.332387 vs 1.317349 on frame 2 / m/z 1276.05
//!     of this file: mzdata's precursor params are timsrust-linear while its arrays use the vendor
//!     model), and through 0.17.0-rc.1 by 1 to 4 ulp (the ims-compact lane evaluated its limits in
//!     the SDK's order of operations);
//!   * say what the mzdata lane leaves out: the points of an MS2 frame outside every isolation
//!     window (`bruker:out-of-window-points-dropped`), with the frames' own count as
//!     `source_points`;
//!   * export the archive to mzML without panicking on the non-PSI accession (it becomes a
//!     `userParam`).
//!
//! The tables are read straight from the archive's Parquet members (the per-spectrum reader API
//! takes ~50 s over the 16k window-spectra of the mzdata lane). Needs 2485.d from the corpus, so it is
//! `#[ignore]`d: CI reports it as not run rather than as passed. Run with `--include-ignored`
//! and `MZPEAK_CORPUS`; `MZPC_REQUIRE_CORPUS=1` makes a missing fixture fail instead of skip.

use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

use arrow::array::{Array, ArrayRef, AsArray, LargeListArray, LargeStringArray, ListArray, StringArray};
use arrow::datatypes::{Float32Type, Float64Type, UInt64Type};
use arrow::record_batch::RecordBatch;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

#[path = "common/corpus.rs"]
mod corpus;

const DOT_D: &str =
    "ims-examples/PXD059079/20230830_100SPD_NCI7_0p12ng_HS_01_S1-B1_1_2485.d";
const LOWER_NAME: &str = "isolation window inverse reduced ion mobility lower limit";
/// One selected ion per dia-PASEF window on this file (3,594 MS2 frames × ~4.45 windows).
const WINDOWS: usize = 15_977;


fn run(args: &[&str], envs: &[(&str, &str)]) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_mzpeak-convert"));
    cmd.args(args);
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let st = cmd.status().expect("failed to run mzpeak-convert");
    assert!(st.success(), "mzpeak-convert {args:?} failed: {st}");
}

/// Frame id from either lane's spectrum id (`frame=N` / `merged=0 frame=N startScan=..`).
fn frame_of(id: &str) -> i64 {
    id.split_whitespace()
        .find_map(|t| t.strip_prefix("frame="))
        .and_then(|n| n.parse().ok())
        .unwrap_or_else(|| panic!("no frame= in spectrum id {id:?}"))
}

fn str_at(a: &ArrayRef, i: usize) -> &str {
    if let Some(s) = a.as_any().downcast_ref::<StringArray>() {
        s.value(i)
    } else {
        a.as_any().downcast_ref::<LargeStringArray>().expect("string column").value(i)
    }
}

fn list_item(a: &ArrayRef, i: usize) -> ArrayRef {
    if let Some(l) = a.as_any().downcast_ref::<LargeListArray>() {
        l.value(i)
    } else {
        a.as_any().downcast_ref::<ListArray>().expect("list column").value(i)
    }
}

/// Every record batch of one Parquet member of the archive (members are STORED, so the entry is
/// copied out verbatim and opened as a file).
fn batches(archive: &Path, member: &str, dir: &Path) -> Vec<RecordBatch> {
    let f = std::fs::File::open(archive).unwrap();
    let mut z = zip::ZipArchive::new(f).unwrap();
    let mut e = z.by_name(member).unwrap_or_else(|_| panic!("{member} missing"));
    let out = dir.join(format!("{}-{member}", archive.file_name().unwrap().to_string_lossy()));
    let mut o = std::fs::File::create(&out).unwrap();
    std::io::copy(&mut e, &mut o).unwrap();
    let rdr = ParquetRecordBatchReaderBuilder::try_new(std::fs::File::open(&out).unwrap())
        .unwrap()
        .with_batch_size(1 << 16)
        .build()
        .unwrap();
    rdr.map(|b| b.unwrap()).collect()
}

/// The MZP:1000006/7 band on one selected-ion row's `parameters` list.
fn band_of(params: &ArrayRef, i: usize) -> (f64, f64) {
    let item = list_item(params, i);
    let st = item.as_struct();
    let acc = st.column_by_name("accession").unwrap();
    let unit = st.column_by_name("unit").unwrap();
    let name = st.column_by_name("name").unwrap();
    let val = st
        .column_by_name("value")
        .unwrap()
        .as_struct()
        .column_by_name("float")
        .unwrap()
        .as_primitive::<Float64Type>();
    let (mut lo, mut hi) = (None, None);
    for k in 0..st.len() {
        if acc.is_null(k) {
            continue;
        }
        match str_at(acc, k) {
            "MZP:1000006" => {
                assert_eq!(str_at(unit, k), "MS:1002814");
                assert_eq!(str_at(name, k), LOWER_NAME);
                lo = Some(val.value(k));
            }
            "MZP:1000007" => {
                assert_eq!(str_at(unit, k), "MS:1002814");
                hi = Some(val.value(k));
            }
            _ => {}
        }
    }
    (lo.expect("MZP:1000006 on selected ion"), hi.expect("MZP:1000007 on selected ion"))
}

/// mzdata's spectrum-level `ion mobility lower/upper limit` pair on a `spectra_metadata` row's
/// `parameters` list, if present (the mzdata lane writes it; the native lane does not).
fn spectrum_limits(params: &ArrayRef, i: usize) -> Option<(f64, f64)> {
    let item = list_item(params, i);
    let st = item.as_struct();
    let name = st.column_by_name("name").unwrap();
    let val = st
        .column_by_name("value")
        .unwrap()
        .as_struct()
        .column_by_name("float")
        .unwrap()
        .as_primitive::<Float64Type>();
    let (mut lo, mut hi) = (None, None);
    for k in 0..st.len() {
        match str_at(name, k) {
            "ion mobility lower limit" => lo = Some(val.value(k)),
            "ion mobility upper limit" => hi = Some(val.value(k)),
            _ => {}
        }
    }
    lo.zip(hi)
}

/// (frame, isolation target in milli-Th) → (1/K0, band lower, band upper) for every selected ion,
/// asserting the band on each row along the way — and, where the spectrum carries mzdata's
/// spectrum-level `ion mobility lower/upper limit` pair, that it is ordered and equals the band
/// (mzdata emits it inverted; the remap must order it). Also returns how many spectra carried
/// that pair (all MS2 spectra of the mzdata lane; none of the native lane).
fn collect(archive: &Path, dir: &Path) -> (HashMap<(i64, i64), (f64, f64, f64)>, usize) {
    let mut frames: HashMap<u64, i64> = HashMap::new();
    let mut limits: HashMap<u64, (f64, f64)> = HashMap::new();
    for b in batches(archive, "spectra_metadata.parquet", dir) {
        let idx = b.column_by_name("index").unwrap().as_primitive::<UInt64Type>();
        let ids = b.column_by_name("id").unwrap();
        let params = b.column_by_name("parameters").unwrap();
        for i in 0..b.num_rows() {
            frames.insert(idx.value(i), frame_of(str_at(ids, i)));
            if let Some((lo, hi)) = spectrum_limits(params, i) {
                assert!(lo <= hi, "{}: spectrum-level limits inverted ({lo} > {hi})", str_at(ids, i));
                limits.insert(idx.value(i), (lo, hi));
            }
        }
    }
    // Precursor rows and selected-ion rows are written in lockstep (one ion per window here).
    let mut targets: Vec<(u64, f32)> = Vec::new();
    for b in batches(archive, "spectra_metadata_precursors.parquet", dir) {
        let si = b.column_by_name("source_index").unwrap().as_primitive::<UInt64Type>();
        let t = b
            .column_by_name("isolation_window")
            .unwrap()
            .as_struct()
            .column_by_name("isolation_window_target")
            .unwrap()
            .as_primitive::<Float32Type>();
        for i in 0..b.num_rows() {
            targets.push((si.value(i), t.value(i)));
        }
    }
    let mut out = HashMap::new();
    let mut row = 0usize;
    for b in batches(archive, "spectra_metadata_selected_ions.parquet", dir) {
        let si = b.column_by_name("source_index").unwrap().as_primitive::<UInt64Type>();
        let imv = b.column_by_name("ion_mobility_value").unwrap().as_primitive::<Float64Type>();
        let params = b.column_by_name("parameters").unwrap();
        for i in 0..b.num_rows() {
            let (psi, target) = targets[row];
            row += 1;
            assert_eq!(psi, si.value(i), "precursor/selected-ion rows out of step at row {row}");
            assert!(!imv.is_null(i), "row {row}: no ion_mobility_value");
            let im = imv.value(i);
            let (lo, hi) = band_of(params, i);
            assert!(lo <= im && im <= hi, "row {row}: {lo} <= {im} <= {hi} violated");
            if let Some(&(slo, shi)) = limits.get(&si.value(i)) {
                assert!(
                    (slo - lo).abs() < 1e-12 && (shi - hi).abs() < 1e-12,
                    "row {row}: spectrum-level limits {slo}..{shi} != band {lo}..{hi}"
                );
            }
            let key = (frames[&si.value(i)], (target as f64 * 1000.0).round() as i64);
            assert!(out.insert(key, (im, lo, hi)).is_none(), "duplicate window {key:?}");
        }
    }
    assert_eq!(row, targets.len());
    (out, limits.len())
}

const OUT_OF_WINDOW: &str = "bruker:out-of-window-points-dropped";

/// The archive's index `metadata`.
fn index_metadata(archive: &Path) -> serde_json::Value {
    let f = std::fs::File::open(archive).unwrap();
    let mut z = zip::ZipArchive::new(f).unwrap();
    let mut e = z.by_name("mzpeak_index.json").unwrap();
    let mut s = String::new();
    std::io::Read::read_to_string(&mut e, &mut s).unwrap();
    serde_json::from_str::<serde_json::Value>(&s).unwrap()["metadata"].clone()
}

fn cv_ids(archive: &Path) -> Vec<String> {
    index_metadata(archive)["cv_list"].as_array().unwrap().iter().map(|c| c["id"].as_str().unwrap().to_string()).collect()
}

/// mzdata's TDF reader hands a diaPASEF frame over as one spectrum per window, and the points in
/// the TIMS scans between and around the windows are in none. The first 120 spectra of 2485.d are
/// 28 whole frames (3 MS1, 25 MS2) and the first two windows of a 29th: the frames' own count
/// (`Frames.NumPeaks`) is 677,468, the reader hands over 666,854 of them, and 10,614 points of
/// the 25 MS2 frames (26.5 % of their 40,001) are dropped. Through 0.17.0-rc.1 the archive said
/// `source_points == stored_points` and declared nothing; the direct mzML export drops the same
/// points and now warns with the same count.
#[test]
#[ignore = "needs the 142 MB 2485.d timsTOF corpus fixture (MZPEAK_CORPUS); run with --include-ignored"]
fn the_mzdata_lane_declares_the_points_outside_every_isolation_window() {
    let Some(dot_d) = corpus::corpus_path(DOT_D) else { return };
    let tmp = std::env::temp_dir().join(format!("mzpc-outofwindow-{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();
    let d = dot_d.to_str().unwrap();
    let convert = |args: &[&str]| -> String {
        let out = Command::new(env!("CARGO_BIN_EXE_mzpeak-convert")).args(args).env("MZPC_MAX_SPECTRA", "120").output().expect("failed to run mzpeak-convert");
        assert!(out.status.success(), "mzpeak-convert {args:?} failed: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stderr).into_owned()
    };
    const COUNT: &str = "10614 of the 677468 points of the 28 frames read lie outside every isolation window of their frame (25 frames)";

    let noims = tmp.join("noims.mzpeak");
    let err = convert(&[d, "--no-ims-compact", "-o", noims.to_str().unwrap(), "--force", "--no-vendor"]);
    assert!(err.contains(COUNT) && err.contains(OUT_OF_WINDOW), "{err}");
    let meta = index_metadata(&noims);
    assert!(meta["transformations"].as_array().unwrap().iter().any(|t| t == OUT_OF_WINDOW), "{}", meta["transformations"]);
    // 677,468 of the 28 whole frames + the 414 points read of the frame the cap cut through.
    let facet = &meta["fidelity"]["spectra_peaks"];
    assert_eq!((facet["source_points"].as_u64(), facet["stored_points"].as_u64()), (Some(677_882), Some(667_268)), "{facet}");
    // Mirrored into the conversion's processing method, like every entry.
    let mirrored = meta["data_processing_method_list"].to_string();
    assert!(mirrored.contains(OUT_OF_WINDOW), "{mirrored}");

    // The default lane stores the same frames whole and declares nothing of the kind.
    let compact = tmp.join("compact.mzpeak");
    let err = convert(&[d, "-o", compact.to_str().unwrap(), "--force", "--no-vendor"]);
    assert!(!err.contains("outside every isolation window"), "{err}");
    let meta = index_metadata(&compact);
    assert!(!meta["transformations"].as_array().unwrap().iter().any(|t| t == OUT_OF_WINDOW), "{}", meta["transformations"]);

    // The direct export reads the same way: it warns, having no list to declare it in.
    let mzml = tmp.join("direct.mzML");
    let err = convert(&[d, "-o", mzml.to_str().unwrap(), "--force"]);
    assert!(err.contains(COUNT) && err.contains("mzML has no transformations list"), "{err}");
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
#[ignore = "needs the 142 MB 2485.d timsTOF corpus fixture (MZPEAK_CORPUS); run with --include-ignored"]
fn both_timstof_lanes_carry_the_mzp_band_and_agree_on_precursor_mobility() {
    let Some(dot_d) = corpus::corpus_path(DOT_D) else { return };
    let tmp = std::env::temp_dir().join(format!("mzpc-imband-{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();
    let compact = tmp.join("compact.mzpeak");
    let noims = tmp.join("noims.mzpeak");
    let d = dot_d.to_str().unwrap();
    run(&[d, "-o", compact.to_str().unwrap(), "--force", "--no-vendor"], &[]);
    run(&[d, "--no-ims-compact", "-o", noims.to_str().unwrap(), "--force", "--no-vendor"], &[]);

    for a in [&compact, &noims] {
        let ids = cv_ids(a);
        assert!(ids.iter().any(|i| i == "MZP"), "{}: cv_list {ids:?} lacks MZP", a.display());
    }

    let (a, _) = collect(&compact, &tmp);
    let (b, b_limits) = collect(&noims, &tmp);
    assert_eq!(a.len(), WINDOWS, "ims-compact: one selected ion per dia-PASEF window");
    assert_eq!(b.len(), WINDOWS, "--no-ims-compact: one selected ion per dia-PASEF window");
    assert_eq!(b_limits, WINDOWS, "--no-ims-compact: every window spectrum carries the ordered pair");
    // Bit for bit: the mzdata lane's limits are its mobility arrays' own values (0.17.0), and the
    // ims-compact lane's are what its mobility grid decodes to. Through 0.17.0-rc.1 the ims-compact
    // limits were evaluated in the SDK's order of operations and differed by 1 to 4 ulp in 6,788
    // lower and 7,591 upper limits of the 15,977 windows.
    let mut differing = 0usize;
    for (k, va) in &a {
        let vb = b.get(k).unwrap_or_else(|| panic!("window {k:?} missing from --no-ims-compact"));
        differing += usize::from([(va.0, vb.0), (va.1, vb.1), (va.2, vb.2)].iter().any(|(x, y)| x.to_bits() != y.to_bits()));
    }
    assert_eq!(differing, 0, "of {WINDOWS} windows, the lanes state another 1/K0 or band limit for {differing}");

    // What the mzdata lane leaves out is declared, and its `source_points` is the file's count:
    // the ims-compact archive stores every point of every frame (SUM(Frames.NumPeaks)).
    let (compact_meta, noims_meta) = (index_metadata(&compact), index_metadata(&noims));
    let every_point = compact_meta["fidelity"]["spectra_peaks"]["stored_points"].as_u64().expect("ims-compact stored_points");
    let facet = &noims_meta["fidelity"]["spectra_peaks"];
    let (source, stored) = (facet["source_points"].as_u64().unwrap(), facet["stored_points"].as_u64().unwrap());
    assert_eq!(source, every_point, "--no-ims-compact source_points is not the frames' own count");
    assert!(stored < source, "--no-ims-compact stores {stored} of {source} points: nothing was outside a window?");
    let declared = |m: &serde_json::Value| m["transformations"].as_array().unwrap().iter().any(|t| t == OUT_OF_WINDOW);
    assert!(declared(&noims_meta) && !declared(&compact_meta), "{} / {}", noims_meta["transformations"], compact_meta["transformations"]);
    // The regression: frame 2, isolation m/z 1276.05 (first window of the first MS2 frame) on the
    // vendor ModelType-2 model at the window midpoint — not timsrust's linear 1.317349.
    let (im, lo, hi) = a[&(2, 1_276_051)];
    // (0.12.5: the exact vendor model; the anchored approximation before it gave 1.332429, band
    // 1.305633..1.359207. The band's upper edge is Bruker's own invK0Begin for this window.)
    assert!((im - 1.332387).abs() < 1e-6, "ModelType-2 midpoint expected, got {im}");
    assert!((lo - 1.305615).abs() < 1e-6 && (hi - 1.359142).abs() < 1e-6, "band {lo}..{hi}");

    // `--no-tims-recalibration` must keep the lanes together too: both then write timsrust's
    // linear value for the same window (1.317349), not one lane linear and the other ModelType-2.
    // Capped to the first frames (frame 2 is the first MS2 frame) to keep this cheap.
    let compact_lin = tmp.join("compact-linear.mzpeak");
    let noims_lin = tmp.join("noims-linear.mzpeak");
    let cap = [("MZPC_MAX_SPECTRA", "40")];
    run(&[d, "-o", compact_lin.to_str().unwrap(), "--force", "--no-vendor", "--no-tims-recalibration"], &cap);
    run(
        &[d, "--no-ims-compact", "-o", noims_lin.to_str().unwrap(), "--force", "--no-vendor", "--no-tims-recalibration"],
        &cap,
    );
    let (a_lin, _) = collect(&compact_lin, &tmp);
    let (b_lin, b_lin_limits) = collect(&noims_lin, &tmp);
    assert!(b_lin_limits > 0 && b_lin_limits == b_lin.len(), "capped mzdata lane: {b_lin_limits} pairs / {} windows", b_lin.len());
    let (ia, la, ha) = a_lin[&(2, 1_276_051)];
    let (ib, lb, hb) = b_lin[&(2, 1_276_051)];
    assert!((ia - 1.317349).abs() < 1e-6, "ims-compact --no-tims-recalibration: linear expected, got {ia}");
    assert!((ib - 1.317349).abs() < 1e-6, "--no-ims-compact --no-tims-recalibration: linear expected, got {ib}");
    assert!((la - lb).abs() < 1e-9 && (ha - hb).abs() < 1e-9, "linear bands differ: {la}..{ha} vs {lb}..{hb}");
    assert!((la - 1.291012).abs() < 1e-6 && (ha - 1.343686).abs() < 1e-6, "linear band {la}..{ha}");

    // mzML export: no panic on the MZP accession; the band is a userParam.
    let mzml = tmp.join("compact.mzML");
    run(&[compact.to_str().unwrap(), "-o", mzml.to_str().unwrap(), "--force"], &[("MZPC_MAX_SPECTRA", "300")]);
    let xml = std::fs::read_to_string(&mzml).unwrap();
    let needle = format!("<userParam type=\"xsd:double\" name=\"{LOWER_NAME}\"");
    assert!(xml.contains(&needle), "band missing from mzML export");
    assert!(!xml.contains("accession=\"MZP"), "MZP accession leaked into mzML");
    let _ = std::fs::remove_dir_all(&tmp);
}

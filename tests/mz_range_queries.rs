//! m/z range queries through the vendored reader return exactly the points of a full read filtered
//! in memory — on the three facet kinds where they did not, or could not be trusted to:
//!
//! * a FITTED LINEAR-GRID chunk facet (a lattice mzML's centroids: `MS:1003826` rows under
//!   per-spectrum `MS:1003824` models, real m/z bounds). Through 0.13 this input took an
//!   integer-lattice point facet whose m/z predicate returned nothing but its one f64 spectrum,
//!   because the predicate pushed into Parquet dropped NULLs and the grid column was never
//!   projected. (The sqrt-grid twin of this lives in `tof_grid_facets.rs`, which owns that fixture.)
//! * a CHUNKED m/z facet: the answer was right but the chunk page index never existed — the reader
//!   looked up `chunk.mz_chunk_values_chunk_start`, a column no archive has. It is looked up by the
//!   array index now, which makes the next point load-bearing:
//! * a CHUNKED GRID facet (the 0.12.x timsTOF ims-chunked archives, `chunk.tof_chunk_*`): the m/z
//!   window was compared with TOF-bin bounds to select rows and with integer TOF values to filter
//!   points, and the result carried no m/z. Corpus-gated (`#[ignore]`), it needs a real archive.
//! * `RangeIndex` paired the start and end columns' pages BY POSITION. Two columns need not paginate
//!   alike, and then matching rows were skipped. Pinned on the smallest legal example.

use std::path::{Path, PathBuf};
use std::process::Command;

use mzdata::mzpeaks::coordinate::SimpleInterval;
use mzpeak_prototyping::reader::index::{PageIndex, RangeIndex};
use mzpeak_prototyping::MzPeakReader;

#[path = "common/range_query.rs"]
mod range_query;
#[path = "common/corpus.rs"]
mod corpus;

fn scratch(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("mzpc-mzrange-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn convert(input: &Path, output: &Path, extra: &[&str]) {
    let st = Command::new(env!("CARGO_BIN_EXE_mzpeak-convert")).arg(input).arg("-o").arg(output).arg("-q").args(extra).status().unwrap();
    assert!(st.success(), "converting {} failed: {st}", input.display());
}

fn everything() -> SimpleInterval<f64> {
    SimpleInterval::new(-1.0, 1.0e9)
}

#[test]
fn an_mz_window_over_a_fitted_linear_grid_facet_matches_the_filtered_full_read() {
    let dir = scratch("lattice");
    let archive = dir.join("lattice.mzpeak");
    convert(&Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/mz_lattice_1e9.mzML"), &archive, &[]);
    let mut reader = MzPeakReader::new(&archive).unwrap();
    let n = reader.len() as u64;
    // 12 spectra of 90 peaks over 120-1900 Da; spectrum 7 carries one apex off the 1e-9 lattice
    // (it fits the linear grid like the others — the window must still hit it).
    for window in [Some((400.0, 1200.0)), None] {
        let mut want = Vec::new();
        let mut off_lattice = 0;
        for i in 0..n {
            let before = want.len();
            range_query::expected(i, &reader.get_spectrum_peak_arrays_for(i).unwrap().expect("peak arrays"), window, &mut want);
            if i == 7 { off_lattice = want.len() - before }
        }
        want.sort_by(|a, b| a.partial_cmp(b).unwrap());
        assert!(off_lattice > 0 && want.len() > 10 * off_lattice, "the window must hit lattice and off-lattice spectra alike");
        let (it, _) = reader.query_peaks(everything(), window.map(|(lo, hi)| SimpleInterval::new(lo, hi)), None, None).unwrap();
        assert_eq!(range_query::rows(it), want, "window {window:?}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_mz_window_over_a_chunked_facet_matches_the_filtered_full_read() {
    let dir = scratch("chunked");
    let archive = dir.join("tiny.mzpeak");
    // Delta chunking: lossless, so the comparison can be bit for bit.
    convert(&Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tiny_centroid_only.mzML"), &archive, &["--no-numpress", "--no-mz-lattice"]);
    let mut reader = MzPeakReader::new(&archive).unwrap();
    let n = reader.len() as u64;
    let mut all = Vec::new();
    for i in 0..n {
        if let Some(arrays) = reader.get_spectrum_peak_arrays_for(i).unwrap() {
            range_query::expected(i, &arrays, None, &mut all);
        }
    }
    assert!(!all.is_empty(), "the fixture has centroids");
    let mut mzs: Vec<f64> = all.iter().map(|r| f64::from_bits(r.1)).collect();
    mzs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    // The middle half of the observed m/z, so the window cuts through chunks rather than around them.
    let window = (mzs[mzs.len() / 4], mzs[3 * mzs.len() / 4]);
    let mut want: Vec<_> = all.iter().copied().filter(|r| (window.0..=window.1).contains(&f64::from_bits(r.1))).collect();
    want.sort_by(|a, b| a.partial_cmp(b).unwrap());
    assert!(!want.is_empty() && want.len() < all.len());
    let (it, _) = reader.query_peaks(everything(), Some(SimpleInterval::new(window.0, window.1)), None, None).unwrap();
    assert_eq!(range_query::rows(it), want);
    let _ = std::fs::remove_dir_all(&dir);
}

/// 100 chunk rows. The START column sits in one page (every start = 100); the END column in two
/// (rows 0-49 end at 200, rows 50-99 at 1000). A query at m/z 500 matches rows 50-99. Pairing pages
/// by position compared it with [100, 200], took the start page's 100 rows, and skipped them all.
#[test]
fn range_index_survives_bounds_columns_with_different_page_boundaries() {
    let page = |min: f64, max: f64, start_row: i64, end_row: i64, page_i: usize| {
        serde_json::json!({"row_group_i": 0, "page_i": page_i, "min": min, "max": max, "start_row": start_row, "end_row": end_row})
    };
    let starts: PageIndex<f64> = serde_json::from_value(serde_json::json!([page(100.0, 100.0, 0, 100, 0)])).unwrap();
    let ends: PageIndex<f64> = serde_json::from_value(serde_json::json!([page(200.0, 200.0, 0, 50, 0), page(1000.0, 1000.0, 50, 100, 1)])).unwrap();
    let selected = |lo: f64, hi: f64| -> Vec<(bool, usize)> {
        RangeIndex::new(&starts, &ends).row_selection_overlaps(&SimpleInterval::new(lo, hi)).iter().map(|s| (s.skip, s.row_count)).collect()
    };
    assert_eq!(selected(500.0, 500.0), vec![(true, 50), (false, 50)], "rows 50-99 hold [100, 1000] and must be kept");
    assert_eq!(selected(150.0, 150.0), vec![(false, 100)], "every chunk spans 150");
    assert_eq!(selected(50.0, 60.0).iter().filter(|s| !s.0).map(|s| s.1).sum::<usize>(), 0, "nothing starts at or below 60");
    assert_eq!(selected(2000.0, 3000.0).iter().filter(|s| !s.0).map(|s| s.1).sum::<usize>(), 0, "nothing ends at or above 2000");
}

/// The corpus archive of PXD059079 2485 is a 0.12.x ims-chunked archive: `tof` main axis, TOF-bin
/// bounds, per-frame `tof_c0`/`tof_c1`. A window query over a slice of the run must equal the full
/// read of the same frames filtered in memory — m/z reconstructed with each frame's OWN pair.
#[test]
#[ignore = "needs the PXD059079 2485 timsTOF corpus archive (MZPEAK_CORPUS); run with --include-ignored"]
fn an_mz_window_over_a_chunked_tof_grid_facet_matches_the_filtered_full_read() {
    let Some(archive) = corpus::corpus_path("ims-examples/PXD059079/20230830_100SPD_NCI7_0p12ng_HS_01_S1-B1_1_2485.mzpeak") else { return };
    let mut reader = MzPeakReader::new(&archive).unwrap();
    let time_of = |reader: &mut MzPeakReader, i: u64| reader.get_spectrum_metadata(i).unwrap().expect("metadata").acquisition.start_time();
    let (t0, t1) = (time_of(&mut reader, 1000), time_of(&mut reader, 1011));   // MS1 + MS2 frames
    let window = (500.0, 550.0);
    let (it, times) = reader.query_peaks(SimpleInterval::new(t0, t1), Some(SimpleInterval::new(window.0, window.1)), None, None).unwrap();
    let got = range_query::rows(it);
    let mut frames: Vec<u64> = times.keys().copied().collect();
    frames.sort_unstable();
    assert!(frames.len() >= 10, "the time slice must span several frames, got {frames:?}");
    let mut want = Vec::new();
    for i in &frames {
        range_query::expected(*i, &reader.get_spectrum_peak_arrays_for(*i).unwrap().expect("peak arrays"), Some(window), &mut want);
    }
    want.sort_by(|a, b| a.partial_cmp(b).unwrap());
    assert!(want.len() > 1000, "m/z 500-550 over {} frames should hold thousands of points, got {}", frames.len(), want.len());
    assert_eq!(got.len(), want.len(), "point count");
    assert!(got == want, "same count but different points");
}

/// The chunk page index exists since this fix, so it prunes for the first time — on every chunked
/// archive ever written. Pin it on a real one with many pages: both facets of a 70 MB Q Exactive
/// run, a window query over a slice of the run against the full read of the same spectra.
#[test]
#[ignore = "needs the thermo-qexactive-plus corpus archive (MZPEAK_CORPUS); run with --include-ignored"]
fn the_chunk_page_index_prunes_nothing_it_should_keep_on_a_real_archive() {
    let Some(archive) = corpus::corpus_path("general-ms/thermo-qexactive-plus/NEG_Hg22CP_GPSC_1.mzpeak") else { return };
    let mut reader = MzPeakReader::new(&archive).unwrap();
    let n = reader.len() as u64;
    let time_of = |reader: &mut MzPeakReader, i: u64| reader.get_spectrum_metadata(i).unwrap().expect("metadata").acquisition.start_time();
    for (lo_i, hi_i) in [(n / 3, n / 3 + 40), (n - 45, n - 5)] {
        let (t0, t1) = (time_of(&mut reader, lo_i), time_of(&mut reader, hi_i));
        for window in [(200.0, 210.0), (300.0, 450.0)] {
            for profile in [true, false] {
                let q = Some(SimpleInterval::new(window.0, window.1));
                let (got, frames) = if profile {
                    let (it, times) = reader.extract_signal(SimpleInterval::new(t0, t1), q, None, None).unwrap();
                    (range_query::rows(it), times)
                } else {
                    let (it, times) = reader.query_peaks(SimpleInterval::new(t0, t1), q, None, None).unwrap();
                    (range_query::rows(it), times)
                };
                let mut want = Vec::new();
                for i in frames.keys() {
                    let arrays = if profile { reader.get_spectrum_arrays(*i).unwrap() } else { reader.get_spectrum_peak_arrays_for(*i).unwrap() };
                    if let Some(arrays) = arrays.filter(|a| a.mzs().is_ok()) {
                        range_query::expected(*i, &arrays, Some(window), &mut want);
                    }
                }
                want.sort_by(|a, b| a.partial_cmp(b).unwrap());
                // This run is profile-only: its peaks facet is empty, so that half only proves "no panic".
                assert!(!profile || want.len() > 1000, "the profile window must hold real data, got {}", want.len());
                assert_eq!(got.len(), want.len(), "point count, spectra {lo_i}-{hi_i}, window {window:?}, profile {profile}");
                assert!(got == want, "same count, different points");
            }
        }
    }
}

/// timsTOF `--no-ims-chunked`: a POINT facet with an integer `tof` column and NO m/z column at all.
/// The m/z window used to be ignored entirely (every point of the time slice came back, without an
/// m/z axis). Converts 2485.d into scratch, so it needs the vendor folder from the corpus.
#[test]
#[ignore = "needs the 142 MB 2485.d timsTOF corpus fixture (MZPEAK_CORPUS); run with --include-ignored"]
fn an_mz_window_over_a_flat_tof_facet_matches_the_filtered_full_read() {
    let Some(dot_d) = corpus::corpus_path("ims-examples/PXD059079/20230830_100SPD_NCI7_0p12ng_HS_01_S1-B1_1_2485.d") else { return };
    let dir = scratch("flat-tof");
    let archive = dir.join("2485.flat.mzpeak");
    convert(&dot_d, &archive, &["--no-ims-chunked", "--no-vendor"]);
    let mut reader = MzPeakReader::new(&archive).unwrap();
    let time_of = |reader: &mut MzPeakReader, i: u64| reader.get_spectrum_metadata(i).unwrap().expect("metadata").acquisition.start_time();
    let (t0, t1) = (time_of(&mut reader, 1000), time_of(&mut reader, 1011));
    let window = (500.0, 550.0);
    let (it, times) = reader.query_peaks(SimpleInterval::new(t0, t1), Some(SimpleInterval::new(window.0, window.1)), None, None).unwrap();
    let got = range_query::rows(it);
    let mut want = Vec::new();
    for i in times.keys() {
        range_query::expected(*i, &reader.get_spectrum_peak_arrays_for(*i).unwrap().expect("peak arrays"), Some(window), &mut want);
    }
    want.sort_by(|a, b| a.partial_cmp(b).unwrap());
    assert!(want.len() > 1000, "m/z 500-550 over {} frames should hold thousands of points, got {}", times.len(), want.len());
    assert_eq!(got.len(), want.len(), "point count");
    assert!(got == want, "same count but different points");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The archive the defect was reported on: Shimadzu LCMS-9030, a sqrt-grid PROFILE facet with
/// per-spectrum pairs beside an Int64-lattice PEAKS facet, every `point.mz` cell NULL. Both facets
/// returned zero points for any m/z window.
#[test]
#[ignore = "needs the shimadzu-lcms-9030-qtof corpus archive (MZPEAK_CORPUS); run with --include-ignored"]
fn an_mz_window_over_the_shimadzu_grid_archive_matches_the_filtered_full_read() {
    let Some(archive) = corpus::corpus_path("general-ms/shimadzu-lcms-9030-qtof/Blind_P1_pos_012.mzpeak") else { return };
    let mut reader = MzPeakReader::new(&archive).unwrap();
    let time_of = |reader: &mut MzPeakReader, i: u64| reader.get_spectrum_metadata(i).unwrap().expect("metadata").acquisition.start_time();
    let (t0, t1) = (time_of(&mut reader, 2000), time_of(&mut reader, 2400));
    let window = (80.0, 300.0);
    for profile in [true, false] {
        let q = Some(SimpleInterval::new(window.0, window.1));
        let (got, frames) = if profile {
            let (it, times) = reader.extract_signal(SimpleInterval::new(t0, t1), q, None, None).unwrap();
            (range_query::rows(it), times)
        } else {
            let (it, times) = reader.query_peaks(SimpleInterval::new(t0, t1), q, None, None).unwrap();
            (range_query::rows(it), times)
        };
        let mut want = Vec::new();
        for i in frames.keys() {
            let arrays = if profile { reader.get_spectrum_arrays(*i).unwrap() } else { reader.get_spectrum_peak_arrays_for(*i).unwrap() };
            if let Some(arrays) = arrays.filter(|a| a.mzs().is_ok()) {
                range_query::expected(*i, &arrays, Some(window), &mut want);
            }
        }
        want.sort_by(|a, b| a.partial_cmp(b).unwrap());
        assert!(want.len() > 500, "{} facet: the window must hold real data, got {}", if profile { "profile" } else { "peaks" }, want.len());
        assert_eq!(got.len(), want.len(), "point count, profile {profile}");
        assert!(got == want, "same count, different points (profile {profile})");
    }
}

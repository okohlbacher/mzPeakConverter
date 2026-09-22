//! The grid layout of the timsTOF ims-compact chunked facet (`src/tdf_grid.rs`), on PXD059079 2485:
//!
//! * the facet has the reference implementation's columns, the array index declares the two
//!   `chunk_transform` grid columns (`MS:1003826`), the index lists and bounds are byte-stream-split;
//! * every frame decodes through the vendored reader to the SAME points as the 0.12.5 corpus
//!   archive: intensities and point counts identical, m/z within 1e-6 ppm (the same vendor model in
//!   the reference implementation's arithmetic), 1/K0 within 4 ulp (two forms of the same rational);
//!   the stored TOF bins are identical integers;
//! * an m/z window query returns the filtered full read;
//! * `--ims-grid` on the corpus archive writes a peaks facet byte-identical to the fresh conversion's.
//!
//! Corpus-gated: needs `2485.d` and its 0.12.5 archive.

use std::path::{Path, PathBuf};
use std::process::Command;

use arrow::array::AsArray;
use arrow::datatypes::{Float64Type, Int32Type, UInt32Type, UInt64Type};
use mzdata::mzpeaks::coordinate::SimpleInterval;
use mzdata::prelude::*;
use mzpeak_prototyping::MzPeakReader;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::basic::Encoding;
use parquet::file::reader::{FileReader, SerializedFileReader};

#[path = "common/corpus.rs"]
mod corpus;
#[path = "common/range_query.rs"]
mod range_query;

const DOT_D: &str = "ims-examples/PXD059079/20230830_100SPD_NCI7_0p12ng_HS_01_S1-B1_1_2485.d";
const ARCHIVE: &str = "ims-examples/PXD059079/20230830_100SPD_NCI7_0p12ng_HS_01_S1-B1_1_2485.mzpeak";

fn scratch(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("mzpc-tdfgrid-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn convert(input: &Path, output: &Path, extra: &[&str]) {
    let st = Command::new(env!("CARGO_BIN_EXE_mzpeak-convert")).arg(input).arg("-o").arg(output).arg("-q").args(extra).status().unwrap();
    assert!(st.success(), "converting {} failed: {st}", input.display());
}

fn member(archive: &Path, name: &str) -> Vec<u8> {
    let mut zip = zip::ZipArchive::new(std::fs::File::open(archive).unwrap()).unwrap();
    let mut e = zip.by_name(name).unwrap_or_else(|_| panic!("{name} missing"));
    let mut buf = Vec::new();
    std::io::Read::read_to_end(&mut e, &mut buf).unwrap();
    buf
}

/// (tof bins, intensities, 1/K0) of every point of `frame`, from the raw facet: the 0.12.5 layout
/// (`tof_chunk_start` + cumsum of `tof_chunk_values`) or the grid layout (cumsum of
/// `mz_grid.indices`, scans through the mobility model are checked by the reader test instead).
fn raw_tof_bins(peaks: &[u8], grid: bool) -> Vec<(u64, Vec<u32>)> {
    let reader = ParquetRecordBatchReaderBuilder::try_new(bytes::Bytes::from(peaks.to_vec())).unwrap().build().unwrap();
    let mut out: Vec<(u64, Vec<u32>)> = Vec::new();
    for batch in reader {
        let batch = batch.unwrap();
        let root = batch.column(0).as_struct();
        let si = root.column_by_name("spectrum_index").unwrap().as_primitive::<UInt64Type>();
        for r in 0..batch.num_rows() {
            let bins: Vec<u32> = if grid {
                let g = root.column_by_name("mz_grid").unwrap().as_struct();
                let idx = g.column_by_name("indices").unwrap().as_list::<i64>().value(r);
                let idx = idx.as_primitive::<UInt32Type>();
                let mut acc = 0u32;
                idx.values().iter().map(|d| { acc += d; acc }).collect()
            } else {
                let seed = root.column_by_name("tof_chunk_start").unwrap().as_primitive::<Float64Type>().value(r) as u32;
                let d = root.column_by_name("tof_chunk_values").unwrap().as_list::<i64>().value(r);
                let mut acc = seed;
                std::iter::once(seed).chain(d.as_primitive::<Int32Type>().values().iter().map(|d| { acc += *d as u32; acc })).collect()
            };
            match out.last_mut() {
                Some((f, v)) if *f == si.value(r) => v.extend(bins),
                _ => out.push((si.value(r), bins)),
            }
        }
    }
    out
}

#[test]
#[ignore = "needs the 2485.d timsTOF corpus fixture and its 0.12.5 archive (MZPEAK_CORPUS); run with --include-ignored"]
fn the_grid_layout_stores_the_same_points_as_the_tof_layout_and_reads_back() {
    let (Some(dot_d), Some(old)) = (corpus::corpus_path(DOT_D), corpus::corpus_path(ARCHIVE)) else { return };
    let dir = scratch("layout");
    let new = dir.join("2485.grid.mzpeak");
    convert(&dot_d, &new, &["--no-vendor"]);

    // --- the facet: columns, transforms, encodings ---
    let peaks = member(&new, "spectra_peaks.parquet");
    let pf = SerializedFileReader::new(bytes::Bytes::from(peaks.clone())).unwrap();
    let meta = pf.metadata();
    let kv = meta.file_metadata().key_value_metadata().unwrap();
    let index: serde_json::Value = serde_json::from_str(kv.iter().find(|k| k.key == "spectrum_array_index").unwrap().value.as_deref().unwrap()).unwrap();
    let entries = index["entries"].as_array().unwrap();
    let entry = |path: &str| entries.iter().find(|e| e["path"] == path).unwrap_or_else(|| panic!("no array-index entry {path}"));
    assert_eq!(entry("chunk.mz_chunk_start")["buffer_format"], "chunk_start");
    assert_eq!(entry("chunk.mz_chunk_values")["buffer_format"], "chunk_values");
    assert_eq!(entry("chunk.mz_grid")["buffer_format"], "chunk_transform");
    assert_eq!(entry("chunk.mz_grid")["transform"], "MS:1003826");
    assert_eq!(entry("chunk.mz_grid")["data_type"], "MS:1000523", "the decoded type, float64");
    assert_eq!(entry("chunk.mean_inverse_reduced_ion_mobility_grid")["transform"], "MS:1003826");
    assert_eq!(entry("chunk.mean_inverse_reduced_ion_mobility_grid")["array_type"], "MS:1003006");
    assert!(entries.iter().all(|e| !e["path"].as_str().unwrap().contains("tof_chunk")), "no TOF-layout column survives");
    let rg = meta.row_group(0);
    let enc = |path: &str| {
        let col = (0..rg.num_columns()).map(|i| rg.column(i)).find(|c| c.column_path().string() == path).unwrap_or_else(|| panic!("no column {path}"));
        col.encodings().collect::<Vec<Encoding>>()
    };
    for p in ["chunk.mz_chunk_start", "chunk.mz_chunk_end", "chunk.intensity.list.item", "chunk.mz_grid.indices.list.item", "chunk.mean_inverse_reduced_ion_mobility_grid.indices.list.item"] {
        assert!(enc(p).contains(&Encoding::BYTE_STREAM_SPLIT), "{p} is not byte-stream-split: {:?}", enc(p));
        assert!(!enc(p).contains(&Encoding::RLE_DICTIONARY), "{p} still dictionary-encoded");
    }
    assert!(enc("chunk.spectrum_index").contains(&Encoding::DELTA_BINARY_PACKED));

    // --- the stored TOF bins are the same integers ---
    let old_bins = raw_tof_bins(&member(&old, "spectra_peaks.parquet"), false);
    let new_bins = raw_tof_bins(&peaks, true);
    assert_eq!(old_bins.len(), new_bins.len(), "frames with points");
    for ((fo, bo), (fn_, bn)) in old_bins.iter().zip(&new_bins) {
        assert_eq!(fo, fn_);
        assert!(bo == bn, "frame {fo}: TOF bins differ ({} vs {} points)", bo.len(), bn.len());
    }

    // --- every frame decodes to the same points through the reader ---
    let mut r_old = MzPeakReader::new(&old).unwrap();
    let mut r_new = MzPeakReader::new(&new).unwrap();
    assert_eq!(r_old.len(), r_new.len());
    let (mut n_points, mut worst_ppm, mut worst_k0_ulp) = (0usize, 0.0f64, 0.0f64);
    for i in 0..r_new.len() as u64 {
        let (a, b) = (r_old.get_spectrum_peak_arrays_for(i).unwrap(), r_new.get_spectrum_peak_arrays_for(i).unwrap());
        let (Some(a), Some(b)) = (a.as_ref(), b.as_ref()) else { assert!(a.is_none() && b.is_none(), "frame {i}: one side has no arrays"); continue };
        let (mz_a, mz_b) = (a.mzs().unwrap(), b.mzs().unwrap());
        assert_eq!(mz_a.len(), mz_b.len(), "frame {i}: point count");
        assert_eq!(a.intensities().unwrap().as_ref(), b.intensities().unwrap().as_ref(), "frame {i}: intensities");
        let (k_a, k_b) = (a.ion_mobility().unwrap().0, b.ion_mobility().unwrap().0);
        assert_eq!(k_a.len(), mz_a.len(), "frame {i}: mobility length");
        for ((x, y), (p, q)) in mz_a.iter().zip(mz_b.iter()).zip(k_a.iter().zip(k_b.iter())) {
            worst_ppm = worst_ppm.max(((x - y) / x).abs() * 1e6);
            worst_k0_ulp = worst_k0_ulp.max((p - q).abs() / (f64::EPSILON * p.abs()));
        }
        n_points += mz_a.len();
    }
    assert!(n_points > 30_000_000, "compared {n_points} points");
    assert!(worst_ppm < 1e-6, "m/z: worst {worst_ppm:.2e} ppm");
    assert!(worst_k0_ulp <= 4.0, "1/K0: worst {worst_k0_ulp} ulp");

    // --- an m/z window query returns the filtered full read ---
    let time_of = |r: &mut MzPeakReader, i: u64| r.get_spectrum_metadata(i).unwrap().unwrap().acquisition.start_time();
    let (t0, t1) = (time_of(&mut r_new, 1000), time_of(&mut r_new, 1011));
    let window = (500.0, 550.0);
    let (it, times) = r_new.query_peaks(SimpleInterval::new(t0, t1), Some(SimpleInterval::new(window.0, window.1)), None, None).unwrap();
    let got = range_query::rows(it);
    let mut want = Vec::new();
    for i in times.keys() {
        range_query::expected(*i, &r_new.get_spectrum_peak_arrays_for(*i).unwrap().unwrap(), Some(window), &mut want);
    }
    want.sort_by(|a, b| a.partial_cmp(b).unwrap());
    assert!(want.len() > 1000);
    assert!(got == want, "range query: {} vs {} points", got.len(), want.len());

    // --- the rewrite of the 0.12.5 archive is the same facet ---
    let rewritten = dir.join("2485.rewrite.mzpeak");
    convert(&old, &rewritten, &["--ims-grid"]);
    assert!(member(&rewritten, "spectra_peaks.parquet") == peaks, "rewrite and fresh conversion differ");
    let _ = std::fs::remove_dir_all(&dir);
}

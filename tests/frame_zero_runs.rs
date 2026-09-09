//! Does the writer keep every point of an ion-mobility FRAME — several drift bins' profile traces
//! interleaved on one m/z axis, with the bins' zero flanks and equal m/z values across bins — when
//! its zero-run mask is off? The Waters lane writes such frames with the mask off (2026-09-09 review:
//! masked across bins, the mask deleted per-bin trace boundaries), and the first archive written
//! that way still lacked the flank zeros. This pins the writer half of that question on the host.

use std::fs::File;
use std::path::Path;

use mzdata::params::Unit;
use mzdata::prelude::*;
use mzdata::spectrum::bindata::{ArrayType, BinaryArrayMap, BinaryDataArrayType, DataArray};
use mzdata::spectrum::{Chromatogram, ChromatogramDescription, MultiLayerSpectrum, SignalContinuity, SpectrumDescription};
use mzpeak_prototyping::chunk_series::ChunkingStrategy;
use mzpeak_prototyping::writer::{AbstractMzPeakWriter, MzPeakWriterType};
use mzpeak_prototyping::MzPeakReader;
use parquet::basic::{Compression, ZstdLevel};

/// Four drift bins on one 40-point m/z axis; bin b carries a run at k = 10b+5 .. 10b+12 with a zero
/// flank on each side, exactly the shape MassLynx hands back per bin. Interleaved and sorted by
/// (m/z, drift): equal m/z values across bins, zero runs longer than two once neighbouring bins'
/// flanks meet.
fn frame() -> (MultiLayerSpectrum, usize) {
    let mut points: Vec<(f64, f32, f32)> = Vec::new();
    for bin in 0..4u32 {
        let drift = 0.0392495 * bin as f32;
        let start = 10 * bin as usize + 4;
        for k in start..=start + 9 {
            let mz = 100.0 + 0.01 * k as f64;
            let intensity = if k == start || k == start + 9 { 0.0 } else { (k as f32) * 1.5 };
            points.push((mz, intensity, drift));
        }
        // a second, zero-flanked run in every bin at the same m/z values → cross-bin ties of zeros
        for k in 30..=33 {
            let mz = 100.0 + 0.01 * k as f64;
            let intensity = if k == 30 || k == 33 { 0.0 } else { 7.0 + bin as f32 };
            points.push((mz, intensity, drift));
        }
    }
    points.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.2.total_cmp(&b.2)));
    let n = points.len();
    let mut arrays = BinaryArrayMap::new();
    let mut mz_da = DataArray::wrap(&ArrayType::MZArray, BinaryDataArrayType::Float64, Vec::new());
    mz_da.update_buffer(&points.iter().map(|p| p.0).collect::<Vec<_>>()).unwrap();
    mz_da.unit = Unit::MZ;
    arrays.add(mz_da);
    let mut int_da = DataArray::wrap(&ArrayType::IntensityArray, BinaryDataArrayType::Float32, Vec::new());
    int_da.update_buffer(&points.iter().map(|p| p.1).collect::<Vec<_>>()).unwrap();
    int_da.unit = Unit::DetectorCounts;
    arrays.add(int_da);
    let mut im_da = DataArray::wrap(&ArrayType::RawIonMobilityArray, BinaryDataArrayType::Float32, Vec::new());
    im_da.update_buffer(&points.iter().map(|p| p.2).collect::<Vec<_>>()).unwrap();
    im_da.unit = Unit::Millisecond;
    arrays.add(im_da);
    let descr = SpectrumDescription {
        id: "function=1 process=0 scan=1".into(),
        index: 0,
        ms_level: 1,
        signal_continuity: SignalContinuity::Profile,
        ..Default::default()
    };
    (MultiLayerSpectrum::new(descr, Some(arrays), None, None), n)
}

fn write(path: &Path, strategy: ChunkingStrategy, mask_zero_runs: bool) -> usize {
    let (spec, n) = frame();
    let handle = File::create(path).unwrap();
    let builder = MzPeakWriterType::<File>::builder()
        .chunked_encoding(Some(strategy))
        .chromatogram_chunked_encoding(None)
        .compression(Compression::ZSTD(ZstdLevel::try_new(3).unwrap()))
        .sample_array_types_from_spectra(std::iter::once(spec.clone()));
    let mut writer = builder.build(handle, mask_zero_runs);
    writer.write_spectrum(&spec).unwrap();
    let mut arrays = BinaryArrayMap::new();
    arrays.add(DataArray::wrap(&ArrayType::TimeArray, BinaryDataArrayType::Float64, Vec::new()));
    arrays.add(DataArray::wrap(&ArrayType::IntensityArray, BinaryDataArrayType::Float64, Vec::new()));
    writer.write_chromatogram(&Chromatogram::new(ChromatogramDescription::default(), arrays)).unwrap();
    let zip = writer.finish_parquet().unwrap();
    zip.finish().unwrap();
    n
}

fn read_back(path: &Path) -> (usize, usize, usize, Vec<(f64, f32)>) {
    let mut reader = MzPeakReader::new(path).unwrap();
    let arrays = reader.get_spectrum_arrays(0).unwrap().expect("frame arrays");
    let mz = arrays.mzs().unwrap().to_vec();
    let it = arrays.intensities().unwrap().to_vec();
    let zeros = it.iter().filter(|v| **v == 0.0).count();
    let ties = mz.windows(2).filter(|w| w[0] == w[1]).count();
    (mz.len(), zeros, ties, mz.iter().zip(&it).map(|(a, b)| (*a, *b)).collect())
}

#[test]
fn a_frame_keeps_every_point_when_the_zero_run_mask_is_off() {
    let mut failures: Vec<String> = Vec::new();
    for (label, strategy) in [
        ("numpress", ChunkingStrategy::NumpressLinear { chunk_size: 50.0 }),
        ("delta", ChunkingStrategy::Delta { chunk_size: 50.0 }),
        ("basic", ChunkingStrategy::Basic { chunk_size: 50.0 }),
    ] {
        for mask in [false, true] {
            let out = std::env::temp_dir().join(format!("mzpc-frame-zeros-{}-{label}-{mask}.mzpeak", std::process::id()));
            let _ = std::fs::remove_file(&out);
            let n = write(&out, strategy, mask);
            let (got, zeros, ties, pts) = read_back(&out);
            eprintln!("{label} mask={mask}: wrote {n} points, read {got} ({zeros} zeros, {ties} ties)");
            if mask {
                // With the mask on, the six-zero run at m/z 100.33–100.34 (five bins' flanks meeting)
                // keeps its first and last zero only: 4 points fewer — the effect the Waters lane
                // turns the mask off to avoid.
                assert_eq!(got, n - 4, "{label}: the mask keeps the first and last zero of the cross-bin run");
            }
            if !mask && got != n {
                let (spec, _) = frame();
                let arrays = spec.raw_arrays().unwrap();
                let input: Vec<(f64, f32)> = arrays.mzs().unwrap().iter().zip(arrays.intensities().unwrap().iter()).map(|(a, b)| (*a, *b)).collect();
                let mut missing = input.clone();
                for p in &pts {
                    if let Some(i) = missing.iter().position(|q| (q.0 - p.0).abs() < 1e-3 && q.1 == p.1) {
                        missing.remove(i);
                    }
                }
                eprintln!("   missing after round trip: {missing:?}");
                eprintln!("   input : {input:?}");
                eprintln!("   output: {pts:?}");
                failures.push(label.to_string());
            }
            if std::env::var_os("MZPC_KEEP").is_none() {
                let _ = std::fs::remove_file(&out);
            }
        }
    }
    assert!(failures.is_empty(), "points lost with the mask off under: {failures:?}");
}

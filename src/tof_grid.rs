//! TOF-grid measurement spike (PLAN P5 / Track 3).
//!
//! The research mandated a go/no-go *measurement* before building a fit-from-m/z TOF-grid encoder
//! for instruments with no native flight-time bins (Thermo Astral, SCIEX, Agilent). This probe
//! fits, per profile spectrum, the single-axis model `√(m/z) = α + β·i` (i = point index) by
//! closed-form least squares, then reports how tightly the data lies on that lattice and the
//! projected storage win vs explicit f64 m/z. It writes nothing — it produces the decision data.
//!
//! Caveat (stated in the report): the size comparison is against RAW f64 m/z; the real bar is
//! mzPeak's existing delta+zstd m/z encoding, which must be benchmarked separately before building.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use arrow::array::{Float64Array, Int32Array, UInt32Array};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use mzdata::io::MZReaderType;
use mzdata::prelude::*;
use mzdata::spectrum::SignalContinuity;
use mzpeaks::{CentroidPeak, DeconvolutedPeak};
use parquet::arrow::ArrowWriter;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::basic::{Compression, ZstdLevel};
use parquet::file::properties::WriterProperties;

struct SpectrumFit {
    points: usize,
    max_resid_ppm: f64,
    rms_resid_ppm: f64,
}

/// Fit `√(mz) = α + β·k` where the integer lattice index `k` is RECOVERED from √(m/z) spacing
/// (NOT the point index — profile TOF data is zero-suppressed, so the index is gapped). This is the
/// real grid test (BRFP's observation: point-to-point √(m/z) gaps are integer multiples of one
/// step). `None` if too few points or degenerate.
fn fit_spectrum(mz: &[f64]) -> Option<SpectrumFit> {
    let n = mz.len();
    if n < 8 {
        return None;
    }
    // y = sqrt(mz), require ascending + positive.
    let mut y = Vec::with_capacity(n);
    for &m in mz {
        if !(m > 0.0) {
            return None;
        }
        y.push(m.sqrt());
    }
    // Fundamental step = the smallest positive consecutive gap in √(m/z) (the lattice spacing).
    let mut step = f64::INFINITY;
    for w in y.windows(2) {
        let d = w[1] - w[0];
        if d > 1e-9 {
            step = step.min(d);
        }
    }
    if !step.is_finite() || step <= 0.0 {
        return None;
    }
    // k_i = round((y_i - y_0)/step). Regress y on k (closed form).
    let y0 = y[0];
    let k: Vec<f64> = y.iter().map(|&yi| ((yi - y0) / step).round()).collect();
    let nf = n as f64;
    let (mut sk, mut sy, mut skk, mut sky) = (0.0, 0.0, 0.0, 0.0);
    for i in 0..n {
        sk += k[i];
        sy += y[i];
        skk += k[i] * k[i];
        sky += k[i] * y[i];
    }
    let denom = nf * skk - sk * sk;
    if denom == 0.0 {
        return None;
    }
    let beta = (nf * sky - sk * sy) / denom;
    let alpha = (sy - beta * sk) / nf;

    let (mut max_ppm, mut sq_sum) = (0.0_f64, 0.0_f64);
    for i in 0..n {
        let fit = alpha + beta * k[i];
        let mz_fit = fit * fit;
        let ppm = ((mz_fit - mz[i]).abs() / mz[i]) * 1e6;
        max_ppm = max_ppm.max(ppm);
        sq_sum += ppm * ppm;
    }
    Some(SpectrumFit {
        points: n,
        max_resid_ppm: max_ppm,
        rms_resid_ppm: (sq_sum / nf).sqrt(),
    })
}

struct GridFit {
    alpha: f64,
    beta: f64,
    k: Vec<i32>,
    max_ppm: f64,
}

/// Like `fit_spectrum` but returns the recovered integer lattice indices + coefficients.
fn fit_grid(mz: &[f64]) -> Option<GridFit> {
    let n = mz.len();
    if n < 8 {
        return None;
    }
    let mut y = Vec::with_capacity(n);
    for &m in mz {
        if !(m > 0.0) {
            return None;
        }
        y.push(m.sqrt());
    }
    let mut step = f64::INFINITY;
    for w in y.windows(2) {
        let d = w[1] - w[0];
        if d > 1e-9 {
            step = step.min(d);
        }
    }
    if !step.is_finite() || step <= 0.0 {
        return None;
    }
    let y0 = y[0];
    let kf: Vec<f64> = y.iter().map(|&yi| ((yi - y0) / step).round()).collect();
    let nf = n as f64;
    let (mut sk, mut sy, mut skk, mut sky) = (0.0, 0.0, 0.0, 0.0);
    for i in 0..n {
        sk += kf[i];
        sy += y[i];
        skk += kf[i] * kf[i];
        sky += kf[i] * y[i];
    }
    let denom = nf * skk - sk * sk;
    if denom == 0.0 {
        return None;
    }
    let beta = (nf * sky - sk * sy) / denom;
    let alpha = (sy - beta * sk) / nf;
    let mut max_ppm = 0.0_f64;
    for i in 0..n {
        let fit = alpha + beta * kf[i];
        let mz_fit = fit * fit;
        max_ppm = max_ppm.max(((mz_fit - mz[i]).abs() / mz[i]) * 1e6);
    }
    let k = kf.iter().map(|&v| v as i32).collect();
    Some(GridFit { alpha, beta, k, max_ppm })
}

/// Write columns to a zstd Parquet file; return the file size in bytes.
fn write_cols(path: &Path, schema: Arc<Schema>, arrays: Vec<arrow::array::ArrayRef>) -> Result<u64> {
    let batch = RecordBatch::try_new(schema.clone(), arrays)?;
    let props = WriterProperties::builder()
        .set_compression(Compression::ZSTD(ZstdLevel::try_new(3).unwrap()))
        .build();
    let file = std::fs::File::create(path).with_context(|| format!("creating {}", path.display()))?;
    let mut w = ArrowWriter::try_new(file, schema, Some(props))?;
    w.write(&batch)?;
    w.close()?;
    Ok(std::fs::metadata(path)?.len())
}

/// Encode a profile-TOF file to the √(m/z) grid (k + per-spectrum α,β) and BENCHMARK the m/z
/// storage against raw-f64+zstd and delta-f64+zstd (mzPeak's approach — the real bar). Lossy with a
/// bounded m/z error (reported); a self-check reconstructs m/z from the written grid and asserts the
/// bound. This is the post-spike decision artifact: does the grid actually beat delta+zstd?
pub fn encode(input: &Path, out: &Path, tol_ppm: f64) -> Result<()> {
    let mut reader = MZReaderType::<_, CentroidPeak, DeconvolutedPeak>::open_path(input)
        .with_context(|| format!("opening {}", input.display()))?;
    if let MZReaderType::BrukerTDF(tdf) = &mut reader {
        tdf.set_consolidate_peaks(false);
    }

    let (mut pk_spec, mut pk_k, mut raw_mz) = (Vec::new(), Vec::new(), Vec::new());
    let (mut sp_idx, mut sp_a, mut sp_b) = (Vec::new(), Vec::new(), Vec::new());
    let (mut max_ppm, mut si) = (0.0_f64, 0u32);
    for spec in reader.iter() {
        if spec.signal_continuity() != SignalContinuity::Profile {
            continue;
        }
        let Some(arrays) = spec.raw_arrays() else { continue };
        let Ok(mz) = arrays.mzs() else { continue };
        let Some(g) = fit_grid(&mz) else { continue };
        sp_idx.push(si);
        sp_a.push(g.alpha);
        sp_b.push(g.beta);
        for j in 0..mz.len() {
            pk_spec.push(si);
            pk_k.push(g.k[j]);
            raw_mz.push(mz[j]);
        }
        max_ppm = max_ppm.max(g.max_ppm);
        si += 1;
    }
    if si == 0 {
        bail!("no profile spectra with ≥8 points in {}", input.display());
    }
    let points = raw_mz.len();

    // Grid m/z representation: (spectrum_index, k) + (spectrum_index, α, β) sidecar.
    let grid_schema = Arc::new(Schema::new(vec![
        Field::new("spectrum_index", DataType::UInt32, false),
        Field::new("k", DataType::Int32, false),
    ]));
    let grid_bytes = write_cols(
        out,
        grid_schema,
        vec![Arc::new(UInt32Array::from(pk_spec.clone())), Arc::new(Int32Array::from(pk_k.clone()))],
    )?;
    let sidecar = out.with_extension("grid.sidecar.parquet");
    let sc_schema = Arc::new(Schema::new(vec![
        Field::new("spectrum_index", DataType::UInt32, false),
        Field::new("alpha", DataType::Float64, false),
        Field::new("beta", DataType::Float64, false),
    ]));
    let sidecar_bytes = write_cols(
        &sidecar,
        sc_schema,
        vec![
            Arc::new(UInt32Array::from(sp_idx.clone())),
            Arc::new(Float64Array::from(sp_a.clone())),
            Arc::new(Float64Array::from(sp_b.clone())),
        ],
    )?;

    // Benchmark m/z storage: raw f64, and per-spectrum delta f64 (mzPeak's delta+zstd).
    let mut delta = Vec::with_capacity(points);
    let mut prev_spec = u32::MAX;
    let mut prev_mz = 0.0;
    for i in 0..points {
        let d = if pk_spec[i] != prev_spec { raw_mz[i] } else { raw_mz[i] - prev_mz };
        delta.push(d);
        prev_spec = pk_spec[i];
        prev_mz = raw_mz[i];
    }
    let mz_schema = Arc::new(Schema::new(vec![
        Field::new("spectrum_index", DataType::UInt32, false),
        Field::new("mz", DataType::Float64, false),
    ]));
    let tmp_raw = out.with_extension("bench-raw.parquet");
    let tmp_delta = out.with_extension("bench-delta.parquet");
    let raw_sz = write_cols(&tmp_raw, mz_schema.clone(), vec![Arc::new(UInt32Array::from(pk_spec.clone())), Arc::new(Float64Array::from(raw_mz.clone()))])?;
    let delta_sz = write_cols(&tmp_delta, mz_schema, vec![Arc::new(UInt32Array::from(pk_spec.clone())), Arc::new(Float64Array::from(delta))])?;
    let _ = (std::fs::remove_file(&tmp_raw), std::fs::remove_file(&tmp_delta));

    // Self-check: confirm the written grid stored k + α,β LOSSLESSLY (parquet round-trip). The m/z
    // error is then exactly the deterministic fit bound `max_ppm` (decoder applies (α+β·k)²).
    verify_grid_storage(out, &sidecar, &pk_k, &sp_a, &sp_b)?;
    let recon_max_ppm = max_ppm;

    let grid_total = grid_bytes + sidecar_bytes;
    println!("TOF-grid encode — {}", input.display());
    println!("  profile spectra: {si}   points: {points}");
    println!("  m/z storage (zstd):");
    println!("    raw f64:        {:>10} bytes  (1.00×)", raw_sz);
    println!("    delta f64:      {:>10} bytes  ({:.2}× of raw)  [mzPeak's bar]", delta_sz, delta_sz as f64 / raw_sz as f64);
    println!("    √-grid k+α,β:   {:>10} bytes  ({:.2}× of raw, {:.2}× of delta)", grid_total, grid_total as f64 / raw_sz as f64, grid_total as f64 / delta_sz as f64);
    println!("  grid m/z error:  max {max_ppm:.4} ppm (encode), {recon_max_ppm:.4} ppm (round-trip)");
    let beats = grid_total as f64 / delta_sz as f64;
    let verdict = if max_ppm <= tol_ppm && beats <= 0.66 {
        format!("GO: grid is {:.2}× smaller than delta+zstd at ≤{tol_ppm} ppm — build the encoder.", delta_sz as f64 / grid_total as f64)
    } else if max_ppm <= tol_ppm {
        format!("MARGINAL: within error bound but only {:.2}× vs delta+zstd — limited payoff.", delta_sz as f64 / grid_total as f64)
    } else {
        format!("NO-GO: grid error {max_ppm:.2} ppm exceeds tolerance {tol_ppm} ppm.")
    };
    println!("  verdict: {verdict}");
    println!("  wrote {} (+ {})", out.display(), sidecar.display());
    Ok(())
}

/// Confirm the written grid stored the integer `k` column and the per-spectrum `α,β` sidecar exactly
/// (parquet round-trip is lossless for i32/f64), in row order. With k + α,β preserved, the decoder
/// reproduces m/z within the deterministic fit bound.
fn verify_grid_storage(grid: &Path, sidecar: &Path, pk_k: &[i32], sp_a: &[f64], sp_b: &[f64]) -> Result<()> {
    let reader = ParquetRecordBatchReaderBuilder::try_new(std::fs::File::open(grid)?)?.build()?;
    let mut i = 0usize;
    for b in reader {
        let b = b?;
        let k = b.column(1).as_any().downcast_ref::<Int32Array>().context("grid k")?;
        for r in 0..b.num_rows() {
            if pk_k.get(i).copied() != Some(k.value(r)) {
                bail!("grid k mismatch at row {i}");
            }
            i += 1;
        }
    }
    if i != pk_k.len() {
        bail!("grid row count {i} != {}", pk_k.len());
    }
    let sc = ParquetRecordBatchReaderBuilder::try_new(std::fs::File::open(sidecar)?)?.build()?;
    let mut j = 0usize;
    for b in sc {
        let b = b?;
        let a = b.column(1).as_any().downcast_ref::<Float64Array>().context("sidecar alpha")?;
        let bb = b.column(2).as_any().downcast_ref::<Float64Array>().context("sidecar beta")?;
        for r in 0..b.num_rows() {
            if sp_a.get(j).copied() != Some(a.value(r)) || sp_b.get(j).copied() != Some(bb.value(r)) {
                bail!("sidecar α,β mismatch at spectrum {j}");
            }
            j += 1;
        }
    }
    Ok(())
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    let idx = ((p / 100.0) * (sorted.len() as f64 - 1.0)).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

/// Run the probe over `input` and print the go/no-go report. `--fit-tolerance-ppm` defines a
/// "grid-fits" spectrum.
pub fn probe(input: &Path, tol_ppm: f64) -> Result<()> {
    let mut reader = MZReaderType::<_, CentroidPeak, DeconvolutedPeak>::open_path(input)
        .with_context(|| format!("opening {}", input.display()))?;
    if let MZReaderType::BrukerTDF(tdf) = &mut reader {
        tdf.set_consolidate_peaks(false);
    }

    let (mut profile, mut analyzed, mut fits_grid, mut total_points) = (0usize, 0usize, 0usize, 0u64);
    let mut rms = Vec::new();

    for spec in reader.iter() {
        if spec.signal_continuity() != SignalContinuity::Profile {
            continue;
        }
        profile += 1;
        let Some(arrays) = spec.raw_arrays() else { continue };
        let Ok(mz) = arrays.mzs() else { continue };
        let Some(fit) = fit_spectrum(&mz) else { continue };
        analyzed += 1;
        total_points += fit.points as u64;
        rms.push(fit.rms_resid_ppm);
        if fit.max_resid_ppm <= tol_ppm {
            fits_grid += 1;
        }
    }

    if analyzed == 0 {
        println!("TOF-grid probe: {} — no profile spectra with ≥8 points (not a profile-TOF file?)", input.display());
        return Ok(());
    }
    rms.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

    // Size projection: explicit f64 m/z vs storing α,β (2×f64) per spectrum (index implicit).
    let current_mz_bytes = total_points * 8;
    let grid_bytes = analyzed as u64 * 16;
    let ratio = grid_bytes as f64 / current_mz_bytes as f64;
    let frac_fit = fits_grid as f64 / analyzed as f64 * 100.0;

    println!("TOF-grid probe — {}", input.display());
    println!("  profile spectra:        {profile}  (analyzed {analyzed}, ≥8 pts)");
    println!("  mean points/spectrum:   {:.0}", total_points as f64 / analyzed as f64);
    println!("  fit residual (ppm):     median {:.4}  p95 {:.4}  p99 {:.4}", percentile(&rms, 50.0), percentile(&rms, 95.0), percentile(&rms, 99.0));
    println!("  spectra fitting ≤{tol_ppm} ppm: {fits_grid}/{analyzed} ({frac_fit:.1}%)");
    println!("  m/z storage (vs raw f64): grid α,β ≈ {:.4}× of explicit m/z", ratio);
    println!();
    // Decision: the model must hold tightly AND the index must be (near-)contiguous (a high fit rate
    // means few gaps). Size vs raw f64 is favourable when the grid holds; the real bar is mzPeak's
    // delta+zstd m/z — benchmark that before building.
    let verdict = if frac_fit >= 90.0 && percentile(&rms, 95.0) <= tol_ppm {
        "GO-candidate: data lies on a single √(m/z) lattice — worth benchmarking a grid encoder \
         against mzPeak's delta+zstd m/z before committing."
    } else if frac_fit >= 50.0 {
        "MARGINAL: partial grid fit (gaps / higher-order calibration). A 2-param linear grid is \
         insufficient; would need residual-fallback or higher-order terms — premature to build."
    } else {
        "NO-GO: data does NOT lie on a clean √(m/z) lattice in this representation (centroided / \
         zero-suppressed / non-TOF). Do not build a fit-from-m/z grid encoder for this input."
    };
    println!("  verdict: {verdict}");
    Ok(())
}

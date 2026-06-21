//! Native Bruker TDF integer-TOF reader + `ims-compact` encoder (PLAN P2).
//!
//! mzdata's TDF API converts the raw `u32` TOF bins to `f64` m/z and discards the integer
//! (`io/tdf/arrays.rs`), so a *lossless* compact encoder cannot get its inputs there. We read the
//! native frames via `timsrust` — the exact crate mzdata wraps — so the TOF bins are the true
//! instrument values, not a derived `round((√mz-a)/b)`. The reader is exposed behind the
//! [`NativeTofReader`] capability so it can later be re-pointed at an upstream mzdata accessor
//! without touching the encoder (NATIVE-TOF-DESIGN.md).
//!
//! Encoding (ported from BRFP `write_tdf_to_ims_compact`): rows grouped by `spectrum_index`
//! (frame), within a frame mobility-major (scan ascending) then TOF ascending; the TOF column is
//! **delta-reset per (frame, scan)** — first peak of each scan holds the absolute bin, the rest
//! hold non-negative increments. Lossless: `m/z = (a + b·tof)²` with `a,b` stored in file KV
//! metadata; the decoder recovers the exact integer TOF and thus the exact m/z the instrument
//! calibration produces.

use std::path::Path;

use anyhow::{Context, Result, bail};
use arrow::array::{Float64Array, UInt32Array};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use parquet::arrow::ArrowWriter;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::basic::{Compression, Encoding, ZstdLevel};
use parquet::file::properties::WriterProperties;
use parquet::file::metadata::KeyValue;
use parquet::schema::types::ColumnPath;
use std::sync::Arc;

use mzdata::curie;
use mzdata::params::{Param, Unit};
use mzdata::prelude::ParamDescribed;
use mzdata::spectrum::bindata::{ArrayType, BinaryArrayMap, BinaryDataArrayType, DataArray};
use mzdata::spectrum::{MultiLayerSpectrum, SignalContinuity, SpectrumDescription};

use timsrust::converters::{ConvertableDomain, Scan2ImConverter, Tof2MzConverter};
use timsrust::readers::{FrameReader, MetadataReader};
use timsrust::MSLevel;

/// The TOF→m/z calibration model: `m/z = (a + b·tof)²`. `a = √(mz_min)`, `b = (√(mz_max)−a)/tof_max`.
#[derive(Debug, Clone, Copy)]
pub struct TofMzModel {
    pub a: f64,
    pub b: f64,
}

impl TofMzModel {
    /// Extract the exact coefficients from a timsrust converter through its public `convert`
    /// (the fields are private): `convert(0)=a²`, `convert(1)=(a+b)²`.
    fn from_converter(c: &Tof2MzConverter) -> Self {
        let a = c.convert(0u32).sqrt();
        let b = c.convert(1u32).sqrt() - a;
        Self { a, b }
    }
}

/// One native TIMS frame == one mzPeak spectrum. `scan_offsets[s]..scan_offsets[s+1]` indexes the
/// peaks belonging to mobility scan `s`.
pub struct RawFrame {
    pub index: usize,
    pub ms_level: u8,
    pub scan_offsets: Vec<usize>,
    pub tof: Vec<u32>,
    pub intensity: Vec<u32>,
}

/// Native integer-TOF reader over a Bruker `.d` (TDF). The mzdata-integration seam: a future
/// upstream native-TOF API would back this same surface.
pub struct NativeTofReader {
    frames: FrameReader,
    im: Scan2ImConverter,
    pub model: TofMzModel,
}

impl NativeTofReader {
    pub fn open(dot_d: &Path) -> Result<Self> {
        let tdf = dot_d.join("analysis.tdf");
        if !tdf.exists() {
            bail!("{} is not a TDF .d (no analysis.tdf)", dot_d.display());
        }
        let meta = MetadataReader::new(&tdf)
            .map_err(|e| anyhow::anyhow!("reading TDF metadata: {e}"))?;
        let frames = FrameReader::new(dot_d)
            .map_err(|e| anyhow::anyhow!("opening TDF frames: {e}"))?;
        let model = TofMzModel::from_converter(&meta.mz_converter);
        Ok(Self { frames, im: meta.im_converter, model })
    }

    pub fn len(&self) -> usize {
        self.frames.len()
    }

    pub fn frame(&self, i: usize) -> Result<RawFrame> {
        let f = self
            .frames
            .get(i)
            .map_err(|e| anyhow::anyhow!("reading frame {i}: {e}"))?;
        let ms_level = match f.ms_level {
            MSLevel::MS1 => 1,
            MSLevel::MS2 => 2,
            _ => 0,
        };
        Ok(RawFrame {
            index: f.index,
            ms_level,
            scan_offsets: f.scan_offsets,
            tof: f.tof_indices,
            intensity: f.intensities,
        })
    }

    #[inline]
    pub fn mobility_for_scan(&self, scan: usize) -> f64 {
        self.im.convert(scan as u32)
    }

    /// The `ims_calibration` JSON for the archive index. Archive variant stores ABSOLUTE tof (no
    /// delta), so readers reconstruct `m/z = (a + b·tof)²` directly.
    pub fn calibration_json(&self) -> serde_json::Value {
        serde_json::json!({
            "codec": "ims-compact",
            "lossless": "tof",
            "mz_from_tof": "(a + b*tof)^2",
            "tof_encoding": "absolute",
            "a": self.model.a,
            "b": self.model.b,
        })
    }

    /// Build the IN-ARCHIVE ims-compact spectrum for frame `i`: the signal arrays are
    /// `nonstandard("tof")` (Int32, replaces `m/z array`) + `IntensityArray` (f32) +
    /// `MeanInverseReducedIonMobilityArray` (f64). m/z is reconstructed by readers from the index
    /// `ims_calibration` (per the mzPeakViewer handoff). Peaks are mobility-major then TOF order.
    pub fn ims_compact_spectrum(&self, i: usize) -> Result<MultiLayerSpectrum> {
        let frame = self.frame(i)?;
        let n_scans = frame.scan_offsets.len().saturating_sub(1);
        let (mut tof, mut intensity, mut mobility) = (Vec::new(), Vec::new(), Vec::new());
        for s in 0..n_scans {
            let (lo, hi) = (frame.scan_offsets[s], frame.scan_offsets[s + 1]);
            if lo >= hi {
                continue;
            }
            let m = self.mobility_for_scan(s);
            for k in lo..hi {
                // TOF bins fit i32 in practice (digitizer ~4e5), but the column type is Int32 — guard
                // the cast so an out-of-range bin is a hard error, never a silent wrap to garbage m/z.
                let bin = i32::try_from(frame.tof[k])
                    .map_err(|_| anyhow::anyhow!("TOF bin {} exceeds i32 range", frame.tof[k]))?;
                tof.push(bin);
                intensity.push(frame.intensity[k] as f32);
                mobility.push(m);
            }
        }

        let mut arrays = BinaryArrayMap::new();
        let mut tof_da = DataArray::wrap(&ArrayType::nonstandard("tof"), BinaryDataArrayType::Int32, Vec::new());
        tof_da.update_buffer(tof.as_slice()).map_err(|e| anyhow::anyhow!("encoding tof: {e}"))?;
        arrays.add(tof_da);
        let mut int_da = DataArray::wrap(&ArrayType::IntensityArray, BinaryDataArrayType::Float32, Vec::new());
        int_da.update_buffer(intensity.as_slice()).map_err(|e| anyhow::anyhow!("encoding intensity: {e}"))?;
        int_da.unit = Unit::DetectorCounts;
        arrays.add(int_da);
        let mut mob_da = DataArray::wrap(
            &ArrayType::MeanInverseReducedIonMobilityArray,
            BinaryDataArrayType::Float64,
            Vec::new(),
        );
        mob_da.update_buffer(mobility.as_slice()).map_err(|e| anyhow::anyhow!("encoding mobility: {e}"))?;
        arrays.add(mob_da);

        let mut descr = SpectrumDescription {
            id: format!("frame={}", frame.index),
            index: i,
            ms_level: frame.ms_level.max(1),
            signal_continuity: SignalContinuity::Centroid,
            ..Default::default()
        };
        descr.add_param(Param::builder().name("mass spectrum").curie(curie!(MS:1000294)).build());
        Ok(MultiLayerSpectrum::new(descr, Some(arrays), None, None))
    }

}

/// One native peak in deterministic (frame → scan → peak) order. The delta-reset key is the
/// explicit `(spectrum, scan)` pair — NOT the float mobility (a measured value, never an identity).
struct NativePeak {
    spectrum: u32,
    scan: u32,
    tof: u32,
    intensity: u32,
    mobility: f64,
}

/// Collect ONE frame's peaks (canonical order, empty mobility scans skipped). Bounded by a single
/// frame — the unit of streaming for the encoder + verifier.
fn frame_native_peaks(reader: &NativeTofReader, fi: usize) -> Result<Vec<NativePeak>> {
    let frame = reader.frame(fi)?;
    let spectrum = u32::try_from(frame.index)
        .map_err(|_| anyhow::anyhow!("frame index {} exceeds u32", frame.index))?;
    let n_scans = frame.scan_offsets.len().saturating_sub(1);
    let mut out = Vec::new();
    for s in 0..n_scans {
        let (lo, hi) = (frame.scan_offsets[s], frame.scan_offsets[s + 1]);
        if lo >= hi {
            continue;
        }
        let scan = u32::try_from(s).map_err(|_| anyhow::anyhow!("scan index {s} exceeds u32"))?;
        let mobility = reader.mobility_for_scan(s);
        for k in lo..hi {
            out.push(NativePeak { spectrum, scan, tof: frame.tof[k], intensity: frame.intensity[k], mobility });
        }
    }
    Ok(out)
}

/// Streaming iterator over native peaks — holds at most one frame's peaks in memory at a time.
struct NativePeakIter<'a> {
    reader: &'a NativeTofReader,
    fi: usize,
    buf: std::vec::IntoIter<NativePeak>,
}
impl<'a> NativePeakIter<'a> {
    fn new(reader: &'a NativeTofReader) -> Self {
        Self { reader, fi: 0, buf: Vec::new().into_iter() }
    }
    fn try_next(&mut self) -> Result<Option<NativePeak>> {
        loop {
            if let Some(p) = self.buf.next() {
                return Ok(Some(p));
            }
            if self.fi >= self.reader.len() {
                return Ok(None);
            }
            self.buf = frame_native_peaks(self.reader, self.fi)?.into_iter();
            self.fi += 1;
        }
    }
}

/// Encode a TDF `.d` to a standalone ims-compact Parquet file (data-only spectra representation),
/// then verify losslessness against an INDEPENDENT second native read. Returns (rows, output_bytes).
/// Streams one frame at a time: each frame is delta-reset + written as its own RecordBatch, and the
/// ArrowWriter coalesces them into properly-sized row groups — so memory stays bounded by the largest
/// single frame, never the whole run.
pub fn encode_ims_compact(dot_d: &Path, out: &Path) -> Result<(usize, u64)> {
    let reader = NativeTofReader::open(dot_d).context("opening native TDF reader")?;
    let model = reader.model;

    let schema = Arc::new(Schema::new(vec![
        Field::new("spectrum_index", DataType::UInt32, false),
        Field::new("scan_index", DataType::UInt32, false),
        Field::new("tof", DataType::UInt32, false),
        Field::new("intensity", DataType::UInt32, false),
        Field::new("mobility", DataType::Float64, false),
    ]));

    // Honest claim: TOF bins are bit-exact; m/z is RECONSTRUCTED via (a+b·tof)² where a,b are
    // extracted through the public converter and may differ from timsrust's internal coefficients
    // by ≤1 ULP — so this is `lossless: "tof"`, not m/z-bit-exact.
    let calib = serde_json::json!({
        "codec": "ims-compact",
        "lossless": "tof",
        "mz_from_tof": "(a + b*tof)^2",
        "tof_encoding": "per_scan_reset_delta_mod2_32",
        "scan_reset_key": "(spectrum_index, scan_index)",
        "a": model.a,
        "b": model.b,
    });

    // Write to a temp path; only rename to the final output AFTER verification passes, so a failed
    // verify never leaves a corrupt/partial archive in place.
    let tmp = out.with_extension("parquet.tmp");

    // BYTE_STREAM_SPLIT (dictionary off) on the integer tof + intensity columns: it transposes the
    // bytes so the small per-(spectrum,scan) deltas / low-entropy counts compress far better under
    // zstd. Lossless (a pure byte reordering). Safe to ship since mzpeak-validate's pyarrow was
    // lifted past 12 (BSS-INT32 needs parquet-format >= 2.10).
    let bss = |b: parquet::file::properties::WriterPropertiesBuilder, col: &str| {
        b.set_column_encoding(ColumnPath::from(col), Encoding::BYTE_STREAM_SPLIT)
            .set_column_dictionary_enabled(ColumnPath::from(col), false)
    };
    let mut pb = WriterProperties::builder()
        .set_compression(Compression::ZSTD(ZstdLevel::try_new(3).unwrap()));
    pb = bss(pb, "tof");
    pb = bss(pb, "intensity");
    let props = pb.build();

    let file = std::fs::File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
    let mut writer = ArrowWriter::try_new(file, schema.clone(), Some(props))?;
    writer.append_key_value_metadata(KeyValue::new("ims_calibration".to_string(), calib.to_string()));

    let mut rows = 0usize;
    for fi in 0..reader.len() {
        let peaks = frame_native_peaks(&reader, fi)?;
        if peaks.is_empty() {
            continue;
        }
        let n = peaks.len();
        // TOF is delta-reset per (spectrum, scan): first peak of each scan holds the absolute bin,
        // the rest hold modulo-2^32 increments (decoder mirrors with wrapping_add → lossless for ANY
        // ordering). Resets fall on frame boundaries too, so per-frame encoding is self-contained.
        let mut col_spectrum = Vec::with_capacity(n);
        let mut col_scan = Vec::with_capacity(n);
        let mut col_tof = Vec::with_capacity(n);
        let mut col_int = Vec::with_capacity(n);
        let mut col_mob = Vec::with_capacity(n);
        let (mut prev_key, mut prev_tof) = (None::<(u32, u32)>, 0u32);
        for p in &peaks {
            let key = (p.spectrum, p.scan);
            let stored = if prev_key != Some(key) { p.tof } else { p.tof.wrapping_sub(prev_tof) };
            col_spectrum.push(p.spectrum);
            col_scan.push(p.scan);
            col_tof.push(stored);
            col_int.push(p.intensity);
            col_mob.push(p.mobility);
            prev_key = Some(key);
            prev_tof = p.tof;
        }
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(UInt32Array::from(col_spectrum)),
                Arc::new(UInt32Array::from(col_scan)),
                Arc::new(UInt32Array::from(col_tof)),
                Arc::new(UInt32Array::from(col_int)),
                Arc::new(Float64Array::from(col_mob)),
            ],
        )?;
        writer.write(&batch)?;
        rows += n;
    }
    if rows == 0 {
        let _ = std::fs::remove_file(&tmp);
        bail!("no peaks read from {}", dot_d.display());
    }
    writer.close()?;

    verify_streaming(dot_d, &tmp).context("ims-compact lossless verify (streaming, independent native re-read)")?;
    std::fs::rename(&tmp, out).with_context(|| format!("finalizing {}", out.display()))?;

    let bytes = std::fs::metadata(out)?.len();
    Ok((rows, bytes))
}

/// Lossless proof: re-read the native TDF (a FRESH read, independent of the encode pass — so a
/// silently-skipped frame/scan would be caught by the count comparison), decode the Parquet's
/// delta-reset TOF, and assert spectrum/scan/TOF/intensity all match the native bins row-for-row.
/// Both sides stream — at most one native frame + one parquet batch are resident at once.
fn verify_streaming(dot_d: &Path, parquet: &Path) -> Result<()> {
    let reader = NativeTofReader::open(dot_d)?;
    let mut native = NativePeakIter::new(&reader);

    let file = std::fs::File::open(parquet)?;
    let batches = ParquetRecordBatchReaderBuilder::try_new(file)?.build()?;
    let (mut prev_key, mut acc) = (None::<(u32, u32)>, 0u32);
    let mut i = 0usize;
    for batch in batches {
        let batch = batch?;
        let spectrum = u32_col(&batch, 0)?;
        let scan = u32_col(&batch, 1)?;
        let tof = u32_col(&batch, 2)?;
        let intensity = u32_col(&batch, 3)?;
        for r in 0..batch.num_rows() {
            let key = (spectrum.value(r), scan.value(r));
            let decoded_tof = if prev_key != Some(key) {
                tof.value(r)
            } else {
                acc.wrapping_add(tof.value(r))
            };
            acc = decoded_tof;
            prev_key = Some(key);

            let Some(n) = native.try_next()? else {
                bail!("parquet has more rows than native (at row {})", i + 1);
            };
            if (n.spectrum, n.scan, n.tof, n.intensity) != (key.0, key.1, decoded_tof, intensity.value(r)) {
                bail!(
                    "row {i} mismatch: native (s{} sc{} tof{} int{}) vs parquet (s{} sc{} tof{} int{})",
                    n.spectrum, n.scan, n.tof, n.intensity, key.0, key.1, decoded_tof, intensity.value(r)
                );
            }
            i += 1;
        }
    }
    if native.try_next()?.is_some() {
        bail!("native has more rows than parquet ({i})");
    }
    Ok(())
}

fn u32_col(batch: &RecordBatch, idx: usize) -> Result<UInt32Array> {
    batch
        .column(idx)
        .as_any()
        .downcast_ref::<UInt32Array>()
        .cloned()
        .with_context(|| format!("column {idx} not UInt32"))
}

/// Read back the written Parquet and confirm row count + calibration are present (archive-side
/// sanity for the encoder). Returns the decoded calibration string.
pub fn read_back_calibration(path: &Path) -> Result<(usize, String)> {
    let file = std::fs::File::open(path)?;
    let builder = ParquetRecordBatchReaderBuilder::try_new(file)?;
    let kv = builder
        .metadata()
        .file_metadata()
        .key_value_metadata()
        .and_then(|kvs| kvs.iter().find(|kv| kv.key == "ims_calibration"))
        .and_then(|kv| kv.value.clone())
        .unwrap_or_default();
    let mut rows = 0usize;
    for batch in builder.build()? {
        rows += batch?.num_rows();
    }
    Ok((rows, kv))
}

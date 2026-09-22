//! The grid-transformation layout for the timsTOF ims-compact CHUNKED facet (Joshua Klein's "2a",
//! mzpeak_prototyping `e62e18c`; our review `RESOLUTIONS.md`): every chunk row keeps its REAL m/z
//! bounds, `mz_chunk_values` is NULL, `chunk_encoding` is `MS:1003826` (coordinate grid encoding),
//! and the coordinates live as integer grid indices in one struct column per dimension —
//! `mz_grid { grid_type, parameters, indices }` (`[first TOF bin, deltas…]`) and
//! `mean_inverse_reduced_ion_mobility_grid { … }` (TIMS scan numbers) — each carrying the vendor's
//! own calibration model for that frame. Nothing is fitted: the m/z model is the `MzCalibration`
//! row (SDK-exact to 1e-9 ppm, `tests/fixtures/tdf_diapasef_sdk_golden.json`), the mobility model
//! the `TimsCalibration` row, so every frame is exact, including the `C2 ≠ 0` / `C4 ≠ 0` frames
//! the 0.12.x per-frame pair could not express.
//!
//! Implemented as a REWRITE of a finished 0.12.x ims-chunked archive (the models and the per-frame
//! inputs are all in it): the native lane converts as before, then rewrites its own output; an
//! existing archive is upgraded with `mzpeak-convert old.mzpeak -o new.mzpeak --ims-grid`. Every
//! other member is copied byte for byte. Sizes on PXD059079 2485 (37.8 M points): 0.12.5 facet
//! 115.0 MB, this layout byte-stream-split 112.4 MB, with the reference implementation's default
//! encodings 124.4 MB.
//!
//! ponytail: a second pass over the peaks facet instead of teaching the vendored chunk writer to
//! emit struct columns; the writer streams its facets into the zip, so there is no cheaper hook.
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::Arc;

use anyhow::{anyhow, bail, ensure, Context, Result};
use arrow::array::{
    Array, ArrayRef, AsArray, Float64Array, LargeListArray, LargeStringArray, RecordBatch, StringArray,
    StructArray, UInt32Array,
};
use arrow::buffer::OffsetBuffer;
use arrow::datatypes::{DataType, Field, Fields, Float64Type, Int32Type, Schema, UInt64Type};
use mzpeak_prototyping::archive::{FileEntry, ZipArchiveWriter};
use mzpeak_prototyping::grid::{timstof_mobility, timstof_mz, GRID_ENCODING, TIMSTOF_MZ_GRID, TIMSTOF_TIMS_GRID};
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::arrow::ArrowWriter;
use parquet::basic::{Compression, Encoding, ZstdLevel};
use parquet::file::metadata::KeyValue;
use parquet::file::properties::WriterProperties;
use parquet::file::reader::{ChunkReader, Length};
use parquet::schema::types::ColumnPath;

const PEAKS: &str = "spectra_peaks.parquet";
const METADATA: &str = "spectra_metadata.parquet";
const ROW_GROUP_ROWS: usize = 8192;

/// What the rewrite did, for the log and the tests.
#[derive(Debug, Clone)]
pub struct GridRewriteReport {
    pub frames: usize,
    pub chunk_rows: u64,
    pub points: u64,
    /// Points whose TIMS scan number, evaluated through the reference mobility model, reproduces
    /// the stored 1/K0 bit for bit (the rest are within 4 ulp — the two evaluation forms differ in
    /// the last bits; a larger difference aborts the rewrite).
    pub mobility_exact: u64,
    pub peaks_bytes_before: u64,
    pub peaks_bytes_after: u64,
}

/// A STORED zip member as a seekable Parquet source, so a multi-GB facet is streamed row group by
/// row group instead of read into memory.
struct MemberReader {
    file: File,
    start: u64,
    len: u64,
}

impl Length for MemberReader {
    fn len(&self) -> u64 {
        self.len
    }
}

impl ChunkReader for MemberReader {
    type T = std::io::Take<File>;

    fn get_read(&self, start: u64) -> parquet::errors::Result<Self::T> {
        let mut f = self.file.try_clone()?;
        f.seek(SeekFrom::Start(self.start + start))?;
        Ok(f.take(self.len.saturating_sub(start)))
    }

    fn get_bytes(&self, start: u64, length: usize) -> parquet::errors::Result<bytes::Bytes> {
        let mut f = self.file.try_clone()?;
        f.seek(SeekFrom::Start(self.start + start))?;
        let mut buf = vec![0u8; length];
        f.read_exact(&mut buf)?;
        Ok(buf.into())
    }
}

/// Counts what passes through to the zip, so the report can say what the new facet weighs.
struct Counting<W: Write> {
    inner: W,
    n: u64,
}

impl<W: Write> Write for Counting<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.n += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// The vendor's `MzCalibration` row as the archive's `vendor_mz_calibration` block records it.
#[derive(Debug, Clone, Copy)]
struct MzRow {
    c0: f64,
    c1: f64,
    c2: f64,
    c3: f64,
    c4: f64,
    dc1: f64,
    dc2: f64,
    t1: f64,
    t2: f64,
    timebase: f64,
    delay: f64,
}

impl MzRow {
    fn from_json(v: &serde_json::Value) -> Option<Self> {
        let f = |k: &str| v.get(k).and_then(|x| x.as_f64());
        Some(Self {
            c0: f("C0")?,
            c1: f("C1")?,
            c2: f("C2").unwrap_or(0.0),
            c3: f("C3").unwrap_or(0.0),
            c4: f("C4").unwrap_or(0.0),
            dc1: f("dC1").unwrap_or(0.0),
            dc2: f("dC2").unwrap_or(0.0),
            t1: f("T1").unwrap_or(f64::NAN),
            t2: f("T2").unwrap_or(f64::NAN),
            timebase: f("DigitizerTimebase")?,
            delay: f("DigitizerDelay")?,
        })
    }

    /// The reference implementation's 7 parameters (`MzCalibrationModel2`, mzdata
    /// `io/tdf/calibration.rs`): `[C0, 1e6/sqrt(C1·cf), C2/cf, C3, C4, timebase, delay]` with
    /// `cf = 1 + (dC1·(T1 − t1) + dC2·(T2 − t2))/1e6`; a frame without `T1` takes the row's (`cf = 1`).
    fn parameters(&self, t1_frame: Option<f64>, t2_frame: Option<f64>) -> [f64; 7] {
        let dt1 = t1_frame.filter(|t| t.is_finite()).map_or(0.0, |t| self.t1 - t);
        let dt2 = t2_frame.filter(|t| t.is_finite()).map_or(0.0, |t| self.t2 - t);
        let cf = 1.0 + (self.dc1 * (if dt1.is_finite() { dt1 } else { 0.0 }) + self.dc2 * (if dt2.is_finite() { dt2 } else { 0.0 })) / 1.0e6;
        let beta = (1.0e12 / (self.c1 * cf)).sqrt();
        [self.c0, beta, self.c2 / cf, self.c3, self.c4, self.timebase, self.delay]
    }
}

/// The TIMS ModelType-2 row as the reference implementation's 4 parameters `[C6, C7, offset, slope]`.
#[derive(Debug, Clone, Copy)]
struct TimsModel {
    c6: f64,
    c7: f64,
    offset: f64,
    slope: f64,
}

impl TimsModel {
    fn from_json(v: &serde_json::Value) -> Option<Self> {
        let f = |k: &str| v.get(k).and_then(|x| x.as_f64());
        let (c0, c1, c2, c3, c4, c6, c7) = (f("C0")?, f("C1")?, f("C2")?, f("C3")?, f("C4")?, f("C6")?, f("C7")?);
        let slope = if c1 == 0.0 { 0.0 } else { (c3 - c2) / c1 };
        Some(Self { c6, c7, offset: c2 - slope * (c4 + c0), slope })
    }

    fn parameters(&self) -> [f64; 4] {
        [self.c6, self.c7, self.offset, self.slope]
    }

    /// The scan number whose model value is `k0` (the inverse of [`timstof_mobility`]).
    fn scan_of(&self, k0: f64) -> f64 {
        ((k0 * self.c7 / (1.0 - k0 * self.c6)) - self.offset) / self.slope
    }
}

/// Field metadata as the reference implementation writes it (what a reader that rebuilds the array
/// index from the Arrow schema sees).
fn field_meta(pairs: &[(&str, &str)]) -> HashMap<String, String> {
    pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

fn grid_struct_type() -> DataType {
    DataType::Struct(Fields::from(vec![
        Field::new("grid_type", DataType::LargeUtf8, false),
        Field::new("parameters", DataType::LargeList(Arc::new(Field::new("item", DataType::Float64, false))), false),
        Field::new("indices", DataType::LargeList(Arc::new(Field::new("item", DataType::UInt32, false))), false),
    ]))
}

fn large_list(values: ArrayRef, offsets: Vec<i64>, item: Field) -> LargeListArray {
    LargeListArray::try_new(Arc::new(item), OffsetBuffer::new(offsets.into()), values, None).expect("list offsets")
}

fn grid_struct(grid_type: &str, n: usize, params: Vec<f64>, per_row: usize, idx_offsets: Vec<i64>, idx_values: Vec<u32>) -> StructArray {
    let DataType::Struct(fields) = grid_struct_type() else { unreachable!() };
    let types: ArrayRef = Arc::new(LargeStringArray::from(vec![grid_type; n]));
    let p_offsets: Vec<i64> = (0..=n as i64).map(|i| i * per_row as i64).collect();
    let params: ArrayRef = Arc::new(large_list(Arc::new(Float64Array::from(params)), p_offsets, Field::new("item", DataType::Float64, false)));
    let indices: ArrayRef = Arc::new(large_list(Arc::new(UInt32Array::from(idx_values)), idx_offsets, Field::new("item", DataType::UInt32, false)));
    StructArray::new(fields, vec![types, params, indices], None)
}

/// Per-frame model inputs from `spectra_metadata` (`tdf_t1`, `tdf_t2`, `tdf_mz_calibration_id`).
struct FrameInputs {
    t1: Vec<Option<f64>>,
    t2: Vec<Option<f64>>,
    cal_id: Vec<Option<i64>>,
}

fn frame_inputs(meta_bytes: Vec<u8>) -> Result<FrameInputs> {
    let reader = ParquetRecordBatchReaderBuilder::try_new(bytes::Bytes::from(meta_bytes))?.build()?;
    let mut t1 = Vec::new();
    let mut t2 = Vec::new();
    let mut cal = Vec::new();
    for batch in reader {
        let batch = batch?;
        let col = |suffix: &str| batch.schema().fields().iter().position(|f| f.name().ends_with(suffix)).map(|i| batch.column(i).clone());
        let index = batch.column_by_name("index").ok_or_else(|| anyhow!("spectra_metadata has no `index` column"))?.as_primitive::<UInt64Type>().clone();
        let f = |c: Option<ArrayRef>| c.map(|c| arrow::compute::cast(&c, &DataType::Float64).expect("numeric").as_primitive::<Float64Type>().clone());
        let (a, b) = (f(col("tdf_t1")), f(col("tdf_t2")));
        let id = col("tdf_mz_calibration_id").map(|c| arrow::compute::cast(&c, &DataType::Int64).expect("integer").as_primitive::<arrow::datatypes::Int64Type>().clone());
        for r in 0..batch.num_rows() {
            let i = index.value(r) as usize;
            if t1.len() <= i {
                t1.resize(i + 1, None);
                t2.resize(i + 1, None);
                cal.resize(i + 1, None);
            }
            t1[i] = a.as_ref().and_then(|c| c.is_valid(r).then(|| c.value(r)));
            t2[i] = b.as_ref().and_then(|c| c.is_valid(r).then(|| c.value(r)));
            cal[i] = id.as_ref().and_then(|c| c.is_valid(r).then(|| c.value(r)));
        }
    }
    Ok(FrameInputs { t1, t2, cal_id: cal })
}

fn read_member<R: Read + Seek>(zip: &mut zip::ZipArchive<R>, name: &str) -> Result<Vec<u8>> {
    let mut f = zip.by_name(name).with_context(|| format!("member {name} not found"))?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)?;
    Ok(buf)
}

/// Rewrite the ims-chunked peaks facet of `input` into the grid layout, copying everything else,
/// into `output`. `bss`: byte-stream-split on the index lists and the bounds (the measured best);
/// `false` leaves them to Parquet's default (dictionary, then plain), as the reference
/// implementation's files are encoded.
pub fn rewrite_archive(input: &Path, output: &Path, zstd_level: i32, bss: bool) -> Result<GridRewriteReport> {
    let f = File::open(input).with_context(|| format!("opening {}", input.display()))?;
    let mut zip = zip::ZipArchive::new(BufReader::new(f)).with_context(|| format!("reading {} as a mzPeak ZIP", input.display()))?;
    let index: serde_json::Value = serde_json::from_slice(&read_member(&mut zip, "mzpeak_index.json")?).context("parsing mzpeak_index.json")?;
    let meta = index.get("metadata").and_then(|m| m.as_object()).ok_or_else(|| anyhow!("mzpeak_index.json has no metadata"))?;
    let cal = meta.get("ims_calibration").ok_or_else(|| anyhow!("{} is not a timsTOF ims-compact archive (no ims_calibration block)", input.display()))?;
    ensure!(
        cal.get("tof_encoding").and_then(|v| v.as_str()) == Some("m/z-chunked"),
        "{} is not a 0.12.x ims-chunked archive (ims_calibration.tof_encoding = {}); only that layout is rewritten",
        input.display(),
        cal.get("tof_encoding").map_or("absent".to_string(), |v| v.to_string())
    );
    let mz_rows: HashMap<i64, MzRow> = meta
        .get("vendor_mz_calibration")
        .and_then(|v| v.get("mz_calibration"))
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("no vendor_mz_calibration.mz_calibration block: the archive cannot say the vendor's m/z model"))?
        .iter()
        .filter_map(|r| Some((r.get("Id")?.as_i64()?, MzRow::from_json(r)?)))
        .collect();
    ensure!(!mz_rows.is_empty(), "vendor_mz_calibration holds no usable row");
    let tims = meta
        .get("vendor_tims_calibration")
        .and_then(|v| v.get("tims_calibration"))
        .and_then(|v| v.as_array())
        .and_then(|a| a.first())
        .and_then(TimsModel::from_json)
        .ok_or_else(|| anyhow!("no vendor_tims_calibration block: the archive cannot say the vendor's mobility model (written since 0.12.5)"))?;
    let inputs = frame_inputs(read_member(&mut zip, METADATA)?)?;
    let first_row = *mz_rows.keys().min().unwrap();
    let p7: Vec<[f64; 7]> = (0..inputs.t1.len())
        .map(|i| {
            let id = inputs.cal_id[i].filter(|id| mz_rows.contains_key(id)).unwrap_or(first_row);
            mz_rows[&id].parameters(inputs.t1[i], inputs.t2[i])
        })
        .collect();
    let p4 = tims.parameters();

    // The peaks facet, streamed from the zip.
    let (data_start, data_len, member_names) = {
        let names: Vec<String> = zip.file_names().map(str::to_string).collect();
        let m = zip.by_name(PEAKS).with_context(|| format!("{PEAKS} not found"))?;
        ensure!(m.compression() == zip::CompressionMethod::Stored, "{PEAKS} is not a stored zip member");
        (m.data_start(), m.size(), names)
    };
    let src = ParquetRecordBatchReaderBuilder::try_new(MemberReader { file: File::open(input)?, start: data_start, len: data_len })?;
    let src_kv: Vec<KeyValue> = src.metadata().file_metadata().key_value_metadata().cloned().unwrap_or_default();
    let src_index: serde_json::Value = src_kv
        .iter()
        .find(|kv| kv.key == "spectrum_array_index")
        .and_then(|kv| kv.value.as_deref())
        .map(|s| serde_json::from_str(s))
        .transpose()?
        .ok_or_else(|| anyhow!("{PEAKS} has no spectrum_array_index"))?;
    let src_schema = src.schema().clone();
    let DataType::Struct(src_fields) = src_schema.field(0).data_type() else { bail!("{PEAKS}: the first column is not the chunk struct") };
    ensure!(src_schema.field(0).name() == "chunk", "{PEAKS} is not a chunk-layout facet");
    let intensity_field = src_fields.iter().find(|f| f.name() == "intensity").ok_or_else(|| anyhow!("{PEAKS} has no intensity column"))?.clone();
    let intensity_entry = src_index["entries"]
        .as_array()
        .and_then(|e| e.iter().find(|e| e["path"] == "chunk.intensity"))
        .cloned()
        .ok_or_else(|| anyhow!("{PEAKS}: no array-index entry for chunk.intensity"))?;
    let mut reader = Some(src.with_batch_size(ROW_GROUP_ROWS).build()?);

    // The new facet's schema: the reference implementation's columns, names and field metadata.
    let mz_meta = |fmt: &str| field_meta(&[("array_accession", "MS:1000514"), ("array_name", "m/z array"), ("buffer_format", fmt), ("buffer_priority", "primary"), ("context", "spectrum"), ("data_type_accession", "MS:1000523"), ("sorting_rank", "0"), ("unit", "MS:1000040")]);
    let mut grid_mz_meta = mz_meta("chunk_transform");
    grid_mz_meta.insert("transform".into(), GRID_ENCODING.to_string());
    let grid_im_meta = {
        let mut m = field_meta(&[("array_accession", "MS:1003006"), ("array_name", "mean inverse reduced ion mobility array"), ("buffer_format", "chunk_transform"), ("buffer_priority", "primary"), ("context", "spectrum"), ("data_type_accession", "MS:1000523"), ("unit", "MS:1002814")]);
        m.insert("transform".into(), GRID_ENCODING.to_string());
        m
    };
    let chunk_fields = Fields::from(vec![
        Field::new("spectrum_index", DataType::UInt64, false),
        Field::new("mz_chunk_start", DataType::Float64, true).with_metadata(mz_meta("chunk_start")),
        Field::new("mz_chunk_end", DataType::Float64, true).with_metadata(mz_meta("chunk_end")),
        Field::new("mz_chunk_values", DataType::LargeList(Arc::new(Field::new("item", DataType::Float64, true))), true).with_metadata(mz_meta("chunk_values")),
        Field::new("chunk_encoding", DataType::Utf8, true).with_metadata(mz_meta("chunk_encoding")),
        (*intensity_field).clone(),
        Field::new("mz_grid", grid_struct_type(), true).with_metadata(grid_mz_meta),
        Field::new("mean_inverse_reduced_ion_mobility_grid", grid_struct_type(), true).with_metadata(grid_im_meta),
    ]);
    let schema = Arc::new(Schema::new(vec![Field::new("chunk", DataType::Struct(chunk_fields.clone()), false)]));
    let entry = |path: &str, data_type: &str, array_type: &str, array_name: &str, unit: &str, fmt: &str, transform: Option<&str>, rank: Option<u32>| {
        serde_json::json!({"context": "spectrum", "path": path, "data_type": data_type, "array_type": array_type, "array_name": array_name, "unit": unit,
                           "buffer_format": fmt, "transform": transform, "data_processing_id": null, "buffer_priority": "primary", "sorting_rank": rank})
    };
    let mz = |path: &str, fmt: &str, transform: Option<&str>| entry(path, "MS:1000523", "MS:1000514", "m/z array", "MS:1000040", fmt, transform, Some(0));
    let array_index = serde_json::json!({"prefix": "chunk", "entries": [
        mz("chunk.mz_chunk_start", "chunk_start", None),
        mz("chunk.mz_chunk_end", "chunk_end", None),
        mz("chunk.mz_chunk_values", "chunk_values", None),
        mz("chunk.chunk_encoding", "chunk_encoding", None),
        intensity_entry,
        mz("chunk.mz_grid", "chunk_transform", Some(&GRID_ENCODING.to_string())),
        entry("chunk.mean_inverse_reduced_ion_mobility_grid", "MS:1000523", "MS:1003006", "mean inverse reduced ion mobility array", "MS:1002814", "chunk_transform", Some(&GRID_ENCODING.to_string()), None),
    ]});
    let mut kv: Vec<KeyValue> = src_kv.into_iter().filter(|kv| kv.key != "ARROW:schema" && kv.key != "spectrum_array_index").collect();
    kv.push(KeyValue::new("spectrum_array_index".to_string(), array_index.to_string()));

    let level = ZstdLevel::try_new(zstd_level).map_err(|e| anyhow!("invalid zstd level {zstd_level}: {e}"))?;
    let path = |parts: &[&str]| ColumnPath::from(parts.iter().map(|s| s.to_string()).collect::<Vec<_>>());
    let mut props = WriterProperties::builder()
        .set_compression(Compression::ZSTD(level))
        .set_max_row_group_row_count(Some(ROW_GROUP_ROWS))
        .set_key_value_metadata(Some(kv))
        .set_column_dictionary_enabled(path(&["chunk", "spectrum_index"]), false)
        .set_column_encoding(path(&["chunk", "spectrum_index"]), Encoding::DELTA_BINARY_PACKED)
        .set_column_dictionary_enabled(path(&["chunk", "intensity", "list", "item"]), false)
        .set_column_encoding(path(&["chunk", "intensity", "list", "item"]), Encoding::BYTE_STREAM_SPLIT);
    if bss {
        for p in [
            vec!["chunk", "mz_chunk_start"],
            vec!["chunk", "mz_chunk_end"],
            vec!["chunk", "mz_grid", "indices", "list", "item"],
            vec!["chunk", "mean_inverse_reduced_ion_mobility_grid", "indices", "list", "item"],
        ] {
            props = props.set_column_dictionary_enabled(path(&p), false).set_column_encoding(path(&p), Encoding::BYTE_STREAM_SPLIT);
        }
    }
    let props = props.build();

    // Write: every member in its original order, the peaks facet transformed on the way.
    let orig_files: HashMap<String, FileEntry> = index["files"]
        .as_array()
        .map(|files| files.iter().filter_map(|f| serde_json::from_value::<FileEntry>(f.clone()).ok()).map(|e| (e.name.clone(), e)).collect())
        .unwrap_or_default();
    let tmp = output.with_extension("mzpeak.tmp");
    let tmp_guard = crate::TmpGuard::new(&tmp);
    let out = File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
    let mut w = ZipArchiveWriter::new(out);
    let mut report = GridRewriteReport { frames: inputs.t1.len(), chunk_rows: 0, points: 0, mobility_exact: 0, peaks_bytes_before: data_len, peaks_bytes_after: 0 };
    for name in &member_names {
        if name == "mzpeak_index.json" {
            continue;
        }
        let fe = orig_files.get(name).cloned().unwrap_or_else(|| {
            FileEntry::new(name.clone(), mzpeak_prototyping::archive::EntityType::Other("other".into()), mzpeak_prototyping::archive::DataKind::Other("other".into()))
        });
        if name == PEAKS {
            w.start_for_entry(fe).map_err(|e| anyhow!("starting member {name}: {e}"))?;
            let mut sink = Counting { inner: &mut w, n: 0 };
            {
                let mut writer = ArrowWriter::try_new(&mut sink, schema.clone(), Some(props.clone()))?;
                for batch in reader.take().ok_or_else(|| anyhow!("{PEAKS} appears twice in {}", input.display()))? {
                    let batch = batch?;
                    let out = transform_batch(&batch, &chunk_fields, &p7, &p4, &tims, &mut report)?;
                    writer.write(&out)?;
                }
                writer.close()?;
            }
            report.peaks_bytes_after = sink.n;
        } else {
            let mut src = zip.by_name(name).with_context(|| format!("opening member {name}"))?;
            w.add_file_from_read(&mut src, None::<&String>, Some(fe)).with_context(|| format!("copying member {name}"))?;
        }
    }

    // Index metadata: everything carried, `ims_calibration` replaced, the rewrite recorded.
    let tof_cal = serde_json::json!({
        "codec": "ims-compact",
        "layout": "grid-transform",
        "tof_encoding": "grid",
        "chunk_bounds": "mz",
        "chunk_width_th": cal.get("chunk_width_th").cloned().unwrap_or(serde_json::Value::Null),
        "intensity_dtype": cal.get("intensity_dtype").cloned().unwrap_or(serde_json::Value::Null),
        "exact": true,
        "mz_grid": {
            "column": "chunk.mz_grid",
            "model": TIMSTOF_MZ_GRID.to_string(),
            "parameters": "[C0, 1e6/sqrt(C1*cf), C2/cf, C3, C4, DigitizerTimebase, DigitizerDelay] of the frame's MzCalibration row, cf = 1 + (dC1*(T1 - Frames.T1) + dC2*(T2 - Frames.T2))/1e6",
            "indices": "[first TOF bin, deltas...] (uint32); t_ns = fma(bin, timebase, delay); solve t_ns = C0 + beta*u + C2*u^2 (+ C3*u^3) for u; m/z = u^2 - C4",
            "evaluation": "mzdata::io::tdf::MzCalibrationModel2::convert_f64 (reference implementation); the chunk bounds are that evaluation at the first and last bin",
            "verified": "1e-9 ppm vs Bruker timsdata SDK on a C2 != 0, C4 != 0 file (tests/fixtures/tdf_diapasef_sdk_golden.json)",
        },
        "ion_mobility_grid": {
            "column": "chunk.mean_inverse_reduced_ion_mobility_grid",
            "model": TIMSTOF_TIMS_GRID.to_string(),
            "parameters": "[C6, C7, offset, slope] of TimsCalibration ModelType 2: slope = (C3 - C2)/C1, offset = C2 - slope*(C4 + C0)",
            "indices": "TIMS scan number (uint32, 0-based, not delta-coded); 1/K0 = 1/(C6 + C7/(offset + slope*scan))",
            "evaluation": "mzdata::io::tdf::TimsCalibrationModel2::convert (reference implementation); differs from the closed form W/(C7 + C6*W) by at most 1 ulp",
        },
        "encoding": if bss { "byte-stream-split on the index lists and the bounds" } else { "Parquet default (dictionary, then plain)" },
        "previous_layout": {"tof_encoding": "m/z-chunked", "note": "rewritten from the 0.12.x layout (integer TOF bounds, tof_chunk_values deltas, tof_c0/tof_c1 per spectrum); the tof_c0/tof_c1 spectra_metadata columns are kept as provenance and are no longer read"},
    });
    for (k, v) in meta {
        let v = match k.as_str() {
            "ims_calibration" => &tof_cal,
            "data_processing_method_list" => {
                let mut list = v.clone();
                if let Some(arr) = list.as_array_mut() {
                    arr.push(serde_json::json!({
                        "id": "mzpeak_convert_grid_layout",
                        "methods": [{"order": 1, "software_reference": "mzpeak-convert", "parameters": [
                            {"accession": null, "name": "conversion options", "unit": null, "value": format!("--ims-grid --grid-encoding {}", if bss { "bss" } else { "plain" })},
                            {"accession": "MS:1000530", "name": "file format conversion", "unit": null, "value": null}]}]
                    }));
                }
                w.add_index_metadata(k, &list).map_err(|e| anyhow!("index metadata {k}: {e}"))?;
                continue;
            }
            "transformations" => {
                let mut list = v.clone();
                if let Some(arr) = list.as_array_mut() {
                    arr.push(serde_json::json!("grid-encode:mz,ion_mobility"));
                }
                w.add_index_metadata(k, &list).map_err(|e| anyhow!("index metadata {k}: {e}"))?;
                continue;
            }
            _ => v,
        };
        w.add_index_metadata(k, v).map_err(|e| anyhow!("index metadata {k}: {e}"))?;
    }
    w.finish().map_err(|e| anyhow!("finalizing {}: {e}", output.display()))?;
    tmp_guard.finish(output)?;
    log::info!(
        "grid layout: {} frames, {} chunk rows, {} points; mobility scan round trip exact on {} points; peaks facet {} -> {} bytes ({:+.2}%)",
        report.frames, report.chunk_rows, report.points, report.mobility_exact, report.peaks_bytes_before, report.peaks_bytes_after,
        (report.peaks_bytes_after as f64 / report.peaks_bytes_before as f64 - 1.0) * 100.0
    );
    Ok(report)
}

/// One row group of the 0.12.x facet → one of the grid layout.
fn transform_batch(batch: &RecordBatch, out_fields: &Fields, p7: &[[f64; 7]], p4: &[f64; 4], tims: &TimsModel, report: &mut GridRewriteReport) -> Result<RecordBatch> {
    let root = batch.column(0).as_struct();
    let col = |n: &str| root.column_by_name(n).ok_or_else(|| anyhow!("{PEAKS}: no `{n}` column; not a 0.12.x ims-chunked facet"));
    let si = col("spectrum_index")?.as_primitive::<UInt64Type>().clone();
    let starts = col("tof_chunk_start")?.as_primitive::<Float64Type>().clone();
    let deltas = col("tof_chunk_values")?.clone();
    let encodings = col("chunk_encoding")?.clone();
    let mob = col("mean_inverse_reduced_ion_mobility")?.clone();
    let intensity = col("intensity")?.clone();
    let n = batch.num_rows();
    let list_rows = |a: &ArrayRef, r: usize| -> ArrayRef {
        if let Some(l) = a.as_list_opt::<i64>() {
            l.value(r)
        } else if let Some(l) = a.as_list_opt::<i32>() {
            l.value(r)
        } else {
            panic!("{PEAKS}: list column has type {:?}", a.data_type())
        }
    };
    let enc_at = |r: usize| -> String {
        if let Some(a) = encodings.as_string_opt::<i32>() {
            a.value(r).to_string()
        } else if let Some(a) = encodings.as_string_opt::<i64>() {
            a.value(r).to_string()
        } else {
            String::new()
        }
    };

    let mut mz_start = Vec::with_capacity(n);
    let mut mz_end = Vec::with_capacity(n);
    let mut idx_offsets = Vec::with_capacity(n + 1);
    let mut idx_values: Vec<u32> = Vec::new();
    let mut scan_offsets = Vec::with_capacity(n + 1);
    let mut scan_values: Vec<u32> = Vec::new();
    let mut mz_params = Vec::with_capacity(7 * n);
    idx_offsets.push(0i64);
    scan_offsets.push(0i64);
    for r in 0..n {
        ensure!(enc_at(r) == "MS:1003089", "{PEAKS}: chunk row {r} is {:?}, not delta-encoded TOF (MS:1003089)", enc_at(r));
        let frame = si.value(r) as usize;
        let p = p7.get(frame).ok_or_else(|| anyhow!("spectrum {frame} has no metadata row"))?;
        let seed = starts.value(r);
        ensure!(seed >= 0.0 && seed.fract() == 0.0 && seed <= u32::MAX as f64, "{PEAKS}: tof_chunk_start {seed} is not a TOF bin");
        let d = list_rows(&deltas, r);
        let d = d.as_primitive::<Int32Type>();
        let mut last = seed as u32;
        idx_values.push(seed as u32);
        for k in 0..d.len() {
            let v = d.value(k);
            ensure!(v >= 0, "{PEAKS}: negative TOF delta {v} in chunk row {r}");
            last = last.checked_add(v as u32).ok_or_else(|| anyhow!("TOF bin overflow in chunk row {r}"))?;
            idx_values.push(v as u32);
        }
        idx_offsets.push(idx_values.len() as i64);
        mz_start.push(timstof_mz(p, seed));
        mz_end.push(timstof_mz(p, last as f64));
        mz_params.extend_from_slice(p);
        let k0s = list_rows(&mob, r);
        let k0s = k0s.as_primitive::<Float64Type>();
        for k in 0..k0s.len() {
            let k0 = k0s.value(k);
            let scan = tims.scan_of(k0).round();
            ensure!(scan >= 0.0 && scan <= u32::MAX as f64, "1/K0 {k0} inverts to scan {scan}, outside the TIMS model");
            let back = timstof_mobility(p4, scan);
            if back == k0 {
                report.mobility_exact += 1;
            } else {
                ensure!(
                    (back - k0).abs() <= 4.0 * f64::EPSILON * k0.abs(),
                    "1/K0 {k0} (spectrum {frame}) is not on the vendor's TIMS scan grid (scan {scan} evaluates to {back}); \
                     an archive written with --no-tims-recalibration holds timsrust's linear approximation and cannot take the grid layout"
                );
            }
            scan_values.push(scan as u32);
        }
        scan_offsets.push(scan_values.len() as i64);
        ensure!(k0s.len() == d.len() + 1, "{PEAKS}: chunk row {r} has {} mobility values for {} points", k0s.len(), d.len() + 1);
    }
    report.chunk_rows += n as u64;
    report.points += idx_values.len() as u64;

    let mz_grid = grid_struct(&TIMSTOF_MZ_GRID.to_string(), n, mz_params, 7, idx_offsets, idx_values);
    let im_grid = grid_struct(&TIMSTOF_TIMS_GRID.to_string(), n, p4.iter().copied().cycle().take(4 * n).collect(), 4, scan_offsets, scan_values);
    let null_values = LargeListArray::new_null(Arc::new(Field::new("item", DataType::Float64, true)), n);
    let intensity = if intensity.as_list_opt::<i64>().is_some() {
        intensity
    } else {
        arrow::compute::cast(&intensity, out_fields.iter().find(|f| f.name() == "intensity").unwrap().data_type())?
    };
    let columns: Vec<ArrayRef> = vec![
        Arc::new(si),
        Arc::new(Float64Array::from(mz_start)),
        Arc::new(Float64Array::from(mz_end)),
        Arc::new(null_values),
        Arc::new(StringArray::from(vec![GRID_ENCODING.to_string(); n])),
        intensity,
        Arc::new(mz_grid),
        Arc::new(im_grid),
    ];
    let chunk = StructArray::new(out_fields.clone(), columns, None);
    let schema = Arc::new(Schema::new(vec![Field::new("chunk", DataType::Struct(out_fields.clone()), false)]));
    Ok(RecordBatch::try_new(schema, vec![Arc::new(chunk)])?)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The 7 parameters built from a vendor row reproduce Bruker's own `tims_index_to_mz` on the
    /// SDK sample of a file with `C2 ≠ 0`, `C4 ≠ 0` and a real temperature offset — through the
    /// reference implementation's evaluation, which is what a reader of this layout runs.
    #[test]
    fn frame_parameters_reproduce_the_vendor_sdk() {
        let g: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/tdf_diapasef_sdk_golden.json")).unwrap()).unwrap();
        let rows: HashMap<i64, MzRow> = g["mz_calibration"].as_array().unwrap().iter().map(|r| (r["Id"].as_i64().unwrap(), MzRow::from_json(r).unwrap())).collect();
        let mut worst = 0.0f64;
        for p in g["points"].as_array().unwrap() {
            let sdk = p["mz_sdk"].as_f64().unwrap();
            if !(sdk > 0.0) {
                continue;
            }
            let params = rows[&p["cal_id"].as_i64().unwrap()].parameters(p["t1"].as_f64(), p["t2"].as_f64());
            let mz = timstof_mz(&params, p["tof"].as_f64().unwrap());
            worst = worst.max(((mz - sdk) / sdk).abs() * 1e6);
        }
        assert!(worst < 1e-6, "worst {worst:.2e} ppm");
    }

    /// Every scan of the 2485 TIMS row round-trips through the inverse and the model bit for bit
    /// or within the last bit — the condition the rewrite enforces on every stored 1/K0.
    #[test]
    fn scan_numbers_round_trip_through_the_mobility_model() {
        let row = serde_json::json!({"C0": 1, "C1": 926, "C2": 217.23199652301727, "C3": 74.71319596975974, "C4": 33.0, "C6": 0.020932469715718494, "C7": 131.22279563838268});
        let m = TimsModel::from_json(&row).unwrap();
        let p = m.parameters();
        for scan in 0..=926u32 {
            let k0 = timstof_mobility(&p, scan as f64);
            let back = m.scan_of(k0).round();
            assert_eq!(back as u32, scan, "scan {scan}: k0 {k0} -> {back}");
            let again = timstof_mobility(&p, back);
            assert!((again - k0).abs() <= 4.0 * f64::EPSILON * k0);
        }
    }
}

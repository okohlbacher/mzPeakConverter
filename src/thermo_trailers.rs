//! Thermo scan-trailer vendor facet (PLAN P4, Track 2).
//!
//! mzdata's Thermo reader maps a few trailer values to CV params but does NOT surface the full
//! verbatim scan-trailer bag (Master Scan, Charge State, Monoisotopic M/Z, FAIMS CV, Ion Injection
//! Time, …). We read it directly from `thermorawfilereader` (the crate mzdata wraps) — a second,
//! metadata-only pass over the `.raw` — and emit a `vendor_scan_trailers.parquet` proprietary facet
//! (tall: ordinal, label, value verbatim, value_float when numeric). This is the converter-side of
//! the raw-verbatim-metadata model; values are captured verbatim, never reinterpreted.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use arrow::array::{Float64Array, StringArray, UInt64Array};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use parquet::arrow::ArrowWriter;
use parquet::basic::{Compression, ZstdLevel};
use parquet::file::properties::WriterProperties;

use thermorawfilereader::RawFileReader;

/// Build the `vendor_scan_trailers.parquet` bytes for a Thermo `.raw`, or `None` if the file has no
/// trailers. Tall layout: one row per (spectrum ordinal × trailer label).
pub fn build_trailer_facet(raw_path: &Path) -> Result<Option<Vec<u8>>> {
    let handle = RawFileReader::open(raw_path)
        .map_err(|e| anyhow::anyhow!("opening {} for trailers: {e}", raw_path.display()))?;
    let n = handle.len();

    let (mut ordinals, mut labels, mut values, mut floats) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::<Option<f64>>::new());
    for i in 0..n {
        let Some(trailers) = handle.get_raw_trailers_for(i) else { continue };
        for kv in trailers.iter() {
            let value = kv.value.trim();
            ordinals.push(i as u64);
            labels.push(kv.label.to_string());
            // value_float: parse the verbatim string when it is cleanly numeric (the raw string is
            // always preserved in `value`, so this is a convenience, never a reinterpretation).
            floats.push(value.parse::<f64>().ok());
            values.push(value.to_string());
        }
    }
    if ordinals.is_empty() {
        return Ok(None);
    }

    let schema = Arc::new(Schema::new(vec![
        Field::new("ordinal", DataType::UInt64, false),
        Field::new("label", DataType::Utf8, false),
        Field::new("value", DataType::Utf8, false),
        Field::new("value_float", DataType::Float64, true),
    ]));
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from(ordinals)),
            Arc::new(StringArray::from(labels)),
            Arc::new(StringArray::from(values)),
            Arc::new(Float64Array::from(floats)),
        ],
    )?;

    let props = WriterProperties::builder()
        .set_compression(Compression::ZSTD(ZstdLevel::try_new(3).unwrap()))
        .build();
    let mut buf = Vec::new();
    {
        let mut w = ArrowWriter::try_new(&mut buf, schema, Some(props))
            .context("creating vendor_scan_trailers writer")?;
        w.write(&batch)?;
        w.close()?;
    }
    Ok(Some(buf))
}

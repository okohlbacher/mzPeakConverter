//! Thermo run status-log + wide scan-trailer vendor facets (PLAN P4, Track 2).
//!
//! Two additional proprietary facets read directly from `thermorawfilereader` (the crate mzdata
//! wraps), in the same raw-verbatim-metadata spirit as `thermo_trailers.rs`:
//!
//!  * `vendor_status_log.parquet` — the run status-log timeseries (`get_status_logs()` →
//!    `StatusLogCollection`). Thermo records named instrument status channels (vacuum, source
//!    temperatures, lens voltages, …) sampled over the run; we flatten every typed channel
//!    (string/int/float/bool) into a tall (position, rt, label, value, value_float) table.
//!  * `vendor_scan_trailers_wide.parquet` — a WIDE pivot of the per-spectrum scan trailers: one row
//!    per spectrum ordinal, one TYPED column per distinct trailer label (Float64 when every present
//!    value parses numerically, else Utf8). Complements the tall `vendor_scan_trailers.parquet`.
//!
//! Values are captured verbatim; numeric coercion is a convenience, never a reinterpretation.

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::{Context, Result};
use arrow::array::{ArrayRef, Float64Array, Int64Array, StringArray};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use parquet::arrow::ArrowWriter;
use parquet::basic::{Compression, ZstdLevel};
use parquet::file::properties::WriterProperties;

use thermorawfilereader::RawFileReader;

/// Serialize a single `RecordBatch` to zstd-compressed parquet bytes (mirrors thermo_trailers.rs).
fn write_parquet(schema: Arc<Schema>, batch: RecordBatch, ctx: &'static str) -> Result<Vec<u8>> {
    let props = WriterProperties::builder()
        .set_compression(Compression::ZSTD(ZstdLevel::try_new(3).unwrap()))
        .build();
    let mut buf = Vec::new();
    {
        let mut w = ArrowWriter::try_new(&mut buf, schema, Some(props)).context(ctx)?;
        w.write(&batch)?;
        w.close()?;
    }
    Ok(buf)
}

/// Build the `vendor_status_log.parquet` bytes for a Thermo `.raw`, or `None` if the run carries no
/// status logs. Tall layout: one row per (status channel × sample). `position` is the sample index
/// within its channel, `rt` the retention time, `label` the channel name, `value` the verbatim
/// stringified reading, and `value_float` the numeric reading when the channel is numeric.
pub fn build_status_log_facet(handle: &RawFileReader) -> Result<Option<Vec<u8>>> {
    let Some(logs) = handle.get_status_logs() else {
        return Ok(None);
    };
    if !logs.check() {
        log::debug!("status-log buffer failed validation; skipping vendor_status_log facet");
        return Ok(None);
    }

    let (mut positions, mut rts, mut labels, mut values, mut floats) = (
        Vec::<i64>::new(),
        Vec::<f64>::new(),
        Vec::<String>::new(),
        Vec::<String>::new(),
        Vec::<Option<f64>>::new(),
    );
    let mut push = |label: &str, pos: i64, rt: f64, value: String, vf: Option<f64>| {
        positions.push(pos);
        rts.push(rt);
        labels.push(label.to_string());
        values.push(value);
        floats.push(vf);
    };

    for log in logs.float_logs() {
        for (pos, (rt, v)) in log.iter().enumerate() {
            push(&log.name, pos as i64, rt, v.to_string(), Some(v));
        }
    }
    for log in logs.int_logs() {
        for (pos, (rt, v)) in log.iter().enumerate() {
            push(&log.name, pos as i64, rt, v.to_string(), Some(v as f64));
        }
    }
    for log in logs.str_logs() {
        for (pos, (rt, v)) in log.iter_strings().enumerate() {
            let v = v.trim();
            let vf = v.parse::<f64>().ok();
            push(&log.name, pos as i64, rt, v.to_string(), vf);
        }
    }
    for log in logs.bool_logs() {
        for (pos, (rt, v)) in log.iter_flags().enumerate() {
            push(
                &log.name,
                pos as i64,
                rt,
                v.to_string(),
                Some(if v { 1.0 } else { 0.0 }),
            );
        }
    }

    if positions.is_empty() {
        return Ok(None);
    }

    let schema = Arc::new(Schema::new(vec![
        Field::new("position", DataType::Int64, false),
        Field::new("rt", DataType::Float64, false),
        Field::new("label", DataType::Utf8, false),
        Field::new("value", DataType::Utf8, false),
        Field::new("value_float", DataType::Float64, true),
    ]));
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(Int64Array::from(positions)),
            Arc::new(Float64Array::from(rts)),
            Arc::new(StringArray::from(labels)),
            Arc::new(StringArray::from(values)),
            Arc::new(Float64Array::from(floats)),
        ],
    )?;

    Ok(Some(write_parquet(
        schema,
        batch,
        "creating vendor_status_log writer",
    )?))
}

/// Sanitize a trailer label into a parquet-friendly column name: every run of non-alphanumeric
/// characters collapses to a single `_`, leading/trailing `_` trimmed. Empty result falls back to
/// `col`.
fn sanitize_label(label: &str) -> String {
    let mut out = String::with_capacity(label.len());
    let mut prev_us = false;
    for ch in label.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch);
            prev_us = false;
        } else if !prev_us {
            out.push('_');
            prev_us = true;
        }
    }
    let trimmed = out.trim_matches('_');
    if trimmed.is_empty() {
        "col".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Build the `vendor_scan_trailers_wide.parquet` bytes for a Thermo `.raw`, or `None` if the file
/// has no trailers. Wide layout: one row per spectrum ordinal, one TYPED column per distinct trailer
/// label. A column is Float64 when every present value parses as f64, otherwise Utf8. Column names
/// are the sanitized labels (collisions disambiguated with a numeric suffix).
pub fn build_trailer_wide_facet(handle: &RawFileReader) -> Result<Option<Vec<u8>>> {
    let n = handle.len();

    // Pass 1: collect, per spectrum, the verbatim value for each distinct label (first occurrence
    // wins within a spectrum). `label_order` preserves first-seen order across the whole run.
    let mut label_order: Vec<String> = Vec::new();
    let mut per_spectrum: Vec<BTreeMap<String, String>> = Vec::with_capacity(n);
    let mut seen: BTreeMap<String, ()> = BTreeMap::new();
    let mut any = false;

    for i in 0..n {
        let mut row: BTreeMap<String, String> = BTreeMap::new();
        if let Some(trailers) = handle.get_raw_trailers_for(i) {
            for kv in trailers.iter() {
                any = true;
                let label = kv.label.to_string();
                if seen.insert(label.clone(), ()).is_none() {
                    label_order.push(label.clone());
                }
                row.entry(label).or_insert_with(|| kv.value.trim().to_string());
            }
        }
        per_spectrum.push(row);
    }

    if !any {
        return Ok(None);
    }

    // Decide each column's type: Float64 iff every present value parses as f64.
    // Build the typed arrays in one pass per label.
    let ordinals: Vec<i64> = (0..n as i64).collect();
    let mut fields: Vec<Field> = vec![Field::new("ordinal", DataType::Int64, false)];
    let mut columns: Vec<ArrayRef> = vec![Arc::new(Int64Array::from(ordinals))];
    let mut used_names: BTreeMap<String, usize> = BTreeMap::new();

    for label in &label_order {
        let is_float = per_spectrum.iter().all(|row| match row.get(label) {
            Some(v) => v.parse::<f64>().is_ok(),
            None => true,
        });

        // Disambiguate sanitized column-name collisions.
        let base = sanitize_label(label);
        let name = match used_names.get_mut(&base) {
            Some(count) => {
                *count += 1;
                format!("{base}_{count}")
            }
            None => {
                used_names.insert(base.clone(), 0);
                base
            }
        };

        if is_float {
            let col: Vec<Option<f64>> = per_spectrum
                .iter()
                .map(|row| row.get(label).and_then(|v| v.parse::<f64>().ok()))
                .collect();
            fields.push(Field::new(&name, DataType::Float64, true));
            columns.push(Arc::new(Float64Array::from(col)));
        } else {
            let col: Vec<Option<String>> = per_spectrum
                .iter()
                .map(|row| row.get(label).cloned())
                .collect();
            fields.push(Field::new(&name, DataType::Utf8, true));
            columns.push(Arc::new(StringArray::from(col)));
        }
    }

    let schema = Arc::new(Schema::new(fields));
    let batch = RecordBatch::try_new(schema.clone(), columns)?;

    Ok(Some(write_parquet(
        schema,
        batch,
        "creating vendor_scan_trailers_wide writer",
    )?))
}

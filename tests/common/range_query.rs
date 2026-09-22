//! Shared by the grid range-query regressions: the oracle for an m/z window is a FULL read filtered
//! in memory, and a range query must hand back exactly those points.

use arrow::array::{Array, AsArray, RecordBatch};
use arrow::datatypes::{DataType, Float32Type, Float64Type, UInt64Type};
use arrow::error::ArrowError;
use mzdata::spectrum::BinaryArrayMap;

/// (spectrum index, m/z bits, intensity) of every row a range query returned, sorted. m/z is kept as
/// BITS: the query must reproduce the full read bit for bit, not "closely".
pub fn rows(batches: impl Iterator<Item = Result<RecordBatch, ArrowError>>) -> Vec<(u64, u64, f32)> {
    let mut out = Vec::new();
    for batch in batches {
        let batch = batch.expect("range query batch");
        let root = batch.column(0).as_struct();
        assert!(root.column_by_name("tof_index").is_none() && root.column_by_name("tof").is_none(), "the grid column leaked into the result");
        let index = root.column_by_name("spectrum_index").expect("spectrum_index").as_primitive::<UInt64Type>();
        let mz = root.column_by_name("mz").expect("an m/z column").as_primitive::<Float64Type>();
        // Float32 on the mzML lanes, Int32 on the timsTOF ims-compact facets; the full read hands out f32.
        let intensity = arrow::compute::cast(root.column_by_name("intensity").expect("intensity"), &DataType::Float32).unwrap();
        let intensity = intensity.as_primitive::<Float32Type>();
        assert_eq!(mz.null_count(), 0, "a returned row has no m/z");
        for i in 0..batch.num_rows() {
            out.push((index.value(i), mz.value(i).to_bits(), intensity.value(i)));
        }
    }
    out.sort_by(|a, b| a.partial_cmp(b).unwrap());
    out
}

/// The same triples from one spectrum's fully-read arrays, restricted to `window` (inclusive).
pub fn expected(index: u64, arrays: &BinaryArrayMap, window: Option<(f64, f64)>, into: &mut Vec<(u64, u64, f32)>) {
    let mzs = arrays.mzs().expect("full read has m/z");
    let intensities = arrays.intensities().expect("full read has intensity");
    for (mz, it) in mzs.iter().zip(intensities.iter()) {
        if window.is_none_or(|(lo, hi)| *mz >= lo && *mz <= hi) {
            into.push((index, mz.to_bits(), *it));
        }
    }
}

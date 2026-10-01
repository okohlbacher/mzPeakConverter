//! An empty array gets no grid model. The vendored `GridPolicy::model_from_array_map` unwrapped the
//! range of the array, so one empty spectrum on a grid lane aborted the conversion: since 0.14.0 the
//! native SciEX lane panicked on every `.wiff` holding an empty spectrum (four corpus units fell
//! back to msconvert, which drops those spectra).

use mzdata::spectrum::bindata::{ArrayType, BinaryArrayMap, BinaryDataArrayType, DataArray};
use mzpeak_prototyping::grid::GridPolicy;

fn mz_map(values: &[f64]) -> BinaryArrayMap {
    let mut mz = DataArray::from_name_and_type(&ArrayType::MZArray, BinaryDataArrayType::Float64);
    mz.extend(values).unwrap();
    let mut map = BinaryArrayMap::new();
    map.add(mz);
    map
}

#[test]
fn an_empty_mz_array_gets_no_grid_model() {
    let policy = GridPolicy::new(ArrayType::MZArray, true, None);
    assert!(policy.model_from_array_map(&mz_map(&[]), Some(5e-4)).is_none());
    // A map without the array at all, and a populated one, behave as before.
    assert!(policy.model_from_array_map(&BinaryArrayMap::new(), Some(5e-4)).is_none());
    let grid: Vec<f64> = (0..200).map(|k| (10.0 + 0.01 * k as f64).powi(2)).collect();
    assert!(policy.model_from_array_map(&mz_map(&grid), Some(5e-4)).is_some());
}

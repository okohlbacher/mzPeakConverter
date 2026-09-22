//! Grid-encoded chunk rows (`chunk_encoding = MS:1003826`, "coordinate grid encoding"): the row's
//! coordinates are integer indices into a grid model whose parameters ride in the same row, in a
//! struct column `<array>_grid { grid_type, parameters, indices }` registered in the array index as
//! `buffer_format = chunk_transform`, `transform = MS:1003826` (Joshua Klein's 2a layout,
//! mzpeak_prototyping `e62e18c`, 2026-09-21).
//!
//! Models, with the parameter order and the ARITHMETIC of the reference implementation (mzdata
//! `io::tdf::calibration.rs`, mzpeak_prototyping `grid.rs`) so that a bound written by one reader
//! equals the value decoded by the other bit for bit:
//!
//! | `grid_type`  | parameters | value at index `i` |
//! |--------------|------------|--------------------|
//! | `MS:1003824` linear grid | `[intercept, slope, scale = 1]` | `(i·slope + intercept) / scale` |
//! | `MS:1003825` square root grid | `[intercept, slope, scale = 1]` | `(i·slope + intercept)² / scale` |
//! | `MS:9999002` (placeholder) Bruker timsTOF m/z calibration | `[C0, β, C2, C3, C4, timebase, delay]` | `t = fma(i, timebase, delay)`, solve `t = C0 + β·u + C2·u² (+ C3·u³)` for `u`, `m/z = u² − C4` |
//! | `MS:9999001` (placeholder) Bruker TIMS mobility | `[C6, C7, offset, slope]` | `1 / (C6 + C7 / (offset + slope·i))` |
//!
//! The MAIN-axis index list is delta-coded (`[first, deltas…]`, the start point included); a
//! SECONDARY grid's indices are stored as they are. `MS:9999002` reproduces Bruker's own
//! `tims_index_to_mz` to 1e-9 ppm on a file with `C2 ≠ 0`, `C4 ≠ 0` (SDK sample, 2026-09-22).
use arrow::array::{Array, ArrayRef, AsArray, Float64Array, StructArray};
use arrow::datatypes::{Float64Type, UInt16Type, UInt32Type, UInt8Type};
use mzdata::params::CURIE;

pub const GRID_ENCODING: CURIE = mzdata::curie!(MS:1003826);
pub const LINEAR_GRID: CURIE = mzdata::curie!(MS:1003824);
pub const SQRT_GRID: CURIE = mzdata::curie!(MS:1003825);
pub const TIMSTOF_MZ_GRID: CURIE = mzdata::curie!(MS:9999002);
pub const TIMSTOF_TIMS_GRID: CURIE = mzdata::curie!(MS:9999001);

/// Bruker timsTOF m/z at digitizer index `idx`: `mzdata::io::tdf::MzCalibrationModel2::convert_f64`,
/// operation for operation (fused multiply-add for the flight time, `powi(2)` for the square).
pub fn timstof_mz(p: &[f64], idx: f64) -> f64 {
    let (c0, beta, c2, c3, c4, timebase, delay) = (p[0], p[1], p[2], p[3], p[4], p[5], p[6]);
    let tof = idx.mul_add(timebase, delay);
    let s0 = (tof - c0) / beta;
    let refined = if c3 != 0.0 {
        let mut s = s0;
        let (c2_2, c3_3) = (c2 * 2.0, c3 * 3.0);
        for _ in 0..8 {
            let s2 = s.powi(2);
            let f = s.mul_add(beta, c0) + s2 * c2 + s.powi(3).mul_add(c3, -tof);
            let deriv = s.mul_add(c2_2, beta) + s2 * c3_3;
            if deriv == 0.0 {
                break;
            }
            let step = f / deriv;
            s -= step;
            if step.abs() < 1e-12 {
                break;
            }
        }
        s
    } else if c2 != 0.0 {
        let d = beta.powi(2) - 4.0 * c2 * (c0 - tof);
        if d < 0.0 {
            s0
        } else {
            (c0 - tof) / (-0.5 * (beta + d.sqrt()))
        }
    } else {
        s0
    };
    refined.powi(2) - c4
}

/// Bruker TIMS 1/K0 at scan `idx`: `mzdata::io::tdf::TimsCalibrationModel2::convert`.
pub fn timstof_mobility(p: &[f64], idx: f64) -> f64 {
    let (c6, c7, offset, slope) = (p[0], p[1], p[2], p[3]);
    1.0 / (c6 + c7 / (offset + slope * idx))
}

/// The value of grid index `idx` under `grid_type` with `parameters`; `None` for an unknown model
/// or a parameter list of the wrong length.
pub fn value_at(grid_type: &CURIE, parameters: &[f64], idx: u32) -> Option<f64> {
    let i = idx as f64;
    match *grid_type {
        LINEAR_GRID if (2..=3).contains(&parameters.len()) => {
            Some((i * parameters[1] + parameters[0]) / parameters.get(2).copied().unwrap_or(1.0))
        }
        SQRT_GRID if (2..=3).contains(&parameters.len()) => {
            Some((i * parameters[1] + parameters[0]).powi(2) / parameters.get(2).copied().unwrap_or(1.0))
        }
        TIMSTOF_MZ_GRID if parameters.len() == 7 => Some(timstof_mz(parameters, i)),
        TIMSTOF_TIMS_GRID if parameters.len() == 4 => Some(timstof_mobility(parameters, i)),
        _ => None,
    }
}

/// Decode every row of a `<array>_grid` struct column into one flat list of values.
/// `delta_coded`: the indices are `[first, deltas…]` (the main axis) rather than absolute.
pub fn decode_rows(arr: &StructArray, delta_coded: bool) -> Result<Vec<f64>, String> {
    let grid_type = arr
        .column_by_name("grid_type")
        .ok_or("grid struct has no `grid_type`")?;
    let parameters = arr
        .column_by_name("parameters")
        .ok_or("grid struct has no `parameters`")?;
    let indices = arr
        .column_by_name("indices")
        .ok_or("grid struct has no `indices`")?;
    let type_at = |i: usize| -> Result<CURIE, String> {
        let s = if let Some(a) = grid_type.as_string_opt::<i64>() {
            a.value(i)
        } else if let Some(a) = grid_type.as_string_opt::<i32>() {
            a.value(i)
        } else {
            return Err(format!("grid_type has type {:?}", grid_type.data_type()));
        };
        s.parse::<CURIE>().map_err(|e| format!("grid_type {s:?}: {e}"))
    };
    let params_at = |i: usize| -> Result<Vec<f64>, String> {
        let row: ArrayRef = if let Some(a) = parameters.as_list_opt::<i64>() {
            a.value(i)
        } else if let Some(a) = parameters.as_list_opt::<i32>() {
            a.value(i)
        } else {
            return Err(format!("parameters has type {:?}", parameters.data_type()));
        };
        Ok(row.as_primitive::<Float64Type>().values().to_vec())
    };
    let indices_at = |i: usize| -> Result<Vec<u32>, String> {
        let row: ArrayRef = if let Some(a) = indices.as_list_opt::<i64>() {
            a.value(i)
        } else if let Some(a) = indices.as_list_opt::<i32>() {
            a.value(i)
        } else {
            return Err(format!("indices has type {:?}", indices.data_type()));
        };
        Ok(match row.data_type() {
            arrow::datatypes::DataType::UInt32 => row.as_primitive::<UInt32Type>().values().to_vec(),
            arrow::datatypes::DataType::UInt16 => row.as_primitive::<UInt16Type>().values().iter().map(|&v| v as u32).collect(),
            arrow::datatypes::DataType::UInt8 => row.as_primitive::<UInt8Type>().values().iter().map(|&v| v as u32).collect(),
            arrow::datatypes::DataType::Int32 => row.as_primitive::<arrow::datatypes::Int32Type>().values().iter().map(|&v| v as u32).collect(),
            other => return Err(format!("grid indices have type {other:?}")),
        })
    };
    let mut out = Vec::new();
    for i in 0..arr.len() {
        if arr.is_null(i) {
            continue;
        }
        let gt = type_at(i)?;
        let p = params_at(i)?;
        let mut idx = indices_at(i)?;
        if delta_coded {
            let mut acc: u32 = 0;
            for v in idx.iter_mut() {
                acc = acc.wrapping_add(*v);
                *v = acc;
            }
        }
        out.reserve(idx.len());
        for k in idx {
            out.push(value_at(&gt, &p, k).ok_or_else(|| format!("unknown grid model {gt} with {} parameters", p.len()))?);
        }
    }
    Ok(out)
}

/// [`decode_rows`] as an Arrow array.
pub fn decode_rows_arrow(arr: &StructArray, delta_coded: bool) -> Result<ArrayRef, String> {
    Ok(std::sync::Arc::new(Float64Array::from(decode_rows(arr, delta_coded)?)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linear_and_sqrt_grids() {
        assert_eq!(value_at(&LINEAR_GRID, &[10.0, 0.5], 4), Some(12.0));
        assert_eq!(value_at(&LINEAR_GRID, &[10.0, 0.5, 2.0], 4), Some(6.0));
        assert_eq!(value_at(&SQRT_GRID, &[1.0, 1.0], 2), Some(9.0));
        assert_eq!(value_at(&SQRT_GRID, &[1.0, 1.0], 2), Some(9.0));
        assert!(value_at(&SQRT_GRID, &[1.0], 2).is_none());
        assert!(value_at(&mzdata::curie!(MS:1000000), &[1.0, 1.0], 2).is_none());
    }

    /// The TIMS model in the reference form against the closed form our converter evaluates
    /// (`W/(C7 + C6·W)`): the same function, agreeing to the last bits.
    #[test]
    fn tims_model_matches_the_closed_form() {
        let (c0, c1, c2, c3, c4, c6, c7) = (1.0, 926.0, 217.23199652301727, 74.71319596975974, 33.0, 0.020932469715718494, 131.22279563838268);
        let slope = (c3 - c2) / c1;
        let offset = c2 - slope * (c4 + c0);
        for scan in [33u32, 100, 500, 926] {
            let w = c2 + (c3 - c2) * (scan as f64 - c4 - c0) / c1;
            let ours = w / (c7 + c6 * w);
            let theirs = timstof_mobility(&[c6, c7, offset, slope], scan as f64);
            assert!((ours - theirs).abs() <= 4.0 * f64::EPSILON * ours, "scan {scan}: {ours} vs {theirs}");
        }
    }
}

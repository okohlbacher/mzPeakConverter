//! Encoding pre-scan: before a run is written, a sample of it is written once per trial through
//! the real writer, each trial trying another encoding for every data-facet column, and each column
//! keeps whichever encoding came out smallest.
//!
//! Why measure instead of a fixed rule: the best encoding depends on the data. On timsTOF grid
//! indices byte-stream split beats dictionary by ~10 %; on Waters HDMSe frames (200 drift bins
//! sharing one m/z axis, half the m/z deltas zero) dictionary wins, float mobility under
//! byte-stream split is 3× larger, delta m/z (exact for these float32 values) beats numpress, and
//! integral intensities as int32 are a third smaller than as float32 (measured on PXD063409 CK1,
//! 2026-09-29).
//!
//! Parquet records its encoding per page, so readers need nothing to read any arm; int32 intensity
//! is a declared array data type (MS:1000519). The `encoding_prescan` index block states what was
//! measured and chosen.

use mzdata::spectrum::{ArrayType, BinaryDataArrayType, DataArray, MultiLayerSpectrum};
use mzpeak_prototyping::writer::{ColumnEncoding, DataColumnEncodings};

/// How the m/z axis is stored: delta chunks under a Parquet encoding, or numpress-linear.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MzArm {
    Delta(ColumnEncoding),
    Numpress,
}

/// How intensities are stored: their float32 values, or the same values as int32.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IntensityArm {
    pub int32: bool,
    pub encoding: ColumnEncoding,
}

/// One trial: the arm each column is written under.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Trial {
    pub mz: MzArm,
    pub intensity: IntensityArm,
    pub ion_mobility: ColumnEncoding,
}

impl Trial {
    /// The writer's column overrides for this trial.
    pub fn encodings(&self) -> DataColumnEncodings {
        DataColumnEncodings {
            mz_values: match self.mz {
                MzArm::Delta(e) => e,
                MzArm::Numpress => ColumnEncoding::Writer,
            },
            intensity: self.intensity.encoding,
            ion_mobility: self.ion_mobility,
        }
    }
}

/// The candidate arms per column. Each list starts with what the writer does without a pre-scan
/// (lossless first for m/z), so a tie keeps the old behaviour.
pub struct Arms {
    mz: Vec<MzArm>,
    intensity: Vec<IntensityArm>,
    ion_mobility: Vec<ColumnEncoding>,
}

impl Arms {
    /// `numpress`: numpress-linear may be chosen (it was the requested default; an explicit
    /// `--no-numpress` removes it). `int32`: every sampled intensity is an integer in i32 range.
    pub fn new(numpress: bool, int32: bool) -> Self {
        use ColumnEncoding::*;
        let mut mz = vec![MzArm::Delta(Dictionary), MzArm::Delta(ByteStreamSplit), MzArm::Delta(Plain)];
        if numpress {
            mz.push(MzArm::Numpress);
        }
        let mut intensity = vec![
            IntensityArm { int32: false, encoding: ByteStreamSplit },
            IntensityArm { int32: false, encoding: Dictionary },
        ];
        if int32 {
            intensity.push(IntensityArm { int32: true, encoding: ByteStreamSplit });
            intensity.push(IntensityArm { int32: true, encoding: Dictionary });
        }
        Self { mz, intensity, ion_mobility: vec![Dictionary, ByteStreamSplit, Plain] }
    }

    /// Enough trials that every arm of every column is written once; a column with fewer arms
    /// repeats its last one. Columns compress independently, so one trial measures all three.
    pub fn trials(&self) -> Vec<Trial> {
        let n = self.mz.len().max(self.intensity.len()).max(self.ion_mobility.len());
        let pick = |k: usize, len: usize| k.min(len - 1);
        (0..n)
            .map(|k| Trial {
                mz: self.mz[pick(k, self.mz.len())],
                intensity: self.intensity[pick(k, self.intensity.len())],
                ion_mobility: self.ion_mobility[pick(k, self.ion_mobility.len())],
            })
            .collect()
    }
}

/// Compressed bytes of each column group in one trial, summed over the data and peak facets.
/// `mz` is everything that is neither intensity nor ion mobility (the index column and chunk
/// bounds are the same in every trial, so only the m/z representation moves it).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Measured {
    pub mz: u64,
    pub intensity: u64,
    pub ion_mobility: u64,
}

impl Measured {
    /// Add one Parquet column chunk, by its dotted column path.
    pub fn add(&mut self, path: &str, bytes: u64) {
        let leaf = path.trim_end_matches(".list.item");
        if leaf == "intensity" || leaf.ends_with(".intensity") {
            self.intensity += bytes;
        } else if path.contains("ion_mobility") {
            self.ion_mobility += bytes;
        } else {
            self.mz += bytes;
        }
    }
}

/// The winning arm of each column, over the trials run.
pub fn choose(results: &[(Trial, Measured)]) -> Trial {
    // `min_by_key` keeps the FIRST of equal minima: the arm the lists put first.
    let best = |key: fn(&Measured) -> u64| results.iter().min_by_key(|(_, m)| key(m)).map(|(t, _)| *t);
    let first = results[0].0;
    Trial {
        mz: best(|m| m.mz).map_or(first.mz, |t| t.mz),
        intensity: best(|m| m.intensity).map_or(first.intensity, |t| t.intensity),
        ion_mobility: best(|m| m.ion_mobility).map_or(first.ion_mobility, |t| t.ion_mobility),
    }
}

/// The smallest float32 intensity arm: where the run goes when a spectrum turns out to carry an
/// intensity int32 cannot hold exactly.
pub fn best_float_intensity(results: &[(Trial, Measured)]) -> IntensityArm {
    results
        .iter()
        .filter(|(t, _)| !t.intensity.int32)
        .min_by_key(|(_, m)| m.intensity)
        .map(|(t, _)| t.intensity)
        .unwrap_or(IntensityArm { int32: false, encoding: ColumnEncoding::Writer })
}

/// Can every intensity of this spectrum be stored as int32 without changing its value?
pub fn intensities_fit_int32(spec: &MultiLayerSpectrum) -> bool {
    let Some(arrays) = spec.arrays.as_ref() else { return true };
    let Some(da) = arrays.get(&ArrayType::IntensityArray) else { return true };
    if da.dtype == BinaryDataArrayType::Int32 {
        return true;
    }
    match arrays.intensities() {
        Ok(v) => v.iter().all(|&x| fits_int32(x)),
        Err(_) => false,
    }
}

fn fits_int32(x: f32) -> bool {
    // f32 represents every integer it holds exactly; 2^31 itself is out of range.
    x.is_finite() && x.fract() == 0.0 && x >= -2_147_483_648.0 && x < 2_147_483_648.0
}

/// Store this spectrum's intensities as int32, the same values. `false`, leaving the spectrum as
/// it was, when one of them is not an integer in i32 range.
pub fn intensity_to_int32(spec: &mut MultiLayerSpectrum) -> bool {
    let Some(arrays) = spec.arrays.as_mut() else { return true };
    let Some(da) = arrays.get(&ArrayType::IntensityArray) else { return true };
    if da.dtype == BinaryDataArrayType::Int32 {
        return true;
    }
    let values: Vec<i32> = match arrays.intensities() {
        Ok(v) if v.iter().all(|&x| fits_int32(x)) => v.iter().map(|&x| x as i32).collect(),
        _ => return false,
    };
    let old = arrays.get(&ArrayType::IntensityArray).expect("checked above");
    let mut new = DataArray::wrap(&ArrayType::IntensityArray, BinaryDataArrayType::Int32, Default::default());
    new.unit = old.unit;
    new.params = old.params.clone();
    new.update_buffer(&values).expect("Int32 array takes i32 values");
    arrays.add(new);
    true
}

/// A readable label for an arm, as the index block and the log print it.
pub fn encoding_label(e: ColumnEncoding) -> &'static str {
    match e {
        ColumnEncoding::Writer => "writer default",
        ColumnEncoding::Dictionary => "dictionary",
        ColumnEncoding::ByteStreamSplit => "byte-stream split",
        ColumnEncoding::Plain => "plain",
    }
}

pub fn mz_label(a: MzArm) -> String {
    match a {
        MzArm::Numpress => "numpress-linear".into(),
        MzArm::Delta(e) => format!("delta, {}", encoding_label(e)),
    }
}

pub fn intensity_label(a: IntensityArm) -> String {
    format!("{}, {}", if a.int32 { "int32" } else { "float32" }, encoding_label(a.encoding))
}

/// The `encoding_prescan` index block: the sample, every arm's bytes, the choice.
pub fn block(
    results: &[(Trial, Measured)],
    chosen: Trial,
    sample_spectra: usize,
    sample_points: usize,
    has_mobility: bool,
    int32_eligible: bool,
) -> serde_json::Value {
    let mut mz = serde_json::Map::new();
    let mut intensity = serde_json::Map::new();
    let mut mobility = serde_json::Map::new();
    for (t, m) in results {
        mz.entry(mz_label(t.mz)).or_insert(m.mz.into());
        intensity.entry(intensity_label(t.intensity)).or_insert(m.intensity.into());
        if has_mobility {
            mobility.entry(encoding_label(t.ion_mobility)).or_insert(m.ion_mobility.into());
        }
    }
    let mut measured = serde_json::json!({ "mz": mz, "intensity": intensity });
    let mut choice = serde_json::json!({
        "mz": mz_label(chosen.mz),
        "intensity": intensity_label(chosen.intensity),
    });
    if has_mobility {
        measured["ion_mobility"] = mobility.into();
        choice["ion_mobility"] = encoding_label(chosen.ion_mobility).into();
    }
    serde_json::json!({
        "method": "the sample written once per trial through the archive writer; compressed bytes \
                   per column summed over the spectrum data and peak facets; each column keeps its \
                   smallest arm (ties keep the writer default)",
        "sample": { "spectra": sample_spectra, "points": sample_points },
        "int32_intensity_eligible": int32_eligible,
        "measured_bytes": measured,
        "chosen": choice,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use mzdata::spectrum::{BinaryArrayMap, SpectrumDescription};

    fn spectrum(intensity: &[f32]) -> MultiLayerSpectrum {
        let mut arrays = BinaryArrayMap::new();
        let mut da = DataArray::wrap(&ArrayType::IntensityArray, BinaryDataArrayType::Float32, Default::default());
        da.update_buffer(intensity).unwrap();
        da.unit = mzdata::params::Unit::DetectorCounts;
        arrays.add(da);
        MultiLayerSpectrum::new(SpectrumDescription::default(), Some(arrays), None, None)
    }

    #[test]
    fn trials_cover_every_arm_of_every_column() {
        let arms = Arms::new(true, true);
        let trials = arms.trials();
        assert_eq!(trials.len(), 4);
        for a in &arms.mz {
            assert!(trials.iter().any(|t| t.mz == *a), "{a:?}");
        }
        for a in &arms.intensity {
            assert!(trials.iter().any(|t| t.intensity == *a), "{a:?}");
        }
        for a in &arms.ion_mobility {
            assert!(trials.iter().any(|t| t.ion_mobility == *a), "{a:?}");
        }
        // Without numpress or int32 there are three m/z arms and two intensity arms.
        let small = Arms::new(false, false).trials();
        assert_eq!(small.len(), 3);
        assert!(small.iter().all(|t| t.mz != MzArm::Numpress && !t.intensity.int32));
    }

    #[test]
    fn each_column_keeps_its_smallest_arm_and_ties_keep_the_first() {
        let t = Arms::new(true, true).trials();
        let m = |mz, intensity, ion_mobility| Measured { mz, intensity, ion_mobility };
        let results = vec![(t[0], m(10, 50, 7)), (t[1], m(10, 40, 9)), (t[2], m(12, 30, 7)), (t[3], m(9, 35, 8))];
        let c = choose(&results);
        assert_eq!(c.mz, MzArm::Numpress);
        assert_eq!(c.intensity, t[2].intensity);
        assert_eq!(c.ion_mobility, ColumnEncoding::Dictionary, "the tie with trial 2 keeps trial 0");
        assert_eq!(best_float_intensity(&results), t[1].intensity);
    }

    #[test]
    fn measured_splits_columns_by_role() {
        let mut m = Measured::default();
        m.add("chunk.spectrum_index", 1);
        m.add("chunk.mz_chunk_values.list.item", 10);
        m.add("chunk.intensity.list.item", 100);
        m.add("chunk.raw_ion_mobility.list.item", 1000);
        m.add("point.intensity", 10_000);
        assert_eq!(m, Measured { mz: 11, intensity: 10_100, ion_mobility: 1000 });
    }

    #[test]
    fn int32_conversion_is_exact_or_refused() {
        let mut s = spectrum(&[0.0, 5.0, 16198.0, 2.0e9]);
        assert!(intensities_fit_int32(&s));
        assert!(intensity_to_int32(&mut s));
        let da = s.arrays.as_ref().unwrap().get(&ArrayType::IntensityArray).unwrap();
        assert_eq!(da.dtype, BinaryDataArrayType::Int32);
        assert_eq!(da.unit, mzdata::params::Unit::DetectorCounts);
        assert_eq!(s.arrays.as_ref().unwrap().intensities().unwrap().to_vec(), vec![0.0, 5.0, 16198.0, 2.0e9]);
        for bad in [[1.5f32], [f32::NAN], [3.0e9]] {
            let mut s = spectrum(&bad);
            assert!(!intensities_fit_int32(&s));
            assert!(!intensity_to_int32(&mut s));
            let da = s.arrays.as_ref().unwrap().get(&ArrayType::IntensityArray).unwrap();
            assert_eq!(da.dtype, BinaryDataArrayType::Float32, "a refused spectrum is left as it was");
        }
    }
}

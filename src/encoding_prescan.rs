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
//!
//! **The default m/z encoding** of every other chunked lane (mzML, imzML, Thermo, the native readers
//! without a full pre-scan) is decided by the same means, for the m/z column alone ([`pick_mz`],
//! owner decision D1/D13 of 2026-10-01, principle P2: exact where it costs nothing). Where every
//! sampled m/z is a 32-bit value ([`all_32bit`]) delta is exact whatever the spacing and smaller
//! than numpress-linear on every file measured (imzML: chilli −38 %, LA-ESI −36 %, DESI and the
//! Example files −12 %), so it is chosen without a trial. Otherwise the sample is written under
//! delta and under numpress-linear and the smaller arm is kept; on a tie the exact one, where delta
//! counts as exact only when no sampled chunk is at risk (a chunk whose values span more than a
//! factor of two, where `b + fl(a − b)` can round for 64-bit values: `fidelity`). Measured on the
//! corpus: delta on QC01, SZB8102938 and PXD009465 t04176 (−14 to −22 % and exact), numpress on a
//! Bruker microTOF profile run (delta +73 % there). A lane that stores m/z exactly (the Bruker TSF
//! lane) measures delta against the point layout instead and keeps the smaller EXACT arm.

use mzdata::spectrum::{ArrayType, BinaryDataArrayType, DataArray, MultiLayerSpectrum};
use mzpeak_prototyping::writer::{ColumnEncoding, DataColumnEncodings};

/// How the m/z axis is stored: delta chunks under a Parquet encoding, numpress-linear, or the point
/// layout (one row per m/z–intensity pair, the f64 value as it is; the arm a lane that stores m/z
/// exactly measures against delta).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MzArm {
    Delta(ColumnEncoding),
    Numpress,
    Point,
}

impl MzArm {
    /// Does this arm return every 64-bit m/z exactly? Delta does when no chunk is at risk
    /// (`delta_chunks_at_risk`, the writer's count over the sample), the point layout always,
    /// numpress-linear never.
    pub fn is_exact(self, delta_chunks_at_risk: u64) -> bool {
        match self {
            MzArm::Delta(_) => delta_chunks_at_risk == 0,
            MzArm::Point => true,
            MzArm::Numpress => false,
        }
    }
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
                MzArm::Numpress | MzArm::Point => ColumnEncoding::Writer,
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

/// Is every one of these m/z a 32-bit float value (`(x as f32) as f64 == x`)? False for no values.
/// Delta returns such values exactly whatever their spacing: two 24-bit mantissas differ exactly in
/// 64-bit arithmetic, and the decoder's running sum is again a 32-bit value at every step.
pub fn all_32bit(mz: impl IntoIterator<Item = f64>) -> bool {
    let mut any = false;
    for x in mz {
        any = true;
        if (x as f32) as f64 != x {
            return false;
        }
    }
    any
}

/// One m/z arm of the default rule, measured: the compressed bytes the sample took under it (the
/// m/z columns against numpress-linear, the whole facets against the point layout) and the
/// writer's count of delta chunks at risk in the sample.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MzTrial {
    pub arm: MzArm,
    pub bytes: u64,
    pub delta_chunks_at_risk: u64,
}

/// Why the default rule chose its arm, as the index block states it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Basis {
    /// Every sampled m/z is a 32-bit value: delta is exact and smaller, no trial.
    ThirtyTwoBit,
    /// The smaller arm.
    Smaller,
    /// A tie on bytes: the exact arm.
    TieExact,
    /// A tie on bytes between arms none of which is exact: the first listed (delta, whose bound is
    /// one unit in the last place, before numpress-linear).
    TieInexact,
    /// A lane that stores m/z exactly: the exact arm, delta having chunks at risk in the sample.
    OnlyExact,
}

impl Basis {
    pub fn label(self) -> &'static str {
        match self {
            Basis::ThirtyTwoBit => "every sampled m/z is a 32-bit value: delta returns them exactly and is smaller",
            Basis::Smaller => "the smaller arm",
            Basis::TieExact => "a tie on bytes: the exact arm",
            Basis::TieInexact => "a tie on bytes between inexact arms: delta, whose bound is one unit in the last place",
            Basis::OnlyExact => "the exact arm: delta chunks of the sample span more than a factor of two",
        }
    }
}

/// The default rule over measured arms (principle P2): the smallest; on a tie the exact one, delta
/// counting as exact only when no sampled chunk is at risk; a tie between inexact arms keeps the
/// first listed. With `exact_only` (a lane that stores m/z exactly) only exact arms compete, and the
/// first listed arm is returned, under [`Basis::OnlyExact`], when none is.
pub fn pick_mz(trials: &[MzTrial], exact_only: bool) -> (MzArm, Basis) {
    let exact = |t: &&MzTrial| t.arm.is_exact(t.delta_chunks_at_risk);
    let first = trials.first().expect("at least one arm");
    let candidates: Vec<&MzTrial> = trials.iter().filter(|t| !exact_only || exact(t)).collect();
    let Some(min) = candidates.iter().map(|t| t.bytes).min() else {
        return (first.arm, Basis::OnlyExact);
    };
    // An inexact arm smaller than every exact one lost on exactness alone.
    let inexact_min = trials.iter().filter(|t| !exact(t)).map(|t| t.bytes).min();
    let smallest: Vec<&MzTrial> = candidates.iter().copied().filter(|t| t.bytes == min).collect();
    match smallest.as_slice() {
        [one] if exact_only && inexact_min.is_some_and(|m| m < min) => (one.arm, Basis::OnlyExact),
        [one] => (one.arm, Basis::Smaller),
        several => match several.iter().find(|t| exact(t)) {
            Some(t) => (t.arm, Basis::TieExact),
            None => (several[0].arm, Basis::TieInexact),
        },
    }
}

/// The `encoding_prescan` index block of a lane that decided the m/z arm alone: the sample, each
/// arm's bytes where arms were written, the delta chunks at risk in the sample, the choice and why.
pub fn mz_block(sample_spectra: usize, sample_points: usize, trials: &[MzTrial], chosen: MzArm, basis: Basis) -> serde_json::Value {
    let mut block = serde_json::json!({
        "method": if trials.is_empty() {
            "every sampled m/z tested for being a 32-bit float value; delta chosen without a trial \
             when all are (exact whatever their spacing, and smaller than numpress-linear on such data)"
        } else {
            "the sample written once per m/z arm through the archive writer; compressed bytes summed \
             over the spectrum data and peak facets; the smaller arm kept, on a tie the exact one \
             (delta is exact when no sampled chunk spans more than a factor of two)"
        },
        "sample": { "spectra": sample_spectra, "points": sample_points },
        "chosen": { "mz": mz_label(chosen) },
        "basis": basis.label(),
    });
    if !trials.is_empty() {
        let mut mz = serde_json::Map::new();
        for t in trials {
            mz.insert(mz_label(t.arm), t.bytes.into());
        }
        block["measured_bytes"] = serde_json::json!({ "mz": mz });
        if let Some(d) = trials.iter().find(|t| matches!(t.arm, MzArm::Delta(_))) {
            block["delta_chunks_at_risk"] = d.delta_chunks_at_risk.into();
        }
    }
    block
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
        MzArm::Point => "point".into(),
        // The writer's own Parquet encoding of the chunk values: the arm the default rule writes.
        MzArm::Delta(ColumnEncoding::Writer) => "delta".into(),
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

    /// The default rule's decisions (owner decision D1/D13): a 32-bit sample takes delta without a
    /// trial; a 64-bit sample takes the smaller arm; a tie takes the exact arm, delta counting as
    /// exact only when no sampled chunk is at risk; a lane that stores m/z exactly takes the point
    /// layout when delta has a chunk at risk, and delta when it is exact and smaller.
    #[test]
    fn the_default_rule_picks_32bit_delta_the_smaller_arm_and_the_exact_arm_on_ties() {
        use ColumnEncoding::Writer;
        // Every value a 32-bit one, including a value no f32 holds in the mix, and no values at all.
        assert!(all_32bit([100.5f64, 171.33333f32 as f64, 0.0, 4000.25]));
        assert!(!all_32bit([100.5f64, 171.33333]));
        assert!(!all_32bit(std::iter::empty()));

        let delta = |bytes, at_risk| MzTrial { arm: MzArm::Delta(Writer), bytes, delta_chunks_at_risk: at_risk };
        let numpress = |bytes| MzTrial { arm: MzArm::Numpress, bytes, delta_chunks_at_risk: 0 };
        let point = |bytes| MzTrial { arm: MzArm::Point, bytes, delta_chunks_at_risk: 0 };
        // A 64-bit sample where numpress is smaller (the microTOF case): numpress.
        assert_eq!(pick_mz(&[delta(173, 0), numpress(100)], false), (MzArm::Numpress, Basis::Smaller));
        // Delta smaller, chunks at risk or not: delta (the declaration is the fidelity block's).
        assert_eq!(pick_mz(&[delta(80, 0), numpress(100)], false), (MzArm::Delta(Writer), Basis::Smaller));
        assert_eq!(pick_mz(&[delta(80, 3), numpress(100)], false), (MzArm::Delta(Writer), Basis::Smaller));
        // A tie: the exact arm; with delta at risk neither is exact and delta, listed first, stays.
        assert_eq!(pick_mz(&[delta(100, 0), numpress(100)], false), (MzArm::Delta(Writer), Basis::TieExact));
        assert_eq!(pick_mz(&[delta(100, 1), numpress(100)], false), (MzArm::Delta(Writer), Basis::TieInexact));
        assert_eq!(pick_mz(&[numpress(100), delta(100, 0)], false), (MzArm::Delta(Writer), Basis::TieExact), "order does not decide a tie against an exact arm");
        // A lane that stores m/z exactly: delta against the point layout.
        assert_eq!(pick_mz(&[delta(90, 0), point(100)], true), (MzArm::Delta(Writer), Basis::Smaller));
        assert_eq!(pick_mz(&[delta(90, 2), point(100)], true), (MzArm::Point, Basis::OnlyExact));
        assert_eq!(pick_mz(&[delta(110, 2), point(100)], true), (MzArm::Point, Basis::Smaller));
        assert_eq!(pick_mz(&[delta(100, 0), point(100)], true), (MzArm::Delta(Writer), Basis::TieExact));
        assert_eq!(pick_mz(&[delta(90, 2)], true), (MzArm::Delta(Writer), Basis::OnlyExact), "no exact arm offered: the first stays");

        let block = mz_block(12, 3456, &[delta(80, 3), numpress(100)], MzArm::Delta(Writer), Basis::Smaller);
        assert_eq!(block["measured_bytes"]["mz"], serde_json::json!({"delta": 80, "numpress-linear": 100}));
        assert_eq!((&block["delta_chunks_at_risk"], &block["chosen"]["mz"], &block["basis"]), (&serde_json::json!(3), &serde_json::json!("delta"), &serde_json::json!(Basis::Smaller.label())));
        let block = mz_block(12, 3456, &[], MzArm::Delta(Writer), Basis::ThirtyTwoBit);
        assert!(block.get("measured_bytes").is_none() && block["chosen"]["mz"] == "delta" && block["sample"]["points"] == 3456, "{block}");
    }
}

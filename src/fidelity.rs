//! The `fidelity` index block, and the check behind `--lossless`.
//!
//! `transformations` names what a conversion changed; this block says by how much. Per signal
//! facet (`spectra_data`, `spectra_peaks`): the points the reader handed over against the points
//! stored (the difference is what the zero-run mask left out), the numeric types the source
//! declares for m/z and intensity where the lane knows them (mzML and imzML: the binary data
//! types) against what the facet stores them as (the column's type, or `grid:<index type>` where
//! the m/z are grid indices), and, in `mz_error`, one entry per m/z encoding that can move a value,
//! with its maximum absolute and relative error.
//!
//! Where the numbers come from:
//!
//! * **Source side**: [`SourceTally`], counted by the lane over the spectra it writes, with the
//!   routing rule of the writer (`write_spectrum_data`): profile and unknown-continuity arrays go
//!   to `spectra_data`, centroid arrays and peak sets to `spectra_peaks`.
//! * **Stored side**: read back from the two facets as they sit in the archive, after the Parquet
//!   members are closed and before the index is written ([`stored`]): the footer's point count,
//!   the schema's column types, and the chunk rows' bounds, encoding and numpress bytes. Nothing
//!   is taken from what the lane was configured to do.
//! * **numpress-linear** is a BOUND, not a measurement: per stored chunk `0.5 / fixed point` plus
//!   four units in the last place of the chunk's largest m/z, the fixed point read from the first
//!   eight bytes of the encoded chunk. `0.5 / fixed point` alone is what the codec's rounding
//!   allows in exact arithmetic; the f64 products and the division add up to four ulp (observed:
//!   1e-14 Da beyond the bare bound at m/z 771, so a check against the bare bound fails). The
//!   relative figure divides each chunk's bound by the chunk's smallest m/z. A measured maximum
//!   would need the source values next to the decoded ones, which no single point of the write
//!   path holds without vendored changes; the bound is tight to one part in 1e6 on real data.
//! * **delta** stores `fl(a − b)`, and `b + fl(a − b) == a` is guaranteed only for `a ≤ 2b`
//!   (Sterbenz) or for m/z that are 32-bit values. A chunk whose last m/z is at most twice its
//!   first is therefore exact by construction; the others are counted, with the largest m/z at
//!   risk. Their error is a BOUND as well, one unit in the last place of the value ([`ulp`]): the
//!   decoder adds the stored differences up from the chunk's first m/z, and a decoded value `y`
//!   stays within `ulp(x)` of its source `x` through the chunk, by induction over its two kinds of
//!   step. Where `x' ≤ 2x` the difference is exact and `y' = fl(x' + (y − x))`, which rounding
//!   keeps inside `x' ± ulp(x')` because `ulp(x') ≥ ulp(x)`. Where `x' > 2x` the difference is off
//!   by at most `ulp(x')/2` and `ulp(x) ≤ ulp(x')/2`, so the sum is again inside `x' ± ulp(x')`.
//!   The absolute figure is the ulp of the largest m/z at risk, the relative one 2⁻⁵² (an ulp is
//!   never a larger part of its value). The proof needs positive, ascending m/z: an at-risk chunk
//!   that starts at or below zero leaves the entry without a figure.
//! * **grid fits** (`grid-fit:<tol>Da`, `tof-grid:<ppm>ppm`) state the tolerance every fitted value
//!   was accepted within, taken from the `transformations` entry, and the other figure derived from
//!   it over the m/z range of the grid rows stored: the absolute tolerance over the smallest m/z,
//!   the relative one times the largest. A `tof-grid` lane also hands over the largest error its
//!   accepted fits left ([`ObservedMzError`], [`SourceTally::set_observed`]): the entry states it
//!   beside the tolerance, as `observed_max_abs_error` / `observed_max_rel_error_ppm` (MEASURED,
//!   stored value against source value, over every gridded point).
//! * **`bruker:mz-calibrant-omitted`** takes the bound the lane states in
//!   `ims_calibration.max_error_ppm` (the largest calibrant correction, in ppm of the stored m/z)
//!   and that bound times the largest m/z of the grid rows; without the bound it is `not measured`.
//!   The lane's figure is `Calibrant::max_abs_ppm`: the largest correction at 2,001 evenly spaced
//!   m/z over the calibrant range, a sampled maximum of a low-degree polynomial and not a proven
//!   supremum (the manual says so).
//!
//! Three `transformations` entries are decided from the same evidence as this block, in
//! [`finish_archive`](crate::finish_archive), so that neither key can say what the other does not:
//! `delta-ulp` exactly when a `delta` entry is written here ([`delta_ulp_declared`], from the
//! writer's count of the chunk rows this block then reads back, by the same rule),
//! `intensity-f32-rounding` when a facet's `intensity_values_rounded` is above zero and
//! `intensity-type-narrowing` when its `intensity_values_narrowed` is ([`intensities_changed`]).
//! The two counts are of source intensities the stored column does not hold: the lane counts,
//! spectrum by spectrum, what each column type the facet could have would do to the values
//! ([`IntensityTally`]), and the type the writer's schema has for the facet picks the count
//! ([`resolve_intensities`]) before the facets are closed. A value is rounded where it passes
//! through a float32 (mzdata's centroid peak set, a `--tof-grid` grid row) or lands in a float32
//! column (the column's type is sampled from a few spectra; a later spectrum of a wider type is
//! cast into it), and narrowed where the column is of another type that does not hold it.

use std::collections::BTreeSet;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

use anyhow::{Context, Result, bail};
use arrow::array::{Array, ArrayRef, AsArray, StructArray};
use arrow::datatypes::{DataType, Field, Fields, Float64Type, Schema, UInt8Type};
use mzdata::prelude::*;
use mzdata::spectrum::bindata::{ArrayType, BinaryArrayMap, BinaryDataArrayType};
use mzdata::spectrum::{MultiLayerSpectrum, RefPeakDataLevel, SignalContinuity};
use mzpeak_prototyping::archive::ArchiveFacetReader;
use mzpeaks::{CentroidLike, DeconvolutedCentroidLike};
use parquet::arrow::ProjectionMask;
use parquet::arrow::arrow_reader::{ArrowReaderMetadata, ParquetRecordBatchReaderBuilder};
use rayon::prelude::*;
use serde_json::{Value, json};

/// The index block's key.
pub const BLOCK: &str = "fidelity";

/// The two signal facets the block describes, by archive member and block key.
const FACETS: [(&str, &str); 2] = [("spectra_data.parquet", "spectra_data"), ("spectra_peaks.parquet", "spectra_peaks")];

/// `transformations` entries that change the stored spectrum signal: a point dropped, re-ordered
/// or summed, or an m/z or intensity value moved. `--lossless` fails when any is declared. Every
/// other entry describes metadata (a dropped reference, a pixel size, a position), a chromatogram
/// or device trace, or a precursor window, and does not count. A NEW entry that touches spectrum
/// signal belongs here, or in [`SIGNAL_TRANSFORMATION_PREFIXES`] when it carries a parameter.
pub const SIGNAL_TRANSFORMATIONS: [&str; 21] = [
    "zero-run-mask",
    "numpress-linear",
    DELTA_ULP,
    INTENSITY_F32_ROUNDING,
    INTENSITY_TYPE_NARROWING,
    "sort-by-mz",
    "sort-by-wavelength",
    "shimadzu:span-trim",
    "shimadzu:coarse-mz",
    "agilent:drop-zero-samples",
    "agilent:intensity-f32-rounding",
    "agilent:nonfinite-intensity-to-zero",
    "agilent:truncate-unequal-arrays",
    "sciex:nan-intensity-to-zero",
    "sciex:clamp-intensity-to-f32",
    "sciex:truncate-unequal-arrays",
    "waters:drop-functions",
    "waters:sonar-summed",
    "bruker:mz-calibration-chord",
    "bruker:mz-calibrant-omitted",
    "bruker:out-of-window-points-dropped",
];

/// `transformations` entry: a 64-bit m/z facet holds delta chunks that are not exact by
/// construction, so a decoded m/z can be one unit in the last place off its source (the block's
/// `delta` entry bounds it). Declared by [`finish_archive`](crate::finish_archive).
pub const DELTA_ULP: &str = "delta-ulp";

/// `transformations` entry: an intensity was stored as the float32 nearest to a source value no
/// float32 holds (a 64-bit float, or an integer above 2^24). The facet's `intensity_values_rounded`
/// counts them. The lane-neutral sibling of `agilent:intensity-f32-rounding`, which the
/// `--agilent-grid` reader declares for its own counts. Declared by
/// [`finish_archive`](crate::finish_archive).
pub const INTENSITY_F32_ROUNDING: &str = "intensity-f32-rounding";

/// `transformations` entry: an intensity was cast into a column of another type than float32 that
/// does not hold its value: a fractional or out-of-range value in an integer column (a facet has
/// one intensity column, typed from the spectra the writer samples; a file that turns from
/// integer to float intensities can have the floats cut to integers), a 64-bit integer above 2^53
/// in a float64 column. The facet's `intensity_values_narrowed` counts them. Declared by
/// [`finish_archive`](crate::finish_archive).
pub const INTENSITY_TYPE_NARROWING: &str = "intensity-type-narrowing";

/// Signal entries that carry their bound: `grid-fit:1e-6Da`, `tof-grid:5ppm`.
pub const SIGNAL_TRANSFORMATION_PREFIXES: [&str; 2] = ["grid-fit:", "tof-grid:"];

/// Is this `transformations` entry one of the signal set?
pub fn is_signal_transformation(entry: &str) -> bool {
    SIGNAL_TRANSFORMATIONS.contains(&entry) || SIGNAL_TRANSFORMATION_PREFIXES.iter().any(|p| entry.starts_with(p))
}

fn type_name(dtype: BinaryDataArrayType) -> &'static str {
    match dtype {
        BinaryDataArrayType::Float32 => "float32",
        BinaryDataArrayType::Float64 => "float64",
        BinaryDataArrayType::Int32 => "int32",
        BinaryDataArrayType::Int64 => "int64",
        BinaryDataArrayType::ASCII => "ascii",
        BinaryDataArrayType::Unknown => "unknown",
    }
}

/// How many values of an intensity array a float32 cannot hold: the count of `v` with
/// `(v as f32) != v`. A NaN counts as kept (a float32 holds one), a 32-bit float array as exact.
pub fn f32_cast_changes(arrays: &BinaryArrayMap) -> u64 {
    let mut tally = IntensityTally::default();
    tally.add(arrays, true);
    tally.rounded
}

/// What the intensities handed to the writer for one facet lose, counted for every type the
/// facet's intensity column can have. The column's type is the writer's schema, sampled from a few
/// spectra before the first is written, and a spectrum whose array is of another type is cast into
/// it; which of these counts happened is known when the schema is ([`resolve_intensities`]).
///
/// Two ways in. A centroid spectrum stored from mzdata's peak set (`through_f32`): the peak set
/// holds the float32 nearest each source value, whatever the column, and that float32 is what the
/// column gets. Arrays the writer takes as they are: each value is cast to the column's type.
#[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
struct IntensityTally {
    /// Source values no float32 holds, of spectra stored from a float32 peak set or handed over
    /// by the lane as float32: rounded whatever the column's type.
    rounded: u64,
    /// Float values of arrays the writer takes that no float32 holds: rounded in a float32 column.
    float_not_f32: u64,
    /// Float values handed to the column (a peak set's float32, a taken array's own value) that
    /// are not an int32 / int64: cut to one, or clamped, in an integer column.
    float_not_i32: u64,
    float_not_i64: u64,
    /// Integer values of arrays the writer takes that a float32 / float64 / int32 does not hold.
    /// The point layout casts them into the column; the chunked layout files an integer array of
    /// another type than the column's as an auxiliary array, at its own type, and changes nothing.
    int_not_f32: u64,
    int_not_f64: u64,
    int_not_i32: u64,
}

impl IntensityTally {
    fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// One float handed to the column: is it a value of the signed 32-bit and 64-bit integers?
    /// (No NaN is, and no infinity.)
    fn float(&mut self, x: f64) {
        const I32: f64 = 2_147_483_648.0;
        const I64: f64 = 9_223_372_036_854_775_808.0;
        let integral = x.trunc() == x;
        self.float_not_i32 += u64::from(!(integral && x >= -I32 && x < I32));
        self.float_not_i64 += u64::from(!(integral && x >= -I64 && x < I64));
    }

    /// One source value `x` whose nearest float32 is `narrow`; `exact` when they are one value.
    fn value(&mut self, x: f64, narrow: f32, exact: bool, through_f32: bool) {
        if through_f32 {
            self.rounded += u64::from(!exact);
            self.float(f64::from(narrow));
        } else {
            self.float_not_f32 += u64::from(!exact);
            self.float(x);
        }
    }

    /// One source integer.
    fn integer(&mut self, x: i64, through_f32: bool) {
        let narrow = x as f32;
        let exact = narrow as i128 == i128::from(x);
        if through_f32 {
            self.rounded += u64::from(!exact);
            self.float(f64::from(narrow));
        } else {
            self.int_not_f32 += u64::from(!exact);
            self.int_not_f64 += u64::from((x as f64) as i128 != i128::from(x));
            self.int_not_i32 += u64::from(i32::try_from(x).is_err());
        }
    }

    /// The intensity array of one spectrum, at the type the reader handed it over at.
    fn add(&mut self, arrays: &BinaryArrayMap, through_f32: bool) {
        let Some(a) = arrays.get(&ArrayType::IntensityArray) else { return };
        match a.dtype() {
            BinaryDataArrayType::Float64 => {
                if let Ok(v) = a.to_f64() {
                    v.iter().for_each(|x| self.value(*x, *x as f32, x.is_nan() || f64::from(*x as f32) == *x, through_f32));
                }
            }
            BinaryDataArrayType::Float32 => {
                if let Ok(v) = a.to_f32() {
                    v.iter().for_each(|x| self.value(f64::from(*x), *x, true, through_f32));
                }
            }
            BinaryDataArrayType::Int32 => {
                if let Ok(v) = a.to_i32() {
                    v.iter().for_each(|x| self.integer(i64::from(*x), through_f32));
                }
            }
            BinaryDataArrayType::Int64 => {
                if let Ok(v) = a.to_i64() {
                    v.iter().for_each(|x| self.integer(*x, through_f32));
                }
            }
            _ => {}
        }
    }

    fn json(&self) -> Value {
        json!({
            "rounded": self.rounded,
            "float_not_f32": self.float_not_f32,
            "float_not_i32": self.float_not_i32,
            "float_not_i64": self.float_not_i64,
            "int_not_f32": self.int_not_f32,
            "int_not_f64": self.int_not_f64,
            "int_not_i32": self.int_not_i32,
        })
    }

    fn from_json(v: &Value) -> Self {
        let n = |key: &str| v[key].as_u64().unwrap_or(0);
        Self {
            rounded: n("rounded"),
            float_not_f32: n("float_not_f32"),
            float_not_i32: n("float_not_i32"),
            float_not_i64: n("float_not_i64"),
            int_not_f32: n("int_not_f32"),
            int_not_f64: n("int_not_f64"),
            int_not_i32: n("int_not_i32"),
        }
    }

    /// `(rounded, narrowed)` in a facet of `layout` whose intensity column is a `stored` (the names
    /// [`stored_type`] gives): the values stored as the nearest float32, and those a column of
    /// another type does not hold. Without a column of a known type, what the peak sets rounded.
    fn resolve(&self, layout: &str, stored: Option<&str>) -> (u64, u64) {
        // The chunk builder casts floats only; an integer array of another type is not cast.
        let cast = |n: u64| if layout == "point" { n } else { 0 };
        match stored {
            Some("float32") => (self.rounded + self.float_not_f32 + cast(self.int_not_f32), 0),
            Some("float64") => (self.rounded, cast(self.int_not_f64)),
            Some("int32") => (self.rounded, self.float_not_i32 + cast(self.int_not_i32)),
            Some("int64") => (self.rounded, self.float_not_i64),
            _ => (self.rounded, 0),
        }
    }
}

/// The largest error a lane measured for one of its m/z statements, stored value against source
/// value over every point the statement covers (the `--tof-grid` fits: each gridded point's
/// reconstructed m/z against the m/z the reader handed over).
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct ObservedMzError {
    /// In m/z; `None` when the lane measures the relative error only.
    pub max_abs: Option<f64>,
    pub max_rel_ppm: f64,
}

impl ObservedMzError {
    /// Fold in one spectrum: its source m/z and the m/z stored for the same points, in order.
    pub fn observe(&mut self, source: &[f64], stored: &[f64]) {
        for (x, y) in source.iter().zip(stored) {
            let err = (x - y).abs();
            self.max_abs = Some(self.max_abs.unwrap_or(0.0).max(err));
            if *x > 0.0 {
                self.max_rel_ppm = self.max_rel_ppm.max(err / x * 1e6);
            }
        }
    }
}

/// A lane's fidelity block ([`SourceTally::block`], or an empty `(BLOCK, {})` for a lane that does
/// not count its source) with the error it measured for the `transformations` entry `encoding`
/// added; [`complete`] writes the figures into that entry of `mz_error`.
pub fn with_observed(mut block: (String, Value), encoding: &str, observed: ObservedMzError) -> (String, Value) {
    block.1[OBSERVED][encoding] = json!({
        "max_abs_error": observed.max_abs.map(round_up),
        "max_rel_error_ppm": round_up(observed.max_rel_ppm),
    });
    block
}

/// Lane-block key of the measured errors ([`with_observed`]); read by [`complete`], not written out.
const OBSERVED: &str = "observed";
/// Lane-block key of a facet's [`IntensityTally`]; [`resolve_intensities`] turns it into the two
/// counts below, and it is not written out.
const INTENSITY_TALLY: &str = "intensity_tally";
/// Facet key: source intensities stored as the nearest float32, which is another value
/// ([`INTENSITY_F32_ROUNDING`]).
const INTENSITY_ROUNDED: &str = "intensity_values_rounded";
/// Facet key: source intensities cast into a column of another type than float32 that does not
/// hold them ([`INTENSITY_TYPE_NARROWING`]).
const INTENSITY_NARROWED: &str = "intensity_values_narrowed";

/// What the reader handed over for one facet.
#[derive(Default)]
struct FacetSource {
    points: u64,
    mz: BTreeSet<&'static str>,
    intensity: BTreeSet<&'static str>,
    /// What the facet's intensity column does to the values, for each type it can have.
    intensities: IntensityTally,
}

impl FacetSource {
    /// A raw array map: its points (the m/z array's length, else the widest array's, as the
    /// writer counts a grid spectrum that carries an index axis and no m/z) and its array types.
    /// Returns the points.
    fn add_arrays(&mut self, arrays: &BinaryArrayMap, types: bool) -> u64 {
        let n = match arrays.get(&ArrayType::MZArray) {
            Some(mz) => mz.data_len().unwrap_or(0),
            None => arrays.iter().filter_map(|(_, a)| a.data_len().ok()).max().unwrap_or(0),
        };
        self.points += n as u64;
        if types && n > 0 {
            self.add_types(arrays);
        }
        n as u64
    }

    fn add_types(&mut self, arrays: &BinaryArrayMap) {
        if let Some(a) = arrays.get(&ArrayType::MZArray) {
            self.mz.insert(type_name(a.dtype()));
        }
        if let Some(a) = arrays.get(&ArrayType::IntensityArray) {
            self.intensity.insert(type_name(a.dtype()));
        }
    }

    fn json(&self, types: bool) -> Value {
        let mut out = json!({"source_points": self.points});
        if types && !(self.mz.is_empty() && self.intensity.is_empty()) {
            out["source_types"] = json!({"mz": self.mz, "intensity": self.intensity});
        }
        if !self.intensities.is_empty() {
            out[INTENSITY_TALLY] = self.intensities.json();
        }
        out
    }
}

/// Which signal facet the writer files a spectrum under.
#[derive(Clone, Copy, PartialEq)]
enum Filed {
    Data,
    Peaks,
}

/// The lane's half of the block: the points and array types of every spectrum it handed to the
/// writer, by the facet the writer files them under.
pub struct SourceTally {
    data: FacetSource,
    peaks: FacetSource,
    /// The reader hands each array over at the binary type its source declares (mzML, imzML), so
    /// the types seen are the source's. A vendor reader's array types are its own choice.
    declared_types: bool,
    /// Where the last counted spectrum's points went ([`Self::add_not_handed_over`]).
    last: Filed,
}

impl SourceTally {
    pub fn new(declared_types: bool) -> Self {
        Self { data: FacetSource::default(), peaks: FacetSource::default(), declared_types, last: Filed::Peaks }
    }

    fn facet(&mut self, filed: Filed) -> &mut FacetSource {
        match filed {
            Filed::Data => &mut self.data,
            Filed::Peaks => &mut self.peaks,
        }
    }

    /// Count one spectrum, as the writer will route it (`AbstractMzPeakWriter::write_spectrum_data`).
    /// Call it on the spectrum as it is handed to `write_spectrum`. Returns the points counted.
    ///
    /// Its intensities are counted with it ([`IntensityTally`]), by the way they reach the column.
    /// A centroid spectrum that carries a peak set copied from its arrays (mzdata's mzML reader
    /// builds one for every centroid spectrum) is stored from the peak set, whose intensity is a
    /// float32 whatever the file declares. The writer takes the arrays themselves when there is
    /// no peak set (profile and unknown-continuity spectra, a centroid spectrum without one) and
    /// when the arrays hold more than the peak set (`centroid_arrays_beyond_peaks`: a per-peak
    /// ion mobility or charge array), and casts each to the type of its column. A peak set beside
    /// profile arrays, or one of another length than the arrays, is nobody's copy and its own source.
    pub fn observe<C: CentroidLike, D: DeconvolutedCentroidLike>(&mut self, spec: &MultiLayerSpectrum<C, D>) -> u64 {
        self.count(spec, true)
    }

    /// [`Self::observe`] without the intensities, for a lane that hands the writer another
    /// spectrum than the one it read (`--tof-grid` re-shapes the arrays): the points and types are
    /// the source's, and the lane says what becomes of the intensities
    /// ([`Self::add_intensity_rounded`], [`Self::add_intensities_taken`]).
    pub fn observe_points<C: CentroidLike, D: DeconvolutedCentroidLike>(&mut self, spec: &MultiLayerSpectrum<C, D>) -> u64 {
        self.count(spec, false)
    }

    fn count<C: CentroidLike, D: DeconvolutedCentroidLike>(&mut self, spec: &MultiLayerSpectrum<C, D>, intensities: bool) -> u64 {
        // A wavelength spectrum goes to its own facet, which this block does not describe.
        if spec.spectrum_type().is_some_and(|t| !t.is_mass_spectrum()) {
            return 0;
        }
        let types = self.declared_types;
        let continuity = spec.signal_continuity();
        match spec.peaks() {
            RefPeakDataLevel::Missing => 0,
            RefPeakDataLevel::RawData(arrays) => {
                self.last = if continuity == SignalContinuity::Centroid { Filed::Peaks } else { Filed::Data };
                if intensities {
                    self.facet(self.last).intensities.add(arrays, false);
                }
                self.facet(self.last).add_arrays(arrays, types)
            }
            peaks @ (RefPeakDataLevel::Centroid(_) | RefPeakDataLevel::Deconvoluted(_)) => {
                let n = peaks.len();
                let centroid_set = matches!(peaks, RefPeakDataLevel::Centroid(_)) && continuity == SignalContinuity::Centroid;
                self.peaks.points += n as u64;
                self.last = Filed::Peaks;
                let mut counted = n as u64;
                if let Some(arrays) = spec.raw_arrays() {
                    if continuity == SignalContinuity::Profile {
                        // Profile arrays beside a peak set: both facets are written.
                        counted += self.data.add_arrays(arrays, types);
                        if intensities {
                            self.data.intensities.add(arrays, false);
                        }
                    } else {
                        // The peak set was built from these arrays; their types are the source's.
                        if types && n > 0 {
                            self.peaks.add_types(arrays);
                        }
                        let copy = arrays.get(&ArrayType::MZArray).and_then(|a| a.data_len().ok()) == Some(n);
                        let beyond = arrays.iter().any(|(t, _)| !matches!(t, ArrayType::MZArray | ArrayType::IntensityArray));
                        if copy && intensities {
                            self.peaks.intensities.add(arrays, !(centroid_set && beyond));
                        }
                    }
                }
                counted
            }
        }
    }

    /// Intensities of the last counted spectrum that the lane itself narrowed to float32 before
    /// the writer saw them (the `--tof-grid` grid rows): `n` source values no float32 holds.
    pub fn add_intensity_rounded(&mut self, n: u64) {
        self.facet(self.last).intensities.rounded += n;
    }

    /// The arrays the lane hands the writer for the last counted spectrum, with no peak set
    /// beside them (a `--tof-grid` spectrum off the grid keeps its source arrays): the writer
    /// casts the intensities to the type of the facet's column.
    pub fn add_intensities_taken(&mut self, arrays: &BinaryArrayMap) {
        self.facet(self.last).intensities.add(arrays, false);
    }

    /// Points the source holds that the reader did not hand over (`--no-ims-compact` on a TDF:
    /// the points of an MS2 frame outside every isolation window), added to the source side of
    /// the facet the last counted spectrum went to, so `source_points` is the file's count.
    pub fn add_not_handed_over(&mut self, points: u64) {
        self.facet(self.last).points += points;
    }

    /// The `(key, value)` pair a lane puts among its index blocks; [`complete`] merges the stored
    /// side into it.
    pub fn block(&self) -> (String, Value) {
        let t = self.declared_types;
        (BLOCK.to_string(), json!({"spectra_data": self.data.json(t), "spectra_peaks": self.peaks.json(t)}))
    }
}

/// One member of a ZIP that is still being written: name, where its bytes start, how many.
#[derive(Debug, PartialEq, Eq)]
struct Member {
    name: String,
    offset: u64,
    size: u64,
}

/// The members of a ZIP archive that has no central directory yet, from its local file headers.
///
/// The archive writer streams each Parquet facet into the `.tmp` file and writes the directory
/// only when it finishes, after the index — which is where this block has to go. A closed member's
/// local header holds its sizes (the writer seeks back and patches them, in the ZIP64 extra field
/// for the large-file entries it writes); the member still open has size 0 and ends the walk.
/// Stored (uncompressed) members only, which is all the writer produces.
fn unfinished_zip_members(file: &mut File) -> io::Result<Vec<Member>> {
    const LOCAL_HEADER: u32 = 0x0403_4b50;
    const ZIP64_EXTRA: u16 = 0x0001;
    let len = file.metadata()?.len();
    let mut members = Vec::new();
    let mut pos = 0u64;
    while pos + 30 <= len {
        let mut h = [0u8; 30];
        file.seek(SeekFrom::Start(pos))?;
        file.read_exact(&mut h)?;
        let u16_at = |i: usize| u16::from_le_bytes([h[i], h[i + 1]]);
        let u32_at = |i: usize| u32::from_le_bytes([h[i], h[i + 1], h[i + 2], h[i + 3]]);
        // Bit 3: sizes in a trailing data descriptor, which a seekable writer does not use.
        if u32_at(0) != LOCAL_HEADER || u16_at(6) & 0x0008 != 0 || u16_at(8) != 0 {
            break;
        }
        let (mut size, name_len, extra_len) = (u64::from(u32_at(18)), usize::from(u16_at(26)), usize::from(u16_at(28)));
        let mut tail = vec![0u8; name_len + extra_len];
        file.read_exact(&mut tail)?;
        let (name, extra) = tail.split_at(name_len);
        if size == u64::from(u32::MAX) {
            // ZIP64: uncompressed then compressed size, each present when its 32-bit field is
            // saturated. Stored members have the two equal; take the compressed one.
            let both = u32_at(22) == u32::MAX;
            let mut e = extra;
            size = 0;
            while e.len() >= 4 {
                let (id, n) = (u16::from_le_bytes([e[0], e[1]]), usize::from(u16::from_le_bytes([e[2], e[3]])));
                let body = &e[4..(4 + n).min(e.len())];
                let at = if both { 8 } else { 0 };
                if id == ZIP64_EXTRA && body.len() >= at + 8 {
                    size = u64::from_le_bytes(body[at..at + 8].try_into().expect("eight bytes"));
                    break;
                }
                e = &e[(4 + n).min(e.len())..];
            }
        }
        let offset = pos + 30 + (name_len + extra_len) as u64;
        members.push(Member { name: String::from_utf8_lossy(name).into_owned(), offset, size });
        if size == 0 {
            // The open member (or an empty one): what follows is its data, not a header.
            break;
        }
        pos = offset + size;
    }
    Ok(members)
}

/// One unit in the last place of a positive finite f64.
fn ulp(x: f64) -> f64 {
    let x = x.abs();
    f64::from_bits(x.to_bits() + 1) - x
}

/// The error bound of one numpress-linear chunk (see the module docs): absolute, in m/z.
fn numpress_bound(fixed_point: f64, largest_mz: f64) -> f64 {
    0.5 / fixed_point + 4.0 * ulp(largest_mz)
}

/// What the stored m/z chunks of one facet say about its encodings.
#[derive(Default, Debug, PartialEq)]
struct ChunkStats {
    numpress_chunks: u64,
    min_fixed_point: f64,
    numpress_abs: f64,
    /// `None` once a numpress chunk starts at m/z ≤ 0, where a relative error has no meaning.
    numpress_rel: Option<f64>,
    delta_chunks: u64,
    /// Delta chunks whose last m/z is more than twice their first.
    delta_at_risk: u64,
    delta_largest_at_risk: f64,
    /// At-risk delta chunks that start at or below zero (or at no number), where the one-ulp bound
    /// is not proven.
    delta_unbounded: u64,
    /// Rows whose m/z sit in the values or numpress column.
    value_chunks: u64,
    /// Rows whose m/z are grid indices (`MS:1003826`), and the m/z range they span.
    grid_chunks: u64,
    grid_min: f64,
    grid_max: f64,
}

impl ChunkStats {
    fn new() -> Self {
        Self { min_fixed_point: f64::INFINITY, numpress_rel: Some(0.0), grid_min: f64::INFINITY, ..Self::default() }
    }

    /// One numpress-linear chunk: its bounds and its encoded bytes (the first eight are read).
    fn numpress(&mut self, start: f64, end: f64, encoded: &[u8]) {
        self.value_chunks += 1;
        let Some(head) = encoded.first_chunk::<8>() else { return };
        // MS-Numpress writes the fixed point first, most significant byte first.
        let fixed_point = f64::from_be_bytes(*head);
        let bound = numpress_bound(fixed_point, start.abs().max(end.abs()));
        self.numpress_chunks += 1;
        self.min_fixed_point = self.min_fixed_point.min(fixed_point);
        self.numpress_abs = self.numpress_abs.max(bound);
        self.numpress_rel = match self.numpress_rel {
            Some(rel) if start > 0.0 => Some(rel.max(bound / start)),
            _ => None,
        };
    }

    fn delta(&mut self, start: f64, end: f64) {
        self.value_chunks += 1;
        self.delta_chunks += 1;
        if !(start > 0.0 && end <= 2.0 * start) {
            self.delta_at_risk += 1;
            self.delta_largest_at_risk = self.delta_largest_at_risk.max(end);
            self.delta_unbounded += u64::from(!(start > 0.0 && end.is_finite()));
        }
    }

    /// One grid row: the m/z are indices into the row's model, and the values column is empty.
    fn grid(&mut self, start: f64, end: f64) {
        self.grid_chunks += 1;
        self.grid_min = self.grid_min.min(start);
        self.grid_max = self.grid_max.max(end);
    }

    /// Fold another part of the same facet in.
    fn merge(&mut self, other: &Self) {
        self.numpress_chunks += other.numpress_chunks;
        self.min_fixed_point = self.min_fixed_point.min(other.min_fixed_point);
        self.numpress_abs = self.numpress_abs.max(other.numpress_abs);
        self.numpress_rel = match (self.numpress_rel, other.numpress_rel) {
            (Some(a), Some(b)) => Some(a.max(b)),
            _ => None,
        };
        self.delta_chunks += other.delta_chunks;
        self.delta_at_risk += other.delta_at_risk;
        self.delta_largest_at_risk = self.delta_largest_at_risk.max(other.delta_largest_at_risk);
        self.delta_unbounded += other.delta_unbounded;
        self.value_chunks += other.value_chunks;
        self.grid_chunks += other.grid_chunks;
        self.grid_min = self.grid_min.min(other.grid_min);
        self.grid_max = self.grid_max.max(other.grid_max);
    }

    /// What holds the facet's m/z: the values column's type, `grid:<index type>` when every row
    /// is a grid row, both when the facet mixes them (a `--tof-grid` run keeps an off-grid
    /// spectrum as 64-bit values beside the gridded ones).
    fn mz_storage(&self, values: Option<String>, grid_index: Option<&str>) -> Option<String> {
        if self.grid_chunks == 0 {
            return values;
        }
        let grid = format!("grid:{}", grid_index.unwrap_or("unknown"));
        match values {
            Some(v) if self.value_chunks > 0 => Some(format!("{v}+{grid}")),
            _ => Some(grid),
        }
    }
}

/// One signal facet as it is stored.
#[derive(Debug)]
struct StoredFacet {
    layout: String,
    points: Option<u64>,
    /// What holds the m/z ([`ChunkStats::mz_storage`]), and the intensity column's type.
    mz_type: Option<String>,
    intensity_type: Option<String>,
    chunks: ChunkStats,
}

/// The stored value type of a facet column: the list item's for a chunk column.
fn stored_type(dt: &DataType) -> String {
    match dt {
        DataType::Float32 => "float32".into(),
        DataType::Float64 => "float64".into(),
        DataType::Int32 => "int32".into(),
        DataType::Int64 => "int64".into(),
        DataType::List(f) | DataType::LargeList(f) => stored_type(f.data_type()),
        other => other.to_string().to_lowercase(),
    }
}

fn meta<'a>(field: &'a Field, key: &str) -> &'a str {
    field.metadata().get(key).map_or("", String::as_str)
}

const MZ_ARRAY: &str = "MS:1000514";
const INTENSITY_ARRAY: &str = "MS:1000515";
const NUMPRESS_LINEAR: &str = "MS:1002312";
const DELTA: &str = "MS:1003089";
const GRID: &str = "MS:1003826";

/// The first eight bytes of row `i` of a list-of-bytes column: a numpress chunk's fixed point.
fn list_head(column: &ArrayRef, i: usize) -> Option<[u8; 8]> {
    if column.is_null(i) {
        return None;
    }
    let (values, start, end) = match column.data_type() {
        DataType::LargeList(_) => {
            let list = column.as_list::<i64>();
            (list.values(), list.value_offsets()[i] as usize, list.value_offsets()[i + 1] as usize)
        }
        DataType::List(_) => {
            let list = column.as_list::<i32>();
            (list.values(), list.value_offsets()[i] as usize, list.value_offsets()[i + 1] as usize)
        }
        _ => return None,
    };
    let bytes = values.as_primitive_opt::<UInt8Type>()?.values();
    bytes.get(start..end)?.first_chunk::<8>().copied()
}

/// The stored value type of the column of a facet (the fields of its one struct column) that
/// holds the array `accession`.
fn value_column(children: &Fields, accession: &str) -> Option<String> {
    children
        .iter()
        .find(|f| meta(f, "array_accession") == accession && matches!(meta(f, "buffer_format"), "point" | "chunk_values" | "chunk_secondary"))
        .map(|f| stored_type(f.data_type()))
}

/// The block's name for a facet's layout, from the name of its struct column.
fn layout_name(top: &str) -> String {
    match top {
        "chunk" => "chunked".to_string(),
        other => other.to_string(),
    }
}

/// `(layout, intensity column type)` of a signal facet, from the schema its writer was built
/// with: what [`stored_facet`] reads back from the finished member, known before it is closed.
fn intensity_column(schema: &Schema) -> Option<(String, Option<String>)> {
    let top = schema.fields().first()?;
    let DataType::Struct(children) = top.data_type() else { return None };
    Some((layout_name(top.name()), value_column(children, INTENSITY_ARRAY)))
}

/// The lane's block with each facet's [`IntensityTally`] resolved against the facet's schema
/// (`schemas`: `spectra_data`, `spectra_peaks`, as the writer holds them; `None` for a facet it
/// never opened): `intensity_values_rounded` and `intensity_values_narrowed` where they are above
/// zero, and the tally itself removed. Call it before the facets are closed: the two
/// `transformations` entries are declared from the result ([`intensities_changed`]).
pub fn resolve_intensities(lane: &Value, schemas: [Option<&Schema>; 2]) -> Value {
    let mut lane = lane.clone();
    for ((_, key), schema) in FACETS.iter().zip(schemas) {
        let Some(facet) = lane.get_mut(*key).and_then(Value::as_object_mut) else { continue };
        let Some(tally) = facet.remove(INTENSITY_TALLY) else { continue };
        let (layout, stored) = schema.and_then(intensity_column).unwrap_or_default();
        let (rounded, narrowed) = IntensityTally::from_json(&tally).resolve(&layout, stored.as_deref());
        if rounded > 0 {
            facet.insert(INTENSITY_ROUNDED.to_string(), rounded.into());
        }
        if narrowed > 0 {
            facet.insert(INTENSITY_NARROWED.to_string(), narrowed.into());
        }
    }
    lane
}

/// The columns of a chunk facet's m/z main axis that the scan reads, by name.
struct ChunkColumns {
    top: String,
    start: String,
    end: String,
    encoding: String,
    numpress: Option<String>,
}

/// The chunk rows of one row group: bounds, encoding and, where there is one, the numpress column.
fn scan_row_group(tmp: &Path, member: &Member, metadata: &ArrowReaderMetadata, columns: &ChunkColumns, row_group: usize) -> Result<ChunkStats> {
    let reader = ArchiveFacetReader::new(File::open(tmp)?, member.offset, member.size, 0);
    let builder = ParquetRecordBatchReaderBuilder::new_with_metadata(reader, metadata.clone());
    let wanted = [Some(&columns.start), Some(&columns.end), Some(&columns.encoding), columns.numpress.as_ref()];
    let leaves: Vec<usize> = builder
        .parquet_schema()
        .columns()
        .iter()
        .enumerate()
        .filter(|(_, c)| matches!(c.path().parts(), [t, name, ..] if *t == columns.top && wanted.contains(&Some(name))))
        .map(|(i, _)| i)
        .collect();
    let mask = ProjectionMask::leaves(builder.parquet_schema(), leaves);
    let mut chunks = ChunkStats::new();
    for batch in builder.with_projection(mask).with_row_groups(vec![row_group]).build()? {
        let batch = batch?;
        let rows: &StructArray = batch.column(0).as_struct_opt().context("the facet's column is not a struct")?;
        let f64_column = |name: &str| -> Result<ArrayRef> {
            let c = rows.column_by_name(name).with_context(|| format!("no {name} column"))?;
            Ok(arrow::compute::cast(c, &DataType::Float64)?)
        };
        let (starts, ends) = (f64_column(&columns.start)?, f64_column(&columns.end)?);
        let (starts, ends) = (starts.as_primitive::<Float64Type>(), ends.as_primitive::<Float64Type>());
        let encodings = arrow::compute::cast(rows.column_by_name(&columns.encoding).context("no encoding column")?, &DataType::Utf8)?;
        let encodings = encodings.as_string::<i32>();
        let bytes = columns.numpress.as_ref().and_then(|n| rows.column_by_name(n));
        for i in 0..rows.len() {
            if starts.is_null(i) || ends.is_null(i) || encodings.is_null(i) {
                continue;
            }
            match encodings.value(i) {
                NUMPRESS_LINEAR => {
                    let head = bytes.and_then(|c| list_head(c, i));
                    chunks.numpress(starts.value(i), ends.value(i), head.as_ref().map_or(&[], |h| &h[..]));
                }
                DELTA => chunks.delta(starts.value(i), ends.value(i)),
                GRID => chunks.grid(starts.value(i), ends.value(i)),
                // Uncompressed values (`MS:1000576`), or an encoding this scan has no rule for.
                _ => chunks.value_chunks += 1,
            }
        }
    }
    Ok(chunks)
}

/// Read one signal facet back from the archive being written.
fn stored_facet(tmp: &Path, member: &Member) -> Result<StoredFacet> {
    let reader = ArchiveFacetReader::new(File::open(tmp)?, member.offset, member.size, 0);
    let metadata = ArrowReaderMetadata::load(&reader, Default::default())?;
    let points = metadata
        .metadata()
        .file_metadata()
        .key_value_metadata()
        .and_then(|kv| kv.iter().find(|kv| kv.key == mzpeak_prototyping::constants::SPECTRUM_DATA_POINT_COUNT))
        .and_then(|kv| kv.value.as_deref()?.parse().ok());
    let top = metadata.schema().fields().first().context("a facet with no column")?.clone();
    let DataType::Struct(children) = top.data_type() else {
        bail!("{}: the facet's column is not a struct", member.name);
    };
    let layout = layout_name(top.name());
    let (mz_type, intensity_type) = (value_column(children, MZ_ARRAY), value_column(children, INTENSITY_ARRAY));
    // The index type of the m/z grid column (`mz_grid { grid_type, parameters, indices }`).
    let grid_index = children
        .iter()
        .find(|f| meta(f, "array_accession") == MZ_ARRAY && meta(f, "buffer_format") == "chunk_transform" && meta(f, "transform") == GRID)
        .and_then(|f| match f.data_type() {
            DataType::Struct(parts) => parts.iter().find(|p| p.name() == "indices").map(|p| stored_type(p.data_type())),
            _ => None,
        });

    // The chunk rows of an m/z main axis, one row group per worker: the numpress column is most of
    // a default archive, and reading it back on one core cost 6 % of a 68-million-point imzML
    // conversion. A point facet and an empty one have nothing to scan; a grid facet's rows are
    // read for their bounds and encoding only.
    let mut chunks = ChunkStats::new();
    let named = |format: &str| children.iter().find(|f| meta(f, "array_accession") == MZ_ARRAY && meta(f, "buffer_format") == format).map(|f| f.name().clone());
    let numpress = children
        .iter()
        .find(|f| meta(f, "array_accession") == MZ_ARRAY && meta(f, "transform") == NUMPRESS_LINEAR)
        .map(|f| f.name().clone());
    if let (true, Some(start), Some(end), Some(encoding)) = (points.unwrap_or(1) > 0, named("chunk_start"), named("chunk_end"), named("chunk_encoding")) {
        let columns = ChunkColumns { top: top.name().clone(), start, end, encoding, numpress };
        let scanned: Vec<Result<ChunkStats>> = (0..metadata.metadata().num_row_groups())
            .into_par_iter()
            .map(|g| scan_row_group(tmp, member, &metadata, &columns, g))
            .collect();
        for group in scanned {
            chunks.merge(&group?);
        }
    }
    let mz_type = chunks.mz_storage(mz_type, grid_index.as_deref());
    Ok(StoredFacet { layout, points, mz_type, intensity_type, chunks })
}

/// The two signal facets of the archive being written at `tmp`, read back (see the module docs).
fn stored(tmp: &Path) -> Result<Vec<(&'static str, StoredFacet)>> {
    let members = unfinished_zip_members(&mut File::open(tmp)?)?;
    let mut out = Vec::new();
    for (member, key) in FACETS {
        if let Some(m) = members.iter().find(|m| m.name == member && m.size > 0) {
            out.push((key, stored_facet(tmp, m).with_context(|| format!("reading {member} back"))?));
        }
    }
    Ok(out)
}

/// `x` rounded UP to six significant digits: a bound stays a bound, and a figure this short comes
/// back from JSON as the same f64 (the `.mzpeak` filter parses and re-writes the index, and a
/// 17-digit float can return one unit in the last place off).
fn round_up(x: f64) -> f64 {
    if !(x.is_finite() && x > 0.0) {
        return x;
    }
    let scale = 10f64.powi(5 - x.log10().floor() as i32);
    (x * scale * (1.0 + 1e-12)).ceil() / scale
}

/// The number between `prefix` and `suffix` of a parametrised entry (`grid-fit:1e-6Da` → 1e-6).
fn entry_bound(entry: &str, prefix: &str, suffix: &str) -> Option<f64> {
    entry.strip_prefix(prefix)?.strip_suffix(suffix)?.parse().ok()
}

/// Does the lane's source declare nothing but 32-bit m/z for the facet `key`? Such values survive
/// delta exactly whatever their spacing (the differences are taken in 64-bit arithmetic).
fn mz_source_is_f32(lane: Option<&Value>, key: &str) -> bool {
    lane.and_then(|l| l.get(key))
        .and_then(|s| s.get("source_types"))
        .and_then(|t| t["mz"].as_array())
        .is_some_and(|t| !t.is_empty() && t.iter().all(|v| v == "float32"))
}

/// Whether the archive about to be finished gets a `delta` entry in `mz_error`, and with it
/// [`DELTA_ULP`] in `transformations`: a facet holds delta chunks that are not exact by
/// construction and its source m/z are not all 32-bit. `at_risk` is the writer's count of such
/// chunk rows per facet, in [`FACETS`] order (`spectra_data`, `spectra_peaks`), taken as the rows
/// were buffered with the rule [`ChunkStats::delta`] applies to them when [`complete`] reads them
/// back; `lane` is the lane's block. The entry is decided before the facets are closed because
/// the processing method that mirrors it is written with them.
pub fn delta_ulp_declared(lane: Option<&Value>, at_risk: [u64; 2]) -> bool {
    FACETS.iter().zip(at_risk).any(|((_, key), n)| n > 0 && !mz_source_is_f32(lane, key))
}

/// `(rounded, narrowed)` over both facets of a lane block that [`resolve_intensities`] resolved:
/// above zero, [`INTENSITY_F32_ROUNDING`] and [`INTENSITY_TYPE_NARROWING`] are declared.
pub fn intensities_changed(lane: Option<&Value>) -> (u64, u64) {
    let sum = |count: &str| FACETS.iter().filter_map(|(_, key)| lane?.get(key)?.get(count)?.as_u64()).sum();
    (sum(INTENSITY_ROUNDED), sum(INTENSITY_NARROWED))
}

/// The `mz_error` entries of the declared `transformations` that state their own bound: the grid
/// fits' tolerances (with what the lane measured, where it did), and the timsTOF statements.
/// `grid` is the m/z range of the grid rows stored, over both facets, when there are any.
fn declared_mz_errors(transformations: &[&str], lane: Option<&Value>, calibrant_ppm: Option<f64>, grid: Option<(f64, f64)>) -> Vec<Value> {
    let mut mz_error = Vec::new();
    // A tolerance is against the source value and the rows' bounds may be the fitted ones, a
    // tolerance apart: the derived figure allows for that.
    for t in transformations {
        if let Some(tol) = entry_bound(t, "grid-fit:", "Da") {
            let rel = grid.filter(|(min, _)| *min > tol).map(|(min, _)| round_up(tol / (min - tol) * 1e6));
            mz_error.push(json!({"encoding": t, "max_abs_error": tol, "max_rel_error_ppm": rel, "basis": "tolerance"}));
        } else if let Some(ppm) = entry_bound(t, "tof-grid:", "ppm") {
            let abs = grid.filter(|_| ppm < 1e6).map(|(_, max)| round_up(ppm * 1e-6 * max / (1.0 - ppm * 1e-6)));
            let mut entry = json!({"encoding": t, "max_abs_error": abs, "max_rel_error_ppm": ppm, "basis": "tolerance"});
            // What the accepted fits left, where the lane measured it: the tolerance is what a fit
            // had to stay within, and on a real flight-time lattice it stays far inside.
            if let Some(observed) = lane.and_then(|l| l.get(OBSERVED)).and_then(|o| o.get(*t)) {
                entry["observed_max_rel_error_ppm"] = observed["max_rel_error_ppm"].clone();
                if !observed["max_abs_error"].is_null() {
                    entry["observed_max_abs_error"] = observed["max_abs_error"].clone();
                }
            }
            mz_error.push(entry);
        } else if let ("bruker:mz-calibrant-omitted", Some(ppm)) = (*t, calibrant_ppm) {
            // The lane's own bound (`ims_calibration.max_error_ppm`): the largest correction the
            // vendor's calibrant polynomial makes, in ppm of the stored m/z; in m/z, that share of
            // the largest m/z stored.
            let abs = grid.map(|(_, max)| round_up(ppm * 1e-6 * max));
            mz_error.push(json!({"encoding": t, "max_abs_error": abs, "max_rel_error_ppm": round_up(ppm), "basis": "bound"}));
        } else if matches!(*t, "bruker:mz-calibration-chord" | "bruker:mz-calibrant-omitted" | "shimadzu:coarse-mz") {
            mz_error.push(json!({"encoding": t, "max_abs_error": Value::Null, "basis": "not measured"}));
        }
    }
    mz_error
}

/// The finished block: the lane's source side (`lane`, from [`SourceTally::block`], when the lane
/// counts) merged with what is stored at `tmp`, and the m/z error of every encoding present.
/// `transformations` is the lane's declared list, for the grid fits' tolerances; `calibrant_ppm`
/// the bound the lane states for `bruker:mz-calibrant-omitted` (`ims_calibration.max_error_ppm`).
pub fn complete(lane: Option<&Value>, tmp: &Path, transformations: &[&str], calibrant_ppm: Option<f64>) -> Result<Value> {
    let mut block = serde_json::Map::new();
    let mut mz_error: Vec<Value> = Vec::new();
    let mut stored = stored(tmp)?;
    // The m/z range of the grid rows of both facets, for the grid fits' second figure.
    let (mut grid_min, mut grid_max) = (f64::INFINITY, 0.0f64);
    for (_, key) in FACETS {
        let source = lane.and_then(|l| l.get(key));
        let source_points = source.and_then(|s| s["source_points"].as_u64());
        let Some(at) = stored.iter().position(|(k, _)| *k == key) else {
            // Points were handed over for a facet the archive does not hold: say so, with no
            // stored side, rather than leave the facet out as if it had been empty.
            if let Some(n) = source_points.filter(|n| *n > 0) {
                block.insert(key.to_string(), json!({"source_points": n}));
            }
            continue;
        };
        let (_, facet) = stored.swap_remove(at);
        if facet.points.unwrap_or(0) == 0 && source_points.unwrap_or(0) == 0 {
            continue;
        }
        let mut entry = json!({"layout": facet.layout});
        if let Some(n) = source_points {
            entry["source_points"] = n.into();
        }
        if let Some(n) = facet.points {
            entry["stored_points"] = n.into();
        }
        let source_types = source.and_then(|s| s.get("source_types"));
        if let Some(t) = source_types {
            entry["source_types"] = t.clone();
        }
        entry["stored_types"] = json!({"mz": facet.mz_type, "intensity": facet.intensity_type});
        for count in [INTENSITY_ROUNDED, INTENSITY_NARROWED] {
            if let Some(n) = source.and_then(|s| s.get(count)) {
                entry[count] = n.clone();
            }
        }
        block.insert(key.to_string(), entry);

        let c = &facet.chunks;
        if c.grid_chunks > 0 {
            (grid_min, grid_max) = (grid_min.min(c.grid_min), grid_max.max(c.grid_max));
        }
        if c.numpress_chunks > 0 {
            mz_error.push(json!({
                "encoding": "numpress-linear",
                "facet": key,
                "chunks": c.numpress_chunks,
                "min_fixed_point": c.min_fixed_point,
                "max_abs_error": round_up(c.numpress_abs),
                "max_rel_error_ppm": c.numpress_rel.map(|r| round_up(r * 1e6)),
                "basis": "bound",
            }));
        }
        // 32-bit m/z values survive delta exactly whatever their spacing. The same test, on the
        // writer's count of the same rows, declares `delta-ulp` ([`delta_ulp_declared`]).
        if c.delta_chunks > 0 && c.delta_at_risk > 0 && !mz_source_is_f32(lane, key) {
            // One unit in the last place of the value (the module docs have the proof): of the
            // largest m/z at risk in absolute terms, 2⁻⁵² of any value in relative ones.
            let bounded = c.delta_unbounded == 0;
            mz_error.push(json!({
                "encoding": "delta",
                "facet": key,
                "chunks": c.delta_chunks,
                "chunks_not_exact_by_construction": c.delta_at_risk,
                "largest_mz_at_risk": (c.delta_largest_at_risk * 1e6).ceil() / 1e6,
                "max_abs_error": bounded.then(|| round_up(ulp(c.delta_largest_at_risk))),
                "max_rel_error_ppm": bounded.then(|| round_up(f64::EPSILON * 1e6)),
                "basis": if bounded { "bound" } else { "not measured" },
            }));
        }
    }
    mz_error.extend(declared_mz_errors(transformations, lane, calibrant_ppm, (grid_min <= grid_max).then_some((grid_min, grid_max))));
    block.insert("mz_error".to_string(), mz_error.into());
    Ok(block.into())
}

/// Does storing a `source`-typed value in a `stored`-typed column keep every value? Equal types,
/// and the widenings that are exact.
fn keeps_every_value(source: &str, stored: &str) -> bool {
    source == stored || matches!((source, stored), ("float32", "float64") | ("int32", "int64") | ("int32", "float64"))
}

/// `--lossless`: the archive at hand is bit-exact, or the conversion fails. `block` is the finished
/// [`complete`] block of a lane that counted its source; `transformations` its declared list.
///
/// Bit-exact means: every point the source holds is stored, in the source's order, each m/z and
/// intensity with the value the source has (a 32-bit value in a 64-bit column still counts; the
/// block shows the widening). Checked from what is in the archive: no signal transformation
/// declared ([`is_signal_transformation`]), every facet in the point layout with as many points
/// as the reader handed over, no m/z encoding with an error, and no column narrower than its source.
pub fn check_lossless(block: &Value, transformations: &[&str]) -> Result<()> {
    let mut reasons: Vec<String> = Vec::new();
    let signal: Vec<&str> = transformations.iter().copied().filter(|t| is_signal_transformation(t)).collect();
    if !signal.is_empty() {
        reasons.push(format!("the conversion declares {}", signal.join(", ")));
    }
    for (_, key) in FACETS {
        let Some(facet) = block.get(key) else { continue };
        if facet["layout"] != "point" {
            reasons.push(format!("{key} is in the {} layout", facet["layout"].as_str().unwrap_or("?")));
        }
        match (facet["source_points"].as_u64(), facet["stored_points"].as_u64()) {
            (Some(a), Some(b)) if a == b => {}
            (Some(a), Some(b)) => reasons.push(format!("{key} stores {b} of the source's {a} points")),
            _ => reasons.push(format!("{key}: the source or stored point count is unknown")),
        }
        for array in ["mz", "intensity"] {
            let stored = facet["stored_types"][array].as_str();
            let source: Vec<&str> = facet["source_types"][array].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
            if source.is_empty() {
                reasons.push(format!("{key}: the source's {array} type is unknown"));
            }
            for s in source {
                if !stored.is_some_and(|t| keeps_every_value(s, t)) {
                    reasons.push(format!("{key} stores {array} as {} where the source declares {s}", stored.unwrap_or("nothing")));
                }
            }
        }
    }
    for e in block["mz_error"].as_array().into_iter().flatten() {
        reasons.push(format!("m/z is stored with {}", e["encoding"].as_str().unwrap_or("a lossy encoding")));
    }
    if reasons.is_empty() {
        return Ok(());
    }
    bail!("--lossless: the archive would not be bit-exact ({}); nothing was written", reasons.join("; "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::array::Float64Array;
    use mzdata::spectrum::DataArray;
    use mzpeak_prototyping::archive::ZipArchiveWriter;
    use mzpeak_prototyping::chunk_series::ChunkingStrategy;
    use std::io::Write;

    fn scratch(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("mzpc-fidelity-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// A tiny deterministic generator (the suite has no `rand`).
    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self) -> f64 {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (self.0 >> 11) as f64 / (1u64 << 53) as f64
        }
    }

    /// The local-header walk finds every closed member of an archive that has no central directory
    /// yet, at the offsets and sizes the finished archive's directory then states.
    #[test]
    fn closed_members_of_an_unfinished_zip_are_found() {
        let dir = scratch("zip");
        let path = dir.join("a.zip");
        let payloads: [(&str, Vec<u8>); 3] = [("spectra_data.parquet", vec![7u8; 70_000]), ("spectra_peaks.parquet", b"PAR1 peaks".to_vec()), ("open.bin", vec![1, 2, 3])];
        let mut zip = ZipArchiveWriter::new(File::create(&path).unwrap());
        for (name, bytes) in &payloads {
            zip.start_other(name).unwrap();
            zip.write_all(bytes).unwrap();
        }
        zip.flush().unwrap();
        // The third member is still open: its header holds no size yet.
        let found = unfinished_zip_members(&mut File::open(&path).unwrap()).unwrap();
        assert_eq!(found.len(), 3, "{found:?}");
        assert_eq!((found[0].name.as_str(), found[0].size), ("spectra_data.parquet", 70_000));
        assert_eq!((found[1].name.as_str(), found[1].size), ("spectra_peaks.parquet", 10));
        assert_eq!((found[2].name.as_str(), found[2].size), ("open.bin", 0));
        zip.finish().unwrap();
        let mut done = zip::ZipArchive::new(File::open(&path).unwrap()).unwrap();
        for m in &found[..2] {
            let entry = done.by_name(&m.name).unwrap();
            assert_eq!((entry.data_start(), entry.size()), (m.offset, m.size), "{}", m.name);
        }
        let mut bytes = vec![0u8; 10];
        let mut f = File::open(&path).unwrap();
        f.seek(SeekFrom::Start(found[1].offset)).unwrap();
        f.read_exact(&mut bytes).unwrap();
        assert_eq!(bytes, b"PAR1 peaks");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The recorded numpress bound holds for every decoded value, and the bare `0.5 / fixed point`
    /// does not: chunks of m/z are encoded and decoded with the writer's own codec, 200,000 values
    /// over the mass ranges real data covers. Every other chunk holds 32-bit values, as most
    /// imzML does: their products with the fixed point land on exact half steps, which is where
    /// the f64 division then carries a value past the bare bound.
    #[test]
    fn the_numpress_bound_holds_where_the_bare_half_step_does_not() {
        let strategy = ChunkingStrategy::NumpressLinear { chunk_size: 50.0 };
        let mut rng = Lcg(0x5eed);
        let (mut beyond_bare, mut tightest) = (0usize, 0.0f64);
        for chunk in 0..400 {
            // A chunk: up to 50 Th wide, starting anywhere between 0.02 and 3000 Th.
            let lo = 0.02 + 3000.0 * rng.next().powi(3);
            let single = chunk % 2 == 1;
            let mut mz: Vec<f64> = (0..500).map(|_| lo + 50.0 * rng.next()).map(|x| if single { f64::from(x as f32) } else { x }).collect();
            mz.sort_by(f64::total_cmp);
            let (start, end, encoded) = strategy.encode_arrow(&Float64Array::from(mz.clone()));
            let bytes = encoded.as_primitive::<UInt8Type>().values().to_vec();
            let mut stats = ChunkStats::new();
            stats.numpress(start, end, &bytes);
            let fixed_point = stats.min_fixed_point;
            let mut decoded = DataArray::from_name_and_type(&ArrayType::MZArray, BinaryDataArrayType::Float64);
            strategy.decode_arrow(&encoded, start, end, &mut decoded, None);
            let decoded = decoded.to_f64().unwrap();
            assert_eq!(decoded.len(), mz.len(), "chunk {chunk}");
            for (a, b) in mz.iter().zip(decoded.iter()) {
                let d = (a - b).abs();
                assert!(d <= stats.numpress_abs, "chunk {chunk}: |{a} - {b}| = {d:e} above the bound {:e}", stats.numpress_abs);
                assert!(d / a <= stats.numpress_rel.unwrap(), "chunk {chunk}: relative {:e} above {:e}", d / a, stats.numpress_rel.unwrap());
                beyond_bare += usize::from(d > 0.5 / fixed_point);
                tightest = tightest.max(d / stats.numpress_abs);
            }
        }
        assert!(beyond_bare > 0, "no value beyond 0.5/fixed point: the ulp term would be unnecessary");
        assert!(beyond_bare < 100, "{beyond_bare} values beyond 0.5/fixed point: more than rounding");
        assert!(tightest > 0.99, "the bound is loose: the worst value reached {tightest} of it");
    }

    /// Delta is exact by construction in a chunk whose last m/z is at most twice its first, and
    /// not otherwise: the rule the block counts chunks by, on the pairs the codec rounds.
    #[test]
    fn delta_is_exact_within_a_factor_of_two_and_not_beyond() {
        let mut rng = Lcg(42);
        let roundtrips = |a: f64, b: f64| b + (a - b) == a;
        let (mut within, mut beyond) = (0usize, 0usize);
        for _ in 0..200_000 {
            let b = 1.0 + 49.0 * rng.next();
            let near = b * (1.0 + rng.next());
            assert!(roundtrips(near, b), "{b} -> {near} is within a factor of two and must be exact");
            within += 1;
            let far = b * (2.0 + 6.0 * rng.next());
            beyond += usize::from(!roundtrips(far, b));
        }
        assert!(within > 0 && beyond > 1000, "beyond a factor of two some pairs must round: {beyond}");
        let mut stats = ChunkStats::new();
        stats.delta(60.0, 110.0);
        stats.delta(1.0078, 38.96);
        assert_eq!((stats.delta_chunks, stats.delta_at_risk, stats.delta_largest_at_risk, stats.delta_unbounded), (2, 1, 38.96, 0));
        // A chunk from zero or below: at risk, and outside what the one-ulp bound is proven for.
        stats.delta(0.0, 12.5);
        assert_eq!((stats.delta_at_risk, stats.delta_unbounded, stats.value_chunks), (2, 1, 3));
    }

    /// The bound a delta entry records: through a whole chunk, every decoded m/z is within one
    /// unit in the last place of its source value — so within the ulp of the chunk's last m/z and
    /// within 2⁻⁵² of the value — and the bound is reached. Chunks of sparse ascending m/z at every
    /// mass, encoded and decoded with the writer's own codec; an error made at one step is carried
    /// into the next, which is what the induction in the module docs is about.
    #[test]
    fn a_delta_chunk_decodes_within_one_ulp_of_every_source_value() {
        let strategy = ChunkingStrategy::Delta { chunk_size: 50.0 };
        let mut rng = Lcg(0xde17a);
        let (mut off, mut off_above_1000, mut carried, mut reached) = (0usize, 0usize, 0usize, false);
        for chunk in 0..4000 {
            // 2 to 60 values from somewhere in 0.5 … 500 Th, each step a factor of 1 to 4 (or,
            // every third chunk, of 1 to 1.2: a dense stretch after an error, which carries it).
            let mut x = 0.5 + 500.0 * rng.next().powi(4);
            let n = 2 + (58.0 * rng.next()) as usize;
            let dense_after = if chunk % 3 == 0 { 3 } else { usize::MAX };
            let mz: Vec<f64> = (0..n)
                .map(|i| {
                    x *= if i >= dense_after { 1.0 + 0.2 * rng.next() } else { 1.0 + 3.0 * rng.next().powi(2) };
                    x
                })
                .collect();
            let (start, end, encoded) = strategy.encode_arrow(&Float64Array::from(mz.clone()));
            let mut stats = ChunkStats::new();
            stats.delta(start, end);
            let mut decoded = DataArray::from_name_and_type(&ArrayType::MZArray, BinaryDataArrayType::Float64);
            strategy.decode_arrow(&encoded, start, end, &mut decoded, None);
            let decoded = decoded.to_f64().unwrap();
            assert_eq!(decoded.len(), mz.len(), "chunk {chunk}");
            let mut previous_off = false;
            for (a, b) in mz.iter().zip(decoded.iter()) {
                let d = (a - b).abs();
                assert!(d <= ulp(*a), "chunk {chunk}: {a} decoded as {b}, {d:e} off, more than its ulp {:e}", ulp(*a));
                assert!(d <= ulp(end) && d / a <= f64::EPSILON, "chunk {chunk}: {a} decoded as {b}");
                if d > 0.0 {
                    assert!(stats.delta_at_risk == 1 && stats.delta_unbounded == 0, "chunk {chunk} [{start}, {end}] is not counted at risk");
                    off += 1;
                    off_above_1000 += usize::from(*a > 1000.0);
                    carried += usize::from(previous_off);
                    reached |= d == ulp(end);
                }
                previous_off = d > 0.0;
            }
        }
        assert!(off > 1000, "only {off} values rounded: the chunks are not sparse enough to test the bound");
        assert!(off_above_1000 > 100, "only {off_above_1000} values above m/z 1000 rounded: the error is not a low-mass one");
        assert!(carried > 100, "only {carried} errors followed another: carrying is not exercised");
        assert!(reached, "no value reached the ulp of its chunk's last m/z: the bound is loose");
    }

    /// What `stored_types.mz` says: the values column's type, the grid's index type where every
    /// row is a grid row, both where the facet mixes them.
    #[test]
    fn grid_rows_are_named_as_the_mz_storage() {
        let float64 = || Some("float64".to_string());
        let mut stats = ChunkStats::new();
        assert_eq!(stats.mz_storage(float64(), Some("uint32")), float64(), "no row scanned: the column type");
        stats.grid(301.5, 1200.25);
        stats.grid(99.75, 800.0);
        assert_eq!((stats.grid_chunks, stats.grid_min, stats.grid_max), (2, 99.75, 1200.25));
        assert_eq!(stats.mz_storage(float64(), Some("uint32")).as_deref(), Some("grid:uint32"));
        assert_eq!(stats.mz_storage(None, None).as_deref(), Some("grid:unknown"));
        let mut other = ChunkStats::new();
        other.delta(100.0, 150.0);
        stats.merge(&other);
        assert_eq!(stats.mz_storage(float64(), Some("uint32")).as_deref(), Some("float64+grid:uint32"));
        assert_eq!(other.mz_storage(float64(), Some("uint32")), float64());
    }

    /// A recorded bound is rounded up, to six digits, and survives a JSON round trip unchanged.
    #[test]
    fn recorded_figures_round_up_and_survive_json() {
        for x in [1.7960996212517e-7, 4.656754985961351e-10, 0.0002330768570048625, 7.105427357601002e-15, 123456.7, 1.0, 9.999999e-3] {
            let r = round_up(x);
            assert!(r >= x && r <= x * 1.00001, "{x} -> {r}");
            let back: f64 = serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
            assert_eq!(back.to_bits(), r.to_bits(), "{x} -> {r} -> {back}");
            assert!(serde_json::to_string(&r).unwrap().len() <= 12, "{r}");
        }
        assert_eq!(round_up(0.0), 0.0);
    }

    /// The signal set: what `--lossless` fails on, and what it lets through.
    #[test]
    fn signal_transformations_are_told_from_the_rest() {
        for t in [
            "zero-run-mask",
            "numpress-linear",
            "delta-ulp",
            "intensity-f32-rounding",
            "sort-by-mz",
            "grid-fit:1e-6Da",
            "tof-grid:5ppm",
            "agilent:drop-zero-samples",
            "waters:sonar-summed",
            "bruker:out-of-window-points-dropped",
        ] {
            assert!(is_signal_transformation(t), "{t}");
        }
        for t in [
            "mzml:dangling-reference-dropped",
            "chromatogram-time-to-minutes",
            "sort-by-time",
            "imzml:pixel-size-unit-assumed-um",
            "imaging:pixel-count-from-positions",
            "thermo:target-only-isolation-window",
            "bruker:trace-unit-rescale",
        ] {
            assert!(!is_signal_transformation(t), "{t}");
        }
    }

    /// Every vendor-namespaced `transformations` entry the sources name is classified: in the signal
    /// set, or in the list here of entries that leave spectrum signal alone. A new entry lands in
    /// neither, and this test asks for the decision `--lossless` depends on. (The `imzml:`,
    /// `imaging:` and `mzml:` namespaces describe metadata and are not signal by their kind.)
    #[test]
    fn every_vendor_entry_in_the_sources_is_classified() {
        const NOT_SIGNAL: [&str; 8] = [
            "thermo:target-only-isolation-window",
            "thermo:invalid-precursor-reference-dropped",
            "bruker:trace-unit-rescale",
            "bruker:trace-sort-dedup",
            "bruker:raster-index-shifted-to-base-1",
            "bruker:pixel-size-from-beam-scan-size",
            "waters:off-grid-position-dropped",
            "waters:laser-position-fitted-to-grid",
        ];
        const NAMESPACES: [&str; 6] = ["agilent:", "bruker:", "sciex:", "shimadzu:", "thermo:", "waters:"];
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut seen = BTreeSet::new();
        for entry in std::fs::read_dir(src).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_none_or(|e| e != "rs") {
                continue;
            }
            // Every other `"`-separated piece of a line is a string literal's text (near enough:
            // an escaped quote only splits a literal into pieces that match nothing).
            for literal in std::fs::read_to_string(&path).unwrap().lines().flat_map(|l| l.split('"').skip(1).step_by(2).map(str::to_string).collect::<Vec<_>>()) {
                let Some(ns) = NAMESPACES.iter().find(|ns| literal.starts_with(**ns)) else { continue };
                let name = &literal[ns.len()..];
                if !name.is_empty() && name.contains('-') && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-') {
                    seen.insert(literal);
                }
            }
        }
        assert!(seen.len() >= 20, "the scan found only {seen:?}");
        let unclassified: Vec<&String> = seen.iter().filter(|e| !is_signal_transformation(e) && !NOT_SIGNAL.contains(&e.as_str())).collect();
        assert!(
            unclassified.is_empty(),
            "transformations entries that are neither in fidelity::SIGNAL_TRANSFORMATIONS (they change \
             spectrum signal, and --lossless must fail on them) nor in this test's NOT_SIGNAL list: {unclassified:?}"
        );
        for e in NOT_SIGNAL {
            assert!(!is_signal_transformation(e) && seen.contains(e), "{e} is listed here but gone from the sources, or is signal");
        }
    }

    /// A spectrum as a lane hands it to the writer: m/z and `intensity` arrays, a per-peak ion
    /// mobility array when `mobility`, and, when `peak_set`, the centroid peak set mzdata's mzML
    /// reader builds from the two (64-bit m/z, 32-bit intensity).
    fn spectrum(continuity: SignalContinuity, mz: &[f64], intensity: DataArray, peak_set: bool, mobility: bool) -> MultiLayerSpectrum<mzpeaks::CentroidPeak, mzpeaks::DeconvolutedPeak> {
        let mut arrays = BinaryArrayMap::new();
        let mut mz_array = DataArray::wrap(&ArrayType::MZArray, BinaryDataArrayType::Float64, Vec::new());
        mz_array.update_buffer(mz).unwrap();
        arrays.add(mz_array);
        arrays.add(intensity);
        if mobility {
            let mut im = DataArray::wrap(&ArrayType::MeanInverseReducedIonMobilityArray, BinaryDataArrayType::Float64, Vec::new());
            im.update_buffer(&vec![1.0f64; mz.len()]).unwrap();
            arrays.add(im);
        }
        let description = mzdata::spectrum::SpectrumDescription { signal_continuity: continuity, ..Default::default() };
        let peaks = peak_set.then(|| mzpeaks::PeakSet::new(mz.iter().enumerate().map(|(i, m)| mzpeaks::CentroidPeak::new(*m, 1.0, i as u32)).collect()));
        MultiLayerSpectrum::new(description, Some(arrays), peaks, None)
    }

    /// An intensity array of `dtype` over `values`' little-endian bytes.
    macro_rules! intensity_array {
        ($dtype:ident, $values:expr) => {
            DataArray::wrap(&ArrayType::IntensityArray, BinaryDataArrayType::$dtype, $values.iter().flat_map(|x| x.to_le_bytes()).collect())
        };
    }

    /// What a float32 cannot hold is counted per source type: a 64-bit float that is not a
    /// float32, an integer above 2^24 that is not a multiple of its float32 spacing. NaN and a
    /// 32-bit array count nothing.
    #[test]
    fn values_a_float32_cannot_hold_are_counted_by_source_type() {
        let count = |a: DataArray| {
            let mut arrays = BinaryArrayMap::new();
            arrays.add(a);
            f32_cast_changes(&arrays)
        };
        assert_eq!(count(intensity_array!(Float64, [1.5f64, 16777217.0, 0.1, f64::NAN, 1e300, -2.0])), 3, "16777217, 0.1 and 1e300");
        assert_eq!(count(intensity_array!(Int32, [0i32, 16777216, 16777217, 16777218, i32::MAX, -16777217])), 3);
        assert_eq!(count(intensity_array!(Int64, [1i64 << 40, (1 << 40) + 1, i64::MAX, 7])), 2);
        assert_eq!(count(intensity_array!(Float32, [0.1f32, 3.0e38])), 0);
        assert_eq!(f32_cast_changes(&BinaryArrayMap::new()), 0);
    }

    /// A facet schema as the writer builds it: one struct column named for the layout, holding an
    /// intensity column of `dtype` (a list of it in the chunked layout).
    fn facet_schema(layout: &str, dtype: DataType) -> Schema {
        let chunked = layout == "chunk";
        let meta = [("array_accession", INTENSITY_ARRAY), ("buffer_format", if chunked { "chunk_secondary" } else { "point" })];
        let dtype = if chunked { DataType::LargeList(std::sync::Arc::new(Field::new("item", dtype, true))) } else { dtype };
        let intensity = Field::new("intensity", dtype, true).with_metadata(meta.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect());
        Schema::new(vec![Field::new(layout, DataType::Struct(vec![intensity].into()), true)])
    }

    /// `(rounded, narrowed)` of the lane block of `tally`, per facet (`spectra_data`,
    /// `spectra_peaks`), when both facets store their intensities as `dtype` in `layout`.
    fn changed(tally: &SourceTally, layout: &str, dtype: DataType) -> [(u64, u64); 2] {
        let schema = facet_schema(layout, dtype);
        let lane = resolve_intensities(&tally.block().1, [Some(&schema), Some(&schema)]);
        assert!(FACETS.iter().all(|(_, key)| lane[key].get(INTENSITY_TALLY).is_none()), "the tally is not written out: {lane}");
        FACETS.map(|(_, key)| (lane[key][INTENSITY_ROUNDED].as_u64().unwrap_or(0), lane[key][INTENSITY_NARROWED].as_u64().unwrap_or(0)))
    }

    /// The counts behind `intensity-f32-rounding` and `intensity-type-narrowing` follow the way a
    /// spectrum's intensities reach the column AND the column's type. A centroid spectrum stored
    /// from the peak set mzdata built from its arrays is rounded whatever the column. Arrays the
    /// writer takes (a per-peak mobility array beside the peak set, no peak set, profile signal)
    /// are cast to the column's type: rounded in a float32 column, exact in a float64 one, cut in
    /// an integer one. Through 0.17.0-rc.1 none of it was counted; the first version of this count
    /// took arrays the writer takes for exact, which they are only in a column of their own type.
    #[test]
    fn intensities_are_counted_by_route_and_by_the_type_of_the_column() {
        let mz = [100.0, 200.0, 300.0, 400.0];
        // 16777217 and 0.1 are not float32 values; 0.1 alone is no integer.
        let wide = || intensity_array!(Float64, [16777217.0f64, 2.0, 0.1, 4.0]);
        let tally_of = |spec: &MultiLayerSpectrum<mzpeaks::CentroidPeak, mzpeaks::DeconvolutedPeak>| {
            let mut tally = SourceTally::new(true);
            tally.observe(spec);
            tally
        };

        // Stored from the peak set: rounded in every column; the float32 handed on is 16777216
        // and 0.1f32, of which an integer column holds the first.
        let mut tally = SourceTally::new(true);
        assert_eq!(tally.observe(&spectrum(SignalContinuity::Centroid, &mz, wide(), true, false)), 4, "the points counted are returned");
        for layout in ["chunk", "point"] {
            assert_eq!(changed(&tally, layout, DataType::Float32), [(0, 0), (2, 0)], "{layout}");
            assert_eq!(changed(&tally, layout, DataType::Float64), [(0, 0), (2, 0)], "{layout}");
            assert_eq!(changed(&tally, layout, DataType::Int32), [(0, 0), (2, 1)], "{layout}");
        }
        let lane = resolve_intensities(&tally.block().1, [None, Some(&facet_schema("chunk", DataType::Float32))]);
        assert_eq!(lane["spectra_peaks"], json!({"source_points": 4, "source_types": {"mz": ["float64"], "intensity": ["float64"]}, "intensity_values_rounded": 2}));
        assert_eq!(intensities_changed(Some(&lane)), (2, 0));
        assert_eq!(intensities_changed(None), (0, 0));

        // Arrays the writer takes, into the peak facet (centroid) or the data facet (profile):
        // the column's type decides.
        for (what, spec, facet) in [
            ("the arrays hold a mobility array, and are what is stored", spectrum(SignalContinuity::Centroid, &mz, wide(), true, true), 1),
            ("no peak set: the arrays are stored", spectrum(SignalContinuity::Centroid, &mz, wide(), false, false), 1),
            ("profile arrays", spectrum(SignalContinuity::Profile, &mz, wide(), false, false), 0),
            ("profile arrays beside a peak set", spectrum(SignalContinuity::Profile, &mz, wide(), true, false), 0),
            ("unknown continuity goes with profile", spectrum(SignalContinuity::Unknown, &mz, wide(), false, false), 0),
        ] {
            let tally = tally_of(&spec);
            for layout in ["chunk", "point"] {
                assert_eq!(changed(&tally, layout, DataType::Float32)[facet], (2, 0), "{what}, {layout}: a float32 column rounds");
                assert_eq!(changed(&tally, layout, DataType::Float64)[facet], (0, 0), "{what}, {layout}: a float64 column holds every value");
                assert_eq!(changed(&tally, layout, DataType::Int32)[facet], (0, 1), "{what}, {layout}: an int32 column cuts 0.1");
                assert_eq!(changed(&tally, layout, DataType::Int64)[facet], (0, 1), "{what}, {layout}");
                assert_eq!(changed(&tally, layout, DataType::Float32)[1 - facet], (0, 0), "{what}: the other facet");
            }
        }
        // 32-bit float intensities: exact in either float column.
        let tally = tally_of(&spectrum(SignalContinuity::Centroid, &mz, intensity_array!(Float32, [1.0f32, 2.0, 3.5, 4.0]), true, false));
        assert_eq!((changed(&tally, "chunk", DataType::Float32)[1], changed(&tally, "point", DataType::Float64)[1]), ((0, 0), (0, 0)));
        assert_eq!(changed(&tally, "chunk", DataType::Int32)[1], (0, 1), "3.5 in an integer column");

        // Integer arrays the writer takes. The point layout casts them into the column; the chunk
        // builder files an integer array of another type as an auxiliary array, unchanged.
        let tally = tally_of(&spectrum(SignalContinuity::Profile, &mz, intensity_array!(Int64, [16777217i64, (1 << 53) + 1, 1 << 40, 7]), false, false));
        assert_eq!(changed(&tally, "point", DataType::Float32)[0], (2, 0), "16777217 and 2^53 + 1");
        assert_eq!(changed(&tally, "point", DataType::Float64)[0], (0, 1), "2^53 + 1");
        assert_eq!(changed(&tally, "point", DataType::Int32)[0], (0, 2), "2^53 + 1 and 2^40");
        assert_eq!(changed(&tally, "point", DataType::Int64)[0], (0, 0));
        for dtype in [DataType::Float32, DataType::Float64, DataType::Int32, DataType::Int64] {
            assert_eq!(changed(&tally, "chunk", dtype.clone())[0], (0, 0), "chunked, {dtype}");
        }

        // A peak set of another length is not a copy of the arrays.
        let mut spec = spectrum(SignalContinuity::Centroid, &mz, wide(), true, false);
        spec.peaks = Some(mzpeaks::PeakSet::new(vec![mzpeaks::CentroidPeak::new(150.0, 1.0, 0)]));
        assert_eq!(changed(&tally_of(&spec), "chunk", DataType::Float32), [(0, 0), (0, 0)]);

        // A lane that re-shapes the spectrum counts the points here and the intensities itself
        // (`--tof-grid`): its own float32 rows, and the arrays it leaves for the writer to cast.
        // Points the reader did not hand over go to the facet's source side.
        let mut tally = SourceTally::new(false);
        assert_eq!(tally.observe_points(&spectrum(SignalContinuity::Centroid, &mz, wide(), true, false)), 4);
        assert_eq!(changed(&tally, "chunk", DataType::Float32), [(0, 0), (0, 0)], "observe_points counts no intensity");
        tally.add_intensity_rounded(3);
        assert_eq!(tally.observe_points(&spectrum(SignalContinuity::Centroid, &mz, wide(), true, false)), 4);
        tally.add_intensities_taken(spectrum(SignalContinuity::Centroid, &mz, wide(), false, false).raw_arrays().unwrap());
        tally.add_not_handed_over(10);
        assert_eq!(changed(&tally, "chunk", DataType::Float32), [(0, 0), (5, 0)]);
        assert_eq!(changed(&tally, "chunk", DataType::Float64), [(0, 0), (3, 0)]);
        let block = tally.block().1;
        assert_eq!((&block["spectra_peaks"]["source_points"], &block["spectra_data"]), (&json!(18), &json!({"source_points": 0})));
    }

    /// `delta-ulp` is declared for a facet with at-risk delta chunks unless its source m/z are all
    /// 32-bit — the test [`complete`] puts to the same rows for the `delta` entry.
    #[test]
    fn delta_ulp_follows_the_at_risk_chunks_of_a_64_bit_facet() {
        let lane = |data: &[&str], peaks: &[&str]| json!({"spectra_data": {"source_types": {"mz": data}}, "spectra_peaks": {"source_types": {"mz": peaks}}});
        assert!(!delta_ulp_declared(None, [0, 0]));
        assert!(delta_ulp_declared(None, [0, 3]), "a lane that states no source types");
        assert!(delta_ulp_declared(Some(&lane(&["float64"], &["float64"])), [2, 0]));
        assert!(!delta_ulp_declared(Some(&lane(&["float32"], &["float64"])), [2, 0]), "32-bit m/z are exact under delta");
        assert!(delta_ulp_declared(Some(&lane(&["float32"], &["float64"])), [2, 1]));
        assert!(delta_ulp_declared(Some(&lane(&["float32", "float64"], &[])), [2, 0]), "a facet that mixes the two");
        assert!(delta_ulp_declared(Some(&lane(&[], &[])), [0, 1]), "no type seen is not 32-bit");
    }

    /// The entries that state their own bound: a `tof-grid` entry carries what the lane measured
    /// beside its tolerance, and `bruker:mz-calibrant-omitted` the bound `ims_calibration` states,
    /// with its share of the largest m/z stored; without a bound it stays `not measured`.
    #[test]
    fn declared_entries_state_the_measured_error_and_the_lane_s_bound() {
        // Errors that are exact in binary: 2^-30 at m/z 100 (the largest relative one), 2^-29 at
        // m/z 400 (the largest absolute one), and a point at m/z 0, which has no relative error.
        let (small, large) = (2f64.powi(-30), 2f64.powi(-29));
        let mut observed = ObservedMzError::default();
        observed.observe(&[100.0, 400.0, 0.0], &[100.0 + small, 400.0 - large, small]);
        assert_eq!(observed, ObservedMzError { max_abs: Some(large), max_rel_ppm: small / 100.0 * 1e6 });
        let lane = with_observed(SourceTally::new(true).block(), "tof-grid:5ppm", observed).1;
        let grid = Some((99.5, 1500.0));
        let entries = declared_mz_errors(&["zero-run-mask", "tof-grid:5ppm"], Some(&lane), None, grid);
        assert_eq!(entries.len(), 1, "{entries:?}");
        let e = &entries[0];
        assert_eq!((&e["encoding"], &e["basis"], e["max_rel_error_ppm"].as_f64()), (&json!("tof-grid:5ppm"), &json!("tolerance"), Some(5.0)));
        let (abs, rel) = (e["observed_max_abs_error"].as_f64().unwrap(), e["observed_max_rel_error_ppm"].as_f64().unwrap());
        // Recorded rounded up to six digits.
        assert!((large..large * 1.00001).contains(&abs) && (observed.max_rel_ppm..observed.max_rel_ppm * 1.00001).contains(&rel), "{e}");
        assert!(abs < e["max_abs_error"].as_f64().unwrap() && rel < 5.0);
        // A lane that measured the relative error only (the native SciEX fits), and one that
        // measured nothing: the tolerance alone.
        let relative = with_observed((BLOCK.to_string(), json!({})), "tof-grid:5ppm", ObservedMzError { max_abs: None, max_rel_ppm: 1.7534567 }).1;
        let e = &declared_mz_errors(&["tof-grid:5ppm"], Some(&relative), None, grid)[0];
        assert_eq!((e["observed_max_rel_error_ppm"].as_f64(), e.get("observed_max_abs_error")), (Some(1.75346), None), "{e}");
        let e = &declared_mz_errors(&["tof-grid:5ppm"], None, None, grid)[0];
        assert!(e.get("observed_max_rel_error_ppm").is_none(), "{e}");

        // SBA415: `ims_calibration.max_error_ppm`, and grid rows up to m/z 1700.
        let ppm = 0.6557159657616491;
        let e = &declared_mz_errors(&["bruker:mz-calibrant-omitted"], None, Some(ppm), Some((100.0, 1700.0)))[0];
        assert_eq!((&e["basis"], e["max_rel_error_ppm"].as_f64(), e["max_abs_error"].as_f64()), (&json!("bound"), Some(0.655716), Some(0.00111472)), "{e}");
        assert!(e["max_rel_error_ppm"].as_f64().unwrap() >= ppm && e["max_abs_error"].as_f64().unwrap() >= ppm * 1e-6 * 1700.0);
        let e = &declared_mz_errors(&["bruker:mz-calibrant-omitted"], None, Some(ppm), None)[0];
        assert_eq!((&e["basis"], &e["max_abs_error"]), (&json!("bound"), &Value::Null), "no grid row stored: {e}");
        // A row without the calibrant has no bound to state, and the chord never has.
        for t in ["bruker:mz-calibrant-omitted", "bruker:mz-calibration-chord", "shimadzu:coarse-mz"] {
            let e = &declared_mz_errors(&[t], None, if t.ends_with("chord") { Some(ppm) } else { None }, grid)[0];
            assert_eq!((&e["basis"], &e["max_abs_error"]), (&json!("not measured"), &Value::Null), "{t}: {e}");
        }
    }

    fn exact_block() -> Value {
        json!({
            "spectra_data": {
                "layout": "point", "source_points": 10, "stored_points": 10,
                "source_types": {"mz": ["float64"], "intensity": ["float32"]},
                "stored_types": {"mz": "float64", "intensity": "float32"},
            },
            "mz_error": [],
        })
    }

    /// `--lossless` passes an exact block with metadata-only transformations, and names each
    /// reason it fails for: a signal entry, a dropped point, a chunked facet, an m/z error, a
    /// narrowed column.
    #[test]
    fn the_lossless_check_fails_closed() {
        check_lossless(&exact_block(), &["mzml:dangling-reference-dropped", "chromatogram-time-to-minutes"]).unwrap();
        let fails = |block: &Value, t: &[&str], needle: &str| {
            let e = check_lossless(block, t).unwrap_err().to_string();
            assert!(e.contains(needle), "{e}");
        };
        fails(&exact_block(), &["sort-by-mz"], "declares sort-by-mz");
        let mut b = exact_block();
        b["spectra_data"]["stored_points"] = 9.into();
        fails(&b, &[], "stores 9 of the source's 10 points");
        let mut b = exact_block();
        b["spectra_data"]["layout"] = "chunked".into();
        b["mz_error"] = json!([{"encoding": "delta"}]);
        fails(&b, &[], "chunked layout");
        fails(&b, &[], "stored with delta");
        let mut b = exact_block();
        b["spectra_data"]["source_types"]["intensity"] = json!(["float32", "float64"]);
        fails(&b, &[], "stores intensity as float32 where the source declares float64");
        let mut b = exact_block();
        b["spectra_data"]["source_types"] = json!({"mz": ["float32"], "intensity": ["int32"]});
        b["spectra_data"]["stored_types"]["intensity"] = "int64".into();
        check_lossless(&b, &[]).unwrap();
        let mut b = exact_block();
        b["spectra_data"].as_object_mut().unwrap().remove("source_types");
        fails(&b, &[], "type is unknown");
        // Points handed over for a facet the archive does not hold.
        let mut b = exact_block();
        b["spectra_peaks"] = json!({"source_points": 5});
        fails(&b, &[], "spectra_peaks: the source or stored point count is unknown");
    }
}

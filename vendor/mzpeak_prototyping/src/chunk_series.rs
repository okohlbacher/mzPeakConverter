use std::collections::{HashMap, HashSet};
use std::ops::AddAssign;
use std::sync::Arc;

use arrow::array::{
    Array, ArrayBuilder, ArrayRef, ArrowPrimitiveType, AsArray, Float32Array, Float32Builder,
    Float64Array, Float64Builder, Int32Array, Int32Builder, Int64Array, Int64Builder,
    LargeListBuilder, PrimitiveArray, StructArray, StructBuilder, UInt8Array, UInt8Builder,
    UInt64Array, UInt64Builder,
};
use arrow::compute::kernels::nullif;
use arrow::datatypes::{
    DataType, Field, Fields, Float32Type, Float64Type, Int32Type, Int64Type, Schema, UInt8Type,
};
use itertools::Itertools;
use mzdata::params::CURIE;
use mzdata::prelude::ByteArrayView;
use mzdata::spectrum::{
    ArrayType, BinaryArrayMap, BinaryDataArrayType, DataArray, bindata::ArrayRetrievalError,
};

use bytemuck::Pod;

use num_traits::{Float, NumCast, ToPrimitive};

use crate::buffer_descriptors::{BufferOverrideTable, BufferPriority};
use crate::writer::StructVisitor;
use crate::{
    buffer_descriptors::BufferTransform,
    filter::{
        _skip_zero_runs_gen, RegressionDeltaModel, fill_nulls_for, is_zero_pair_mask,
        null_chunk_every_k, null_delta_decode, null_delta_encode,
    },
    peak_series::{
        BufferContext, BufferFormat, BufferName, array_to_arrow_type, data_array_to_arrow_array,
    },
    spectrum::AuxiliaryArray,
    writer::{CURIEBuilder, VisitorBase},
};

pub fn delta_decode<T: Float + Pod + AddAssign>(
    it: &[T],
    start_value: T,
    accumulator: &mut DataArray,
) -> usize {
    let mut state = start_value;
    accumulator.push(state).unwrap();
    for val in it.iter().copied() {
        state += val;
        accumulator.push(state).unwrap();
    }
    it.len() + 1
}

pub const NO_COMPRESSION: CURIE = mzdata::curie!(MS:1000576);
pub const DELTA_ENCODE: CURIE = mzdata::curie!(MS:1003089);
pub const NUMPRESS_LINEAR: CURIE = mzdata::curie!(MS:1002312);
pub const NUMPRESS_SLOF: CURIE = mzdata::curie!(MS:1002314);

/// Different methods for encoding chunks along a coordinate dimension
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ChunkingStrategy {
    /// Values are encoded as-is without any transformation. While this doesn't have any compression
    /// benefits, it provides compatibility for sparse data. The start and end values are included in
    /// the encoded data.
    Basic { chunk_size: f64 },
    /// Values are stored as deltas w.r.t. previous values. The first value stored is w.r.t. to the chunk
    /// starting value, but the starting value is not encoded in the chunk values array itself. Decoding
    /// of the chunk values requires the chunk start value be read from the chunk metadata.
    Delta { chunk_size: f64 },
    /// Values are stored using MS-Numpress linear compression. The entire chunk is encoded in a byte buffer
    /// which may not align with a multi-byte value type and must be stored in a dedicated byte array. The
    /// start and end values are included in the encoded chunk as well as the chunk metadata.
    NumpressLinear { chunk_size: f64 },
}

impl ChunkingStrategy {
    /// Convert the chunking stategy to a CURIE
    pub const fn as_curie(&self) -> CURIE {
        match self {
            Self::Basic { chunk_size: _ } => NO_COMPRESSION,
            Self::Delta { chunk_size: _ } => DELTA_ENCODE,
            Self::NumpressLinear { chunk_size: _ } => NUMPRESS_LINEAR,
        }
    }

    /// Given the name of the main axis for this chunk, generate all extra [`arrow::datatypes::Field`]
    /// instances needed to populate the schema to include this chunk type.
    pub fn extra_arrays(&self, main_axis_name: &BufferName) -> Vec<Field> {
        match self {
            ChunkingStrategy::Basic { chunk_size: _ } => vec![],
            ChunkingStrategy::Delta { chunk_size: _ } => vec![],
            ChunkingStrategy::NumpressLinear { chunk_size: _ } => {
                let name = main_axis_name
                    .clone()
                    .with_format(BufferFormat::ChunkTransform)
                    .with_transform(Some(BufferTransform::NumpressLinear));

                let bytes = Field::new(
                    name.to_string(),
                    DataType::LargeList(Arc::new(Field::new("item", DataType::UInt8, false))),
                    true,
                )
                .with_metadata(name.as_field_metadata());
                vec![bytes]
            }
        }
    }

    /// Encode any extra data arrays from this [`ArrowArrayChunk`] beyond the `chunk_values` array.
    ///
    /// This exists primarily to serve `Numpress` encoding right now, but future encodings might need
    /// it too.
    pub fn encode_extra_arrow(
        &self,
        main_axis_name: &BufferName,
        chunk: &ArrowArrayChunk,
        chunk_builder: &mut StructBuilder,
        schema: &Schema,
        visited: &mut HashSet<usize>,
    ) {
        match self {
            ChunkingStrategy::Basic { chunk_size: _ } => {}
            ChunkingStrategy::Delta { chunk_size: _ } => {}
            ChunkingStrategy::NumpressLinear { chunk_size: _ } => {
                let fields = self.extra_arrays(main_axis_name);
                let byte_col = &fields[0];
                let idx = schema
                    .fields()
                    .iter()
                    .position(|p| p.name() == byte_col.name())
                    .unwrap();

                if visited.contains(&idx) {
                    return;
                }
                visited.insert(idx);

                let b: &mut LargeListBuilder<Box<dyn ArrayBuilder>> =
                    chunk_builder.field_builder(idx).unwrap();
                let inner = b
                    .values()
                    .as_any_mut()
                    .downcast_mut::<UInt8Builder>()
                    .unwrap();
                if matches!(chunk.chunk_encoding, Self::NumpressLinear { chunk_size: _ }) {
                    let bytes: &UInt8Array = chunk.chunk_values.as_primitive();
                    inner.extend(bytes);
                    b.append(true);
                } else {
                    b.append_null();
                }
            }
        }
    }

    /// Get the step size along the main axis for the chunking strategy
    pub const fn chunk_size(&self) -> f64 {
        match self {
            ChunkingStrategy::Basic { chunk_size } => *chunk_size,
            ChunkingStrategy::Delta { chunk_size } => *chunk_size,
            ChunkingStrategy::NumpressLinear { chunk_size } => *chunk_size,
        }
    }

    /// Encode a chunk of an [`PrimitiveArray`] into minimal chunk start, end, and chunk values.
    ///
    /// Assumes the provided `array` has already been cut as a chunk of the desired width.
    ///
    /// # Note
    /// If the array is empty or all null, start and end will be 0.0 and chunk values may be empty.
    pub fn encode_arrow<T: ArrowPrimitiveType>(
        &self,
        array: &PrimitiveArray<T>,
    ) -> (f64, f64, ArrayRef)
    where
        T::Native: Float,
        PrimitiveArray<T>: From<Vec<Option<T::Native>>>,
    {
        // VENDORED PATCH (mzML2mzPeak backlog 999.19; W3 chunk_bounds_spectra_data + W5
        // chunk_bounds_chromatograms_data; group upstreaming with 999.1). The original computed
        // `start` and `end` from a SINGLE iterator: `it.next()` consumed the first non-null value,
        // then `it.next_back()` read the back of the REMAINING iterator. For a chunk that reduces to
        // one non-null value the iterator was already empty after `next()`, so `end` fell through to
        // 0.0 — yielding `chunk_start > chunk_end (= 0)` and the validator's "chunk start > end".
        // Read start and end from two INDEPENDENT iterators so a single-point chunk gives end == start.
        let start: f64 = array
            .iter()
            .flatten()
            .next()
            .map(|v| v.to_f64().unwrap_or(0.0))
            .unwrap_or(0.0);
        let end: f64 = array
            .iter()
            .flatten()
            .next_back()
            .map(|v| v.to_f64().unwrap_or(0.0))
            .unwrap_or(0.0);
        match self {
            ChunkingStrategy::Basic { chunk_size: _ } => (
                start,
                end,
                Arc::new(array.slice(1, array.len().saturating_sub(1)).clone()),
            ),
            ChunkingStrategy::Delta { chunk_size: _ } => {
                (start, end, Arc::new(null_delta_encode(array)))
            }
            ChunkingStrategy::NumpressLinear { chunk_size: _ } => {
                let bytes_of = if matches!(array.data_type(), DataType::Float64) {
                    let array: &PrimitiveArray<Float64Type> =
                        array.as_any().downcast_ref().unwrap();
                    DataArray::compress_numpress_linear(array.values()).unwrap()
                } else {
                    let values: Vec<_> = array
                        .iter()
                        .map(|v| v.and_then(|v| v.to_f64()).unwrap_or_default())
                        .collect();
                    DataArray::compress_numpress_linear(&values).unwrap()
                };
                let array = Arc::new(UInt8Array::from(bytes_of));
                (start, end, array)
            }
        }
    }

    /// Decode the encoded main axis of a chunk into a [`DataArray`].
    ///
    /// Requires the `start_value` of the chunk to provide context.
    ///
    /// ## Warning
    /// If the [`DataArray`] stored type is not bit-level compatible
    /// with the data type that `array` contains or is decoded into,
    /// the data will be meaningless.
    pub fn decode_arrow(
        &self,
        array: &ArrayRef,
        start_value: f64,
        end_value: f64,
        accumulator: &mut DataArray,
        delta_model: Option<&RegressionDeltaModel<f64>>,
    ) -> usize {
        if start_value == 0.0 && end_value == 0.0 {
            return 0;
        }
        macro_rules! decode_delta {
            ($array:ident, $dtype:ty, $native:ty, $debug:literal) => {{
                let it = $array.as_primitive::<$dtype>();
                if it.null_count() > 0 {
                    let decoded = null_delta_decode(it, start_value as $native);
                    if let Some(delta_model) = delta_model {
                        let values = fill_nulls_for(&decoded, delta_model);
                        accumulator.extend(&values).unwrap();
                        values.len()
                    } else {
                        log::debug!($debug);
                        accumulator.extend(decoded.values()).unwrap();
                        decoded.len()
                    }
                } else {
                    delta_decode(it.values(), start_value as $native, accumulator)
                }
            }};
        }

        match self {
            ChunkingStrategy::Basic { chunk_size: _ } => match array.data_type() {
                DataType::Float32 => {
                    let it = array.as_primitive::<Float32Type>();
                    if it.null_count() > 0 {
                        if let Some(model) = delta_model {
                            let it = fill_nulls_for(it, model);
                            accumulator.push(start_value as f32).unwrap();
                            accumulator.extend(&it).unwrap();
                        } else {
                            accumulator.push(start_value as f32).unwrap();
                            accumulator.extend(it.values()).unwrap();
                        }
                    } else {
                        accumulator.push(start_value as f32).unwrap();
                        accumulator.extend(it.values()).unwrap();
                    }
                    it.len() + 1
                }
                DataType::Float64 => {
                    let it = array.as_primitive::<Float64Type>();
                    if it.null_count() > 0 {
                        if let Some(model) = delta_model {
                            let it = fill_nulls_for(it, model);
                            accumulator.push(start_value as f64).unwrap();
                            accumulator.extend(&it).unwrap();
                        } else {
                            accumulator.push(start_value).unwrap();
                            accumulator.extend(it.values()).unwrap();
                        }
                    } else {
                        accumulator.push(start_value).unwrap();
                        accumulator.extend(it.values()).unwrap();
                    }
                    it.len() + 1
                }
                _ => panic!(
                    "Data type {:?} is not supported by basic decoding",
                    array.data_type()
                ),
            },
            ChunkingStrategy::Delta { chunk_size: _ } => match array.data_type() {
                DataType::Float32 => {
                    decode_delta!(
                        array,
                        Float32Type,
                        f32,
                        "f32 delta decoding contained nulls but no delta model provided"
                    )
                }
                DataType::Float64 => {
                    decode_delta!(
                        array,
                        Float64Type,
                        f64,
                        "f64 delta decoding contained nulls but no delta model provided"
                    )
                }
                _ => panic!(
                    "Data type {:?} is not supported by chunk decoding",
                    array.data_type()
                ),
            },
            ChunkingStrategy::NumpressLinear { chunk_size: _ } => match array.data_type() {
                DataType::UInt8 => {
                    let it = array.as_primitive::<UInt8Type>();
                    let buf = it.values();
                    let data: Float64Array = DataArray::decompress_numpress_linear(buf)
                        .unwrap()
                        .into_iter()
                        .map(|v| if v == 0.0 { None } else { Some(v) })
                        .collect();
                    if let Some(delta_model) = delta_model {
                        if data.null_count() > 0 {
                            let data = fill_nulls_for(&data, delta_model);
                            match accumulator.dtype() {
                                BinaryDataArrayType::Float64 => {
                                    accumulator.extend(&data).unwrap();
                                }
                                BinaryDataArrayType::Float32 => {
                                    for v in data {
                                        accumulator.push(v as f32).unwrap();
                                    }
                                }
                                _ => unimplemented!(),
                            }
                        } else {
                            accumulator.extend(data.values()).unwrap();
                        }
                    } else {
                        accumulator.extend(data.values()).unwrap();
                    }
                    data.len()
                }
                _ => panic!(
                    "Data type {:?} is not supported by numpress linear decoding",
                    array.data_type()
                ),
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BufferTransformEncoder(BufferTransform);

impl TryFrom<Option<BufferTransform>> for BufferTransformEncoder {
    type Error = <Self as TryFrom<BufferTransform>>::Error;

    fn try_from(value: Option<BufferTransform>) -> Result<Self, Self::Error> {
        match value {
            Some(value) => value.try_into(),
            None => Err(format!("Cannot convert from empty")),
        }
    }
}

impl TryFrom<BufferTransform> for BufferTransformEncoder {
    type Error = String;

    fn try_from(value: BufferTransform) -> Result<Self, Self::Error> {
        match value {
            BufferTransform::NumpressLinear
            | BufferTransform::NumpressSLOF
            | BufferTransform::NumpressPIC => Ok(Self(value)),
            BufferTransform::NullInterpolate
            | BufferTransform::NullZero
            | BufferTransform::SqrtMzFromTof
            | BufferTransform::LinearMz => Err(format!("{value:?} does not have an encoder")),
        }
    }
}

impl BufferTransformEncoder {
    pub fn to_buffer_name(&self, buffer_name: &BufferName) -> BufferName {
        match self.0 {
            BufferTransform::NumpressLinear => todo!(),
            BufferTransform::NumpressSLOF | BufferTransform::NumpressPIC => buffer_name
                .clone()
                .with_format(BufferFormat::ChunkTransform)
                .with_transform(Some(self.0)),
            _ => unimplemented!("{:?} does not have a buffer name", self.0),
        }
    }

    pub fn to_field(&self, buffer_name: &BufferName) -> Field {
        let buffer_name = self.to_buffer_name(buffer_name);
        match self.0 {
            BufferTransform::NumpressLinear => todo!(),
            BufferTransform::NumpressSLOF | BufferTransform::NumpressPIC => {
                let meta = buffer_name.as_field_metadata();
                let bytes = Field::new(
                    buffer_name.to_string(),
                    DataType::LargeList(Arc::new(Field::new("item", DataType::UInt8, false))),
                    true,
                )
                .with_metadata(meta);
                bytes
            }
            _ => unimplemented!("{:?} does not have a field conversion", self.0),
        }
    }

    pub fn visit_builder(
        &self,
        buffer_name: &BufferName,
        chunk: &ArrowArrayChunk,
        chunk_builder: &mut StructBuilder,
        schema: &Schema,
        visited: &mut HashSet<usize>,
    ) {
        // The buffer name's array is already the name of the field we want, we don't want to
        // use the full buffer name formatting again
        let f = self.to_field(buffer_name);
        let q = f.name();
        let idx = schema.fields().iter().position(|p| p.name() == q).unwrap();

        if visited.contains(&idx) {
            return;
        }
        visited.insert(idx);
        let b: &mut LargeListBuilder<Box<dyn ArrayBuilder>> =
            chunk_builder.field_builder(idx).unwrap();

        if let Some(chunk_segment) = chunk.arrays.get(buffer_name) {
            let inner = b
                .values()
                .as_any_mut()
                .downcast_mut::<UInt8Builder>()
                .unwrap();
            let bytes: &UInt8Array = chunk_segment.as_primitive();
            inner.extend(bytes);
            b.append(true);
        } else {
            b.append_null();
        }
    }

    pub fn encode_arrow(
        &self,
        _buffer_name: &BufferName,
        chunk_segment: &impl AsArray,
    ) -> ArrayRef {
        match self.0 {
            BufferTransform::NumpressLinear => todo!(),
            BufferTransform::NumpressPIC => {
                let mut bytes = Vec::new();
                if let Some(vals) = chunk_segment.as_primitive_opt::<Float32Type>() {
                    let vals = vals.values();
                    numpress_rs::encode_pic(&vals, &mut bytes).unwrap();
                } else if let Some(vals) = chunk_segment.as_primitive_opt::<Float64Type>() {
                    let vals = vals.values();
                    numpress_rs::encode_pic(&vals, &mut bytes).unwrap();
                } else {
                    todo!()
                }
                Arc::new(UInt8Array::from(bytes))
            }
            BufferTransform::NumpressSLOF => {
                let mut bytes = Vec::new();
                if let Some(vals) = chunk_segment.as_primitive_opt::<Float32Type>() {
                    let vals = vals.values();
                    let fp = numpress_rs::optimal_slof_fixed_point(&vals);
                    numpress_rs::encode_slof(&vals, &mut bytes, fp).unwrap();
                } else if let Some(vals) = chunk_segment.as_primitive_opt::<Float64Type>() {
                    let vals = vals.values();
                    let fp = numpress_rs::optimal_slof_fixed_point(&vals);
                    numpress_rs::encode_slof(&vals, &mut bytes, fp).unwrap();
                } else {
                    todo!()
                }
                let bytes = UInt8Array::from(bytes);
                Arc::new(bytes)
            }
            _ => unimplemented!("{:?} does not have an encoder", self.0),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BufferTransformDecoder(BufferTransform);

impl BufferTransformDecoder {
    pub fn decode(&self, buffer_name: &BufferName, array: &impl AsArray) -> ArrayRef {
        macro_rules! decoder {
            ($decoder:path) => {
                let data: &UInt8Array = array.as_primitive();
                match buffer_name.dtype {
                    BinaryDataArrayType::Float64 => {
                        let mut acc: Vec<f64> = Vec::new();
                        $decoder(data.values(), &mut acc).unwrap();
                        return Arc::new(Float64Array::from(acc));
                    }
                    BinaryDataArrayType::Float32 => {
                        let mut acc: Vec<f64> = Vec::new();
                        $decoder(data.values(), &mut acc).unwrap();
                        return Arc::new(Float32Array::from_iter_values(
                            acc.into_iter().map(|v| v as f32),
                        ));
                    }
                    BinaryDataArrayType::Int32 => {
                        let mut acc: Vec<f64> = Vec::new();
                        $decoder(data.values(), &mut acc).unwrap();
                        return Arc::new(Int32Array::from_iter_values(
                            acc.into_iter().map(|v| v as i32),
                        ));
                    }
                    BinaryDataArrayType::Int64 => {
                        let mut acc: Vec<f64> = Vec::new();
                        $decoder(data.values(), &mut acc).unwrap();
                        return Arc::new(Int64Array::from_iter_values(
                            acc.into_iter().map(|v| v as i64),
                        ));
                    }
                    _ => panic!("Cannot decode {:?} into {:?}", self.0, buffer_name.dtype),
                }
            };
        }
        match self.0 {
            BufferTransform::NumpressLinear => todo!(),
            BufferTransform::NumpressSLOF => {
                decoder!(numpress_rs::decode_slof);
            }
            BufferTransform::NumpressPIC => {
                decoder!(numpress_rs::decode_pic);
            }
            _ => unimplemented!("{:?} does not have a decoder", self.0),
        }
    }
}

impl TryFrom<Option<BufferTransform>> for BufferTransformDecoder {
    type Error = <Self as TryFrom<BufferTransform>>::Error;

    fn try_from(value: Option<BufferTransform>) -> Result<Self, Self::Error> {
        match value {
            Some(value) => value.try_into(),
            None => Err(format!("Cannot convert from empty")),
        }
    }
}

impl TryFrom<BufferTransform> for BufferTransformDecoder {
    type Error = String;

    fn try_from(value: BufferTransform) -> Result<Self, Self::Error> {
        match value {
            BufferTransform::NumpressLinear
            | BufferTransform::NumpressSLOF
            | BufferTransform::NumpressPIC => Ok(Self(value)),
            BufferTransform::NullInterpolate
            | BufferTransform::NullZero
            | BufferTransform::SqrtMzFromTof
            | BufferTransform::LinearMz => Err(format!("{value:?} does not have a decoder")),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ArrowArrayChunk {
    /// The index of source entity
    pub series_index: u64,
    series_time: Option<f32>,
    /// The starting coordinate of the chunk axis
    pub chunk_start: f64,
    /// The ending coordinate of the chunk axis
    pub chunk_end: f64,
    /// The buffer name for the main axis of the chunk
    pub chunk_axis: BufferName,
    /// The array values of the chunk, encoded using [`Self::chunk_values`] as [`ChunkingStrategy`]
    pub chunk_values: ArrayRef,
    /// The chunk encoding strategy applied to [`Self::chunk_values`].
    pub chunk_encoding: ChunkingStrategy,
    /// The rest of the arrays of covering this chunk
    pub arrays: HashMap<BufferName, ArrayRef>,
}

/// Returns `true` if `array_type` denotes a SIGNAL array — m/z (MS:1000514),
/// intensity (MS:1000515), or a `tof` flight-time array (carried as the generic
/// non-standard data array MS:1000786 with the value `"tof"`).
///
/// Signal arrays MUST live in the spectra_data / spectra_peaks facet, never spilled
/// into `auxiliary_arrays` (which lands in spectra_metadata). Spilling one is a bug;
/// [`guard_not_signal_array`] catches it.
pub(crate) fn is_signal_array_type(array_type: &ArrayType) -> bool {
    match array_type {
        ArrayType::MZArray | ArrayType::IntensityArray => true,
        ArrayType::NonStandardDataArray { name } => name.as_ref().eq_ignore_ascii_case("tof"),
        _ => false,
    }
}

/// Structural guard for the auxiliary-array spill path: a signal array (m/z, intensity,
/// or `tof`) must never reach `auxiliary_arrays`. If it does, loudly log and trip a
/// `debug_assert!` so the bug is caught in tests/CI, but do NOT panic in release — we
/// don't want to abort an otherwise-good conversion in the field.
pub(crate) fn guard_not_signal_array(array_type: &ArrayType) {
    if is_signal_array_type(array_type) {
        log::error!(
            "BUG: signal array {array_type:?} is being spilled to auxiliary_arrays \
             (metadata facet); signal arrays must live in spectra_data/spectra_peaks"
        );
        debug_assert!(
            false,
            "signal array {array_type:?} must not be spilled to auxiliary_arrays"
        );
    }
}

/// GATED timsTOF ims-compact "chunked" layout (opt-in `--ims-chunked`). Carries the TOF→m/z model
/// coefficients so the chunker can split an INTEGER `tof` main axis on TRUE m/z bin boundaries
/// (`floor(mz / width)`, `mz = (a + b·tof)²`) and record each chunk's `chunk_start`/`chunk_end` as
/// the tight min/max **m/z** of the points it holds — making the facet m/z-page-prunable while
/// storing the lossless integer `tof` (delta-encoded within each chunk).
///
/// This is ONLY ever `Some` on the gated ims-chunked path; every other caller passes `None`, so the
/// default float m/z-axis chunking (all other formats) is byte-for-byte unaffected.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TofMzBoundary {
    pub a: f64,
    pub b: f64,
}

impl TofMzBoundary {
    /// m/z = (a + b·tof)² — the same monotonic model used by the ims-compact reader.
    #[inline]
    pub fn mz(&self, tof: f64) -> f64 {
        let s = self.a + self.b * tof;
        s * s
    }
}

/// Split a sorted (ascending-`tof`) Int32 flight-time array into contiguous runs whose points fall
/// in the same m/z bin (`floor(mz / width)`). Empty bins are skipped implicitly (a run is created
/// only for the points actually present). Assumes a dense (non-null) `tof` array, which the
/// ims-compact writer always produces.
fn mz_boundary_steps(
    tof: &Int32Array,
    boundary: TofMzBoundary,
    width: f64,
) -> Vec<std::ops::Range<usize>> {
    let n = tof.len();
    if n == 0 {
        return Vec::new();
    }
    let bin = |t: i32| -> i64 { (boundary.mz(t as f64) / width).floor() as i64 };
    let mut steps = Vec::new();
    let mut start = 0usize;
    let mut cur = bin(tof.value(0));
    for i in 1..n {
        let bi = bin(tof.value(i));
        if bi != cur {
            steps.push(start..i);
            start = i;
            cur = bi;
        }
    }
    steps.push(start..n);
    steps
}

/// Encode a chunk's Int32 `tof` slice as [absolute_first, delta, delta, ...]: the first element is
/// the absolute TOF bin, the rest are increments from the previous point. Reconstruction is a plain
/// cumulative sum from element 0 (NOT from `chunk_start`, which holds m/z, not tof). Assumes dense
/// (non-null) input.
fn int32_chunk_delta(slice: &Int32Array) -> ArrayRef {
    let n = slice.len();
    let mut out: Vec<i32> = Vec::with_capacity(n);
    let mut prev = 0i32;
    for i in 0..n {
        let v = slice.value(i);
        if i == 0 {
            out.push(v);
        } else {
            out.push(v - prev);
        }
        prev = v;
    }
    Arc::new(Int32Array::from(out))
}

impl ArrowArrayChunk {
    /// A wrapper around [`Self::from_arrays`] and [`Self::to_struct_array`]
    ///
    /// See [`Self::from_arrays`] for parameter descriptions
    pub fn build(
        series_index: u64,
        series_time: Option<f32>,
        buffer_context: BufferContext,
        arrays: &BinaryArrayMap,
        encoding: ChunkingStrategy,
        overrides: &BufferOverrideTable,
        drop_zero_intensity: bool,
        nullify_zero_intensity: bool,
        fields: &Fields,
    ) -> Result<(Option<StructArray>, Vec<AuxiliaryArray>, usize), ArrayRetrievalError> {
        Self::build_with_axis(
            series_index,
            series_time,
            buffer_context,
            arrays,
            encoding,
            overrides,
            drop_zero_intensity,
            nullify_zero_intensity,
            fields,
            None,
            None,
        )
    }

    /// Like [`Self::build`], but allows overriding the chunk main axis (default:
    /// `buffer_context.main_axis()`, i.e. m/z) and supplying a [`TofMzBoundary`] to chunk an
    /// integer `tof` axis on m/z bins. Both extra arguments are `None` on every non-ims-chunked
    /// caller, so default behavior is unchanged.
    pub fn build_with_axis(
        series_index: u64,
        series_time: Option<f32>,
        buffer_context: BufferContext,
        arrays: &BinaryArrayMap,
        encoding: ChunkingStrategy,
        overrides: &BufferOverrideTable,
        drop_zero_intensity: bool,
        nullify_zero_intensity: bool,
        fields: &Fields,
        main_axis_override: Option<BufferName>,
        mz_boundary: Option<TofMzBoundary>,
    ) -> Result<(Option<StructArray>, Vec<AuxiliaryArray>, usize), ArrayRetrievalError> {
        let main_axis = main_axis_override
            .unwrap_or_else(|| buffer_context.main_axis())
            .with_priority(Some(BufferPriority::Primary));
        let (chunks, auxiliary_arrays, n_pts) = ArrowArrayChunk::from_arrays(
            series_index,
            series_time,
            main_axis,
            &arrays,
            encoding,
            overrides,
            drop_zero_intensity,
            nullify_zero_intensity,
            Some(fields),
            mz_boundary,
        )?;
        let chunks = if !chunks.is_empty() {
            let chunks = ArrowArrayChunk::to_struct_array(
                &chunks,
                buffer_context,
                &[
                    encoding,
                    ChunkingStrategy::Basic {
                        chunk_size: encoding.chunk_size(),
                    },
                ],
                series_time.is_some(),
            );
            Some(chunks)
        } else {
            None
        };
        Ok((chunks, auxiliary_arrays, n_pts))
    }

    /// Low level constructor for a single chunk record.
    ///
    /// Prefer [`ArrowArrayChunk::from_arrays`] for constructing a block of [`ArrowArrayChunk`]
    pub fn new(
        series_index: u64,
        series_time: Option<f32>,
        chunk_start: f64,
        chunk_end: f64,
        chunk_axis: BufferName,
        chunk_values: ArrayRef,
        chunk_encoding: ChunkingStrategy,
        arrays: HashMap<BufferName, ArrayRef>,
    ) -> Self {
        Self {
            series_index,
            series_time,
            chunk_start,
            chunk_end,
            chunk_axis,
            chunk_values,
            chunk_encoding,
            arrays,
        }
    }

    /// Convert a series of [`ArrowArrayChunk`] into a [`StructArray`]
    pub fn to_struct_array(
        chunks: &[Self],
        buffer_context: BufferContext,
        encodings: &[ChunkingStrategy],
        include_time: bool,
    ) -> StructArray {
        let this_schema = chunks[0].to_schema(buffer_context, encodings, include_time);
        let mut this_builder =
            StructBuilder::from_fields(this_schema.fields().clone(), chunks.len());

        this_builder.field_builders_mut()[4] =
            Box::new(CURIEBuilder::default()) as Box<dyn ArrayBuilder>;

        let time_index = if include_time {
            let q = buffer_context.time_field();
            let q = q.name();
            this_schema
                .fields()
                .iter()
                .position(|f| *f.name() == *q)
                .unwrap()
        } else {
            0
        };

        let mut visited: HashSet<usize> = HashSet::new();
        for chunk in chunks {
            let mut field_i = 0;
            visited.clear();
            let b = this_builder
                .field_builder::<UInt64Builder>(field_i)
                .unwrap();
            b.append_value(chunk.series_index);
            visited.insert(field_i);
            field_i += 1;
            let b = this_builder
                .field_builder::<Float64Builder>(field_i)
                .unwrap();
            b.append_value(chunk.chunk_start);
            visited.insert(field_i);
            field_i += 1;
            let b = this_builder
                .field_builder::<Float64Builder>(field_i)
                .unwrap();
            b.append_value(chunk.chunk_end);
            visited.insert(field_i);
            field_i += 1;

            let b: &mut LargeListBuilder<Box<dyn ArrayBuilder>> =
                this_builder.field_builder(field_i).unwrap();
            if matches!(
                chunk.chunk_encoding,
                ChunkingStrategy::NumpressLinear { chunk_size: _ }
            ) {
                b.append_null();
            } else {
                macro_rules! primitive_builder {
                    ($builder:ty) => {
                        let inner = b.values().as_any_mut().downcast_mut::<$builder>().unwrap();
                        inner.append_array(chunk.chunk_values.as_primitive());
                    };
                }
                match array_to_arrow_type(chunk.chunk_axis.dtype) {
                    DataType::Int32 => {
                        primitive_builder!(Int32Builder);
                    }
                    DataType::Int64 => {
                        primitive_builder!(Int64Builder);
                    }
                    DataType::Float32 => {
                        primitive_builder!(Float32Builder);
                    }
                    DataType::Float64 => {
                        primitive_builder!(Float64Builder);
                    }
                    DataType::LargeBinary => todo!(),
                    tp => {
                        unimplemented!(
                            "Array type {tp:?} from {:?} not supported",
                            chunk.chunk_axis.dtype
                        )
                    }
                }
                b.append(true);
            }
            visited.insert(field_i);
            field_i += 1;
            for encoding in encodings {
                encoding.encode_extra_arrow(
                    &chunk.chunk_axis,
                    &chunk,
                    &mut this_builder,
                    &this_schema,
                    &mut visited,
                );
            }

            let cb = this_builder.field_builder::<CURIEBuilder>(field_i).unwrap();
            let curie_of = chunk.chunk_encoding.as_curie();
            cb.append_value(&curie_of);
            visited.insert(field_i);
            field_i += 1;

            if include_time {
                let b = this_builder
                    .field_builder::<Float32Builder>(time_index)
                    .unwrap();
                b.append_option(chunk.series_time);
                visited.insert(time_index);
            }

            for (i, f) in this_schema.fields().iter().enumerate().skip(field_i) {
                if visited.contains(&i) {
                    continue;
                }
                if let Some(buf_name) = BufferName::from_field(chunk.chunk_axis.context, f.clone())
                    .map(|f| f.with_format(BufferFormat::ChunkSecondary))
                {
                    let b: &mut LargeListBuilder<Box<dyn ArrayBuilder>> =
                        this_builder.field_builder(i).unwrap();

                    if let Some(transform) =
                        BufferTransformEncoder::try_from(buf_name.transform).ok()
                    {
                        transform.visit_builder(
                            &buf_name,
                            chunk,
                            &mut this_builder,
                            &this_schema,
                            &mut visited,
                        );
                    } else {
                        if let Some(arr) = chunk.arrays.get(&buf_name) {
                            macro_rules! primitive_builder {
                                ($builder:ty) => {
                                    let inner =
                                        b.values().as_any_mut().downcast_mut::<$builder>().unwrap();
                                    inner.append_array(arr.as_primitive());
                                };
                            }
                            match array_to_arrow_type(buf_name.dtype) {
                                DataType::Int32 => {
                                    primitive_builder!(Int32Builder);
                                }
                                DataType::Int64 => {
                                    primitive_builder!(Int64Builder);
                                }
                                DataType::Float32 => {
                                    primitive_builder!(Float32Builder);
                                }
                                DataType::Float64 => {
                                    primitive_builder!(Float64Builder);
                                }
                                DataType::LargeBinary => todo!(),
                                tp => {
                                    unimplemented!(
                                        "Array type {tp:?} from {:?} not supported",
                                        buf_name.dtype
                                    )
                                }
                            }

                            b.append(true);
                        } else {
                            b.append_null();
                        }
                    }
                } else {
                    if !visited.contains(&i) {
                        panic!("A column was not visited: {}", f.name());
                    }
                }
                visited.insert(i);
            }
            this_builder.append(true);
        }
        this_builder.finish()
    }

    /// Construct an Arrow schema from this chunk.
    ///
    /// This schema must hold for just *this* block of chunks. It will
    /// be adapted by [`ChunkBuffers`](crate::writer::ChunkBuffers) to
    /// the file-level schema.
    pub fn to_schema(
        &self,
        buffer_context: BufferContext,
        encodings: &[ChunkingStrategy],
        include_time: bool,
    ) -> Schema {
        let base_name = self.chunk_axis.clone();
        let mut bounds_name = base_name.clone();
        bounds_name.dtype = BinaryDataArrayType::Float64;
        let (start, end) = bounds_name.make_bounds_fields().unwrap();
        let field_meta = base_name.as_field_metadata();
        let chunk_encoding_meta = base_name
            .clone()
            .with_format(BufferFormat::ChunkEncoding)
            .as_field_metadata();
        let mut fields_of = vec![
            buffer_context.index_field(),
            start,
            end,
            Field::new(
                base_name.to_string(),
                DataType::LargeList(Arc::new(Field::new(
                    "item",
                    array_to_arrow_type(base_name.dtype),
                    true,
                ))),
                true,
            )
            .with_metadata(field_meta)
            .into(),
            Field::new(
                "chunk_encoding",
                CURIEBuilder::default().as_struct_type(),
                true,
            )
            .with_metadata(chunk_encoding_meta)
            .into(),
        ];

        for buffer_name in self.arrays.keys().sorted() {
            if let Ok(transform) = BufferTransformEncoder::try_from(buffer_name.transform) {
                let f_of = transform.to_field(buffer_name);
                fields_of.push(Arc::new(f_of));
            } else {
                let f_of = buffer_name.to_field();
                let dtype = DataType::LargeList(Arc::new(Field::new(
                    "item",
                    f_of.data_type().clone(),
                    true,
                )));
                fields_of.push(Arc::new((*f_of).clone().with_data_type(dtype)));
            }
        }

        for enc in encodings.iter() {
            fields_of.extend(enc.extra_arrays(&base_name).into_iter().map(Arc::new));
        }
        if include_time {
            fields_of.push(buffer_context.time_field());
        }
        Schema::new(fields_of)
    }

    /// Construct a series of [`ArrowArrayChunk`]s from a [`BinaryArrayMap`], using a specific array indicated by
    /// [`BufferName`] as the main axis, split and encoded using `chunk_encoding`. This may include a set of
    /// transforms according to `drop_zero_intensity`, `nullify_zero_intensity`.
    ///
    /// If `fields` is provided, any array not found in it will be returned as a [`AuxiliaryArray`].
    pub fn from_arrays(
        series_index: u64,
        series_time: Option<f32>,
        main_axis: BufferName,
        arrays: &BinaryArrayMap,
        chunk_encoding: ChunkingStrategy,
        overrides: &BufferOverrideTable,
        drop_zero_intensity: bool,
        nullify_zero_intensity: bool,
        fields: Option<&Fields>,
        mz_boundary: Option<TofMzBoundary>,
    ) -> Result<(Vec<Self>, Vec<AuxiliaryArray>, usize), ArrayRetrievalError> {
        let mut chunks = Vec::new();

        let mut arrow_arrays = Vec::new();
        let mut intensity_idx = None;
        let mut mz_idx = None;

        let mut auxiliary_arrays = Vec::new();

        let main_axis = overrides
            .map(&main_axis)
            .with_format(BufferFormat::Chunk)
            .with_priority(Some(BufferPriority::Primary))
            .with_sorting_rank(Some(0));

        // Ensure that non-hashing properties of [`BufferName`] propagate from the
        // schema down to physical arrays constructed. Also propagate any transformations
        // in the schema, which *are* hash-dependent but considered safe here.
        let mut fields_of = BufferOverrideTable::default();
        if let Some(fields) = fields.as_ref() {
            for f in fields.iter() {
                if let Some(f) = BufferName::from_field(main_axis.context, f.clone()) {
                    fields_of.insert(f.clone(), f.clone());
                    if f.transform.is_some() {
                        fields_of.insert(f.clone().with_transform(None), f.clone());
                    }
                    // Physical arrays frequently arrive with no unit annotation (e.g. mzML
                    // intensity carries Unit::Unknown) while the schema field declares the
                    // canonical unit. BufferName equality/hash include the unit, so without a
                    // unit-stripped alias the lookup misses, the schema field's Primary priority
                    // never propagates, and the array is wrongly spilled to a huge *uncompressed*
                    // auxiliary array in spectra_metadata instead of its own data-facet column.
                    if !matches!(f.unit, mzdata::params::Unit::Unknown) {
                        fields_of
                            .insert(f.clone().with_unit(mzdata::params::Unit::Unknown), f.clone());
                    }
                }
            }
        }

        let empty_main_axis = match arrays.get(&main_axis.array_type) {
            Some(v) => v.raw_len() == 0,
            None => true,
        };

        if empty_main_axis {
            for arr in arrays
                .iter()
                .filter_map(|(_, arr)| (arr.raw_len() > 0).then(|| arr))
            {
                guard_not_signal_array(&arr.name);
                auxiliary_arrays.push(AuxiliaryArray::from_data_array(arr)?);
            }
            return Ok((Vec::new(), auxiliary_arrays, 0));
        }

        for (_, arr) in arrays.iter() {
            let name = BufferName::from_data_array(main_axis.context, arr);
            let buffer_name0 = if name.array_type == main_axis.array_type {
                main_axis.clone().with_format(BufferFormat::Chunk)
            } else {
                overrides
                    .map(&name)
                    .with_format(BufferFormat::ChunkSecondary)
            };
            let buffer_name = fields_of.map(&buffer_name0);

            if let Some(fields) = fields {
                let field_name = if let Ok(transform) =
                    BufferTransformEncoder::try_from(buffer_name.transform)
                {
                    transform.to_field(&buffer_name).name().clone()
                } else {
                    buffer_name.to_field().name().clone()
                };
                // If the buffer isn't in the fields for this chunk schema, skip it and store an auxiliary array.
                if !fields.find(&field_name).is_some() && buffer_name != main_axis {
                    log::debug!("Skipping {field_name} from {arr:?}, not in schema: {fields:?}",);
                    guard_not_signal_array(&arr.name);
                    auxiliary_arrays.push(AuxiliaryArray::from_data_array(arr)?);
                    continue;
                }
            }
            // Index into `arrow_arrays` (the kept/filtered output), not the source map:
            // schema-skipped arrays are spilled to auxiliary and never pushed here.
            if matches!(buffer_name.array_type, ArrayType::IntensityArray) {
                intensity_idx = Some(arrow_arrays.len());
            } else if matches!(buffer_name.array_type, ArrayType::MZArray) {
                mz_idx = Some(arrow_arrays.len());
            }
            let array = data_array_to_arrow_array(&buffer_name, arr)?;
            arrow_arrays.push((buffer_name, array));
        }

        if let Some(intensity_idx) = intensity_idx {
            let (intensity_name, intensity_array) = arrow_arrays.get(intensity_idx).unwrap();
            if drop_zero_intensity {
                let (kept_indices, n) = match array_to_arrow_type(intensity_name.dtype) {
                    DataType::Float32 => {
                        let intensity_array = intensity_array.as_primitive::<Float32Type>();
                        (_skip_zero_runs_gen(&intensity_array), intensity_array.len())
                    }
                    DataType::Float64 => {
                        let intensity_array = intensity_array.as_primitive::<Float64Type>();
                        (_skip_zero_runs_gen(&intensity_array), intensity_array.len())
                    }
                    DataType::Int32 => {
                        let intensity_array = intensity_array.as_primitive::<Int32Type>();
                        (_skip_zero_runs_gen(&intensity_array), intensity_array.len())
                    }
                    DataType::Int64 => {
                        let intensity_array = intensity_array.as_primitive::<Int64Type>();
                        (_skip_zero_runs_gen(&intensity_array), intensity_array.len())
                    }
                    _ => {
                        unimplemented!("{}", intensity_name)
                    }
                };
                let kept_indices: UInt64Array = kept_indices.into();
                for (_, v) in arrow_arrays.iter_mut() {
                    if v.len() != n {
                        continue;
                    }
                    *v = arrow::compute::take(v, &kept_indices, None).unwrap();
                }
            }

            if let Some(mz_idx) = mz_idx {
                if nullify_zero_intensity {
                    let (intensity_name, intensity_array) =
                        arrow_arrays.get(intensity_idx).unwrap();
                    let (masked, _) = match array_to_arrow_type(intensity_name.dtype) {
                        DataType::Float32 => {
                            let intensity_array = intensity_array.as_primitive::<Float32Type>();
                            (is_zero_pair_mask(&intensity_array), intensity_array.len())
                        }
                        DataType::Float64 => {
                            let intensity_array = intensity_array.as_primitive::<Float64Type>();
                            (is_zero_pair_mask(&intensity_array), intensity_array.len())
                        }
                        DataType::Int32 => {
                            let intensity_array = intensity_array.as_primitive::<Int32Type>();
                            (is_zero_pair_mask(&intensity_array), intensity_array.len())
                        }
                        DataType::Int64 => {
                            let intensity_array = intensity_array.as_primitive::<Int64Type>();
                            (is_zero_pair_mask(&intensity_array), intensity_array.len())
                        }
                        _ => {
                            unimplemented!("{}", intensity_name)
                        }
                    };

                    let (_, intensities) = arrow_arrays.get_mut(intensity_idx).unwrap();
                    *intensities = nullif::nullif(&intensities.clone(), &masked).unwrap();

                    let (_, mzs) = arrow_arrays.get_mut(mz_idx).unwrap();
                    *mzs = nullif::nullif(&mzs.clone(), &masked).unwrap();
                }
            }
        }

        let (_, main_axis_array) = match arrow_arrays.iter().find(|(k, _)| *k == main_axis) {
            Some(x) => x,
            None => {
                log::warn!(
                    "Primary axis array is missing ({main_axis}) for {series_index} post-conversion"
                );
                return Ok((Vec::new(), Vec::new(), 0));
            }
        };

        let n_pts = main_axis_array.len();

        let main_axis = main_axis.clone().with_format(BufferFormat::Chunk);

        // GATED ims-chunked path: chunk an integer `tof` main axis on TRUE m/z bin boundaries and
        // delta-encode `tof` within each chunk. Only taken when `mz_boundary` is `Some` (the
        // opt-in timsTOF `--ims-chunked` layout); the default float m/z path is the `else` below.
        if let Some(boundary) = mz_boundary {
            let tof: &Int32Array = main_axis_array.as_primitive::<Int32Type>();
            let width = chunk_encoding.chunk_size();
            let steps = mz_boundary_steps(tof, boundary, width);
            for step in steps {
                let (s, e) = (step.start, step.end);
                let slice_dyn = main_axis_array.slice(s, e - s);
                let slice: &Int32Array = slice_dyn.as_primitive::<Int32Type>();
                // Tight m/z bounds for the chunk: min/max m/z over its (tof-sorted) points.
                let m0 = boundary.mz(slice.value(0) as f64);
                let m1 = boundary.mz(slice.value(slice.len() - 1) as f64);
                let (chunk_start, chunk_end) = (m0.min(m1), m0.max(m1));
                let chunk_values = int32_chunk_delta(slice);

                let mut chunk_arrays: HashMap<BufferName, ArrayRef> = Default::default();
                for (k, v) in arrow_arrays
                    .iter()
                    .filter(|(k, _)| k.array_type != main_axis.array_type)
                {
                    let k = k.clone().with_format(BufferFormat::ChunkSecondary);
                    let v = v.slice(s, e - s);
                    if let Ok(transform) = BufferTransformEncoder::try_from(k.transform) {
                        let vi = transform.encode_arrow(&k, &v);
                        chunk_arrays.insert(k, vi);
                    } else {
                        chunk_arrays.insert(k, v);
                    }
                }

                chunks.push(Self::new(
                    series_index,
                    series_time,
                    chunk_start,
                    chunk_end,
                    main_axis.clone(),
                    chunk_values,
                    chunk_encoding,
                    chunk_arrays,
                ));
            }
            return Ok((chunks, auxiliary_arrays, n_pts));
        }

        let steps = match array_to_arrow_type(main_axis.dtype) {
            DataType::Float32 => null_chunk_every_k(
                main_axis_array.as_primitive::<Float32Type>(),
                NumCast::from(chunk_encoding.chunk_size()).unwrap(),
            ),
            DataType::Float64 => null_chunk_every_k(
                main_axis_array.as_primitive::<Float64Type>(),
                NumCast::from(chunk_encoding.chunk_size()).unwrap(),
            ),
            _ => unimplemented!("{}", main_axis),
        };

        for step in steps {
            let slice = main_axis_array.slice(step.start, step.end - step.start);
            let (chunk_start, chunk_end, chunk_values) = match array_to_arrow_type(main_axis.dtype)
            {
                DataType::Float32 => {
                    chunk_encoding.encode_arrow(slice.as_primitive::<Float32Type>())
                }
                DataType::Float64 => {
                    chunk_encoding.encode_arrow(slice.as_primitive::<Float64Type>())
                }
                _ => unimplemented!("{}", main_axis),
            };

            let mut chunk_arrays: HashMap<BufferName, ArrayRef> = Default::default();
            for (k, v) in arrow_arrays
                .iter()
                .filter(|(k, _)| k.array_type != main_axis.array_type)
            {
                let k = k.clone().with_format(BufferFormat::ChunkSecondary);
                let v = v.slice(step.start, step.end - step.start);
                if let Ok(transform) = BufferTransformEncoder::try_from(k.transform) {
                    let vi = transform.encode_arrow(&k, &v);
                    chunk_arrays.insert(k, vi);
                } else {
                    chunk_arrays.insert(k, v);
                }
            }

            chunks.push(Self::new(
                series_index,
                series_time,
                chunk_start,
                chunk_end,
                main_axis.clone(),
                chunk_values,
                chunk_encoding,
                chunk_arrays,
            ));
        }

        Ok((chunks, auxiliary_arrays, n_pts))
    }
}

#[cfg(test)]
mod test {
    use std::{
        fs,
        io::{self, prelude::*},
    };

    use crate::filter::{MZDeltaModel, drop_where_column_is_zero_run, nullify_at_zero_pair};

    use super::*;
    use arrow::array::RecordBatch;
    use mzdata::{MZReader, params::Unit, prelude::*};

    /// B.1 structural guard: the signal-array classifier flags m/z, intensity, and `tof`
    /// (a non-standard array), but not benign columns like time or a mobility array.
    #[test]
    fn signal_array_classifier() {
        assert!(is_signal_array_type(&ArrayType::MZArray));
        assert!(is_signal_array_type(&ArrayType::IntensityArray));
        assert!(is_signal_array_type(&ArrayType::nonstandard("tof".to_string())));
        assert!(is_signal_array_type(&ArrayType::nonstandard("TOF".to_string())));
        // Not signal: a different non-standard array, time, mobility.
        assert!(!is_signal_array_type(&ArrayType::nonstandard("noise".to_string())));
        assert!(!is_signal_array_type(&ArrayType::TimeArray));
        assert!(!is_signal_array_type(&ArrayType::MeanIonMobilityArray));
    }

    /// The guard must trip (via its `debug_assert!`) when a signal array would be spilled
    /// to auxiliary_arrays. The `debug_assert!` is compiled out in `--release` (where
    /// `debug-assertions = false`), so this test only runs in debug builds; in release the
    /// guard degrades to a loud `log::error!` (intentionally non-fatal). `signal_array_classifier`
    /// above is the profile-independent contract test.
    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "must not be spilled to auxiliary_arrays")]
    fn guard_fires_on_intensity() {
        guard_not_signal_array(&ArrayType::IntensityArray);
    }

    fn load_chunking_data() -> io::Result<Vec<f64>> {
        let reader = io::BufReader::new(fs::File::open("test/data/chunking_mzs.txt")?);

        let mut mzs: Vec<f64> = Vec::new();
        for line in reader.lines().flatten() {
            if line.is_empty() {
                continue;
            }
            mzs.push(line.parse().unwrap());
        }

        Ok(mzs)
    }

    /// VENDORED PATCH regression (mzML2mzPeak backlog 999.19; W3/W5). A chunk that reduces to a
    /// single non-null value must report `chunk_end == chunk_start`, never 0.0. Before the
    /// two-independent-iterators fix the shared iterator was exhausted by `next()`, so `next_back()`
    /// fell through to 0.0 → `chunk_start > chunk_end (= 0)` (the validator's "chunk start > end").
    #[test]
    fn test_encode_arrow_chunk_end_never_zero() {
        use arrow::array::Float64Array;
        let arr = Float64Array::from(vec![Some(3016.73_f64)]);
        let (s, e, _) = ChunkingStrategy::Basic { chunk_size: 50.0 }.encode_arrow(&arr);
        assert!((s - 3016.73).abs() < 1e-9, "Basic start wrong: {s}");
        assert!((e - s).abs() < 1e-9, "Basic: single-point chunk_end must equal start: start={s} end={e}");
        let (s, e, _) = ChunkingStrategy::Delta { chunk_size: 50.0 }.encode_arrow(&arr);
        assert!((e - s).abs() < 1e-9, "Delta: single-point chunk_end must equal start: start={s} end={e}");
        let (s, e, _) = ChunkingStrategy::NumpressLinear { chunk_size: 50.0 }.encode_arrow(&arr);
        assert!((e - s).abs() < 1e-9, "NumpressLinear: single-point chunk_end must equal start: start={s} end={e}");
        // null-padded single value: same invariant (start and end skip the nulls independently)
        let arr2 = Float64Array::from(vec![None, Some(1171.8_f64), None]);
        let (s2, e2, _) = ChunkingStrategy::NumpressLinear { chunk_size: 50.0 }.encode_arrow(&arr2);
        assert!((s2 - 1171.8).abs() < 1e-9 && (e2 - s2).abs() < 1e-9, "null-padded single point: start={s2} end={e2}");
    }

    #[test]
    fn test_chunking() -> io::Result<()> {
        let mzs = load_chunking_data()?;
        let mzs = Float64Array::from(mzs);
        let intervals = null_chunk_every_k(&mzs, 10.0);

        let mut last = 0.0;
        for iv in intervals.iter() {
            let vs = &mzs.values()[iv.start..iv.end];
            let term = vs.last().copied().unwrap();
            assert!(
                (term - 1.0) > last,
                "{vs:?} was not more than 9 away from {last}"
            );
            last = term;
        }
        Ok(())
    }

    fn get_arrays_from_mzml() -> io::Result<BinaryArrayMap> {
        let mut reader = MZReader::open_path("small.mzML")?;
        let spec = reader.get_spectrum_by_index(0).unwrap();
        Ok(spec.arrays.clone().unwrap())
    }

    #[test]
    fn test_encode_arrow_drop_zeros() -> io::Result<()> {
        let arrays = get_arrays_from_mzml()?;
        let target = BufferName::new(
            BufferContext::Spectrum,
            ArrayType::MZArray,
            BinaryDataArrayType::Float32,
        );

        let (chunks, _, _) = ArrowArrayChunk::from_arrays(
            0,
            None,
            target,
            &arrays,
            ChunkingStrategy::Delta { chunk_size: 50.0 },
            &BufferOverrideTable::default(),
            true,
            false,
            None,
            None,
        )?;

        for chunk in chunks.iter() {
            let n = chunk.chunk_values.len();
            for (_, v) in chunk.arrays.iter() {
                assert_eq!(v.len(), n + 1);
            }
        }

        let rendered = ArrowArrayChunk::to_struct_array(
            &chunks,
            BufferContext::Spectrum,
            &[
                ChunkingStrategy::Basic { chunk_size: 50.0 },
                ChunkingStrategy::Delta { chunk_size: 50.0 },
            ],
            false,
        );

        let f = rendered.column_by_name("mz_chunk_start").unwrap();
        assert_eq!(f.data_type().clone(), DataType::Float64);
        let f = rendered.column_by_name("mz_chunk_end").unwrap();
        assert_eq!(f.data_type().clone(), DataType::Float64);

        let f = rendered.column_by_name("mz_chunk_values").unwrap();
        assert_eq!(
            f.data_type().clone(),
            DataType::LargeList(Arc::new(Field::new("item", DataType::Float32, true)))
        );
        assert_eq!(f.len(), 36);
        let k = f
            .as_list::<i64>()
            .iter()
            .map(|a| a.unwrap().len())
            .sum::<usize>();
        assert_eq!(k, 13553);

        let f = rendered.column_by_name("chunk_encoding").unwrap();
        assert_eq!(f.data_type().clone(), DataType::Utf8);

        let f = rendered.column_by_name("intensity_f32_dc").unwrap();
        assert_eq!(
            f.data_type().clone(),
            DataType::LargeList(Arc::new(Field::new("item", DataType::Float32, true)))
        );
        assert_eq!(f.len(), 36);
        let k = f
            .as_list::<i64>()
            .iter()
            .map(|a| a.unwrap().len())
            .sum::<usize>();
        assert_eq!(k, 13589);

        Ok(())
    }

    #[test]
    fn test_encode_arrow_drop_zeros_null() -> io::Result<()> {
        let mut arrays = get_arrays_from_mzml()?;
        let arr = arrays.get_mut(&ArrayType::MZArray).unwrap();
        arr.store_as(BinaryDataArrayType::Float64)?;

        let mzs = arrays.mzs()?;
        let intens: Vec<f64> = arrays
            .intensities()?
            .iter()
            .map(|w| (*w).sqrt() as f64)
            .collect();

        let betas = crate::filter::select_delta_model(&mzs, Some(&intens));
        let delta_model = RegressionDeltaModel::<f64>::from_f64_iter(betas.into_iter());

        let target = BufferName::new(
            BufferContext::Spectrum,
            ArrayType::MZArray,
            BinaryDataArrayType::Float64,
        );

        let (chunks, _, _) = ArrowArrayChunk::from_arrays(
            0,
            None,
            target,
            &arrays,
            ChunkingStrategy::Delta { chunk_size: 50.0 },
            &BufferOverrideTable::default(),
            true,
            true,
            None,
            None,
        )?;

        for chunk in chunks.iter() {
            for (_, v) in chunk.arrays.iter() {
                assert_eq!(v.len(), chunk.arrays.values().next().unwrap().len());
            }
        }

        let rendered = ArrowArrayChunk::to_struct_array(
            &chunks,
            BufferContext::Spectrum,
            &[
                ChunkingStrategy::Basic { chunk_size: 50.0 },
                ChunkingStrategy::Delta { chunk_size: 50.0 },
            ],
            false,
        );

        let start_values = rendered.column(1).as_primitive::<Float64Type>();
        let end_values = rendered.column(2).as_primitive::<Float64Type>();
        let chunk_values = rendered.column(3).as_list::<i64>();
        let intensity_values = rendered.column(5).as_list::<i64>();

        let intensity_values: Vec<f32> = intensity_values
            .iter()
            .flatten()
            .map(|a| {
                a.as_primitive::<Float32Type>()
                    .iter()
                    .map(|v| v.unwrap_or_default())
                    .collect::<Vec<_>>()
            })
            .flatten()
            .collect();

        let mut accumulator =
            DataArray::from_name_and_type(&ArrayType::MZArray, BinaryDataArrayType::Float64);
        for ((start_value, end_value), chunk_vals) in start_values
            .iter()
            .flatten()
            .zip(end_values.iter().flatten())
            .zip(chunk_values.iter().flatten())
        {
            ChunkingStrategy::Delta { chunk_size: 50.0 }.decode_arrow(
                &chunk_vals,
                start_value as f64,
                end_value as f64,
                &mut accumulator,
                Some(&delta_model),
            );
        }

        assert_eq!(accumulator.data_len()?, intensity_values.len());

        let mz_array = Float64Array::from_iter_values(mzs.iter().copied());
        let intensity_array = Float32Array::from_iter_values(intens.iter().map(|v| *v as f32));
        let mini_schema = Arc::new(Schema::new(vec![
            Arc::new(Field::new("mz_array", DataType::Float64, true)),
            Arc::new(Field::new("intensity_array", DataType::Float32, true)),
        ]));

        let batch = RecordBatch::try_new(
            mini_schema.clone(),
            vec![Arc::new(mz_array) as ArrayRef, Arc::new(intensity_array)],
        )
        .unwrap();

        let trimmed_batch1 = drop_where_column_is_zero_run(&batch, 1).unwrap();
        let trimmed_batch = nullify_at_zero_pair(&trimmed_batch1, 1, &[0, 1]).unwrap();

        assert_eq!(trimmed_batch.num_rows(), accumulator.data_len()?);

        let mz_acc = accumulator.to_f64()?;
        let mz_ref = trimmed_batch.column(0).as_primitive::<Float64Type>();
        let mz_ref = crate::filter::fill_nulls_for(mz_ref, &delta_model);

        for ((i, a), b) in mz_acc
            .iter()
            .copied()
            .enumerate()
            .zip(mz_ref.iter().copied())
        {
            if intensity_values[i] == 0.0 {
                assert!(
                    (a - b).abs() < 1e-6,
                    "{i}: {} {a} - {b} = {}",
                    intensity_values.get(i).unwrap(),
                    a - b
                );
            } else {
                assert_eq!(
                    a,
                    b,
                    "{i}: {} {a} - {b} = {}",
                    intensity_values.get(i).unwrap(),
                    a - b
                );
            }
        }

        Ok(())
    }

    #[test]
    fn test_encode_arrow_transform() -> io::Result<()> {
        let arrays = get_arrays_from_mzml()?;
        let target = BufferName::new(
            BufferContext::Spectrum,
            ArrayType::MZArray,
            BinaryDataArrayType::Float32,
        );

        let intensity_name = BufferName::new(
            BufferContext::Spectrum,
            ArrayType::IntensityArray,
            BinaryDataArrayType::Float32,
        )
        .with_unit(Unit::DetectorCounts);
        let intensity_name_tfm = intensity_name
            .clone()
            .with_transform(Some(BufferTransform::NumpressSLOF));
        let overrides = BufferOverrideTable::from_iter(vec![(intensity_name, intensity_name_tfm)]);

        let (chunks, _, _) = ArrowArrayChunk::from_arrays(
            0,
            None,
            target,
            &arrays,
            ChunkingStrategy::Delta { chunk_size: 50.0 },
            &overrides,
            false,
            false,
            None,
            None,
        )?;

        let rendered = ArrowArrayChunk::to_struct_array(
            &chunks,
            BufferContext::Spectrum,
            &[
                ChunkingStrategy::Basic { chunk_size: 50.0 },
                ChunkingStrategy::Delta { chunk_size: 50.0 },
            ],
            false,
        );

        assert!(
            rendered
                .column_by_name("intensity_f32_dc_numpress_slof_bytes")
                .is_some()
        );

        Ok(())
    }

    #[test]
    fn test_encode_arrow_numpress_linear() -> io::Result<()> {
        let arrays = get_arrays_from_mzml()?;
        let target = BufferName::new(
            BufferContext::Spectrum,
            ArrayType::MZArray,
            BinaryDataArrayType::Float32,
        );

        let intensity_name = BufferName::new(
            BufferContext::Spectrum,
            ArrayType::IntensityArray,
            BinaryDataArrayType::Float32,
        )
        .with_unit(Unit::DetectorCounts);
        let intensity_name_tfm = intensity_name
            .clone()
            .with_transform(Some(BufferTransform::NumpressSLOF));
        let overrides = BufferOverrideTable::from_iter(vec![(intensity_name, intensity_name_tfm)]);

        let (chunks, _, _) = ArrowArrayChunk::from_arrays(
            0,
            None,
            target,
            &arrays,
            ChunkingStrategy::NumpressLinear { chunk_size: 50.0 },
            &overrides,
            false,
            false,
            None,
            None,
        )?;

        let rendered = ArrowArrayChunk::to_struct_array(
            &chunks,
            BufferContext::Spectrum,
            &[
                ChunkingStrategy::Basic { chunk_size: 50.0 },
                ChunkingStrategy::NumpressLinear { chunk_size: 50.0 },
            ],
            false,
        );
        assert!(
            rendered
                .column_by_name("mz_numpress_linear_bytes")
                .is_some()
        );

        assert!(
            rendered
                .column_by_name("intensity_f32_dc_numpress_slof_bytes")
                .is_some()
        );

        let starts = rendered
            .column_by_name("mz_chunk_start")
            .unwrap()
            .as_primitive::<Float64Type>();
        let ends = rendered
            .column_by_name("mz_chunk_end")
            .unwrap()
            .as_primitive::<Float64Type>();
        let bytes_array_list = rendered
            .column_by_name("mz_numpress_linear_bytes")
            .unwrap()
            .as_list::<i64>();
        let block = bytes_array_list.value(0);
        let strategy = ChunkingStrategy::NumpressLinear { chunk_size: 50.0 };
        let mut acc =
            DataArray::from_name_and_type(&ArrayType::MZArray, BinaryDataArrayType::Float64);

        strategy.decode_arrow(&block, starts.value(0), ends.value(0), &mut acc, None);

        assert_eq!(acc.data_len().unwrap(), 1054);
        Ok(())
    }

    #[test]
    fn test_encode_arrow() -> io::Result<()> {
        let arrays = get_arrays_from_mzml()?;
        let target = BufferName::new(
            BufferContext::Spectrum,
            ArrayType::MZArray,
            BinaryDataArrayType::Float32,
        )
        .with_unit(Unit::MZ);

        let (chunks, _, _) = ArrowArrayChunk::from_arrays(
            0,
            None,
            target,
            &arrays,
            ChunkingStrategy::Delta { chunk_size: 50.0 },
            &Default::default(),
            false,
            false,
            None,
            None,
        )?;

        let rendered = ArrowArrayChunk::to_struct_array(
            &chunks,
            BufferContext::Spectrum,
            &[
                ChunkingStrategy::Basic { chunk_size: 50.0 },
                ChunkingStrategy::Delta { chunk_size: 50.0 },
            ],
            true,
        );

        let names = rendered.column_names();
        assert!(names.contains(&"spectrum_time"));

        let f = rendered.column_by_name("mz_chunk_start").unwrap();
        assert_eq!(f.data_type().clone(), DataType::Float64);
        let f = rendered.column_by_name("mz_chunk_end").unwrap();
        assert_eq!(f.data_type().clone(), DataType::Float64);
        let f = rendered.column_by_name("mz_chunk_values").unwrap();
        assert_eq!(
            f.data_type().clone(),
            DataType::LargeList(Arc::new(Field::new("item", DataType::Float32, true)))
        );

        assert_eq!(f.len(), 36);
        let k = f
            .as_list::<i64>()
            .iter()
            .map(|a| a.unwrap().len())
            .sum::<usize>();
        assert_eq!(k, 19877);

        let f = rendered.column_by_name("chunk_encoding").unwrap();
        assert_eq!(f.data_type().clone(), DataType::Utf8);

        let f = rendered.column_by_name("intensity_f32_dc").unwrap();
        assert_eq!(
            f.data_type().clone(),
            DataType::LargeList(Arc::new(Field::new("item", DataType::Float32, true)))
        );
        assert_eq!(f.len(), 36);
        let k = f
            .as_list::<i64>()
            .iter()
            .map(|a| a.unwrap().len())
            .sum::<usize>();
        assert_eq!(k, 19913);

        Ok(())
    }

    #[test_log::test]
    fn test_encode_arrow_drop_zeros_null2() -> io::Result<()> {
        let reader = io::BufReader::new(std::fs::File::open("test/data/sparse_large_gaps.txt")?);
        let mut mzs =
            DataArray::from_name_and_type(&ArrayType::MZArray, BinaryDataArrayType::Float64);
        let mut intensities =
            DataArray::from_name_and_type(&ArrayType::IntensityArray, BinaryDataArrayType::Float32);
        for line in reader.lines().flatten() {
            if let Some((a, b)) = line.split_once("\t") {
                mzs.push(a.parse::<f64>().unwrap())?;
                intensities.push(b.parse::<f32>().unwrap())?;
            }
        }

        let mut arrays = BinaryArrayMap::new();
        arrays.add(mzs);
        arrays.add(intensities);

        let mzs = arrays.mzs()?;
        let weights: Vec<f64> = arrays
            .intensities()?
            .iter()
            .map(|w| (*w).sqrt() as f64)
            .collect();

        let betas = crate::filter::select_delta_model(&mzs, Some(&weights));
        let delta_model = RegressionDeltaModel::<f64>::from_f64_iter(betas.into_iter());

        let target = BufferName::new(
            BufferContext::Spectrum,
            ArrayType::MZArray,
            BinaryDataArrayType::Float64,
        );

        let (chunks, _, _) = ArrowArrayChunk::from_arrays(
            0,
            None,
            target,
            &arrays,
            ChunkingStrategy::Delta { chunk_size: 50.0 },
            &Default::default(),
            true,
            true,
            None,
            None,
        )?;

        let rendered = ArrowArrayChunk::to_struct_array(
            &chunks,
            BufferContext::Spectrum,
            &[
                ChunkingStrategy::Basic { chunk_size: 50.0 },
                ChunkingStrategy::Delta { chunk_size: 50.0 },
            ],
            false,
        );

        let start_values = rendered.column(1).as_primitive::<Float64Type>();
        let end_values = rendered.column(2).as_primitive::<Float64Type>();
        let chunk_values = rendered.column(3).as_list::<i64>();
        let intensity_values = rendered.column(5).as_list::<i64>();

        let intensity_values: Vec<f32> = intensity_values
            .iter()
            .flatten()
            .map(|a| {
                a.as_primitive::<Float32Type>()
                    .iter()
                    .map(|v| v.unwrap_or_default())
                    .collect::<Vec<_>>()
            })
            .flatten()
            .collect();

        let mut accumulator =
            DataArray::from_name_and_type(&ArrayType::MZArray, BinaryDataArrayType::Float64);
        for ((start_value, end_value), chunk_vals) in start_values
            .iter()
            .flatten()
            .zip(end_values.iter().flatten())
            .zip(chunk_values.iter().flatten())
        {
            ChunkingStrategy::Delta { chunk_size: 50.0 }.decode_arrow(
                &chunk_vals,
                start_value as f64,
                end_value as f64,
                &mut accumulator,
                Some(&delta_model),
            );
        }

        assert_eq!(accumulator.data_len()?, intensity_values.len());

        let intens = arrays.intensities()?;

        let mz_array = Float64Array::from_iter_values(mzs.iter().copied());
        let intensity_array = Float32Array::from_iter_values(intens.iter().map(|v| *v as f32));
        let mini_schema = Arc::new(Schema::new(vec![
            Arc::new(Field::new("mz_array", DataType::Float64, true)),
            Arc::new(Field::new("intensity_array", DataType::Float32, true)),
        ]));

        let batch = RecordBatch::try_new(
            mini_schema.clone(),
            vec![Arc::new(mz_array) as ArrayRef, Arc::new(intensity_array)],
        )
        .unwrap();

        let trimmed_batch1 = drop_where_column_is_zero_run(&batch, 1).unwrap();
        let trimmed_batch = nullify_at_zero_pair(&trimmed_batch1, 1, &[0, 1]).unwrap();

        assert_eq!(trimmed_batch.num_rows(), accumulator.data_len()?);

        let mz_acc = accumulator.to_f64()?;
        let mz_ref = trimmed_batch.column(0).as_primitive::<Float64Type>();
        let mz_ref = crate::filter::fill_nulls_for(mz_ref, &delta_model);

        for ((i, a), b) in mz_acc
            .iter()
            .copied()
            .enumerate()
            .zip(mz_ref.iter().copied())
        {
            if intensity_values[i] == 0.0 {
                assert!(
                    (a - b).abs() < 1e-6,
                    "{i}: {} {a} - {b} = {}",
                    intensity_values.get(i).unwrap(),
                    a - b
                );
            } else {
                assert_eq!(
                    a,
                    b,
                    "{i}: {} {a} - {b} = {}",
                    intensity_values.get(i).unwrap(),
                    a - b
                );
            }
        }
        Ok(())
    }

    #[test]
    fn test_encode_arrow_drop_zeros_null3() -> io::Result<()> {
        let reader = io::BufReader::new(std::fs::File::open("test/data/sparse_sciex.txt")?);
        let mut mzs =
            DataArray::from_name_and_type(&ArrayType::MZArray, BinaryDataArrayType::Float64);
        let mut intensities =
            DataArray::from_name_and_type(&ArrayType::IntensityArray, BinaryDataArrayType::Float32);
        for line in reader.lines().flatten() {
            if let Some((a, b)) = line.split_once("\t") {
                mzs.push(a.parse::<f64>().unwrap())?;
                intensities.push(b.parse::<f32>().unwrap())?;
            }
        }

        let mut arrays = BinaryArrayMap::new();
        arrays.add(mzs);
        arrays.add(intensities);

        let mzs = arrays.mzs()?;
        let weights: Vec<f64> = arrays
            .intensities()?
            .iter()
            .map(|w| (*w).sqrt() as f64)
            .collect();

        let betas = crate::filter::select_delta_model(&mzs, Some(&weights));
        let delta_model = RegressionDeltaModel::<f64>::from_f64_iter(betas.into_iter());

        let target = BufferName::new(
            BufferContext::Spectrum,
            ArrayType::MZArray,
            BinaryDataArrayType::Float64,
        );

        let (chunks, _, _) = ArrowArrayChunk::from_arrays(
            0,
            None,
            target,
            &arrays,
            ChunkingStrategy::Delta { chunk_size: 50.0 },
            &Default::default(),
            true,
            true,
            None,
            None,
        )?;

        let rendered = ArrowArrayChunk::to_struct_array(
            &chunks,
            BufferContext::Spectrum,
            &[
                ChunkingStrategy::Basic { chunk_size: 50.0 },
                ChunkingStrategy::Delta { chunk_size: 50.0 },
            ],
            false,
        );

        let start_values = rendered.column(1).as_primitive::<Float64Type>();
        let end_values = rendered.column(2).as_primitive::<Float64Type>();
        let chunk_values = rendered.column(3).as_list::<i64>();
        let intensity_values = rendered.column(5).as_list::<i64>();

        let intensity_values: Vec<f32> = intensity_values
            .iter()
            .flatten()
            .map(|a| {
                a.as_primitive::<Float32Type>()
                    .iter()
                    .map(|v| v.unwrap_or_default())
                    .collect::<Vec<_>>()
            })
            .flatten()
            .collect();

        let mut accumulator =
            DataArray::from_name_and_type(&ArrayType::MZArray, BinaryDataArrayType::Float64);
        for ((start_value, end_value), chunk_vals) in start_values
            .iter()
            .flatten()
            .zip(end_values.iter().flatten())
            .zip(chunk_values.iter().flatten())
        {
            ChunkingStrategy::Delta { chunk_size: 50.0 }.decode_arrow(
                &chunk_vals,
                start_value as f64,
                end_value as f64,
                &mut accumulator,
                Some(&delta_model),
            );
        }

        assert_eq!(accumulator.data_len()?, intensity_values.len());

        let intens = arrays.intensities()?;

        let mz_array = Float64Array::from_iter_values(mzs.iter().copied());
        let intensity_array = Float32Array::from_iter_values(intens.iter().map(|v| *v as f32));
        let mini_schema = Arc::new(Schema::new(vec![
            Arc::new(Field::new("mz_array", DataType::Float64, true)),
            Arc::new(Field::new("intensity_array", DataType::Float32, true)),
        ]));

        let batch = RecordBatch::try_new(
            mini_schema.clone(),
            vec![Arc::new(mz_array) as ArrayRef, Arc::new(intensity_array)],
        )
        .unwrap();

        let trimmed_zero_dropped_batch = drop_where_column_is_zero_run(&batch, 1).unwrap();
        let trimmed_null_filled_batch =
            nullify_at_zero_pair(&trimmed_zero_dropped_batch, 1, &[0, 1]).unwrap();

        assert_eq!(
            trimmed_null_filled_batch.num_rows(),
            accumulator.data_len()?
        );

        let mz_acc = accumulator.to_f64()?;
        let mz_ref = trimmed_null_filled_batch
            .column(0)
            .as_primitive::<Float64Type>();
        let mz_ref = crate::filter::fill_nulls_for(mz_ref, &delta_model);

        for ((i, a), b) in mz_acc
            .iter()
            .copied()
            .enumerate()
            .zip(mz_ref.iter().copied())
        {
            if intensity_values[i] == 0.0 {
                assert!(
                    (a - b).abs() < 1e-6,
                    "{i}: {} {a} - {b} = {}",
                    intensity_values.get(i).unwrap(),
                    a - b
                );
            } else {
                assert_eq!(
                    a,
                    b,
                    "{i}: {} {a} - {b} = {}",
                    intensity_values.get(i).unwrap(),
                    a - b
                );
            }
        }

        Ok(())
    }
}

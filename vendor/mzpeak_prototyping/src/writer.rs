use std::{
    collections::HashMap,
    fs,
    io::{self, prelude::*},
    marker::PhantomData,
    sync::Arc,
};

use arrow::{
    array::{Array, ArrayBuilder, AsArray, RecordBatch},
    datatypes::{Field, FieldRef, Schema},
};
use mzpeaks::{CentroidPeak, DeconvolutedPeak};
use parquet::{
    arrow::{ArrowWriter, arrow_writer::ArrowWriterOptions},
    basic::Compression,
    encryption::encrypt::FileEncryptionProperties,
    file::metadata::KeyValue,
};

use mzdata::{
    io::{RandomAccessSpectrumSource, StreamingSpectrumIterator}, meta::{FileMetadataConfig, MSDataFileMetadata}, params::ControlledVocabulary, prelude::*, spectrum::{BinaryArrayMap, Chromatogram, MultiLayerSpectrum, SignalContinuity}
};

use crate::{
    BufferName, archive::{DataKind, EntityType, FileEntry, MzPeakArchiveType, ZipArchiveWriter}, buffer_descriptors::BufferOverrideTable, constants::{
        CHROMATOGRAM_COUNT, CHROMATOGRAM_DATA_POINT_COUNT, SPECTRUM_COUNT,
        SPECTRUM_DATA_POINT_COUNT, WAVELENGTH_SPECTRUM_COUNT, WAVELENGTH_SPECTRUM_DATA_ARRAYS_NAME,
        WAVELENGTH_SPECTRUM_METADATA_NAME,
    }, param::ControlledVocabularyEntry, peak_series::{ArrayIndex, BufferContext, ToMzPeakDataSeries, array_map_to_schema_arrays}, writer::{base::GenericDataArrayWriter, builder::SpectrumFieldVisitors}
};
use crate::{
    chunk_series::{ArrowArrayChunk, ChunkingStrategy},
    constants::{
        CHROMATOGRAM_ARRAY_INDEX, SPECTRUM_ARRAY_INDEX, WAVELENGTH_SPECTRUM_ARRAY_INDEX,
        WAVELENGTH_SPECTRUM_DATA_POINT_COUNT,
    },
};

mod array_buffer;
mod base;
mod builder;
mod mini_peak;
mod split;
mod visitor;

pub use array_buffer::{
    ArrayBufferWriter, ArrayBufferWriterVariants, ArrayBuffersBuilder, ChunkBuffers, PointBuffers,
};
pub use base::AbstractMzPeakWriter;
pub use builder::{ArrayConversionHelper, MzPeakWriterBuilder, WriteBatchConfig};
pub use split::UnpackedMzPeakWriterType;

pub use visitor::{
    ActivationBuilder, AuxiliaryArrayBuilder, CURIEBuilder, ChromatogramBuilder,
    ChromatogramDetailsBuilder, CustomBuilderFromParameter, IsolationWindowBuilder, ParamBuilder,
    ParamListBuilder, ParamValueBuilder, PrecursorBuilder, ScanBuilder, ScanWindowBuilder,
    SelectedIonBuilder, SpectrumBuilder, SpectrumDetailsBuilder, SpectrumVisitor, StructVisitor,
    StructVisitorBuilder, VisitorBase, WavelengthSpectrumBuilder, inflect_cv_term_to_column_name,
};

pub(crate) use base::implement_mz_metadata;
pub(crate) use mini_peak::MiniPeakWriterType;

/*
Internal helper function that, given an iterator over spectra, will
perform the requested overrides and encodings to the data buffers and
construct a Parquet schema.
*/
struct ArrayTypesSampler<'a> {
    overrides: &'a BufferOverrideTable,
    use_chunked_encoding: Option<ChunkingStrategy>,
    is_profile: i32,
}

impl<'a> ArrayTypesSampler<'a> {
    fn new(
        overrides: &'a BufferOverrideTable,
        use_chunked_encoding: Option<ChunkingStrategy>,
    ) -> Self {
        Self {
            overrides,
            use_chunked_encoding,
            is_profile: 0,
        }
    }

    fn from_binary_array_map(
        &mut self,
        map: &BinaryArrayMap,
        context: BufferContext,
    ) -> Option<Vec<FieldRef>> {
        // generate a schema for this chunked
        if let Some(use_chunked_encoding) = self.use_chunked_encoding {
            ArrowArrayChunk::from_arrays(
                0,
                None,
                context.main_axis(),
                map,
                use_chunked_encoding,
                self.overrides,
                false,
                false,
                None,
                None,
            )
            .ok()
            .and_then(|(chunks, _aux_arrays, _)| {
                chunks.first().map(|c| {
                    c.to_schema(
                        context,
                        &[
                            use_chunked_encoding,
                            ChunkingStrategy::Basic { chunk_size: 50.0 },
                        ],
                        false,
                    )
                    .fields
                    .to_vec()
                })
            })
        } else {
            array_map_to_schema_arrays(
                context,
                map,
                map.get(&context.default_sorted_array())
                    .and_then(|a| a.data_len().ok())
                    .unwrap_or_default(),
                0,
                None,
                self.overrides,
            )
            .map(|r| r.0.to_vec())
            .ok()
        }
    }

    fn from_peak_type<T: ToMzPeakDataSeries + BuildFromArrayMap>(
        &mut self,
        peaks: &[T],
    ) -> Option<Vec<FieldRef>> {
        if self.use_chunked_encoding.is_some() {
            self.from_binary_array_map(
                &BuildArrayMapFrom::as_arrays(peaks),
                BufferContext::Spectrum,
            )
        } else {
            let fields = T::to_fields()
                .into_iter()
                .cloned()
                .map(|field| {
                    if let Some(name) =
                        BufferName::from_field(BufferContext::Spectrum, field.clone())
                    {
                        self.overrides.map(&name).to_field()
                    } else {
                        field
                    }
                })
                .collect();

            Some(fields)
        }
    }

    fn visit_chromatogram(&mut self, chromatogram: &Chromatogram) -> Option<Vec<FieldRef>> {
        self.from_binary_array_map(&chromatogram.arrays, BufferContext::Chromatogram)
    }

    fn visit_spectrum<
        C: CentroidLike + ToMzPeakDataSeries + BuildFromArrayMap + From<CentroidPeak>,
        D: DeconvolutedCentroidLike + ToMzPeakDataSeries + BuildFromArrayMap + From<DeconvolutedPeak>,
    >(
        &mut self,
        s: MultiLayerSpectrum<C, D>,
        prefer_peaks: bool,
    ) -> Option<Vec<FieldRef>> {
        log::trace!("Sampling arrays from {}", s.id());
        if s.signal_continuity() == SignalContinuity::Profile {
            self.is_profile += 1;
        }

        if prefer_peaks {
            match s.peaks() {
                mzdata::spectrum::RefPeakDataLevel::Missing => None,
                mzdata::spectrum::RefPeakDataLevel::RawData(map) => {
                    self.from_binary_array_map(map, BufferContext::Spectrum)
                }
                mzdata::spectrum::RefPeakDataLevel::Centroid(peak_set_vec) => {
                    self.from_peak_type(peak_set_vec.as_slice())
                }
                mzdata::spectrum::RefPeakDataLevel::Deconvoluted(peak_set_vec) => {
                    self.from_peak_type(peak_set_vec.as_slice())
                }
            }
        } else {
            s.raw_arrays()
                .and_then(|map| self.from_binary_array_map(map, BufferContext::Spectrum))
        }
    }

    fn sample_chromatogram_array_types(
        &mut self,
        iter: impl Iterator<Item = Chromatogram>,
    ) -> Vec<FieldRef> {
        let mut arrays: Vec<Arc<Field>> = Vec::new();

        let field_it = iter.flat_map(|s| self.visit_chromatogram(&s)).flatten();

        for field in field_it {
            if !arrays.iter().any(|f| f.name() == field.name()) {
                if let Some(buffer) =
                    BufferName::from_field(BufferContext::Chromatogram, field.clone())
                {
                    log::trace!("Adding {buffer:?} to schema")
                }
                arrays.push(field);
            }
        }

        arrays
    }

    fn sample_spectrum_array_types<
        C: CentroidLike + ToMzPeakDataSeries + BuildFromArrayMap + From<CentroidPeak>,
        D: DeconvolutedCentroidLike + ToMzPeakDataSeries + BuildFromArrayMap + From<DeconvolutedPeak>,
    >(
        &mut self,
        iter: impl Iterator<Item = MultiLayerSpectrum<C, D>>,
        prefer_peaks: bool,
    ) -> Vec<FieldRef> {
        let mut arrays: Vec<Arc<Field>> = Vec::new();

        let field_it = iter
            .flat_map(|s| self.visit_spectrum(s, prefer_peaks))
            .flatten();

        for field in field_it {
            if !arrays.iter().any(|f| f.name() == field.name()) {
                if let Some(buffer) = BufferName::from_field(BufferContext::Spectrum, field.clone())
                {
                    log::trace!("Adding {buffer:?} to schema")
                }
                arrays.push(field);
            }
        }

        if self.is_profile > 0 {
            log::debug!("Detected profile spectra");
        }
        arrays
    }
}

/// Collect arrays fields from an iterator of chromatograms to prepare the data file schema.
///
/// This consumes the entire iterator.
///
/// # Arguments
/// `reader`: The stream of chromatograms to read from
/// `overrides`: The array mapping rules override array data types.
/// `use_chunked_encoding`: The chunk encoding format to use, if any
pub fn sample_array_types_from_chromatograms<I: Iterator<Item = Chromatogram>>(
    iter: I,
    overrides: &BufferOverrideTable,
    use_chunked_encoding: Option<ChunkingStrategy>,
) -> Vec<Arc<Field>> {
    ArrayTypesSampler::new(overrides, use_chunked_encoding).sample_chromatogram_array_types(iter)
}

/// Collect arrays fields from spectra in a [`StreamingSpectrumIterator`] to prepare
/// the data file schema.
///
/// This consumes only the next 10 spectra.
///
/// # Arguments
/// `reader`: The stream of spectra to read from
/// `overrides`: The array mapping rules override array data types.
/// `use_chunked_encoding`: The chunk encoding format to use, if any
pub fn sample_array_types_from_spectrum_stream<
    C: CentroidLike + ToMzPeakDataSeries + BuildFromArrayMap + From<CentroidPeak>,
    D: DeconvolutedCentroidLike + ToMzPeakDataSeries + BuildFromArrayMap + From<DeconvolutedPeak>,
    I: Iterator<Item = MultiLayerSpectrum<C, D>>,
>(
    reader: &mut StreamingSpectrumIterator<C, D, MultiLayerSpectrum<C, D>, I>,
    overrides: &BufferOverrideTable,
    use_chunked_encoding: Option<ChunkingStrategy>,
) -> Vec<Arc<Field>>
where
    MultiLayerSpectrum<C, D>: Clone,
{
    reader.populate_buffer(10);
    let mut sampler = ArrayTypesSampler::new(overrides, use_chunked_encoding);
    sampler.sample_spectrum_array_types(reader.iter_buffer().cloned(), false)
}

/// Collect arrays fields from spectra in a [`RandomAccessSpectrumSource`] to prepare
/// the data file schema.
///
/// This examines the first, 100th, and middle spectrum from `reader`.
///
/// # Arguments
/// `reader`: The stream of spectra to read from
/// `overrides`: The array mapping rules override array data types.
/// `use_chunked_encoding`: The chunk encoding format to use, if any
/// `prefer_peaks`: Try to build array information from the most refined peak data representation available, falling
/// back to
pub fn sample_array_types_from_spectrum_source<
    C: CentroidLike + ToMzPeakDataSeries + BuildFromArrayMap + From<CentroidPeak>,
    D: DeconvolutedCentroidLike + ToMzPeakDataSeries + BuildFromArrayMap + From<DeconvolutedPeak>,
    R: RandomAccessSpectrumSource<C, D, MultiLayerSpectrum<C, D>>,
>(
    reader: &mut R,
    overrides: &BufferOverrideTable,
    use_chunked_encoding: Option<ChunkingStrategy>,
    prefer_peaks: bool,
) -> Vec<Arc<Field>> {
    let n = reader.len();
    if n == 0 {
        return Vec::new();
    }

    if n > 50 {
        let pts = [0, 100.min(n - 1), n / 4, n / 2, (n / 2 + n / 4)];
        log::trace!("{n} spectra detected, sampling arrays from control points {pts:?}");
        let it = pts
            .into_iter()
            .flat_map(|i| reader.get_spectrum_by_index(i));
        ArrayTypesSampler::new(overrides, use_chunked_encoding)
            .sample_spectrum_array_types(it, prefer_peaks)
    } else {
        log::trace!("{n} spectra detected, sampling arrays from all entries");
        let it = reader.iter();
        let fields = ArrayTypesSampler::new(overrides, use_chunked_encoding)
            .sample_spectrum_array_types(it, prefer_peaks);
        reader.reset();
        fields
    }

}

/// Backstop (Option E) for the spectra-peaks facet: drop any `point`-struct child column that is
/// **100% null across the whole file** AND shares its `array_name` with another (populated) sibling.
///
/// The schema sampler can add an alternate-precision twin (e.g. an `intensity_f64` alongside the f32
/// `intensity`, both `array_name = "intensity array"`) that the fixed-precision peak write path never
/// fills. Such a null twin reusing a primary's `array_name` blanks the spectrum in readers that
/// resolve arrays by `array_name` without honoring `buffer_priority`.
///
/// This runs AFTER the peak parquet is finished, so it never changes the write-time schema (which the
/// point-layout write routing depends on) — it just rewrites the finished facet without the dead
/// column. Detection is metadata-only (row-group null counts); the rewrite happens ONLY when a twin
/// is actually present, so the common case pays nothing but a stats scan.
fn prune_all_null_dup_point_columns(
    mut peak_file: fs::File,
) -> Result<fs::File, parquet::errors::ParquetError> {
    use arrow::array::{Array, ArrayRef, StructArray};
    use arrow::datatypes::{DataType, Field, Fields, Schema};
    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
    use std::io::Seek;

    peak_file.rewind()?;
    let builder = ParquetRecordBatchReaderBuilder::try_new(peak_file.try_clone()?)?;
    let arrow_schema = builder.schema().clone();
    let pq_meta = builder.metadata().clone();

    // The peak facet is a single top-level `point` struct; bail (leave untouched) on any other shape.
    let Some((point_idx, point_field)) = arrow_schema
        .fields()
        .iter()
        .enumerate()
        .find(|(_, f)| matches!(f.data_type(), DataType::Struct(_)))
        .map(|(i, f)| (i, f.clone()))
    else {
        peak_file.rewind()?;
        return Ok(peak_file);
    };
    let DataType::Struct(children) = point_field.data_type().clone() else { unreachable!() };
    let n = children.len();
    let total_rows = pq_meta.file_metadata().num_rows();

    // Per-child null counts from row-group statistics (one struct => leaf columns align 1:1 with
    // children, in order). If any stat is missing or the shape is unexpected, skip pruning entirely.
    let mut null_counts = vec![0i64; n];
    let mut have_stats = total_rows > 0;
    'rg: for rg in pq_meta.row_groups() {
        if rg.num_columns() != n {
            have_stats = false;
            break;
        }
        for (i, col) in rg.columns().iter().enumerate() {
            match col.statistics().and_then(|s| s.null_count_opt()) {
                Some(nc) => null_counts[i] += nc as i64,
                None => {
                    have_stats = false;
                    break 'rg;
                }
            }
        }
    }

    let array_name = |f: &Field| f.metadata().get("array_name").cloned();
    let mut drop: Vec<usize> = Vec::new();
    if have_stats {
        for (i, ch) in children.iter().enumerate() {
            if null_counts[i] != total_rows {
                continue; // not all-null
            }
            let Some(an) = array_name(ch) else { continue };
            // keep it unless a DIFFERENT, populated sibling already owns this array_name
            let shadowed = children.iter().enumerate().any(|(j, o)| {
                j != i && null_counts[j] != total_rows && array_name(o).as_deref() == Some(an.as_str())
            });
            if shadowed {
                drop.push(i);
            }
        }
    }
    if drop.is_empty() {
        peak_file.rewind()?;
        return Ok(peak_file);
    }
    log::debug!(
        "spectra_peaks: pruning {} all-null duplicate column(s): {:?}",
        drop.len(),
        drop.iter().map(|&i| children[i].name()).collect::<Vec<_>>()
    );

    // Rewrite the facet with the surviving children only.
    let kept: Vec<usize> = (0..n).filter(|i| !drop.contains(i)).collect();
    let kept_fields: Fields = kept.iter().map(|&i| children[i].clone()).collect();
    let mut out_fields: Vec<_> = arrow_schema.fields().iter().cloned().collect();
    out_fields[point_idx] = Arc::new(
        Field::new(point_field.name(), DataType::Struct(kept_fields.clone()), point_field.is_nullable())
            .with_metadata(point_field.metadata().clone()),
    );
    let out_schema = Arc::new(Schema::new_with_metadata(out_fields, arrow_schema.metadata().clone()));

    let out = tempfile::tempfile()?;
    let mut w = ArrowWriter::try_new(out.try_clone()?, out_schema.clone(), None)?;
    if let Some(kvs) = pq_meta.file_metadata().key_value_metadata() {
        for kv in kvs {
            if kv.key != "ARROW:schema" {
                w.append_key_value_metadata(kv.clone());
            }
        }
    }
    for batch in builder.build()? {
        let batch = batch?;
        let point = batch
            .column(point_idx)
            .as_any()
            .downcast_ref::<StructArray>()
            .expect("point column is a struct");
        let kept_arrays: Vec<ArrayRef> = kept.iter().map(|&i| point.column(i).clone()).collect();
        let new_point = StructArray::new(kept_fields.clone(), kept_arrays, point.nulls().cloned());
        let mut cols: Vec<ArrayRef> = batch.columns().to_vec();
        cols[point_idx] = Arc::new(new_point);
        let new_batch = arrow::record_batch::RecordBatch::try_new(out_schema.clone(), cols)
            .map_err(|e| parquet::errors::ParquetError::General(e.to_string()))?;
        w.write(&new_batch)?;
    }
    w.close()?;

    let mut out = out;
    out.rewind()?;
    Ok(out)
}

/// Array type inference from inputs
impl MzPeakWriterBuilder {
    /// Collect arrays fields from spectra in a [`RandomAccessSpectrumSource`] to prepare
    /// the data file schema.
    ///
    /// This examines the first, 100th, and middle spectrum from `reader`.
    ///
    /// # Arguments
    /// `reader`: The stream of spectra to read from
    pub fn sample_array_types_from_spectrum_source<
        C: CentroidLike + ToMzPeakDataSeries + BuildFromArrayMap + From<CentroidPeak>,
        D: DeconvolutedCentroidLike + ToMzPeakDataSeries + BuildFromArrayMap + From<DeconvolutedPeak>,
        R: RandomAccessSpectrumSource<C, D, MultiLayerSpectrum<C, D>>,
    >(
        mut self,
        reader: &mut R,
    ) -> Self {
        let fields = sample_array_types_from_spectrum_source(
            reader,
            &self.spectrum_overrides(),
            self.chunked_encoding,
            false,
        );

        for f in fields {
            self = self.add_spectrum_field(f);
        }

        self
    }

    /// Like [`Self::sample_array_types_from_spectrum_source`] but driven by an explicit iterator of
    /// sample spectra, for vendor readers that are NOT a [`RandomAccessSpectrumSource`] (SciEX,
    /// Waters, Bruker TSF). Honors the configured chunking strategy so the derived data-facet schema
    /// matches the chunked record batches — without this, dense profile spectra chunk into
    /// `LargeList(Float32)` while a scalar-derived schema expects `Float32`, panicking the writer.
    pub fn sample_array_types_from_spectra<
        C: CentroidLike + ToMzPeakDataSeries + BuildFromArrayMap + From<CentroidPeak>,
        D: DeconvolutedCentroidLike + ToMzPeakDataSeries + BuildFromArrayMap + From<DeconvolutedPeak>,
        I: Iterator<Item = MultiLayerSpectrum<C, D>>,
    >(
        mut self,
        spectra: I,
    ) -> Self {
        let fields = ArrayTypesSampler::new(&self.spectrum_overrides(), self.chunked_encoding)
            .sample_spectrum_array_types(spectra, false);
        for f in fields {
            self = self.add_spectrum_field(f);
        }
        self
    }

    fn take_or_initialize_peak_builder(&mut self) -> ArrayBuffersBuilder {
        let mut point_builder = self
            .store_peaks_and_profiles_apart
            .take()
            .unwrap_or_else(|| {
                ArrayBuffersBuilder::default()
                    .prefix("point")
                    .with_context(BufferContext::Spectrum)
            });
        point_builder = point_builder.extend_overrides(self.spectrum_overrides().into_iter());
        point_builder
    }

    pub fn register_spectrum_peak_type<T: ToMzPeakDataSeries>(mut self) -> Self {
        let point_builder = self.take_or_initialize_peak_builder();
        self.store_peaks_and_profiles_apart(Some(point_builder.add_peak_type::<T>()))
    }

    pub fn sample_array_types_for_peaks_from_spectrum_source<
        C: CentroidLike + ToMzPeakDataSeries + BuildFromArrayMap + From<CentroidPeak>,
        D: DeconvolutedCentroidLike + ToMzPeakDataSeries + BuildFromArrayMap + From<DeconvolutedPeak>,
        R: RandomAccessSpectrumSource<C, D, MultiLayerSpectrum<C, D>>,
    >(
        mut self,
        reader: &mut R,
    ) -> Self {
        let mut point_builder = self.take_or_initialize_peak_builder();
        for f in
            sample_array_types_from_spectrum_source(reader, &self.spectrum_overrides(), None, true)
        {
            point_builder = point_builder.add_field(f);
        }
        self.store_peaks_and_profiles_apart(Some(point_builder))
    }

    /// Collect arrays fields from spectra in a [`StreamingSpectrumIterator`] to prepare
    /// the data file schema.
    ///
    /// This consumes only the next 10 spectra.
    ///
    /// # Arguments
    /// `reader`: The stream of spectra to read from
    pub fn sample_array_types_from_spectrum_stream<
        C: CentroidLike + ToMzPeakDataSeries + BuildFromArrayMap + From<CentroidPeak>,
        D: DeconvolutedCentroidLike + ToMzPeakDataSeries + BuildFromArrayMap + From<DeconvolutedPeak>,
        I: Iterator<Item = MultiLayerSpectrum<C, D>>,
    >(
        mut self,
        reader: &mut StreamingSpectrumIterator<C, D, MultiLayerSpectrum<C, D>, I>,
    ) -> Self
    where
        MultiLayerSpectrum<C, D>: Clone,
    {
        let fields = sample_array_types_from_spectrum_stream(
            reader,
            &self.spectrum_overrides(),
            self.chunked_encoding,
        );

        for f in fields {
            self = self.add_spectrum_field(f);
        }

        self
    }

    /// Collect arrays fields from an iterator of chromatograms to prepare the data file schema.
    ///
    /// This consumes the entire iterator.
    ///
    /// # Arguments
    /// `reader`: The stream of chromatograms to read from
    pub fn sample_array_types_from_chromatograms<I: Iterator<Item = Chromatogram>>(
        mut self,
        iter: I,
    ) -> Self {
        let fields = sample_array_types_from_chromatograms(
            iter,
            &self.chromatogram_overrides(),
            self.chromatogram_chunked_encoding,
        );
        for f in fields {
            self = self.add_chromatogram_field(f);
        }
        self
    }
}

/// Write an mzPeak archive to an uncompressed ZIP archive
pub struct MzPeakWriterType<
    W: Write + Send + Seek,
    C: CentroidLike + ToMzPeakDataSeries = CentroidPeak,
    D: DeconvolutedCentroidLike + ToMzPeakDataSeries = DeconvolutedPeak,
> {
    archive_writer: Option<ArrowWriter<ZipArchiveWriter<W>>>,
    spectrum_data_buffers: ArrayBufferWriterVariants,
    spectrum_peaks_writer: Option<MiniPeakWriterType<fs::File>>,

    chromatogram_data_buffers: ArrayBufferWriterVariants,
    wavelength_spectrum_data_buffers: Option<GenericDataArrayWriter>,

    spectrum_metadata_buffer: SpectrumBuilder,
    /// Temp-parquet spool for spectrum metadata. The metadata facet can't be written into the
    /// single-stream zip until finish, so for high-spectrum-count files its builder would otherwise
    /// hold every spectrum in RAM (~7 GB at 307k spectra). Lazily created once the builder is first
    /// drained on the buffer_size cadence; streamed back batch-by-batch into the metadata entry at
    /// finish. Small files (< buffer_size spectra) never spool and keep the single in-RAM batch path.
    spectrum_metadata_spool: Option<ArrowWriter<fs::File>>,
    chromatogram_metadata_buffer: ChromatogramBuilder,
    wavelength_spectrum_metadata_buffer: WavelengthSpectrumBuilder,

    use_chunked_encoding: Option<ChunkingStrategy>,
    use_chromatogram_chunked_encoding: Option<ChunkingStrategy>,

    buffer_size: usize,
    shuffle_mz: bool,
    compression: Compression,
    encryption_properties: HashMap<String, Arc<FileEncryptionProperties>>,

    #[allow(unused)]
    write_batch_config: WriteBatchConfig,
    mz_metadata: FileMetadataConfig,
    controlled_vocabularies: Vec<ControlledVocabularyEntry>,
    _t: PhantomData<(C, D)>,
}

impl<
    W: Write + Send + Seek,
    C: CentroidLike + ToMzPeakDataSeries,
    D: DeconvolutedCentroidLike + ToMzPeakDataSeries,
> AbstractMzPeakWriter for MzPeakWriterType<W, C, D>
{
    fn append_key_value_metadata(&mut self, key: String, value: Option<String>) {
        self.archive_writer
            .as_mut()
            .unwrap()
            .append_key_value_metadata(KeyValue::new(key, value));
    }

    fn use_chunked_encoding(&self) -> Option<&ChunkingStrategy> {
        self.use_chunked_encoding.as_ref()
    }

    fn use_chromatogram_chunked_encoding(&self) -> Option<&ChunkingStrategy> {
        self.use_chromatogram_chunked_encoding.as_ref()
    }

    fn spectrum_entry_buffer_mut(&mut self) -> &mut SpectrumBuilder {
        &mut self.spectrum_metadata_buffer
    }

    fn spectrum_data_buffer_mut(&mut self) -> &mut ArrayBufferWriterVariants {
        &mut self.spectrum_data_buffers
    }

    fn check_data_buffer(&mut self) -> io::Result<()> {
        // Flush on the spectrum-count threshold, OR a buffered-point ceiling, OR the measured
        // byte size of the buffered arrays — whichever trips first. (A pure byte trigger is not
        // reliable alone: arrow's get_array_memory_size under-reports compressed/chunked buffers, so
        // the count/point thresholds remain the load-bearing bound.)
        if self.spectrum_counter() % (self.buffer_size as u64) == 0
            || self.spectrum_data_buffer_mut().len() >= 4_000_000
            || self.spectrum_data_buffer_mut().memory_size() >= *crate::writer::array_buffer::FLUSH_MEM_BYTES
        {
            self.flush_data_arrays()?;
        }
        // Bound the spectrum-METADATA builder on the same count cadence. It can't stream into the
        // single-stream zip (the metadata entry opens only at finish), so drain it to a side parquet
        // spool. Small files (< buffer_size spectra) never trip this and keep the in-RAM finish path.
        if self.spectrum_counter() % (self.buffer_size as u64) == 0 {
            self.spool_spectrum_metadata()?;
        }
        Ok(())
    }

    fn spectrum_counter(&self) -> u64 {
        self.spectrum_metadata_buffer.index_counter()
    }

    fn spectrum_precursor_counter(&self) -> u64 {
        self.spectrum_metadata_buffer.precursor_index_counter()
    }

    fn spectrum_peak_writer(&mut self) -> Option<&mut MiniPeakWriterType<fs::File>> {
        self.spectrum_peaks_writer.as_mut()
    }

    fn chromatogram_counter(&self) -> u64 {
        self.chromatogram_metadata_buffer.index_counter()
    }

    fn chromatogram_entry_buffer_mut(&mut self) -> &mut ChromatogramBuilder {
        &mut self.chromatogram_metadata_buffer
    }

    fn chromatogram_data_buffer_mut(&mut self) -> &mut ArrayBufferWriterVariants {
        &mut self.chromatogram_data_buffers
    }

    fn wavelength_data_buffer_mut(&mut self) -> &mut GenericDataArrayWriter {
        if self.wavelength_spectrum_data_buffers.is_some() {
            return self.wavelength_spectrum_data_buffers.as_mut().unwrap();
        } else {
            let writer = self.make_wavelength_data_writer();
            self.wavelength_spectrum_data_buffers = Some(writer);
            self.wavelength_spectrum_data_buffers.as_mut().unwrap()
        }
    }

    fn wavelength_entry_buffer_mut(&mut self) -> &mut WavelengthSpectrumBuilder {
        &mut self.wavelength_spectrum_metadata_buffer
    }

    fn set_spectrum_peak_writer(&mut self, writer: MiniPeakWriterType<fs::File>) {
        self.spectrum_peaks_writer = Some(writer);
    }

    fn write_batch_config(&self) -> WriteBatchConfig {
        self.write_batch_config
    }

    fn compression(&self) -> Compression {
        self.compression
    }

    fn shuffle_mz(&self) -> bool {
        self.shuffle_mz
    }

    fn buffer_size(&self) -> usize {
        self.buffer_size
    }

    fn encryption_properties(&self) -> &HashMap<String, Arc<FileEncryptionProperties>> {
        &self.encryption_properties
    }

    fn add_index_metadata(&mut self, key: &str, value: &impl serde::Serialize) -> Result<(), serde_json::Error> {
        if let Some(v) = self.archive_writer.as_mut() {
            v.inner_mut().add_index_metadata(key, value)
        } else {
            Ok(())
        }
    }

    fn mz_metadata(&self) -> &FileMetadataConfig {
        &self.mz_metadata
    }

    fn controlled_vocabularies(&self) -> &[ControlledVocabularyEntry] {
        &self.controlled_vocabularies
    }

    fn controlled_vocabularies_mut(&mut self) -> &mut Vec<ControlledVocabularyEntry> {
        &mut self.controlled_vocabularies
    }
}

impl<
    W: Write + Send + Seek,
    C: CentroidLike + ToMzPeakDataSeries,
    D: DeconvolutedCentroidLike + ToMzPeakDataSeries,
> MSDataFileMetadata for MzPeakWriterType<W, C, D>
{
    mzdata::delegate_impl_metadata_trait!(mz_metadata);
}

impl<
    W: Write + Send + Seek,
    C: CentroidLike + ToMzPeakDataSeries,
    D: DeconvolutedCentroidLike + ToMzPeakDataSeries,
> MzPeakWriterType<W, C, D>
{
    pub fn builder() -> MzPeakWriterBuilder {
        MzPeakWriterBuilder::default()
    }

    pub fn new(
        writer: W,
        spectrum_buffers_builder: ArrayBuffersBuilder,
        chromatogram_buffers_builder: ArrayBuffersBuilder,
        buffer_size: usize,
        mask_zero_intensity_runs: bool,
        shuffle_mz: bool,
        use_chunked_encoding: Option<ChunkingStrategy>,
        use_chromatogram_chunked_encoding: Option<ChunkingStrategy>,
        compression: Compression,
        store_peaks_and_profiles_apart: Option<ArrayBuffersBuilder>,
        write_batch_config: WriteBatchConfig,
        spectrum_fields: SpectrumFieldVisitors,
        encryption_properties: HashMap<String, Arc<FileEncryptionProperties>>,
    ) -> Self {
        let mut spectrum_metadata_buffer = SpectrumBuilder::default();
        spectrum_metadata_buffer.add_visitors_from(spectrum_fields);

        let spectrum_buffers: ArrayBufferWriterVariants = if use_chunked_encoding.is_some() {
            spectrum_buffers_builder
                .build_chunked(
                    Arc::new(Schema::empty()),
                    BufferContext::Spectrum,
                    mask_zero_intensity_runs,
                )
                .into()
        } else {
            let spectrum_buffers = spectrum_buffers_builder.build(
                Arc::new(Schema::empty()),
                BufferContext::Spectrum,
                mask_zero_intensity_runs,
            );
            spectrum_buffers.into()
        };

        // The chromatogram buffer's schema must follow the CHROMATOGRAM chunking flag, not the
        // spectrum one. write_chromatogram_arrays() (base.rs) branches on
        // use_chromatogram_chunked_encoding; building the buffer off use_chunked_encoding instead
        // lets the two disagree (point spectra + chunked-chromatogram flag → point buffer, chunked
        // write → drain panics "N columns vs M fields"). The split writer already uses this flag.
        let chromatogram_buffers: ArrayBufferWriterVariants = if use_chromatogram_chunked_encoding.is_some() {
            chromatogram_buffers_builder
                .build_chunked(
                    Arc::new(Schema::empty()),
                    BufferContext::Chromatogram,
                    false,
                )
                .into()
        } else {
            chromatogram_buffers_builder
                .build(
                    Arc::new(Schema::empty()),
                    BufferContext::Chromatogram,
                    false,
                )
                .into()
        };

        let mut writer = ZipArchiveWriter::new(writer);
        writer.start_spectrum_data().unwrap();

        let spectrum_data_encryption_props = encryption_properties
            .get(&FileEntry::from(MzPeakArchiveType::SpectrumDataArrays).name)
            .cloned();

        let data_props = Self::spectrum_data_writer_props(
            &spectrum_buffers,
            spectrum_buffers.index_path(),
            shuffle_mz,
            &use_chunked_encoding,
            compression,
            write_batch_config,
            spectrum_data_encryption_props,
        );

        let separate_peak_writer = if let Some(peak_buffer_builder) = store_peaks_and_profiles_apart
        {
            let peak_buffer_file =
                tempfile::tempfile().expect("Failed to create temporary file to write peaks to");
            let writer = Self::make_peaks_writer(
                peak_buffer_file,
                peak_buffer_builder,
                write_batch_config,
                compression,
                spectrum_buffers.include_time(),
                shuffle_mz,
                buffer_size,
                &encryption_properties,
            )
            .map_err(|e| log::error!("Failed to open peak writer: {e}"))
            .ok();
            writer
        } else {
            None
        };

        let mut this = Self {
            archive_writer: Some(
                ArrowWriter::try_new_with_options(
                    writer,
                    spectrum_buffers.schema().clone(),
                    ArrowWriterOptions::new().with_properties(data_props),
                )
                .unwrap(),
            ),
            spectrum_peaks_writer: separate_peak_writer,
            use_chunked_encoding,
            use_chromatogram_chunked_encoding,
            spectrum_metadata_buffer,
            spectrum_metadata_spool: None,
            spectrum_data_buffers: spectrum_buffers,
            chromatogram_data_buffers: chromatogram_buffers,
            chromatogram_metadata_buffer: Default::default(),
            buffer_size,
            shuffle_mz,
            mz_metadata: Default::default(),
            compression,
            write_batch_config,
            wavelength_spectrum_data_buffers: None,
            wavelength_spectrum_metadata_buffer: Default::default(),
            _t: PhantomData,
            encryption_properties,
            controlled_vocabularies: vec![ControlledVocabulary::MS.into(), ControlledVocabulary::UO.into()],
        };
        this.add_spectrum_array_metadata();
        this
    }

    implement_mz_metadata!();

    fn add_spectrum_array_metadata(&mut self) {
        let spectrum_array_index: ArrayIndex = self.spectrum_data_buffers.as_array_index();
        self.append_key_value_metadata(
            SPECTRUM_ARRAY_INDEX.into(),
            spectrum_array_index.to_json().into(),
        );
    }

    fn add_chromatogram_array_metadata(&mut self) {
        let chromatogram_array_index: ArrayIndex = self.chromatogram_data_buffers.as_array_index();
        self.append_key_value_metadata(
            CHROMATOGRAM_ARRAY_INDEX.into(),
            Some(chromatogram_array_index.to_json()),
        );
    }

    /// Write a `spectrum` to the MzPeak file
    ///
    /// Data may be buffered until the spectrum data file is ready to be written, but the spectrum data file
    /// is likely being actively written out as the the buffer grows.
    ///
    /// # See also
    /// [`AbstractMzPeakWriter::write_spectrum`]
    pub fn write_spectrum<
        A: ToMzPeakDataSeries + CentroidLike,
        B: ToMzPeakDataSeries + DeconvolutedCentroidLike,
        S: SpectrumLike<A, B> + 'static,
    >(
        &mut self,
        spectrum: &S,
    ) -> io::Result<()> {
        AbstractMzPeakWriter::write_spectrum(self, spectrum)
    }

    fn flush_data_arrays(&mut self) -> io::Result<()> {
        for batch in self.spectrum_data_buffers.drain() {
            if let Some(writer) = self.archive_writer.as_mut() {
                writer.write(&batch)?;
                // Bound the in-progress row group by size for EVERY layout (not just chunked) so a
                // point/non-chunked file can't grow an unbounded row group in RAM.
                if writer.in_progress_size() > 16_000_000 {
                    log::debug!(
                        "Flushing row group buffer with approximately {} bytes",
                        writer.in_progress_size()
                    );
                    writer.flush()?;
                }
            } else {
                panic!("Attempted to write spectrum data but writer does not exist");
            }
        }
        Ok(())
    }

    /// Drain the in-RAM spectrum-metadata builder into the temp-parquet spool (lazily created on
    /// first use). No-op when the builder holds no rows. `finish()` resets the builder but leaves the
    /// `id_to_index` map intact, so cross-batch precursor->spectrum references still resolve.
    fn spool_spectrum_metadata(&mut self) -> io::Result<()> {
        let arrays = self.spectrum_metadata_buffer.finish();
        let batch = RecordBatch::from(arrays.as_struct());
        if batch.num_rows() == 0 {
            return Ok(());
        }
        if self.spectrum_metadata_spool.is_none() {
            let f = tempfile::tempfile()?;
            self.spectrum_metadata_spool =
                Some(ArrowWriter::try_new(f, batch.schema(), None).map_err(io::Error::other)?);
        }
        self.spectrum_metadata_spool
            .as_mut()
            .unwrap()
            .write(&batch)
            .map_err(io::Error::other)
    }

    fn flush_spectrum_metadata_records(&mut self) -> io::Result<()> {
        // Spooled (high-spectrum-count file): flush the tail, then stream the spool back into the
        // metadata entry ONE batch at a time so RAM stays bounded. Otherwise write the lone in-RAM
        // batch directly — no temp-file round-trip for small files.
        if self.spectrum_metadata_spool.is_some() {
            self.spool_spectrum_metadata()?;
            let mut tmp = self
                .spectrum_metadata_spool
                .take()
                .unwrap()
                .into_inner()
                .map_err(io::Error::other)?;
            tmp.rewind()?;
            let reader =
                parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(tmp)
                    .map_err(io::Error::other)?
                    .build()
                    .map_err(io::Error::other)?;
            for batch in reader {
                let batch = batch.map_err(io::Error::other)?;
                let writer = self.archive_writer.as_mut().unwrap();
                writer.write(&batch)?;
                // Bound the metadata row group by bytes (same threshold as the data facet) so a
                // very large run (100k+ spectra) is split into several row groups a reader can
                // decode incrementally, instead of one monolithic row group it must materialize
                // whole on open.
                if writer.in_progress_size() > 16_000_000 {
                    log::debug!(
                        "Flushing metadata row group buffer with approximately {} bytes",
                        writer.in_progress_size()
                    );
                    writer.flush()?;
                }
            }
        } else {
            let arrays = self.spectrum_metadata_buffer.finish();
            let batch = RecordBatch::from(arrays.as_struct());
            self.archive_writer.as_mut().unwrap().write(&batch)?;
        }
        Ok(())
    }

    /// Get the count of waiting spectrum data rows
    pub fn buffered_spectrum_data(&self) -> usize {
        self.spectrum_data_buffers.len()
    }

    fn write_struct_arrays(&mut self, arrays: Arc<dyn Array>) -> io::Result<()> {
        let batch = RecordBatch::from(arrays.as_struct());
        self.archive_writer.as_mut().unwrap().write(&batch)?;
        Ok(())
    }

    fn flush_chromatogram_metadata_records(&mut self) -> io::Result<()> {
        let arrays = self.chromatogram_metadata_buffer.finish();
        self.write_struct_arrays(arrays)
    }

    fn flush_chromatogram_data_records(&mut self) -> io::Result<()> {
        for batch in self.chromatogram_data_buffers.drain() {
            if let Some(writer) = self.archive_writer.as_mut() {
                writer.write(&batch)?;
                // if writer.in_progress_size() > 16_000_000 && use_chunks {
                //     log::debug!(
                //         "Flushing row group buffer with approximately {} bytes",
                //         writer.in_progress_size()
                //     );
                //     writer.flush()?;
                // }
            } else {
                panic!("Attempted to write spectrum data but writer does not exist");
            }
        }
        Ok(())
    }

    fn finish_parquet_inner(
        &mut self,
    ) -> Result<ZipArchiveWriter<W>, parquet::errors::ParquetError> {
        if self.archive_writer.is_some() {
            self.flush_data_arrays()?;
            self.append_key_value_metadata(
                SPECTRUM_COUNT.into(),
                Some(self.spectrum_counter().to_string()),
            );
            let n_p = self
                .spectrum_peak_writer()
                .map(|v| v.point_count())
                .unwrap_or_default()
                + self.spectrum_data_buffers.point_count();
            self.append_key_value_metadata(SPECTRUM_DATA_POINT_COUNT.into(), Some(n_p.to_string()));

            let mut writer = self.archive_writer.take().unwrap().into_inner()?;

            if let Some(peak_file_writer) = self.spectrum_peaks_writer.take() {
                let peak_file = peak_file_writer.finish()?;
                // Option E backstop: drop any all-null column that duplicates a populated sibling's
                // `array_name` (e.g. a spurious `intensity_f64` twin). No-op unless one is present.
                let mut peak_file = prune_all_null_dup_point_columns(peak_file)?;
                log::trace!("Copying peaks file into zip archive");
                peak_file.rewind()?;
                writer.add_file_from_read(
                    &mut peak_file,
                    Some(&MzPeakArchiveType::SpectrumPeakDataArrays.tag_file_suffix()),
                    Some(MzPeakArchiveType::SpectrumPeakDataArrays.into()),
                )?;
            }

            writer.start_spectrum_metadata().unwrap();
            let metadata_fields = self.spectrum_metadata_buffer.schema();
            let encryption_props = self
                .encryption_properties
                .get(
                    FileEntry::from(MzPeakArchiveType::SpectrumMetadata)
                        .name
                        .as_str(),
                )
                .cloned();
            self.archive_writer = Some(ArrowWriter::try_new_with_options(
                writer,
                metadata_fields.clone(),
                ArrowWriterOptions::new().with_properties(Self::spectrum_metadata_writer_props(
                    &metadata_fields,
                    encryption_props,
                )),
            )?);
            self.flush_spectrum_metadata_records()?;
            self.append_metadata();
            self.append_key_value_metadata(
                SPECTRUM_COUNT.into(),
                Some(self.spectrum_counter().to_string()),
            );
            self.append_key_value_metadata(
                SPECTRUM_DATA_POINT_COUNT.into(),
                Some(self.spectrum_data_buffers.point_count().to_string()),
            );

            writer = self.archive_writer.take().unwrap().into_inner()?;

            if !self.wavelength_spectrum_metadata_buffer.is_empty() {
                let encryption_props = self
                    .encryption_properties
                    .get(
                        FileEntry::from(MzPeakArchiveType::WavelengthSpectrumMetadata)
                            .name
                            .as_str(),
                    )
                    .cloned();
                let entry = FileEntry::new(
                    WAVELENGTH_SPECTRUM_METADATA_NAME.into(),
                    EntityType::WavelengthSpectrum,
                    DataKind::Metadata,
                );
                writer
                    .start_for_entry(entry)
                    .map_err(|e| io::Error::other(e))?;
                let metadata_fields = self.wavelength_spectrum_metadata_buffer.schema();
                self.archive_writer = Some(ArrowWriter::try_new_with_options(
                    writer,
                    metadata_fields.clone(),
                    ArrowWriterOptions::new().with_properties(
                        Self::spectrum_metadata_writer_props(&metadata_fields, encryption_props),
                    ),
                )?);

                self.append_key_value_metadata(
                    WAVELENGTH_SPECTRUM_DATA_POINT_COUNT.into(),
                    Some(
                        self.wavelength_spectrum_data_buffers
                            .as_ref()
                            .unwrap()
                            .point_count()
                            .to_string(),
                    ),
                );

                self.append_key_value_metadata(
                    WAVELENGTH_SPECTRUM_COUNT.into(),
                    Some(self.wavelength_spectrum_metadata_buffer.len().to_string()),
                );

                self.append_metadata();

                let arrays = self.wavelength_spectrum_metadata_buffer.finish();
                self.write_struct_arrays(arrays)?;
                writer = self.archive_writer.take().unwrap().into_inner()?;

                let entry = FileEntry::new(
                    WAVELENGTH_SPECTRUM_DATA_ARRAYS_NAME.into(),
                    EntityType::WavelengthSpectrum,
                    DataKind::DataArray,
                );
                writer
                    .start_for_entry(entry)
                    .map_err(|e| io::Error::other(e))?;

                let schema_props =
                    if let Some(buffers) = self.wavelength_spectrum_data_buffers.as_ref() {
                        let encryption_props = self
                            .encryption_properties
                            .get(
                                FileEntry::from(MzPeakArchiveType::WavelengthSpectrumDataArrays)
                                    .name
                                    .as_str(),
                            )
                            .cloned();
                        let schema = buffers.schema().clone();
                        let props = Self::generic_data_writer_props(
                            buffers.buffers(),
                            BufferContext::WavelengthSpectrum
                                .index_field()
                                .name()
                                .to_string(),
                            &buffers.use_chunked_encoding().copied(),
                            self.compression,
                            &["wavelength"],
                            encryption_props,
                        );
                        Some((schema, props))
                    } else {
                        None
                    };
                if let Some((schema, props)) = schema_props {
                    self.archive_writer = Some(ArrowWriter::try_new_with_options(
                        writer,
                        schema,
                        ArrowWriterOptions::new().with_properties(props),
                    )?);
                    self.append_key_value_metadata(
                        WAVELENGTH_SPECTRUM_DATA_POINT_COUNT.into(),
                        Some(
                            self.wavelength_spectrum_data_buffers
                                .as_ref()
                                .unwrap()
                                .point_count()
                                .to_string(),
                        ),
                    );
                    self.append_key_value_metadata(
                        WAVELENGTH_SPECTRUM_ARRAY_INDEX.into(),
                        Some(
                            self.wavelength_spectrum_data_buffers
                                .as_ref()
                                .unwrap()
                                .as_array_index()
                                .to_json(),
                        ),
                    );

                    let buffers = self.wavelength_spectrum_data_buffers.as_mut().unwrap();
                    buffers.drain_into(self.archive_writer.as_mut().unwrap())?;

                    writer = self.archive_writer.take().unwrap().into_inner()?;
                }
            }

            if !self.chromatogram_metadata_buffer.is_empty() {
                writer.start_chromatogram_metadata().unwrap();
                let metadata_fields = self.chromatogram_metadata_buffer.schema();
                let encryption_props = self
                    .encryption_properties
                    .get(
                        FileEntry::from(MzPeakArchiveType::ChromatogramMetadata)
                            .name
                            .as_str(),
                    )
                    .cloned();
                self.archive_writer = Some(ArrowWriter::try_new_with_options(
                    writer,
                    metadata_fields.clone(),
                    ArrowWriterOptions::new().with_properties(
                        Self::spectrum_metadata_writer_props(&metadata_fields, encryption_props),
                    ),
                )?);
                self.flush_chromatogram_metadata_records()?;
                self.append_key_value_metadata(
                    CHROMATOGRAM_COUNT.into(),
                    Some(self.chromatogram_counter().to_string()),
                );
                self.append_key_value_metadata(
                    CHROMATOGRAM_DATA_POINT_COUNT.into(),
                    Some(self.chromatogram_data_buffers.point_count().to_string()),
                );
                writer = self.archive_writer.take().unwrap().into_inner()?;
                let encryption_props = self
                    .encryption_properties
                    .get(
                        FileEntry::from(MzPeakArchiveType::ChromatogramDataArrays)
                            .name
                            .as_str(),
                    )
                    .cloned();
                writer.start_chromatogram_data().unwrap();
                self.archive_writer = Some(ArrowWriter::try_new_with_options(
                    writer,
                    self.chromatogram_data_buffers.schema().clone(),
                    ArrowWriterOptions::new().with_properties(
                        Self::chromatogram_data_writer_props(
                            &self.chromatogram_data_buffers,
                            BufferContext::Chromatogram.index_field().name().to_string(),
                            &None,
                            self.compression,
                            encryption_props,
                        ),
                    ),
                )?);
                self.flush_chromatogram_data_records()?;
                self.add_chromatogram_array_metadata();
                self.append_key_value_metadata(
                    CHROMATOGRAM_DATA_POINT_COUNT.into(),
                    Some(self.chromatogram_data_buffers.point_count().to_string()),
                );
                self.append_metadata();
                if let Err(e) = self.copy_metadata_to_index() {
                    log::error!("Failed to copy metadata to file index: {e}");
                }
                writer = self.archive_writer.take().unwrap().into_inner()?;
                writer.flush()?;
            }

            Ok(writer)
        } else {
            Err(parquet::errors::ParquetError::EOF(
                "Already closed file".into(),
            ))
        }
    }

    /// Finish writing Parquet files and metadata to the ZIP archive, flush the buffer,
    /// and return the ZIP archive writer.
    ///
    /// Use this method when you want to add additional files to the ZIP archive after
    /// the Parquet entries are finished.
    ///
    /// # Note
    /// It is the caller's responsibility to drop the returned [`ZipArchiveWriter`] to
    /// finish writing the ZIP archive
    pub fn finish_parquet(mut self) -> Result<ZipArchiveWriter<W>, parquet::errors::ParquetError> {
        self.finish_parquet_inner()
    }

    /// Finish writing the mzPeak archive, writing out the Parquet files, file index,
    /// and finalizes the ZIP archive.
    pub fn finish(&mut self) -> Result<(), parquet::errors::ParquetError> {
        if self.archive_writer.is_some() {
            let writer = self.finish_parquet_inner()?;
            writer.finish().unwrap();
            Ok(())
        } else {
            Err(parquet::errors::ParquetError::EOF(
                "Already closed file".into(),
            ))
        }
    }
}

impl<
    W: Write + Send + Seek,
    C: CentroidLike + ToMzPeakDataSeries,
    D: DeconvolutedCentroidLike + ToMzPeakDataSeries,
> Drop for MzPeakWriterType<W, C, D>
{
    fn drop(&mut self) {
        if let Err(e) = self.finish() {
            log::trace!("While dropping MzPeakWriterType: {e}")
        }
    }
}

impl<
    W: Write + Send + Seek,
    C: CentroidLike + ToMzPeakDataSeries,
    D: DeconvolutedCentroidLike + ToMzPeakDataSeries,
> SpectrumWriter<C, D> for MzPeakWriterType<W, C, D>
{
    fn write<S: SpectrumLike<C, D> + 'static>(&mut self, spectrum: &S) -> io::Result<usize> {
        self.write_spectrum(spectrum).map(|_| 1)
    }

    fn flush(&mut self) -> io::Result<()> {
        if let Some(w) = self.archive_writer.as_mut() {
            w.flush()?;
        }
        Ok(())
    }

    fn close(&mut self) -> io::Result<()> {
        self.finish()?;
        Ok(())
    }
}

pub type MzPeakWriter<W> = MzPeakWriterType<W, CentroidPeak, DeconvolutedPeak>;

#[cfg(test)]
mod test {
    use arrow::datatypes::{DataType, UInt64Type};
    use mzdata::{
        params::Unit,
        spectrum::{ArrayType, BinaryDataArrayType},
    };

    use crate::{
        BufferName, MzPeakReader, archive::FileEntry, buffer_descriptors::BufferPriority,
        peak_series::BufferFormat, reader::MzPeakSpectrumFacet,
    };

    use super::*;
    use std::io;

    #[test_log::test]
    fn test_array_type_sampling() -> io::Result<()> {
        let mut reader = mzdata::MZReader::open_path("small.mzML")?;
        let overrides1 = BufferOverrideTable::default();
        let array_types =
            sample_array_types_from_spectrum_source(&mut reader, &overrides1, None, false);

        assert_eq!(array_types.len(), 3);
        let mz_buffer = BufferName::new(
            BufferContext::Spectrum,
            ArrayType::MZArray,
            BinaryDataArrayType::Float64,
        )
        .with_unit(Unit::MZ);
        let intensity_buffer = BufferName::new(
            BufferContext::Spectrum,
            ArrayType::IntensityArray,
            BinaryDataArrayType::Float32,
        )
        .with_unit(Unit::DetectorCounts);
        for f in array_types {
            if let Some(name) = BufferName::from_field(BufferContext::Spectrum, f.clone()) {
                if mz_buffer == name {
                } else if intensity_buffer == name {
                } else {
                    panic!("Unexpected {name:?}");
                }
            }
        }

        let array_types = sample_array_types_from_spectrum_source(
            &mut reader,
            &overrides1,
            Some(ChunkingStrategy::Delta { chunk_size: 50.0 }),
            false,
        );

        assert_eq!(array_types.len(), 6);
        for f in array_types {
            if let Some(name) = BufferName::from_field(BufferContext::Spectrum, f.clone()) {
                if mz_buffer
                    .clone()
                    .with_format(BufferFormat::ChunkBoundsStart)
                    == name
                {
                } else if mz_buffer.clone().with_format(BufferFormat::ChunkBoundsEnd) == name {
                } else if mz_buffer.clone().with_format(BufferFormat::Chunk) == name {
                } else if mz_buffer.clone().with_format(BufferFormat::ChunkEncoding) == name {
                } else if intensity_buffer
                    .clone()
                    .with_format(BufferFormat::ChunkSecondary)
                    == name
                {
                } else {
                    panic!("Unexpected {name:?}");
                }
            }
        }

        let mut it = StreamingSpectrumIterator::new(reader.iter());

        let array_types = sample_array_types_from_spectrum_stream(&mut it, &overrides1, None);
        assert_eq!(array_types.len(), 3);
        for f in array_types {
            if let Some(name) = BufferName::from_field(BufferContext::Spectrum, f.clone()) {
                if mz_buffer == name {
                } else if intensity_buffer == name {
                } else {
                    panic!("Unexpected {name:?}");
                }
            }
        }

        let mut builder = MzPeakWriter::<io::Cursor<Vec<u8>>>::builder();
        builder = builder.sample_array_types_from_spectrum_stream(&mut it);
        if let DataType::Struct(fields) = builder.spectrum_arrays.dtype() {
            assert_eq!(fields.len(), 3);
        }
        Ok(())
    }

    #[test_log::test]
    #[test_log(default_log_filter = "debug")]
    fn test_array_building() -> io::Result<()> {
        let mut buf = io::Cursor::new(Vec::<u8>::with_capacity(2usize.pow(16u32)));
        let mut reader = mzdata::MZReader::open_path("small.mzML")?;
        let mut builder = MzPeakWriter::<io::Cursor<Vec<u8>>>::builder();
        builder = builder
            .register_spectrum_peak_type::<CentroidPeak>()
            .sample_array_types_from_spectrum_source(&mut reader)
            // Don't collect ANY chromatogram array types, forcing them into the auxiliary arrays
            // .sample_array_types_from_chromatograms(reader.iter_chromatograms())
            .include_time_with_spectrum_data(true)
            .null_zeros(true)
            .shuffle_mz(true)
            .write_batch_size(Some(5))
            .dictionary_page_size(Some(2usize.pow(16)))
            .row_group_size(Some(2usize.pow(16)))
            .page_size(Some(2usize.pow(16)))
            .add_spectrum_activation_field(
                CustomBuilderFromParameter::from_spec(
                    mzdata::curie!(MS:1000045),
                    "collision energy",
                    DataType::Float64,
                )
                .with_unit_fixed(Unit::Electronvolt.to_curie()),
            );

        let mut writer = builder.build(&mut buf, true);
        writer.copy_metadata_from(&reader);
        let overrides = writer.spectrum_data_buffers.overrides();
        assert!(overrides.iter().any(|(_, v)| {
            v.buffer_priority
                .is_some_and(|v| matches!(v, BufferPriority::Primary))
        }));
        writer.write_all_owned(reader.iter())?;
        for chrom in reader.iter_chromatograms() {
            writer.write_chromatogram(&chrom)?;
        }

        let mut zip_writer = writer.finish_parquet()?;

        zip_writer.start_other(&"example.config")?;
        zip_writer.write_all(b"<config><foo>some XML gobbledygook</foo></config>")?;

        let job_entry = FileEntry::new(
            "job.sig".into(),
            crate::archive::EntityType::Other("other".into()),
            crate::archive::DataKind::Proprietary,
        );
        zip_writer.add_file_from_read(
            &mut b"some binary sludge".as_slice(),
            None::<&String>,
            Some(job_entry),
        )?;

        zip_writer.finish()?;

        let mut new_reader = MzPeakReader::from_buf(buf.into_inner().into())?;
        assert_eq!(reader.len(), new_reader.len());
        assert!(
            new_reader
                .metadata
                .spectra
                .auxiliary_array_counts
                .iter()
                .all(|z| *z == 0)
        );
        assert_eq!(new_reader.list_all_files_in_archive().len(), 8);
        let mut buf = Vec::new();
        new_reader
            .open_stream("example.config")?
            .read_to_end(&mut buf)?;
        assert_eq!(buf, b"<config><foo>some XML gobbledygook</foo></config>");
        reader.reset();
        new_reader.reset();

        for s in new_reader.metadata.spectrum_array_indices().iter() {
            if matches!(s.array_type, ArrayType::MZArray) {
                assert_eq!(s.sorting_rank, Some(0))
            }
        }

        for (a, b) in reader.iter().zip(new_reader.iter()) {
            assert_eq!(a.id(), b.id());
        }

        let chrom = new_reader.get_chromatogram(0).unwrap();
        for (name, arr) in chrom.arrays.iter() {
            assert_eq!(
                arr.data_len(),
                Ok(48),
                "{name:?} was not decoded properly or it did not have 48 points"
            );
        }

        Ok(())
    }

    #[test_log::test]
    #[test_log(default_log_filter = "debug")]
    fn test_wavelengths() -> io::Result<()> {
        let mut buf = io::Cursor::new(Vec::<u8>::with_capacity(2usize.pow(16u32)));
        let mut reader = mzdata::MZReader::open_path(
            "test/data/TOFsulfasMS4GHzDualMode+DADSpectra+UVSignal272-NoProfile.mzML",
        )?;
        let builder = MzPeakWriter::<io::Cursor<Vec<u8>>>::builder();

        let mut writer = builder.build(&mut buf, true);
        writer.copy_metadata_from(&reader);
        let overrides = writer.spectrum_data_buffers.overrides();
        assert!(overrides.iter().any(|(_, v)| {
            v.buffer_priority
                .is_some_and(|v| matches!(v, BufferPriority::Primary))
        }));
        writer.write_all_owned(reader.iter())?;
        for chrom in reader.iter_chromatograms() {
            writer.write_chromatogram(&chrom)?;
        }

        writer.finish()?;
        drop(writer);

        let mut new_reader = MzPeakReader::from_buf(buf.into_inner().into())?;
        let wl_meta_entry = new_reader.file_index().iter().find(|entry| {
            entry.entity_type == EntityType::WavelengthSpectrum
                && entry.data_kind == DataKind::Metadata
        });
        assert!(wl_meta_entry.is_some());
        let wl_meta_entry = wl_meta_entry.unwrap();
        let batches = new_reader.open_parquet(&wl_meta_entry.name)?.build()?;
        for batch in batches {
            let batch = batch.unwrap();
            let spectra = batch.column(0).as_struct();
            let indices = spectra.column(0).as_primitive::<UInt64Type>();
            assert_eq!(indices.len(), 520);
            assert_eq!(arrow::compute::min(indices).unwrap(), 0);
            assert_eq!(arrow::compute::max(indices).unwrap(), 519);

            let spectra = batch.column(1).as_struct();
            let indices = spectra.column(0).as_primitive::<UInt64Type>();
            assert_eq!(indices.len(), 520);
            assert_eq!(arrow::compute::min(indices).unwrap(), 0);
            assert_eq!(arrow::compute::max(indices).unwrap(), 519);
        }

        let facet = new_reader.wavelength_facet(1).unwrap();
        let entry = facet.metadata().array_indices.get(&ArrayType::WavelengthArray).unwrap();
        assert_eq!(entry.sorting_rank, Some(0));

        let n = new_reader.len_wavelength_spectra();
        assert_eq!(n, 520);
        for spec in new_reader.iter_wavelength_spectra()? {
            let arrays = spec.raw_arrays().unwrap();
            assert!(arrays.has_array(&ArrayType::WavelengthArray));
        }
        Ok(())
    }

    #[test_log::test]
    #[test_log(default_log_filter = "debug")]
    fn test_array_building_chunked() -> io::Result<()> {
        let mut buf = io::Cursor::new(Vec::<u8>::with_capacity(2usize.pow(16u32)));

        let mut reader = mzdata::MZReader::open_path("small.mzML")?;
        let mut builder = MzPeakWriter::<io::Cursor<Vec<u8>>>::builder();
        builder = builder
            .chunked_encoding(Some(ChunkingStrategy::Delta { chunk_size: 50.0 }))
            .chromatogram_chunked_encoding(Some(ChunkingStrategy::Delta { chunk_size: 50.0 }))
            .sample_array_types_from_spectrum_source(&mut reader)
            .sample_array_types_from_chromatograms(reader.iter_chromatograms())
            .sample_array_types_for_peaks_from_spectrum_source(&mut reader)
            .null_zeros(true)
            .shuffle_mz(true);

        let mut writer = builder.build(&mut buf, true);
        writer.copy_metadata_from(&reader);
        let overrides = writer.spectrum_data_buffers.overrides();
        assert!(overrides.iter().any(|(_, v)| {
            v.buffer_priority
                .is_some_and(|v| matches!(v, BufferPriority::Primary))
        }));
        writer.write_all_owned(reader.iter().map(|mut s| {
            if matches!(s.signal_continuity(), SignalContinuity::Profile)
                && s.ms_level() == 1
                && s.index() < 10
            {
                s.pick_peaks(3.0).unwrap();
            }
            s
        }))?;
        for chrom in reader.iter_chromatograms() {
            writer.write_chromatogram(&chrom)?;
        }
        writer.finish()?;
        drop(writer);

        let mut new_reader = MzPeakReader::from_buf(buf.into_inner().into())?;
        assert_eq!(reader.len(), new_reader.len());
        assert!(new_reader.metadata.peak_array_indices().is_some());
        for s in new_reader.metadata.spectrum_array_indices().iter() {
            if matches!(s.array_type, ArrayType::MZArray) {
                assert_eq!(s.sorting_rank, Some(0))
            }
        }
        reader.reset();
        new_reader.reset();
        for (a, b) in reader.iter().zip(new_reader.iter()) {
            assert_eq!(a.id(), b.id());
        }
        Ok(())
    }

    #[test_log::test]
    #[test_log(default_log_filter = "debug")]
    fn test_array_building_numpress() -> io::Result<()> {
        let mut buf = io::Cursor::new(Vec::<u8>::with_capacity(2usize.pow(16u32)));

        let mut reader = mzdata::MZReader::open_path("small.mzML")?;
        let mut builder = MzPeakWriter::<fs::File>::builder();

        let spectrum_overrides = ArrayConversionHelper::new(false, true, false, true, true)
            .create_type_overrides(Some(ChunkingStrategy::NumpressLinear { chunk_size: 50.0 }));
        for (k, v) in spectrum_overrides.iter() {
            builder = builder.add_spectrum_array_override(k.clone(), v.clone())
        }
        builder = builder
            .shuffle_mz(true)
            .chunked_encoding(Some(ChunkingStrategy::NumpressLinear { chunk_size: 50.0 }))
            .chromatogram_chunked_encoding(Some(ChunkingStrategy::Delta { chunk_size: 50.0 }))
            .sample_array_types_from_spectrum_source(&mut reader)
            .sample_array_types_from_chromatograms(reader.iter_chromatograms());

        let mut writer = builder.build(&mut buf, true);
        writer.copy_metadata_from(&reader);
        let overrides = writer.spectrum_data_buffers.overrides();
        assert!(overrides.iter().any(|(_, v)| {
            v.buffer_priority
                .is_some_and(|v| matches!(v, BufferPriority::Primary))
        }));

        assert!(
            writer
                .spectrum_data_buffers
                .fields()
                .iter()
                .any(|f| f.name() == "mz_chunk_values")
        );
        assert!(
            writer
                .spectrum_data_buffers
                .fields()
                .iter()
                .any(|f| f.name() == "mz_numpress_linear_bytes")
        );
        assert!(
            writer
                .spectrum_data_buffers
                .fields()
                .iter()
                .any(|f| f.name() == "intensity_numpress_slof_bytes")
        );

        writer.write_all_owned(reader.iter())?;
        for chrom in reader.iter_chromatograms() {
            writer.write_chromatogram(&chrom)?;
        }
        drop(writer);

        let mut new_reader = MzPeakReader::from_buf(buf.into_inner().into())?;
        let array_indices = new_reader.metadata.spectrum_array_indices();
        for arr in array_indices.iter() {
            if arr.path.ends_with("mz_numpress_linear_bytes") {
                assert!(matches!(arr.buffer_format, BufferFormat::ChunkTransform));
            } else if arr.path.ends_with("intensity_numpress_slof_bytes") {
                assert!(matches!(arr.buffer_format, BufferFormat::ChunkTransform));
            } else if arr.path.ends_with("mz_chunk_values") {
                assert!(matches!(arr.buffer_format, BufferFormat::Chunk));
                assert_eq!(arr.sorting_rank, Some(0))
            }
        }
        assert_eq!(reader.len(), new_reader.len());
        reader.reset();
        new_reader.reset();
        for (a, b) in reader.iter().zip(new_reader.iter()) {
            assert_eq!(a.id(), b.id());
        }
        Ok(())
    }
}

use std::io::{self, prelude::*};

use mzdata::prelude::*; // ByteArrayView for `DataArray::data_len`
use mzdata::spectrum::RefPeakDataLevel;
use mzpeaks::{CentroidLike, DeconvolutedCentroidLike};
use parquet::{arrow::ArrowWriter, file::metadata::KeyValue};

use crate::{
    ToMzPeakDataSeries,
    peak_series::{ArrayIndex, array_map_to_schema_arrays_and_excess},
    writer::{ArrayBufferWriter, ArrayBufferWriterVariants, base::EntryMetadataDerivedFromData},
};

/// A small helper for writing peak list data to another stream with very narrow options.
pub struct MiniPeakWriterType<W: Write + Send + Seek> {
    writer: ArrowWriter<W>,
    buffers: ArrayBufferWriterVariants,
    buffer_size: usize,
    n_points: u64,
    n_entries: u64,
}

impl<W: Write + Send + Seek> MiniPeakWriterType<W> {
    pub fn new(
        writer: ArrowWriter<W>,
        buffers: ArrayBufferWriterVariants,
        buffer_size: usize,
    ) -> Self {
        let mut this = Self {
            writer,
            buffers,
            buffer_size,
            n_points: 0,
            n_entries: 0,
        };
        let spectrum_array_index: ArrayIndex = this.buffers.as_array_index();
        this.append_key_value_metadata(
            "spectrum_array_index".to_string(),
            Some(spectrum_array_index.to_json()),
        );
        this
    }

    pub fn append_key_value_metadata(
        &mut self,
        key: impl Into<String>,
        value: impl Into<Option<String>>,
    ) {
        self.writer
            .append_key_value_metadata(KeyValue::new(key.into(), value));
    }

    pub fn write_peaks<
        C: CentroidLike + ToMzPeakDataSeries,
        D: DeconvolutedCentroidLike + ToMzPeakDataSeries,
    >(
        &mut self,
        spectrum_count: u64,
        spectrum_time: Option<f32>,
        peaks: RefPeakDataLevel<C, D>,
    ) -> io::Result<EntryMetadataDerivedFromData> {
        let spectrum_time = if self.buffers.include_time() {
            spectrum_time
        } else {
            None
        };
        let n = peaks.len();
        log::trace!("Writing {n} peaks for {spectrum_count}");
        let (aux, n_peaks) = match peaks {
            RefPeakDataLevel::Centroid(peaks) => {
                self.buffers
                    .add(spectrum_count, spectrum_time, peaks.as_slice())
            }
            RefPeakDataLevel::Deconvoluted(peaks) => {
                self.buffers
                    .add(spectrum_count, spectrum_time, peaks.as_slice())
            }
            RefPeakDataLevel::Missing => unimplemented!(),
            RefPeakDataLevel::RawData(arrays) => {
                // GATED ims-chunked: if this facet is a ChunkBuffers with an m/z boundary, chunk the
                // raw `tof`/intensity/mobility arrays on m/z bins instead of storing flat points.
                // Returns `None` for every other facet, falling through to the normal point path.
                if let Some(res) =
                    self.buffers.add_raw_mz_boundary(spectrum_count, spectrum_time, arrays)
                {
                    let n_peaks = res.map_err(io::Error::other)?;
                    self.n_points += n_peaks as u64;
                    self.n_entries += 1;
                    if self.buffers.len() >= self.buffer_size
                        || self.buffers.memory_size()
                            >= *crate::writer::array_buffer::FLUSH_MEM_BYTES
                    {
                        self.flush()?;
                    }
                    return Ok(EntryMetadataDerivedFromData::new(
                        None,
                        Some(Vec::new()),
                        None,
                        Some(n_peaks),
                    ));
                }
                // `RefPeakDataLevel::len()` derives the point count from the m/z array, which is 0
                // for a custom peak facet that REPLACES m/z with a nonstandard main axis (e.g. an
                // integer `tof`/`tof_index` flight-time column). Fall back to the longest data array
                // so the primary length is correct and the columns land as typed columns instead of
                // spilling to auxiliary (the `primary_array_len == 0` aux path).
                let primary_len = if n == 0 {
                    arrays
                        .iter()
                        .filter_map(|(_, v)| v.data_len().ok())
                        .max()
                        .unwrap_or(0)
                } else {
                    n
                };
                let (fields, cols, aux) = array_map_to_schema_arrays_and_excess(
                    crate::BufferContext::Spectrum,
                    arrays,
                    primary_len,
                    spectrum_count,
                    spectrum_time,
                    Some(self.buffers.fields()),
                    self.buffers.overrides(),
                )?;
                let pts_written = self.buffers.add_arrays(fields, cols, primary_len, false);
                (aux, pts_written)
            }
        };

        // `n_peaks` is the count actually written (equals `n` for peak lists, or the recovered
        // primary length when m/z was replaced by a nonstandard main axis).
        self.n_points += n_peaks as u64;
        self.n_entries += 1;

        // Flush on the point count OR the measured byte size, whichever trips first.
        if self.buffers.len() >= self.buffer_size
            || self.buffers.memory_size() >= *crate::writer::array_buffer::FLUSH_MEM_BYTES
        {
            self.flush()?;
        }
        Ok(EntryMetadataDerivedFromData::new(
            None,
            Some(aux),
            None,
            Some(n_peaks),
        ))
    }

    pub fn flush(&mut self) -> io::Result<()> {
        for batch in self.buffers.drain() {
            self.writer.write(&batch)?;
            // Bound the in-progress row group by its real Parquet byte size. The peak/data facet
            // can accumulate a very large signal row group (timsTOF ims-compact, TOF-grid) under the
            // row-count cap alone, so mirror the main facet's reliable byte cap here. Use
            // `in_progress_size()` (the writer's true accounting), NOT arrow `memory_size()` which
            // under-reports. 64 MB target for the data facet.
            if self.writer.in_progress_size() > 64_000_000 {
                log::debug!(
                    "Flushing peak row group buffer with approximately {} bytes",
                    self.writer.in_progress_size()
                );
                self.writer.flush()?;
            }
        }
        Ok(())
    }

    pub fn finish(mut self) -> Result<W, parquet::errors::ParquetError> {
        self.append_key_value_metadata("spectrum_count", Some(self.n_entries.to_string()));
        self.append_key_value_metadata(
            "spectrum_data_point_count",
            Some(self.n_points.to_string()),
        );
        self.flush()?;
        self.writer.into_inner()
    }

    pub fn point_count(&self) -> u64 {
        self.n_points
    }

    pub fn n_entries(&self) -> u64 {
        self.n_entries
    }
}

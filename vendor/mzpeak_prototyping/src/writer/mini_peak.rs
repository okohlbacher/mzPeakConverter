use std::collections::VecDeque;
use std::io::{self, prelude::*};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;

use arrow::datatypes::SchemaRef;
use arrow::record_batch::RecordBatch;
use mzdata::prelude::*; // ByteArrayView for `DataArray::data_len`
use mzdata::spectrum::RefPeakDataLevel;
use mzpeaks::{CentroidLike, DeconvolutedCentroidLike};
use parquet::arrow::ArrowWriter;
use parquet::arrow::arrow_writer::{ArrowColumnChunk, ArrowRowGroupWriterFactory, compute_leaves};
use parquet::errors::{ParquetError, Result as ParquetResult};
use parquet::file::metadata::KeyValue;
use parquet::file::writer::SerializedFileWriter;

use crate::{
    ToMzPeakDataSeries,
    peak_series::{ArrayIndex, array_map_to_schema_arrays_and_excess},
    writer::{ArrayBufferWriter, ArrayBufferWriterVariants, base::EntryMetadataDerivedFromData},
};

/// The peak facet (`spectra_peaks.parquet`) is ~95% of the mzPeak output bytes and, in the
/// timsTOF ims-compact path, the single-threaded Arrow-encode + zstd of this facet is the entire
/// conversion wall (~97% on a 1.5 GB `.d`, all on ONE core while the perf cores idle). This module
/// parallelizes that encode across cores WITHOUT changing a single output byte:
///
/// * Row groups are cut at exactly `WriterProperties::max_row_group_size` rows (the parquet default,
///   1_048_576) — the same boundary the serial `ArrowWriter` uses internally. (The historical 64 MB
///   `in_progress_size()` byte trigger never fires here: a 1_048_576-row peak row group compresses to
///   ~8.6 MB, far under 64 MB, so the row cap always wins.) Because the boundaries, the column
///   encodings/props, and the value streams are all identical, the encoded bytes are identical too —
///   feeding a row group as one big batch or many small slices produces the same pages, since the
///   column writer buffers values and flushes pages by size, independent of batch boundaries.
/// * Each cut row group is encoded on a bounded worker pool (`ArrowRowGroupWriterFactory` +
///   `compute_leaves` + `ArrowColumnWriter`; zstd runs here, off the writer thread).
/// * Encoded row groups are appended in strict row-group-index order into a `SerializedFileWriter`
///   over the same sink (cheap serial I/O).
///
/// See [`ParallelPeakEncoder`]. The serial [`ArrowWriter`] path is retained verbatim as the
/// default-safe fallback (and for encrypted facets, where per-page nonces would defeat determinism).

/// A small helper for writing peak list data to another stream with very narrow options.
pub struct MiniPeakWriterType<W: Write + Send + Seek + 'static> {
    backend: PeakBackend<W>,
    buffers: ArrayBufferWriterVariants,
    buffer_size: usize,
    n_points: u64,
    n_entries: u64,
}

enum PeakBackend<W: Write + Send + Seek + 'static> {
    /// Original single-threaded high-level writer. Byte-identical reference path.
    Serial(ArrowWriter<W>),
    /// Cross-core row-group encoder (see module docs). Byte-identical to `Serial`.
    Parallel(ParallelPeakEncoder<W>),
}

impl<W: Write + Send + Seek + 'static> MiniPeakWriterType<W> {
    pub fn new(
        writer: ArrowWriter<W>,
        buffers: ArrayBufferWriterVariants,
        buffer_size: usize,
    ) -> Self {
        let mut this = Self {
            backend: PeakBackend::Serial(writer),
            buffers,
            buffer_size,
            n_points: 0,
            n_entries: 0,
        };
        this.init_array_index_metadata();
        this
    }

    /// Build a peak writer that encodes row groups across cores (see module docs). `max_rows` is the
    /// row-group boundary (`WriterProperties::max_row_group_size`); pass the value from the same
    /// properties used to build `file_writer`/`factory` so boundaries match the serial path exactly.
    pub fn new_parallel(
        file_writer: SerializedFileWriter<W>,
        factory: ArrowRowGroupWriterFactory,
        schema: SchemaRef,
        max_rows: usize,
        buffers: ArrayBufferWriterVariants,
        buffer_size: usize,
    ) -> Self {
        let encoder = ParallelPeakEncoder::new(file_writer, factory, schema, max_rows);
        let mut this = Self {
            backend: PeakBackend::Parallel(encoder),
            buffers,
            buffer_size,
            n_points: 0,
            n_entries: 0,
        };
        this.init_array_index_metadata();
        this
    }

    fn init_array_index_metadata(&mut self) {
        let spectrum_array_index: ArrayIndex = self.buffers.as_array_index();
        self.append_key_value_metadata(
            "spectrum_array_index".to_string(),
            Some(spectrum_array_index.to_json()),
        );
    }

    pub fn append_key_value_metadata(
        &mut self,
        key: impl Into<String>,
        value: impl Into<Option<String>>,
    ) {
        let kv = KeyValue::new(key.into(), value);
        match &mut self.backend {
            PeakBackend::Serial(writer) => writer.append_key_value_metadata(kv),
            PeakBackend::Parallel(enc) => enc.push_key_value_metadata(kv),
        }
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
        match &mut self.backend {
            PeakBackend::Serial(writer) => {
                for batch in self.buffers.drain() {
                    writer.write(&batch)?;
                    // Bound the in-progress row group by its real Parquet byte size. The peak/data
                    // facet can accumulate a very large signal row group (timsTOF ims-compact,
                    // TOF-grid) under the row-count cap alone, so mirror the main facet's reliable
                    // byte cap here. Use `in_progress_size()` (the writer's true accounting), NOT
                    // arrow `memory_size()` which under-reports. 64 MB target for the data facet.
                    if writer.in_progress_size() > 64_000_000 {
                        log::debug!(
                            "Flushing peak row group buffer with approximately {} bytes",
                            writer.in_progress_size()
                        );
                        writer.flush()?;
                    }
                }
                Ok(())
            }
            PeakBackend::Parallel(enc) => {
                for batch in self.buffers.drain() {
                    enc.add_batch(batch)?;
                }
                Ok(())
            }
        }
    }

    pub fn finish(mut self) -> Result<W, ParquetError> {
        self.append_key_value_metadata("spectrum_count", Some(self.n_entries.to_string()));
        self.append_key_value_metadata(
            "spectrum_data_point_count",
            Some(self.n_points.to_string()),
        );
        self.flush()?;
        match self.backend {
            PeakBackend::Serial(writer) => writer.into_inner(),
            PeakBackend::Parallel(enc) => enc.finish(),
        }
    }

    pub fn point_count(&self) -> u64 {
        self.n_points
    }

    pub fn n_entries(&self) -> u64 {
        self.n_entries
    }
}

// ---------------------------------------------------------------------------
// Parallel row-group encoder
// ---------------------------------------------------------------------------

type EncodeResult = ParquetResult<(usize, Vec<ArrowColumnChunk>)>;

/// Byte-budget backpressure gate. Bounds the total in-memory size of the input `RecordBatch`es for
/// row groups that are dispatched-but-not-yet-finished-encoding, so more cores never mean more
/// memory. Independent of the core count.
struct InFlight {
    bytes: Mutex<usize>,
    cv: Condvar,
    budget: usize,
}

impl InFlight {
    fn acquire(&self, want: usize) {
        let mut g = self.bytes.lock().unwrap();
        // Always admit at least one job (even a huge one) to avoid deadlock.
        while *g > 0 && *g + want > self.budget {
            g = self.cv.wait(g).unwrap();
        }
        *g += want;
    }

    fn release(&self, amount: usize) {
        let mut g = self.bytes.lock().unwrap();
        *g = g.saturating_sub(amount);
        self.cv.notify_all();
    }
}

/// Encodes the peak facet's row groups across a bounded worker pool and appends them in order.
///
/// Concurrency model:
/// * The caller (single writer thread) accumulates `RecordBatch`es and cuts a row group every
///   `max_rows` rows, splitting the boundary batch so each row group holds EXACTLY `max_rows` rows
///   (last one may be shorter) — reproducing the serial `ArrowWriter` boundaries byte-for-byte.
/// * Each cut row group is `pool.spawn`ed for encoding (create column writers for its final index,
///   write leaves, close → `Vec<ArrowColumnChunk>`; zstd happens here). A per-job oneshot channel
///   carries the result, and the oneshot's receiver is pushed onto an ordered `ready` channel in
///   dispatch (== row-group-index) order.
/// * A dedicated collector thread owns the `SerializedFileWriter`, pulls receivers off `ready` in
///   order, and appends each row group. Only the append + footer are serial; they are cheap.
struct ParallelPeakEncoder<W: Write + Send + Seek + 'static> {
    factory: Arc<ArrowRowGroupWriterFactory>,
    schema: SchemaRef,
    max_rows: usize,
    pending: VecDeque<RecordBatch>,
    pending_rows: usize,
    next_idx: usize,
    pool: rayon::ThreadPool,
    inflight: Arc<InFlight>,
    ready_tx: Option<Sender<Receiver<EncodeResult>>>,
    collector: Option<JoinHandle<ParquetResult<SerializedFileWriter<W>>>>,
    kv: Vec<KeyValue>,
    dead: bool,
}

fn detect_encode_threads() -> usize {
    for var in ["MZPC_ENCODE_THREADS", "RAYON_NUM_THREADS"] {
        if let Ok(v) = std::env::var(var) {
            if let Ok(n) = v.parse::<usize>() {
                if n > 0 {
                    return n;
                }
            }
        }
    }
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
}

fn detect_inflight_budget(threads: usize) -> usize {
    if let Ok(v) = std::env::var("MZPC_ENCODE_INFLIGHT_BYTES") {
        if let Ok(n) = v.parse::<usize>() {
            if n > 0 {
                return n;
            }
        }
    }
    // ~2 in-flight row groups per thread worth of input (a 1M-row peak row group is ~21 MB of Arrow
    // arrays), floored at 256 MB so small core counts still pipeline.
    (threads * 48 * 1024 * 1024).max(256 * 1024 * 1024)
}

impl<W: Write + Send + Seek + 'static> ParallelPeakEncoder<W> {
    fn new(
        file_writer: SerializedFileWriter<W>,
        factory: ArrowRowGroupWriterFactory,
        schema: SchemaRef,
        max_rows: usize,
    ) -> Self {
        let threads = detect_encode_threads();
        let budget = detect_inflight_budget(threads);
        if std::env::var("MZPC_TIMING")
            .map(|v| v != "0" && !v.is_empty())
            .unwrap_or(false)
        {
            eprintln!(
                "[timing] parallel peak encode: threads={threads} inflight_budget={}MB max_row_group_rows={max_rows}",
                budget >> 20
            );
        }
        // Dedicated pool: don't fight the decoder's global rayon pool (idle by the encode-bound tail,
        // but a private pool keeps the width honest and bounded to the detected/overridden count).
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .thread_name(|i| format!("mzpc-peak-encode-{i}"))
            .build()
            .expect("failed to build peak-encode thread pool");

        let (ready_tx, ready_rx) = channel::<Receiver<EncodeResult>>();
        let collector = std::thread::Builder::new()
            .name("mzpc-peak-collector".to_string())
            .spawn(move || -> ParquetResult<SerializedFileWriter<W>> {
                let mut fw = file_writer;
                // `ready_rx` yields per-row-group result receivers in strict dispatch order.
                while let Ok(result_rx) = ready_rx.recv() {
                    let (_idx, chunks) = match result_rx.recv() {
                        Ok(r) => r?,
                        Err(_) => {
                            return Err(ParquetError::General(
                                "peak-encode worker dropped before sending its row group".into(),
                            ));
                        }
                    };
                    let mut rgw = fw.next_row_group()?;
                    for chunk in chunks {
                        chunk.append_to_row_group(&mut rgw)?;
                    }
                    rgw.close()?;
                }
                Ok(fw)
            })
            .expect("failed to spawn peak-collector thread");

        Self {
            factory: Arc::new(factory),
            schema,
            max_rows,
            pending: VecDeque::new(),
            pending_rows: 0,
            next_idx: 0,
            pool,
            inflight: Arc::new(InFlight {
                bytes: Mutex::new(0),
                cv: Condvar::new(),
                budget,
            }),
            ready_tx: Some(ready_tx),
            collector: Some(collector),
            kv: Vec::new(),
            dead: false,
        }
    }

    fn push_key_value_metadata(&mut self, kv: KeyValue) {
        self.kv.push(kv);
    }

    fn add_batch(&mut self, batch: RecordBatch) -> io::Result<()> {
        let n = batch.num_rows();
        if n == 0 {
            return Ok(());
        }
        self.pending.push_back(batch);
        self.pending_rows += n;
        while self.pending_rows >= self.max_rows {
            self.cut_and_dispatch(self.max_rows)?;
        }
        Ok(())
    }

    /// Pull exactly `target` rows off the front of `pending` (splitting the boundary batch) and
    /// dispatch them as one row group.
    fn cut_and_dispatch(&mut self, target: usize) -> io::Result<()> {
        let mut out: Vec<RecordBatch> = Vec::new();
        let mut need = target;
        while need > 0 {
            let front = self
                .pending
                .pop_front()
                .expect("pending_rows accounting is out of sync with pending batches");
            let fr = front.num_rows();
            if fr <= need {
                need -= fr;
                self.pending_rows -= fr;
                out.push(front);
            } else {
                let head = front.slice(0, need);
                let tail = front.slice(need, fr - need);
                self.pending.push_front(tail);
                self.pending_rows -= need;
                need = 0;
                out.push(head);
            }
        }
        self.dispatch(out)
    }

    fn dispatch(&mut self, batches: Vec<RecordBatch>) -> io::Result<()> {
        if self.dead {
            return Err(io::Error::other(
                "peak-encode collector died; aborting (real error surfaces at finish)",
            ));
        }
        let idx = self.next_idx;
        self.next_idx += 1;
        let input_bytes: usize = batches.iter().map(|b| b.get_array_memory_size()).sum();

        // Backpressure: cap the total input bytes in flight (memory-bounded regardless of cores).
        self.inflight.acquire(input_bytes);

        let (result_tx, result_rx) = channel::<EncodeResult>();
        let factory = self.factory.clone();
        let schema = self.schema.clone();
        let inflight = self.inflight.clone();
        self.pool.spawn(move || {
            let res = encode_row_group(&factory, &schema, idx, batches);
            // `batches` consumed by `encode_row_group`; release its input budget now.
            inflight.release(input_bytes);
            let _ = result_tx.send(res);
        });

        // Preserve strict order for the collector. A send error means the collector thread exited
        // (an append/encode error); mark dead and let `finish` surface the real error via join.
        if self
            .ready_tx
            .as_ref()
            .expect("ready_tx present until finish")
            .send(result_rx)
            .is_err()
        {
            self.dead = true;
            self.inflight.release(input_bytes); // collector won't; avoid a stuck budget
        }
        Ok(())
    }

    fn finish(mut self) -> Result<W, ParquetError> {
        // Flush the remainder as the final (possibly short) row group.
        while self.pending_rows >= self.max_rows {
            self.cut_and_dispatch(self.max_rows)
                .map_err(|e| ParquetError::General(e.to_string()))?;
        }
        if self.pending_rows > 0 {
            let target = self.pending_rows;
            self.cut_and_dispatch(target)
                .map_err(|e| ParquetError::General(e.to_string()))?;
        }
        // Close the ordered channel so the collector's recv loop ends once drained, then join.
        drop(self.ready_tx.take());
        let collector = self.collector.take().expect("collector present until finish");
        let mut file_writer = collector
            .join()
            .map_err(|_| ParquetError::General("peak-collector thread panicked".into()))??;
        // Footer key/value metadata, in the same order the serial path appends it.
        for kv in self.kv.drain(..) {
            file_writer.append_key_value_metadata(kv);
        }
        file_writer.into_inner()
    }
}

/// Encode one row group: create column writers for its final `row_group_index`, write every batch's
/// leaves into them, and close into `ArrowColumnChunk`s. This mirrors the private
/// `ArrowRowGroupWriter::write`/`close` used by the high-level `ArrowWriter`, so the encoded output
/// is identical.
fn encode_row_group(
    factory: &ArrowRowGroupWriterFactory,
    schema: &SchemaRef,
    row_group_index: usize,
    batches: Vec<RecordBatch>,
) -> EncodeResult {
    let mut writers = factory.create_column_writers(row_group_index)?;
    for batch in &batches {
        let mut wi = writers.iter_mut();
        for (field, column) in schema.fields().iter().zip(batch.columns()) {
            for leaf in compute_leaves(field.as_ref(), column)? {
                wi.next()
                    .expect("column writer count matches schema leaf count")
                    .write(&leaf)?;
            }
        }
    }
    let chunks: Vec<ArrowColumnChunk> = writers
        .into_iter()
        .map(|w| w.close())
        .collect::<ParquetResult<_>>()?;
    Ok((row_group_index, chunks))
}

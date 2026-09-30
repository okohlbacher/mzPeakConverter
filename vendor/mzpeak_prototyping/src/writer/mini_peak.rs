use std::io::{self, prelude::*};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
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
use parquet::file::properties::WriterProperties;
use parquet::file::writer::SerializedFileWriter;

use crate::{
    ToMzPeakDataSeries, chunk_series::{ArrowArrayChunk, ChunkingStrategy}, peak_series::{ArrayIndex, array_map_to_schema_arrays_and_excess}, writer::{ArrayBufferWriter, ArrayBufferWriterVariants, base::EntryMetadataDerivedFromData, row_group::{RowGroupCut, RowGroupCutter, write_row_groups}},
};

/// The peak facet (`spectra_peaks.parquet`) is ~95% of the mzPeak output bytes and, in the
/// timsTOF ims-compact path, the single-threaded Arrow-encode + zstd of this facet is the entire
/// conversion wall (~97% on a 1.5 GB `.d`, all on ONE core while the perf cores idle). This module
/// parallelizes that encode across cores WITHOUT changing a single output byte:
///
/// * Row groups are cut by one [`RowGroupCutter`] for both backends: at exactly
///   `WriterProperties::max_row_group_row_count` rows (the boundary the serial `ArrowWriter` uses
///   internally) or at the byte cap, whichever comes first (see `row_group`). Because the
///   boundaries, the column encodings/props, and the value streams are all identical, the encoded
///   bytes are identical too — feeding a row group as one big batch or many small slices produces
///   the same pages, since the column writer buffers values and flushes pages by size, independent
///   of batch boundaries.
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
    /// The properties the facet was encoded with, kept so any post-hoc rewrite of the finished
    /// facet (see `prune_all_null_dup_point_columns`) can re-apply them instead of silently
    /// falling back to parquet's defaults (UNCOMPRESSED, no byte-stream-split, no encryption).
    props: WriterProperties,
    /// Where the facet's row groups end, for either backend. DELIBERATE DEVIATION (see `row_group`).
    row_groups: RowGroupCutter,
}

enum PeakBackend<W: Write + Send + Seek + 'static> {
    /// Original single-threaded high-level writer. Byte-identical reference path.
    Serial(ArrowWriter<W>),
    /// Cross-core row-group encoder (see module docs). Byte-identical to `Serial`.
    Parallel(ParallelPeakEncoder<W>),
}

impl<W: Write + Send + Seek + 'static> MiniPeakWriterType<W> {
    /// `max_row_group_bytes`: the byte cap of a row group (see `row_group`); its row cap is `props`'.
    pub fn new(
        writer: ArrowWriter<W>,
        buffers: ArrayBufferWriterVariants,
        buffer_size: usize,
        props: WriterProperties,
        max_row_group_bytes: usize,
    ) -> Self {
        let row_groups = RowGroupCutter::new(props.max_row_group_row_count(), max_row_group_bytes);
        let mut this = Self {
            backend: PeakBackend::Serial(writer),
            buffers,
            buffer_size,
            n_points: 0,
            n_entries: 0,
            props,
            row_groups,
        };
        this.init_array_index_metadata();
        this
    }

    /// Build a peak writer that encodes row groups across cores (see module docs). The row groups
    /// are cut as on the serial path: at `props`' row cap or at `max_row_group_bytes`; `props` must
    /// be the properties `file_writer`/`factory` were built with.
    pub fn new_parallel(
        file_writer: SerializedFileWriter<W>,
        factory: ArrowRowGroupWriterFactory,
        schema: SchemaRef,
        buffers: ArrayBufferWriterVariants,
        buffer_size: usize,
        props: WriterProperties,
        max_row_group_bytes: usize,
    ) -> Self {
        let row_groups = RowGroupCutter::new(props.max_row_group_row_count(), max_row_group_bytes);
        let encoder = ParallelPeakEncoder::new(
            file_writer,
            factory,
            schema,
            props.max_row_group_row_count(),
            max_row_group_bytes,
        );
        let mut this = Self {
            backend: PeakBackend::Parallel(encoder),
            buffers,
            buffer_size,
            n_points: 0,
            n_entries: 0,
            props,
            row_groups,
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

    pub fn buffers(&self) -> &ArrayBufferWriterVariants {
        &self.buffers
    }

    /// The [`WriterProperties`] this facet is being encoded with.
    pub fn properties(&self) -> &WriterProperties {
        &self.props
    }

    pub fn grid_policies(&self) -> Option<&std::collections::HashMap<mzdata::spectrum::ArrayType, crate::grid::GridPolicy>> {
        self.buffers().grid_policies()
    }

    pub fn grid_policies_mut(&mut self) -> Option<&mut std::collections::HashMap<mzdata::spectrum::ArrayType, crate::grid::GridPolicy>> {
        self.buffers.grid_policies_mut()
    }

    pub fn clear_current_grids(&mut self) {
        self.buffers.clear_current_grids();
    }

    pub fn use_chunked_encoding(&self) -> Option<&ChunkingStrategy> {
        self.buffers.chunking_strategy()
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
            // A spectrum with no signal at all is legitimate (newer-timsTOF blank frames), and it
            // reaches this writer whenever the metadata says peaks. Record an empty entry rather
            // than aborting the whole conversion on an `unimplemented!()`.
            RefPeakDataLevel::Missing => (Vec::new(), 0),
            RefPeakDataLevel::RawData(arrays) => {
                // GATED ims-chunked (0.12.x TOF layout, retires with backlog item 2): a ChunkBuffers
                // with an m/z boundary chunks the raw `tof`/intensity/mobility arrays on m/z bins.
                // `None` for every other facet.
                if let Some(res) = self
                    .buffers
                    .add_raw_mz_boundary(spectrum_count, spectrum_time, arrays)
                {
                    let (aux, n_peaks) = res.map_err(io::Error::other)?;
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
                        Some(aux),
                        None,
                        Some(n_peaks),
                    ));
                }
                if let Some(chunk_encoding) = self.use_chunked_encoding().cloned() {
                    let buffer = &mut self.buffers;
                    let (chunks, auxiliary_arrays, n_pts) = ArrowArrayChunk::build(
                        spectrum_count,
                        spectrum_time,
                        buffer.buffer_context(),
                        arrays,
                        &chunk_encoding,
                        buffer.overrides(),
                        buffer.drop_zero_intensity(),
                        buffer.nullify_zero_intensity(),
                        buffer.fields(),
                        buffer.grid_policies(),
                    )?;

                    if let Some(chunks) = chunks {
                        // The facet's point count is the number of POINTS, not of chunk rows
                        // (upstream counts `chunks.len()` here — the A16 defect, again).
                        let (fields, arrays, _nulls) = chunks.into_parts();
                        buffer.add_arrays(fields, arrays, n_pts, false);
                    }

                    (auxiliary_arrays, n_pts)
                } else {
                    // `RefPeakDataLevel::len()` derives the point count from the m/z array, which is 0
                    // for a custom peak facet whose main axis is not m/z (an integer `tof` column).
                    // Fall back to the longest data array so the columns land as typed columns
                    // instead of spilling to auxiliary (the `primary_array_len == 0` aux path).
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
        // Both backends cut the facet's row groups with the same `RowGroupCutter` (row cap or byte
        // cap), so they stay byte-identical. It replaces the serial path's former 64 MB flush on
        // `in_progress_size()`, a compressed-size estimate the parallel path never applied.
        match &mut self.backend {
            PeakBackend::Serial(writer) => {
                for batch in self.buffers.drain() {
                    write_row_groups(writer, &mut self.row_groups, batch, None)?;
                }
                Ok(())
            }
            PeakBackend::Parallel(enc) => {
                for batch in self.buffers.drain() {
                    for cut in self.row_groups.cut(batch) {
                        match cut {
                            RowGroupCut::Rows(rows) => enc.add_rows(rows),
                            RowGroupCut::Close => enc.close_row_group()?,
                        }
                    }
                }
                Ok(())
            }
        }
    }

    pub fn finish(mut self) -> Result<W, ParquetError> {
        // One past the largest spectrum index with a row in this facet (a zero-peak spectrum
        // handed to this writer does not raise it) — the same per-facet definition `spectra_data`
        // uses, and on a centroid-only run a bound that reaches its last non-empty spectrum.
        self.append_key_value_metadata("spectrum_count", Some(self.buffers.entry_count().to_string()));
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
///
/// DELIBERATE DEVIATION (see `row_group`): a row group is charged at most `max_charge`, the budget's
/// per-thread share. A group larger than the whole budget used to be admitted only once nothing else
/// was in flight, so a run of them (8192 timsTOF grid chunks, 270–460 MiB each on PXD076703)
/// encoded one at a time on one core while the other workers idled — 2 h 20 min for a 10 GB `.d`.
/// Capped, up to one such group per worker is in flight: memory stays within
/// `max(budget, threads × largest group)`, and the byte cap of the row groups bounds the latter.
struct InFlight {
    bytes: Mutex<usize>,
    cv: Condvar,
    budget: usize,
    max_charge: usize,
}

impl InFlight {
    fn new(budget: usize, threads: usize) -> Self {
        Self {
            bytes: Mutex::new(0),
            cv: Condvar::new(),
            budget,
            max_charge: (budget / threads.max(1)).max(1),
        }
    }

    /// Admit a row group of `want` input bytes; returns the charge to [`Self::release`].
    fn acquire(&self, want: usize) -> usize {
        let want = want.min(self.max_charge);
        let mut g = self.bytes.lock().unwrap();
        // Always admit at least one job to avoid deadlock.
        while *g > 0 && *g + want > self.budget {
            g = self.cv.wait(g).unwrap();
        }
        *g += want;
        want
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
/// * The caller (single writer thread) accumulates the rows of the open row group and dispatches it
///   where the facet's [`RowGroupCutter`] closes it — the boundaries the serial `ArrowWriter` path
///   gets from the same cutter, byte-for-byte.
/// * Each cut row group is `pool.spawn`ed for encoding (create column writers for its final index,
///   write leaves, close → `Vec<ArrowColumnChunk>`; zstd happens here). A per-job oneshot channel
///   carries the result, and the oneshot's receiver is pushed onto an ordered `ready` channel in
///   dispatch (== row-group-index) order.
/// * A dedicated collector thread owns the `SerializedFileWriter`, pulls receivers off `ready` in
///   order, and appends each row group. Only the append + footer are serial; they are cheap.
struct ParallelPeakEncoder<W: Write + Send + Seek + 'static> {
    factory: Arc<ArrowRowGroupWriterFactory>,
    schema: SchemaRef,
    pending: Vec<RecordBatch>,
    next_idx: usize,
    pool: rayon::ThreadPool,
    inflight: Arc<InFlight>,
    ready_tx: Option<Sender<Receiver<EncodeResult>>>,
    collector: Option<JoinHandle<ParquetResult<SerializedFileWriter<W>>>>,
    kv: Vec<KeyValue>,
    dead: bool,
    /// `$MZPC_TIMING` only: how busy the workers were, reported at `finish`.
    stats: Option<Arc<EncodeStats>>,
}

/// Worker occupancy of the parallel encode, for the `[timing]` report: the summed encode time of
/// every row group against the wall time from the first dispatch to `finish` is the mean number of
/// busy workers.
struct EncodeStats {
    first_dispatch: std::sync::OnceLock<std::time::Instant>,
    encode_nanos: AtomicU64,
    running: AtomicUsize,
    peak_running: AtomicUsize,
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
        max_rows: Option<usize>,
        max_bytes: usize,
    ) -> Self {
        let threads = detect_encode_threads();
        let budget = detect_inflight_budget(threads);
        let timing = std::env::var("MZPC_TIMING")
            .map(|v| v != "0" && !v.is_empty())
            .unwrap_or(false);
        if timing {
            eprintln!(
                "[timing] parallel peak encode: threads={threads} inflight_budget={}MB max_row_group_rows={} max_row_group_bytes={:.1}MB",
                budget >> 20,
                max_rows.map_or("unlimited".to_string(), |n| n.to_string()),
                max_bytes as f64 / (1024.0 * 1024.0)
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
            pending: Vec::new(),
            next_idx: 0,
            pool,
            inflight: Arc::new(InFlight::new(budget, threads)),
            ready_tx: Some(ready_tx),
            collector: Some(collector),
            kv: Vec::new(),
            dead: false,
            stats: timing.then(|| {
                Arc::new(EncodeStats {
                    first_dispatch: std::sync::OnceLock::new(),
                    encode_nanos: AtomicU64::new(0),
                    running: AtomicUsize::new(0),
                    peak_running: AtomicUsize::new(0),
                })
            }),
        }
    }

    fn push_key_value_metadata(&mut self, kv: KeyValue) {
        self.kv.push(kv);
    }

    /// Append rows to the open row group.
    fn add_rows(&mut self, rows: RecordBatch) {
        self.pending.push(rows);
    }

    /// Dispatch the open row group for encoding (a no-op when it is empty).
    fn close_row_group(&mut self) -> io::Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let batches = std::mem::take(&mut self.pending);
        self.dispatch(batches)
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
        let charge = self.inflight.acquire(input_bytes);

        let (result_tx, result_rx) = channel::<EncodeResult>();
        let factory = self.factory.clone();
        let schema = self.schema.clone();
        let inflight = self.inflight.clone();
        let stats = self.stats.clone();
        if let Some(s) = &stats {
            s.first_dispatch.get_or_init(std::time::Instant::now);
        }
        self.pool.spawn(move || {
            let t0 = stats.as_ref().map(|s| {
                let now = s.running.fetch_add(1, Ordering::SeqCst) + 1;
                s.peak_running.fetch_max(now, Ordering::SeqCst);
                std::time::Instant::now()
            });
            let res = encode_row_group(&factory, &schema, idx, batches);
            if let (Some(s), Some(t0)) = (stats.as_ref(), t0) {
                s.encode_nanos.fetch_add(t0.elapsed().as_nanos() as u64, Ordering::SeqCst);
                s.running.fetch_sub(1, Ordering::SeqCst);
            }
            // `batches` consumed by `encode_row_group`; release its input budget now.
            inflight.release(charge);
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
            self.inflight.release(charge); // collector won't; avoid a stuck budget
        }
        Ok(())
    }

    fn finish(mut self) -> Result<W, ParquetError> {
        // Flush the remainder as the final (possibly short) row group.
        self.close_row_group()
            .map_err(|e| ParquetError::General(e.to_string()))?;
        // Close the ordered channel so the collector's recv loop ends once drained, then join.
        drop(self.ready_tx.take());
        let collector = self.collector.take().expect("collector present until finish");
        let mut file_writer = collector
            .join()
            .map_err(|_| ParquetError::General("peak-collector thread panicked".into()))??;
        if let Some(s) = &self.stats {
            let wall = s.first_dispatch.get().map_or(0.0, |t| t.elapsed().as_secs_f64());
            let busy = s.encode_nanos.load(Ordering::SeqCst) as f64 / 1e9;
            eprintln!(
                "[timing] parallel peak encode: {} row groups, {busy:.1} s of encoding in {wall:.1} s \
                 ({:.1} workers busy on average, at most {} at once)",
                self.next_idx,
                if wall > 0.0 { busy / wall } else { 0.0 },
                s.peak_running.load(Ordering::SeqCst)
            );
        }
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

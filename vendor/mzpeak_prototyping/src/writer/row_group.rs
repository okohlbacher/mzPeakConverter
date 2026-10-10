//! Row-group boundaries for the signal facets, bounded by bytes as well as rows.
//!
//! DELIBERATE DEVIATION (not upstream; the `WriteBatchConfig` knob of BACKLOG "Vendoring exit"
//! item 4, a candidate for an upstream proposal). Upstream caps a signal row group by its ROW count
//! alone, and a row is a whole chunk in the chunked layout: 8192 timsTOF grid chunks came to
//! 270–460 MiB of uncompressed pages per group (PXD076703), and a Shimadzu profile facet sat in one
//! 85 MiB group. Parquet reads a row group at a time, so every random spectrum read decoded all of
//! it, and the parallel peak encoder, whose in-flight budget one such group exceeded, encoded them
//! one at a time on one core. The cutter here closes a group at the row cap OR at a byte cap,
//! whichever comes first, from the input batches alone, so the serial and the parallel encoder cut
//! at the same rows and their output stays byte-identical.

use std::io::Write;

use arrow::array::{Array, AsArray, RecordBatch};
use arrow::datatypes::{DataType, UInt64Type};
use parquet::file::properties::WriterProperties;
use parquet::arrow::ArrowWriter;
use parquet::errors::Result as ParquetResult;

/// Default byte cap of one signal-facet row group: 48 MiB of Arrow buffers.
///
/// The validator's `data_row_group_not_monolithic` advisory fires above 64 MiB (67,108,864 bytes)
/// of a group's uncompressed Parquet pages (`total_byte_size`). Those pages are never larger than
/// the Arrow buffers they encode: measured over 63 corpus facets (every lane, point and chunk
/// layouts), `total_byte_size` / Arrow bytes was 0.20–0.986 (0.38–0.986 on the chunk facets; the
/// low end is dictionary and delta encodings). Three quarters of the threshold leaves
/// a quarter for what the estimate cannot see (definition/repetition levels, all-null columns), and
/// it is the per-thread share of the parallel peak encoder's budget (`threads × 48 MiB`), so each
/// encode worker holds one group. `$MZPC_ROW_GROUP_MB` overrides it for measurements.
pub const DEFAULT_ROW_GROUP_BYTES: usize = 48 * 1024 * 1024;

/// The byte cap a writer uses: the configured one, else `$MZPC_ROW_GROUP_MB` (MiB, fractions
/// allowed), else [`DEFAULT_ROW_GROUP_BYTES`].
pub fn row_group_max_bytes(configured: Option<usize>) -> usize {
    configured
        .or_else(|| {
            std::env::var("MZPC_ROW_GROUP_MB")
                .ok()
                .and_then(|v| v.trim().parse::<f64>().ok())
                .filter(|mb| mb.is_finite() && *mb > 0.0)
                .map(|mb| (mb * 1024.0 * 1024.0) as usize)
        })
        .filter(|&n| n > 0)
        .unwrap_or(DEFAULT_ROW_GROUP_BYTES)
}

/// The bytes of Arrow buffers `batch`'s rows occupy: a slice counts only its own range and a list
/// only the child values its offsets reach, so a piece of a larger batch is not charged for the
/// whole of it (`get_array_memory_size` is, and counts spare capacity besides).
pub fn batch_bytes(batch: &RecordBatch) -> usize {
    batch.columns().iter().map(|c| array_bytes(c.as_ref())).sum()
}

fn array_bytes(array: &dyn Array) -> usize {
    let len = array.len();
    let nulls = array.nulls().map_or(0, |_| len.div_ceil(8));
    match array.data_type() {
        DataType::Null => 0,
        DataType::Struct(_) => {
            nulls + array.as_struct().columns().iter().map(|c| array_bytes(c.as_ref())).sum::<usize>()
        }
        DataType::List(_) => {
            let list = array.as_list::<i32>();
            let offsets = list.value_offsets();
            let (start, end) = (offsets[0] as usize, offsets[len] as usize);
            nulls + 4 * len + array_bytes(list.values().slice(start, end - start).as_ref())
        }
        DataType::LargeList(_) => {
            let list = array.as_list::<i64>();
            let offsets = list.value_offsets();
            let (start, end) = (offsets[0] as usize, offsets[len] as usize);
            nulls + 8 * len + array_bytes(list.values().slice(start, end - start).as_ref())
        }
        // Fixed-width and string/binary values: exactly the sliced range (validity included).
        _ => array
            .to_data()
            .get_slice_memory_size()
            .unwrap_or_else(|_| array.get_array_memory_size()),
    }
}

/// One step of [`RowGroupCutter::cut`].
#[derive(Debug)]
pub enum RowGroupCut {
    /// Rows to append to the open row group.
    Rows(RecordBatch),
    /// Close the open row group.
    Close,
}

/// Decides where the row groups of one facet end, from the batches written to it: a group closes
/// once it holds `max_rows` rows or once the next rows would take it past `max_bytes` of
/// [`batch_bytes`] — but ONLY between two entries (spectra, chromatograms). Every group holds
/// whole entries: the archive declares that in `mzpeak_index.json` (`row_groups:
/// "spectrum-aligned"`, mzPeakConverter), so a reader can decode row groups in parallel without
/// looking for an entry carried over from the previous group. An entry larger than a whole group
/// gets a group of its own that exceeds the caps instead of being split; a group that is full at
/// the end of a batch is closed only once the next batch shows the next entry starts there.
///
/// The entry is the first integer `*_index` child of the first struct column (`point.spectrum_index`,
/// `chunk.spectrum_index`, `chromatogram_index`, ...). A batch without one is cut as before, on the
/// caps alone. The writer the cuts feed must not cut by rows itself: see [`unbounded_rows`].
#[derive(Debug, Clone)]
pub struct RowGroupCutter {
    max_rows: usize,
    max_bytes: usize,
    rows: usize,
    bytes: usize,
    /// The entry of the open group's last row.
    last_key: Option<u64>,
    /// The open group is full (or a size estimate asked for a flush): close it at the next entry
    /// boundary.
    pending_close: bool,
}

/// `props` with the row cap removed, for the `ArrowWriter` a [`RowGroupCutter`] feeds: the cutter
/// applies the cap itself, at entry boundaries, and parquet's own cut at exactly `max_rows` rows
/// would split an entry that crosses it.
pub fn unbounded_rows(props: WriterProperties) -> WriterProperties {
    props.into_builder().set_max_row_group_row_count(None).build()
}

/// The entry key of every row of `batch`, or `None` when it has no entry column.
fn entry_keys(batch: &RecordBatch) -> Option<Vec<u64>> {
    let column = batch.columns().iter().find_map(|c| match c.data_type() {
        DataType::Struct(fields) => {
            let s = c.as_struct();
            fields
                .iter()
                .position(|f| f.name().ends_with("_index") && f.data_type().is_integer())
                .map(|k| s.column(k).clone())
        }
        _ => None,
    })?;
    let keys = arrow::compute::cast(&column, &DataType::UInt64).ok()?;
    let keys = keys.as_primitive::<UInt64Type>();
    // A null key (no entry) never equals its neighbour, so every such row is a boundary.
    Some((0..keys.len()).map(|i| if keys.is_null(i) { u64::MAX } else { keys.value(i) }).collect())
}

impl RowGroupCutter {
    /// `max_rows` as `WriterProperties::max_row_group_row_count` (`None` = unlimited).
    pub fn new(max_rows: Option<usize>, max_bytes: usize) -> Self {
        Self {
            max_rows: max_rows.unwrap_or(usize::MAX).max(1),
            max_bytes: max_bytes.max(1),
            rows: 0,
            bytes: 0,
            last_key: None,
            pending_close: false,
        }
    }

    /// The open group was closed elsewhere (a writer's own flush).
    pub fn reset(&mut self) {
        self.rows = 0;
        self.bytes = 0;
        self.last_key = None;
        self.pending_close = false;
    }

    /// Rows in the open group.
    pub fn rows(&self) -> usize {
        self.rows
    }

    /// Close the open group at the next entry boundary.
    pub fn request_close(&mut self) {
        if self.rows > 0 {
            self.pending_close = true;
        }
    }

    /// Feed one batch: its rows, in order, with a [`RowGroupCut::Close`] wherever a group ends.
    pub fn cut(&mut self, mut batch: RecordBatch) -> Vec<RowGroupCut> {
        let mut out = Vec::new();
        let mut keys = entry_keys(&batch);
        while batch.num_rows() > 0 {
            let n = batch.num_rows();
            // May a group end before row `r` of the remaining batch?
            let boundary = |keys: &Option<Vec<u64>>, last: Option<u64>, r: usize| match keys {
                None => true,
                Some(k) if r == 0 => last != Some(k[0]),
                Some(k) => k[r] != k[r - 1],
            };
            let bytes = batch_bytes(&batch);
            if self.rows > 0
                && (self.pending_close || self.rows >= self.max_rows || self.bytes + bytes > self.max_bytes)
                && boundary(&keys, self.last_key, 0)
            {
                out.push(RowGroupCut::Close);
                self.reset();
            }
            // The rows the caps leave room for.
            let room_bytes = self.max_bytes.saturating_sub(self.bytes);
            let mut take = n.min(self.max_rows.saturating_sub(self.rows));
            if self.pending_close {
                // Still open: row 0 continues its entry, which ends the group.
                take = 0;
            }
            if take > 0 && bytes > room_bytes {
                // A piece by its mean row size, halved until it fits.
                take = take.min(((room_bytes as u128 * n as u128) / bytes as u128) as usize);
                while take > 0 && batch_bytes(&batch.slice(0, take)) > room_bytes {
                    take /= 2;
                }
            }
            // Move the cut onto an entry boundary: back to the last one at or before it, else (the
            // entry the group is on does not fit) forward to the next one, or the batch's end.
            if take < n {
                take = match (1..=take).rev().find(|&r| boundary(&keys, self.last_key, r)) {
                    Some(r) => r,
                    None => (take.max(1)..n).find(|&r| boundary(&keys, self.last_key, r)).unwrap_or(n),
                };
            }
            if take == 0 {
                // Nothing fits and row 0 continues the open entry: it cannot close here.
                take = (1..n).find(|&r| boundary(&keys, self.last_key, r)).unwrap_or(n);
            }
            let head = if take == n { batch.clone() } else { batch.slice(0, take) };
            let take_bytes = if take == n { bytes } else { batch_bytes(&head) };
            self.last_key = keys.as_ref().map(|k| k[take - 1]);
            if let Some(k) = keys.as_mut() {
                k.drain(..take);
            }
            batch = batch.slice(take, n - take);
            out.push(RowGroupCut::Rows(head));
            self.rows += take;
            self.bytes += take_bytes;
            if batch.num_rows() > 0 {
                // The remaining rows start an entry (the cut sits on a boundary).
                out.push(RowGroupCut::Close);
                self.reset();
            } else if self.rows >= self.max_rows || self.bytes >= self.max_bytes {
                self.pending_close = true;
            }
        }
        out
    }
}

/// Write `batch` into `writer` along `cutter`'s row groups. `flush_at_encoded_bytes` keeps a
/// facet's existing additional flush on the writer's own (compressed) size estimate; it closes the
/// group at the next entry boundary, through the cutter, so it never splits an entry either.
pub fn write_row_groups<W: Write + Send>(
    writer: &mut ArrowWriter<W>,
    cutter: &mut RowGroupCutter,
    batch: RecordBatch,
    flush_at_encoded_bytes: Option<usize>,
) -> ParquetResult<()> {
    for cut in cutter.cut(batch) {
        match cut {
            RowGroupCut::Rows(rows) => {
                writer.write(&rows)?;
                if flush_at_encoded_bytes.is_some_and(|max| writer.in_progress_size() > max) {
                    log::debug!(
                        "Closing the row group at the next entry: approximately {} bytes",
                        writer.in_progress_size()
                    );
                    cutter.request_close();
                }
            }
            RowGroupCut::Close => writer.flush()?,
        }
    }
    Ok(())
}

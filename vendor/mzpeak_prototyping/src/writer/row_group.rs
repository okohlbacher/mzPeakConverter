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
use arrow::datatypes::DataType;
use parquet::arrow::ArrowWriter;
use parquet::errors::Result as ParquetResult;

/// Default byte cap of one signal-facet row group: 48 MiB of Arrow buffers.
///
/// The validator's `data_row_group_not_monolithic` advisory fires above 64 MiB (67,108,864 bytes)
/// of a group's uncompressed Parquet pages (`total_byte_size`). Those pages are never larger than
/// the Arrow buffers they encode: measured over 63 corpus facets (every lane, point and chunk
/// layouts), `total_byte_size` / Arrow bytes was 0.80–0.986. Three quarters of the threshold leaves
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
/// once it holds `max_rows` rows (the writer's own row cap, cut exactly there as parquet does) or
/// once the next batch would take it past `max_bytes` of [`batch_bytes`]. A batch that does not fit
/// the open group starts the next one instead of being split, so the byte rule keeps a spectrum in
/// one group; only a batch larger than a whole group is split, into pieces of at most `max_bytes`
/// (down to a single row).
#[derive(Debug, Clone)]
pub struct RowGroupCutter {
    max_rows: usize,
    max_bytes: usize,
    rows: usize,
    bytes: usize,
}

impl RowGroupCutter {
    /// `max_rows` as `WriterProperties::max_row_group_row_count` (`None` = unlimited).
    pub fn new(max_rows: Option<usize>, max_bytes: usize) -> Self {
        Self {
            max_rows: max_rows.unwrap_or(usize::MAX).max(1),
            max_bytes: max_bytes.max(1),
            rows: 0,
            bytes: 0,
        }
    }

    /// The open group was closed elsewhere (a writer's own flush).
    pub fn reset(&mut self) {
        self.rows = 0;
        self.bytes = 0;
    }

    /// Rows in the open group.
    pub fn rows(&self) -> usize {
        self.rows
    }

    /// Feed one batch: its rows, in order, with a [`RowGroupCut::Close`] wherever a group ends.
    pub fn cut(&mut self, mut batch: RecordBatch) -> Vec<RowGroupCut> {
        let mut out = Vec::new();
        while batch.num_rows() > 0 {
            let n = batch.num_rows();
            let bytes = batch_bytes(&batch);
            if self.rows > 0 && self.bytes + bytes > self.max_bytes {
                out.push(RowGroupCut::Close);
                self.reset();
            }
            let mut take = n.min(self.max_rows - self.rows);
            let mut take_bytes = bytes;
            if bytes > self.max_bytes {
                // Larger than a whole group: a piece by its mean row size, halved until it fits.
                take = take.min(((self.max_bytes as u128 * n as u128) / bytes as u128).max(1) as usize);
                loop {
                    take_bytes = batch_bytes(&batch.slice(0, take));
                    if take_bytes <= self.max_bytes || take == 1 {
                        break;
                    }
                    take /= 2;
                }
            } else if take < n {
                take_bytes = batch_bytes(&batch.slice(0, take));
            }
            let head = if take == n { batch.clone() } else { batch.slice(0, take) };
            batch = batch.slice(take, n - take);
            out.push(RowGroupCut::Rows(head));
            self.rows += take;
            self.bytes += take_bytes;
            if self.rows >= self.max_rows || self.bytes >= self.max_bytes || batch.num_rows() > 0 {
                out.push(RowGroupCut::Close);
                self.reset();
            }
        }
        out
    }
}

/// Write `batch` into `writer` along `cutter`'s row groups. `flush_at_encoded_bytes` keeps a
/// facet's existing additional flush on the writer's own (compressed) size estimate; a group it
/// closes resets the cutter so the two stay in step.
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
                        "Flushing row group buffer with approximately {} bytes",
                        writer.in_progress_size()
                    );
                    writer.flush()?;
                }
            }
            RowGroupCut::Close => writer.flush()?,
        }
    }
    if writer.in_progress_rows() == 0 {
        cutter.reset();
    }
    Ok(())
}

//! A data facet WITHOUT a Parquet page index must still read.
//!
//! parquet-rs always writes the offset/column index; pyarrow (and other writers) do not unless asked.
//! The vendored reader located a spectrum's rows purely through that index: absent, every lookup was an
//! EMPTY row selection and every spectrum came back with zero points — exit 0, no error (found 2026-09-23
//! on a pyarrow-written chunk facet). The reader now falls back to one entry per row group built from the
//! column-chunk statistics. This test rewrites both data facets of a small archive with the page index
//! disabled and one row per row group, then compares every spectrum with the original.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use arrow::datatypes::Schema;
use mzpeak_prototyping::MzPeakReader;
use parquet::arrow::{ArrowWriter, arrow_reader::ParquetRecordBatchReaderBuilder};
use parquet::file::metadata::KeyValue;
use parquet::file::properties::{EnabledStatistics, WriterProperties};
use parquet::file::reader::{FileReader, SerializedFileReader};

const FACETS: [&str; 2] = ["spectra_data.parquet", "spectra_peaks.parquet"];

fn scratch() -> PathBuf {
    let d = std::env::temp_dir().join(format!("mzpc-nopageindex-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// The facet re-encoded by parquet-rs with NO page index and ONE row per row group (so the row-group
/// fallback has several entries to get right), key-value metadata preserved.
fn strip_page_index(parquet: &[u8]) -> Vec<u8> {
    let builder = ParquetRecordBatchReaderBuilder::try_new(bytes::Bytes::copy_from_slice(parquet)).unwrap();
    // The facet's key-value metadata (`spectrum_array_index`, the counters) is what makes it a facet.
    // `ArrowWriter` does not turn arrow schema metadata into file key-values (it only embeds the
    // encoded schema), so the pairs go through the writer properties explicitly.
    let kvs: Vec<KeyValue> = builder.metadata().file_metadata().key_value_metadata().into_iter().flatten()
        .filter(|kv| kv.key != "ARROW:schema")
        .cloned()
        .collect();
    assert!(kvs.iter().any(|kv| kv.key == "spectrum_array_index"), "source facet has no array index");
    let schema = Arc::new(Schema::new(builder.schema().fields().clone()));
    let batches: Vec<_> = builder.build().unwrap().map(|b| b.unwrap()).collect();
    let props = WriterProperties::builder()
        .set_offset_index_disabled(true)
        .set_statistics_enabled(EnabledStatistics::Chunk)
        .set_max_row_group_row_count(Some(1))
        .set_key_value_metadata(Some(kvs))
        .build();
    let mut out = Vec::new();
    let mut w = ArrowWriter::try_new(&mut out, schema, Some(props)).unwrap();
    for b in &batches {
        w.write(b).unwrap();
    }
    w.close().unwrap();
    let meta = SerializedFileReader::new(bytes::Bytes::from(out.clone())).unwrap();
    let meta = meta.metadata();
    assert!(meta.offset_index().is_none() && meta.column_index().is_none(), "the rewrite still carries a page index");
    assert_eq!(meta.num_row_groups(), batches.iter().map(|b| b.num_rows()).sum::<usize>(), "one row per row group");
    out
}

fn rewrite_archive(src: &Path, dst: &Path) {
    let mut zin = zip::ZipArchive::new(std::fs::File::open(src).unwrap()).unwrap();
    let mut zout = zip::ZipWriter::new(std::fs::File::create(dst).unwrap());
    for i in 0..zin.len() {
        let mut entry = zin.by_index(i).unwrap();
        let name = entry.name().to_string();
        if FACETS.contains(&name.as_str()) {
            let mut raw = Vec::new();
            entry.read_to_end(&mut raw).unwrap();
            let opts = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
            zout.start_file(name, opts).unwrap();
            zout.write_all(&strip_page_index(&raw)).unwrap();
        } else {
            zout.raw_copy_file(entry).unwrap();
        }
    }
    zout.finish().unwrap();
}

#[test]
fn a_facet_without_a_page_index_reads_every_spectrum() {
    let dir = scratch();
    let original = dir.join("tiny.mzpeak");
    let st = Command::new(env!("CARGO_BIN_EXE_mzpeak-convert"))
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/tiny.pwiz.1.1.mzML"))
        .arg("-o").arg(&original).arg("-f").arg("-q")
        .status().unwrap();
    assert!(st.success(), "conversion failed: {st}");
    let stripped = dir.join("tiny.no-page-index.mzpeak");
    rewrite_archive(&original, &stripped);

    let mut a = MzPeakReader::new(&original).unwrap();
    let mut b = MzPeakReader::new(&stripped).unwrap();
    assert_eq!(a.len(), b.len());
    assert!(a.len() > 0);
    let mut points = 0usize;
    for i in 0..a.len() as u64 {
        for (name, x, y) in [
            ("data", a.get_spectrum_arrays(i).unwrap(), b.get_spectrum_arrays(i).unwrap()),
            ("peaks", a.get_spectrum_peak_arrays_for(i).unwrap(), b.get_spectrum_peak_arrays_for(i).unwrap()),
        ] {
            match (x, y) {
                (None, None) => {}
                (Some(x), Some(y)) => {
                    let (mx, my) = (x.mzs().unwrap(), y.mzs().unwrap());
                    assert_eq!(mx.as_ref(), my.as_ref(), "spectrum {i} {name}: m/z");
                    assert_eq!(x.intensities().unwrap().as_ref(), y.intensities().unwrap().as_ref(), "spectrum {i} {name}: intensity");
                    points += mx.len();
                }
                (x, y) => panic!("spectrum {i} {name}: original has arrays = {}, stripped = {}", x.is_some(), y.is_some()),
            }
        }
    }
    assert!(points > 0, "no points compared — the original itself is empty");
    let _ = std::fs::remove_dir_all(&dir);
}

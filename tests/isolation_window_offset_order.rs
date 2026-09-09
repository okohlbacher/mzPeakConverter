//! Both isolation-window offsets must survive when they are listed BEFORE the target m/z.
//!
//! ProteoWizard's Waters MSe writer emits `upper offset, lower offset, target` (SpectrumList_Waters.cpp
//! :308-316). mzdata 0.66.6's mzML reader is stateful here: the first offset moves the window to its
//! `Offset` state, and a second offset arriving in that state fell into a `_ => {}` arm and was dropped,
//! so every pwiz Waters MSe archive came out with `isolation_window_lower_offset = 0` (Capan2 twin:
//! 136,400/136,400 rows `{325, 0, 275}` for a window pwiz declares as 325 ± 275). The reader fix lives
//! in the `[patch.crates-io]` mzdata (see Cargo.toml); this pins what lands in the archive.

use std::fs::File;
use std::process::Command;

use arrow::array::{Array, AsArray};
use arrow::datatypes::Float32Type;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

#[test]
fn offsets_listed_before_the_target_both_survive() {
    let archive = std::env::temp_dir().join(format!("mzpc-iw-order-{}.mzpeak", std::process::id()));
    let st = Command::new(env!("CARGO_BIN_EXE_mzpeak-convert"))
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/isolation_window_offsets_first.mzML"))
        .arg("-o")
        .arg(&archive)
        .arg("--force")
        .status()
        .expect("failed to run mzpeak-convert");
    assert!(st.success(), "conversion failed: {st}");

    let mut zip = zip::ZipArchive::new(File::open(&archive).unwrap()).unwrap();
    let mut buf = Vec::new();
    std::io::Read::read_to_end(&mut zip.by_name("spectra_metadata_precursors.parquet").unwrap(), &mut buf).unwrap();
    let reader = ParquetRecordBatchReaderBuilder::try_new(bytes::Bytes::from(buf)).unwrap().build().unwrap();

    let mut rows = Vec::new();
    for batch in reader {
        let batch = batch.unwrap();
        let iw = batch.column_by_name("isolation_window").expect("isolation_window struct").as_struct();
        let col = |name: &str| iw.column_by_name(name).unwrap().as_primitive::<Float32Type>().clone();
        let (t, lo, hi) = (
            col("isolation_window_target"),
            col("isolation_window_lower_offset"),
            col("isolation_window_upper_offset"),
        );
        for i in 0..iw.len() {
            rows.push((t.value(i), lo.value(i), hi.value(i)));
        }
    }
    // scan=1 lists (upper, lower, target), scan=2 lists (lower, upper, target): 500 − 2 .. 500 + 3 both ways.
    assert_eq!(rows, vec![(500.0, 2.0, 3.0), (500.0, 2.0, 3.0)], "isolation window (target, lower, upper)");
    let _ = std::fs::remove_file(&archive);
}

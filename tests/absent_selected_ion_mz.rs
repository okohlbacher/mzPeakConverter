//! An absent `MS:1000744` must arrive as `null`, never as a measured m/z of 0.
//!
//! mzdata's `SelectedIon.mz` is a plain `f64`, so a source that omits the term hands the writer a
//! 0.0 that is indistinguishable from a reported value. Bruker diaTracer mzML routinely omits it,
//! carrying only charge and peak intensity. Written as 0.0, every consumer that prefers a PRESENT
//! selected-ion m/z over the isolation-window target silently works from precursor m/z 0: measured
//! on a 3,086,644-spectrum diaPASEF run, FASTag returned 0 tags where the same run as mzML gives
//! 62,347,705 -- with exit code 0. The spec is explicit that `null` is how "absent" is spelled
//! (docs/layouts/metadata-tables.md, "Null semantics for metadata").
//!
//! The companion pin is that a STATED m/z still arrives verbatim: a fix that nulled the column
//! unconditionally would pass the first assertion and destroy every ordinary file.

use arrow::array::Array;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

fn convert(input: &Path, tag: &str) -> PathBuf {
    let out = std::env::temp_dir().join(format!("mzpc-absent-mz-{}-{tag}.mzpeak", std::process::id()));
    let _ = std::fs::remove_file(&out);
    let status = Command::new(env!("CARGO_BIN_EXE_mzpeak-convert"))
        .arg(input)
        .arg("-o")
        .arg(&out)
        .arg("--force")
        .status()
        .expect("failed to run mzpeak-convert");
    assert!(status.success(), "conversion of {} failed: {status}", input.display());
    out
}

fn table(archive: &Path, name: &str) -> arrow::array::RecordBatch {
    let mut zip = zip::ZipArchive::new(File::open(archive).unwrap()).unwrap();
    let mut f = zip.by_name(name).unwrap_or_else(|_| panic!("{name} missing"));
    let mut v = Vec::new();
    f.read_to_end(&mut v).unwrap();
    let reader = parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(bytes::Bytes::from(v))
        .unwrap()
        .with_batch_size(1 << 20)
        .build()
        .unwrap();
    let batches: Vec<_> = reader.map(|b| b.unwrap()).collect();
    arrow::compute::concat_batches(&batches[0].schema(), &batches).unwrap()
}

fn mz_column(archive: &Path) -> arrow::array::Float64Array {
    let ions = table(archive, "spectra_metadata_selected_ions.parquet");
    let col = ions
        .column_by_name("selected_ion_mz")
        .unwrap_or_else(|| panic!("selected_ion_mz missing: {:?}", ions.schema()));
    arrow::array::Float64Array::from(col.to_data())
}

#[test]
fn an_omitted_selected_ion_mz_is_null_not_zero() {
    let src = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/no_selected_ion_mz.mzML");
    let archive = convert(Path::new(src), "absent");
    let mz = mz_column(&archive);

    assert!(mz.len() > 0, "the fixture has selected ions to check");
    assert_eq!(mz.null_count(), mz.len(), "every omitted m/z is null, got {mz:?}");
    // The failure this pins: a 0.0 in the column asserts an ion measured at m/z 0.
    for i in 0..mz.len() {
        assert!(!mz.is_valid(i) || mz.value(i) != 0.0, "row {i} carries a measured m/z of 0");
    }
    let _ = std::fs::remove_file(&archive);
}

#[test]
fn a_stated_selected_ion_mz_survives_verbatim() {
    let src = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/isolation_window_offsets_first.mzML");
    let archive = convert(Path::new(src), "stated");
    let mz = mz_column(&archive);

    assert_eq!(mz.null_count(), 0, "a stated m/z is never nulled");
    for i in 0..mz.len() {
        assert_eq!(mz.value(i), 500.0, "row {i}: the fixture states 500");
    }
    let _ = std::fs::remove_file(&archive);
}

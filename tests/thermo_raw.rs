//! The Thermo `.raw` lane, end to end, on a real file.
//!
//! `tests/data/small.RAW` is mzdata's 48-spectrum LTQ FT run (provenance in
//! tests/fixtures/README.md). Nothing else reaches this lane: the vendor facets read straight from
//! `thermorawfilereader` (src/thermo_trailers.rs, src/thermo_status.rs), their embedding into the
//! archive, and the binary's own `DOTNET_ROLL_FORWARD=LatestMajor` for Thermo input (0.9.12).
//!
//! The child runs with `DOTNET_ROLL_FORWARD` removed, so the binary's default is what decides. On
//! a host without a .NET 8 runtime that default is the difference between a conversion and "It was
//! not possible to find a compatible framework version" (verified with
//! `DOTNET_ROLL_FORWARD=Disable`). With .NET 8 installed, as on CI, that part passes either way.

use std::collections::BTreeSet;
use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::process::Command;

use arrow::array::{Array, RecordBatch, UInt64Array};
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

/// `<spectrumList count="48">` in mzdata's small.mzML, converted from this very file (its
/// MS:1000569 SHA-1 is this file's).
const SPECTRA: usize = 48;

fn facet(archive: &Path, name: &str) -> RecordBatch {
    let mut zip = zip::ZipArchive::new(File::open(archive).unwrap()).unwrap();
    let mut bytes = Vec::new();
    zip.by_name(name)
        .unwrap_or_else(|_| panic!("{name} missing from the archive"))
        .read_to_end(&mut bytes)
        .unwrap();
    let builder = ParquetRecordBatchReaderBuilder::try_new(bytes::Bytes::from(bytes)).unwrap();
    let schema = builder.schema().clone();
    let batches: Vec<_> = builder.build().unwrap().map(Result::unwrap).collect();
    arrow::compute::concat_batches(&schema, &batches).unwrap()
}

#[test]
fn a_thermo_raw_converts_with_its_vendor_facets() {
    let dir = std::env::temp_dir().join(format!("mzpc-thermo-raw-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let archive = dir.join("small.mzpeak");

    let out = Command::new(env!("CARGO_BIN_EXE_mzpeak-convert"))
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/small.RAW"))
        .arg("-o")
        .arg(&archive)
        .env_remove("DOTNET_ROLL_FORWARD")
        .output()
        .expect("failed to run mzpeak-convert");
    assert!(
        out.status.success(),
        "conversion of small.RAW failed: {}\n{}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );

    assert_eq!(facet(&archive, "spectra_metadata.parquet").num_rows(), SPECTRA);

    let trailers = facet(&archive, "vendor_scan_trailers.parquet");
    let ordinals = trailers
        .column_by_name("ordinal")
        .and_then(|c| c.as_any().downcast_ref::<UInt64Array>())
        .expect("a UInt64 ordinal column");
    let distinct: BTreeSet<u64> = ordinals.values().iter().copied().collect();
    assert_eq!(distinct, (0..SPECTRA as u64).collect(), "one trailer ordinal per spectrum");

    assert!(facet(&archive, "vendor_status_log.parquet").num_rows() > 0, "empty status log");
    assert_eq!(facet(&archive, "vendor_scan_trailers_wide.parquet").num_rows(), SPECTRA);

    let _ = std::fs::remove_dir_all(&dir);
}

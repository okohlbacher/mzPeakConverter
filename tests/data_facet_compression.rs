//! Regression pin for the compression codec of the spectrum data facets.
//!
//! `prune_all_null_dup_point_columns` rewrites the finished peak facet when the schema sampler
//! left an all-null twin of a populated column. Until 0.9.3 that rewrite built its `ArrowWriter`
//! with `None` properties, so the survivors were re-encoded with parquet's DEFAULTS: UNCOMPRESSED,
//! no byte-stream-split, no delta packing, no encryption — and `--zstd-level` had no effect on
//! them. Numpress-linear always trips it (its schema carries both `mz_numpress_linear_bytes` and
//! an unused `mz_chunk_values`), so every numpress archive shipped an uncompressed peak facet;
//! `--no-numpress` prunes nothing and was unaffected. On DIA_Hela_20ng that was ~380 MB.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::Command;

use parquet::basic::{Compression, Encoding};
use parquet::file::reader::{FileReader, SerializedFileReader};

fn convert_fixture(tag: &str, extra: &[&str]) -> PathBuf {
    let out = std::env::temp_dir().join(format!("mzpc-zstd-{}-{tag}.mzpeak", std::process::id()));
    let _ = std::fs::remove_file(&out);
    let status = Command::new(env!("CARGO_BIN_EXE_mzpeak-convert"))
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/tiny.pwiz.1.1.mzML"))
        .arg("-o")
        .arg(&out)
        .arg("--force")
        .args(extra)
        .status()
        .expect("failed to run mzpeak-convert");
    assert!(status.success(), "conversion failed: {status}");
    out
}

/// `(compression, encodings)` of the intensity leaf column in one facet of the archive.
fn intensity_column(archive: &Path, member: &str) -> (Compression, Vec<Encoding>) {
    let mut zip = zip::ZipArchive::new(File::open(archive).unwrap()).unwrap();
    let extracted = std::env::temp_dir().join(format!("mzpc-zstd-{}-{member}", std::process::id()));
    {
        let mut src = zip.by_name(member).unwrap_or_else(|_| panic!("{member} missing"));
        let mut dst = File::create(&extracted).unwrap();
        std::io::copy(&mut src, &mut dst).unwrap();
    }
    let reader = SerializedFileReader::new(File::open(&extracted).unwrap()).unwrap();
    let rg = reader.metadata().row_group(0);
    let col = rg
        .columns()
        .iter()
        .find(|c| c.column_path().string().contains("intensity"))
        .unwrap_or_else(|| panic!("{member} has no intensity column"));
    let found = (col.compression(), col.encodings().collect::<Vec<_>>());
    let _ = std::fs::remove_file(&extracted);
    found
}

fn assert_intensity_is_compressed(archive: &Path, encoding: &str) {
    for member in ["spectra_data.parquet", "spectra_peaks.parquet"] {
        let (codec, encodings) = intensity_column(archive, member);
        assert!(
            matches!(codec, Compression::ZSTD(_)),
            "{member} intensity is {codec} under {encoding} chunking — the facet was re-encoded \
             with default WriterProperties (post-write rewrite dropping compression)"
        );
        assert!(
            encodings.contains(&Encoding::BYTE_STREAM_SPLIT),
            "{member} intensity lost BYTE_STREAM_SPLIT under {encoding} chunking (encodings: \
             {encodings:?}) — same cause as an UNCOMPRESSED codec"
        );
    }
}

#[test]
fn data_facet_intensity_is_zstd_for_both_chunk_encodings() {
    // Default: numpress-linear m/z, which leaves an all-null `mz_chunk_values` twin behind and so
    // always triggers the post-write prune+rewrite of the peak facet.
    let numpress = convert_fixture("numpress", &[]);
    assert_intensity_is_compressed(&numpress, "numpress-linear");
    let _ = std::fs::remove_file(&numpress);

    // Lossless delta m/z: no twin, no rewrite. The control that stayed correct throughout.
    let delta = convert_fixture("delta", &["--no-numpress"]);
    assert_intensity_is_compressed(&delta, "delta");
    let _ = std::fs::remove_file(&delta);
}

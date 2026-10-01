//! A spectrum facet that stays empty in a chunked archive is a chunked facet (review 2026-09-30 §E).
//!
//! When schema sampling finds no signal — an SRM/MRM run whose spectra are all empty, the profile
//! facet of a centroid-only Thermo run — the chunked builder took upstream's layout-blind default:
//! POINT columns (`mz: f64`, `intensity: f32`) with `point` array-index entries under the `chunk`
//! prefix. 66 facets in 35 corpus archives contradicted their own prefix, carried none of the four
//! columns a chunked file must use exactly once (signal-data.md), and beside a chunked peaks facet
//! put two layout families in one entity (conformance.md). They now carry the chunk columns a
//! default-typed spectrum would have.
//!
//! The fixture is `tiny_centroid_only.mzML` (non-indexed) with every spectrum's arrays emptied.

use std::path::{Path, PathBuf};
use std::process::Command;

use mzdata::prelude::*;
use mzpeak_prototyping::MzPeakReader;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

const TINY: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/tiny_centroid_only.mzML");

/// A scratch directory for ONE test: cargo runs a binary's tests in parallel under one process id.
fn scratch(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("mzpc-empty-chunk-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// The fixture with no signal in any spectrum: array lengths 0, binaries empty. Its chromatograms
/// keep theirs.
fn no_signal_mzml(dir: &Path) -> PathBuf {
    let src = std::fs::read_to_string(TINY).unwrap();
    let (spectra, chromatograms) = src.split_at(src.find("<chromatogramList").expect("fixture has chromatograms"));
    let mut out = spectra.replace(r#"defaultArrayLength="15""#, r#"defaultArrayLength="0""#).replace(r#"encodedLength="160""#, r#"encodedLength="0""#);
    while let Some(start) = out.find("<binary>") {
        let end = start + out[start..].find("</binary>").unwrap() + "</binary>".len();
        out.replace_range(start..end, "<binary />");
    }
    let lengths: Vec<&str> = out.match_indices(r#"defaultArrayLength=""#).map(|(at, key)| &out[at + key.len()..]).collect();
    assert!(lengths.len() == 3 && lengths.iter().all(|rest| rest.starts_with(r#"0""#)), "the fixture moved: a spectrum still has data");
    let path = dir.join("no_signal.mzML");
    std::fs::write(&path, out + chromatograms).unwrap();
    path
}

/// A member's Arrow schema, array index and row count.
fn facet(archive: &Path, name: &str) -> (arrow::datatypes::SchemaRef, serde_json::Value, i64) {
    let mut zip = zip::ZipArchive::new(std::fs::File::open(archive).unwrap()).unwrap();
    let mut bytes = Vec::new();
    std::io::Read::read_to_end(&mut zip.by_name(name).unwrap(), &mut bytes).unwrap();
    let builder = ParquetRecordBatchReaderBuilder::try_new(bytes::Bytes::from(bytes)).unwrap();
    let meta = builder.metadata().file_metadata();
    let index = meta
        .key_value_metadata()
        .and_then(|kv| kv.iter().find(|k| k.key == "spectrum_array_index"))
        .and_then(|k| k.value.as_deref())
        .unwrap_or_else(|| panic!("{name} has no spectrum_array_index"));
    (builder.schema().clone(), serde_json::from_str(index).unwrap(), meta.num_rows())
}

#[test]
fn empty_spectrum_facets_of_a_chunked_archive_are_chunk_facets() {
    let dir = scratch("chunked");
    let input = no_signal_mzml(&dir);
    let out = dir.join("no_signal.mzpeak");
    let st = Command::new(env!("CARGO_BIN_EXE_mzpeak-convert")).arg(&input).arg("-o").arg(&out).arg("-q").status().unwrap();
    assert!(st.success(), "converting the no-signal mzML failed: {st}");

    for name in ["spectra_data.parquet", "spectra_peaks.parquet"] {
        let (schema, index, rows) = facet(&out, name);
        assert_eq!(rows, 0, "{name}: no signal, no rows");
        assert_eq!(index["prefix"], "chunk", "{name}");
        let formats: Vec<&str> = index["entries"].as_array().unwrap().iter().map(|e| e["buffer_format"].as_str().unwrap()).collect();
        assert!(formats.iter().all(|f| f.starts_with("chunk_")), "{name}: a chunked file with {formats:?}");
        for required in ["chunk_start", "chunk_end", "chunk_encoding", "chunk_values"] {
            assert_eq!(formats.iter().filter(|f| **f == required).count(), 1, "{name}: {required} in {formats:?}");
        }
        // Every entry names a column of the `chunk` struct, and every array column has an entry.
        let arrow::datatypes::DataType::Struct(columns) = schema.field_with_name("chunk").unwrap().data_type() else {
            panic!("{name}: `chunk` is not a struct");
        };
        let mut paths: Vec<String> = index["entries"].as_array().unwrap().iter().map(|e| e["path"].as_str().unwrap().to_string()).collect();
        let mut children: Vec<String> =
            columns.iter().filter(|c| c.name() != "spectrum_index").map(|c| format!("chunk.{}", c.name())).collect();
        paths.sort();
        children.sort();
        assert_eq!(paths, children, "{name}");
    }

    // And it reads: three spectra, none with a point.
    let mut reader = MzPeakReader::new(&out).unwrap();
    assert_eq!(reader.len(), 3);
    for i in 0..3 {
        let spectrum = reader.get_spectrum(i).unwrap_or_else(|| panic!("spectrum {i} does not read"));
        assert_eq!(spectrum.peaks().len(), 0, "spectrum {i}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

//! Regression pin for the compression codec of the spectrum data facets.
//!
//! `prune_all_null_dup_point_columns` rewrites the finished peak facet when it holds an all-null
//! `point` column sharing its `array_name` with a populated sibling. Until 0.9.3 that rewrite built its
//! `ArrowWriter` with `None` properties, so the survivors were re-encoded with parquet's DEFAULTS:
//! UNCOMPRESSED, PARQUET_1_0, no byte-stream-split, no delta packing, no encryption — and
//! `--zstd-level` had no effect on them. Numpress-linear tripped it on every archive (its chunk schema
//! carries both `mz_numpress_linear_bytes` and an unused `mz_chunk_values`), so every numpress archive
//! shipped an uncompressed peak facet; `--no-numpress` prunes nothing and was unaffected. On
//! DIA_Hela_20ng that was ~380 MB.
//!
//! No converter lane reaches the rewrite any more: chunk facets are outside its scope, and the schema
//! coalesces precision twins before anything is written. So the CLI test pins that ordinary archives
//! are compressed, and `pruned_peak_facet_keeps_its_writer_properties` builds, through the writer, the
//! one input that still triggers the rewrite.
//!
//! `point_layout_float_mz_is_byte_stream_split` pins the m/z encoding of point facets.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::Command;

use mzdata::params::Unit;
use mzdata::prelude::*;
use mzdata::spectrum::bindata::{ArrayType, BinaryArrayMap, BinaryDataArrayType, DataArray};
use mzdata::spectrum::{
    Chromatogram, ChromatogramDescription, MultiLayerSpectrum, PeakDataLevel, SignalContinuity, SpectrumDescription,
};
use mzpeak_prototyping::peak_series::{INTENSITY_ARRAY, MZ_ARRAY};
use mzpeak_prototyping::writer::{AbstractMzPeakWriter, ArrayBuffersBuilder, MzPeakWriterType};
use mzpeak_prototyping::{BufferContext, BufferName, MzPeakReader};
use parquet::basic::{Compression, Encoding, ZstdLevel};
use parquet::file::reader::{FileReader, SerializedFileReader};

const TINY: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/tiny.pwiz.1.1.mzML");

fn convert_fixture(tag: &str, extra: &[&str]) -> PathBuf {
    convert(TINY, tag, extra)
}

fn convert(input: &str, tag: &str, extra: &[&str]) -> PathBuf {
    let out = std::env::temp_dir().join(format!("mzpc-zstd-{}-{tag}.mzpeak", std::process::id()));
    let _ = std::fs::remove_file(&out);
    let status = Command::new(env!("CARGO_BIN_EXE_mzpeak-convert"))
        .arg(input)
        .arg("-o")
        .arg(&out)
        .arg("--force")
        .args(extra)
        .status()
        .expect("failed to run mzpeak-convert");
    assert!(status.success(), "conversion failed: {status}");
    out
}

/// `member` of `archive`, extracted to a temp file.
fn extract(archive: &Path, member: &str) -> PathBuf {
    let mut zip = zip::ZipArchive::new(File::open(archive).unwrap()).unwrap();
    // Named for the archive too: tests run in parallel and extract the same member from different archives.
    let stem = archive.file_stem().unwrap().to_string_lossy();
    let extracted = std::env::temp_dir().join(format!("mzpc-zstd-{}-{stem}-{member}", std::process::id()));
    let mut src = zip.by_name(member).unwrap_or_else(|_| panic!("{member} missing"));
    let mut dst = File::create(&extracted).unwrap();
    std::io::copy(&mut src, &mut dst).unwrap();
    extracted
}

/// `(compression, encodings)` of the intensity leaf column in one facet of the archive.
fn intensity_column(archive: &Path, member: &str) -> (Compression, Vec<Encoding>) {
    let extracted = extract(archive, member);
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
    // Default: numpress-linear m/z chunks. Their all-null `mz_chunk_values` twin used to trigger the
    // post-write prune+rewrite of the peak facet; chunk facets are outside the prune's scope now, so
    // this archive no longer reaches it (the rewrite is pinned by the writer-level test below).
    let numpress = convert_fixture("numpress", &[]);
    assert_intensity_is_compressed(&numpress, "numpress-linear");
    let _ = std::fs::remove_file(&numpress);

    // Lossless delta m/z: no twin, no rewrite. The control that stayed correct throughout.
    let delta = convert_fixture("delta", &["--no-numpress"]);
    assert_intensity_is_compressed(&delta, "delta");
    let _ = std::fs::remove_file(&delta);
}

/// The input the prune exists for: a `point` peak facet declaring a column that reuses the primary
/// intensity's `array_name` and is never written. A nonstandard array NAMED "intensity array" carries
/// a different accession (MS:1000786), so schema-time coalescing keeps it — the shape a sampled
/// precision twin had before coalescing existed. The finished facet must lose that column (the
/// rewrite ran) and keep ZSTD and PARQUET_2_0 (it was re-encoded with the facet's own properties).
#[test]
fn pruned_peak_facet_keeps_its_writer_properties() {
    let out = std::env::temp_dir().join(format!("mzpc-zstd-{}-pruned.mzpeak", std::process::id()));
    let _ = std::fs::remove_file(&out);
    let twin = BufferName::new(
        BufferContext::Spectrum,
        ArrayType::nonstandard("intensity array"),
        BinaryDataArrayType::Float64,
    )
    .to_field();
    let twin_column = format!("point.{}", twin.name());

    let mut arrays = BinaryArrayMap::new();
    let mut mz = DataArray::wrap(&ArrayType::MZArray, BinaryDataArrayType::Float64, Vec::new());
    mz.update_buffer(&[100.0f64, 200.0, 300.0]).unwrap();
    mz.unit = Unit::MZ;
    arrays.add(mz);
    let mut intensity = DataArray::wrap(&ArrayType::IntensityArray, BinaryDataArrayType::Float32, Vec::new());
    intensity.update_buffer(&[5.0f32, 7.0, 9.0]).unwrap();
    intensity.unit = Unit::DetectorCounts;
    arrays.add(intensity);
    let descr = SpectrumDescription {
        id: "scan=1".into(),
        index: 0,
        ms_level: 1,
        signal_continuity: SignalContinuity::Centroid,
        ..Default::default()
    };
    let spec: MultiLayerSpectrum = MultiLayerSpectrum::new(descr, Some(arrays), None, None);

    let peaks = ArrayBuffersBuilder::default()
        .prefix("point")
        .with_context(BufferContext::Spectrum)
        .add_field(BufferContext::Spectrum.index_field())
        .add_field(MZ_ARRAY.to_field())
        .add_field(INTENSITY_ARRAY.to_field())
        .add_field(twin);
    let mut writer = MzPeakWriterType::<File>::builder()
        .compression(Compression::ZSTD(ZstdLevel::try_new(3).unwrap()))
        .sample_array_types_from_spectra(std::iter::once(spec.clone()))
        .store_peaks_and_profiles_apart(Some(peaks))
        .build(File::create(&out).unwrap(), true);
    writer.write_spectrum(&spec).unwrap();
    let mut chrom = BinaryArrayMap::new();
    chrom.add(DataArray::wrap(&ArrayType::TimeArray, BinaryDataArrayType::Float64, Vec::new()));
    chrom.add(DataArray::wrap(&ArrayType::IntensityArray, BinaryDataArrayType::Float64, Vec::new()));
    writer.write_chromatogram(&Chromatogram::new(ChromatogramDescription::default(), chrom)).unwrap();
    writer.finish_parquet().unwrap().finish().unwrap();

    let facet = extract(&out, "spectra_peaks.parquet");
    let reader = SerializedFileReader::new(File::open(&facet).unwrap()).unwrap();
    let md = reader.metadata();
    let columns: Vec<String> = md.row_group(0).columns().iter().map(|c| c.column_path().string()).collect();
    assert!(
        !columns.contains(&twin_column),
        "{twin_column} survived: the prune never ran, so nothing here reaches its rewrite ({columns:?})"
    );
    assert_eq!(
        md.file_metadata().version(),
        2,
        "the rewritten peak facet is PARQUET_1_0 — re-encoded with default WriterProperties"
    );
    for rg in md.row_groups() {
        for c in rg.columns() {
            assert!(
                matches!(c.compression(), Compression::ZSTD(_)),
                "{} is {} after the rewrite — re-encoded with default WriterProperties",
                c.column_path().string(),
                c.compression()
            );
        }
    }
    let intensity = md.row_group(0).columns().iter().find(|c| c.column_path().string() == "point.intensity");
    let encodings: Vec<Encoding> = intensity.unwrap_or_else(|| panic!("no point.intensity in {columns:?}")).encodings().collect();
    assert!(
        encodings.contains(&Encoding::BYTE_STREAM_SPLIT),
        "point.intensity lost BYTE_STREAM_SPLIT in the rewrite (encodings: {encodings:?})"
    );
    let _ = std::fs::remove_file(&facet);
    let _ = std::fs::remove_file(&out);
}

/// The encodings of `column` in each row group of one facet of the archive.
fn column_encodings(archive: &Path, member: &str, column: &str) -> Vec<Vec<Encoding>> {
    let extracted = extract(archive, member);
    let reader = SerializedFileReader::new(File::open(&extracted).unwrap()).unwrap();
    let found = reader
        .metadata()
        .row_groups()
        .iter()
        .map(|rg| {
            let col = rg.columns().iter().find(|c| c.column_path().string() == column);
            col.unwrap_or_else(|| panic!("{member} has no {column} column")).encodings().collect()
        })
        .collect();
    let _ = std::fs::remove_file(&extracted);
    found
}

fn is_dictionary(e: &Encoding) -> bool {
    matches!(e, Encoding::RLE_DICTIONARY | Encoding::PLAIN_DICTIONARY)
}

/// Float m/z in a point facet is BYTE_STREAM_SPLIT with the dictionary off. The writer had the switch
/// (`shuffle_mz`), but no lane set it, and the global dictionary takes precedence over a column
/// encoding anyway, so every point `mz` column shipped dictionary-encoded: on the native SciEX
/// Sample002 archive that column is 409 MB, 42 % of the archive. The values must read back
/// bit-identical, and chunk facets, which the rule leaves alone, keep their dictionary.
#[test]
fn point_layout_float_mz_is_byte_stream_split() {
    let assert_bss = |archive: &Path, member: &str| {
        let row_groups = column_encodings(archive, member, "point.mz");
        assert!(!row_groups.is_empty(), "{member} has no row group");
        for encodings in row_groups {
            assert!(
                encodings.contains(&Encoding::BYTE_STREAM_SPLIT) && !encodings.iter().any(is_dictionary),
                "{member} point.mz must be BYTE_STREAM_SPLIT without a dictionary (encodings: {encodings:?})"
            );
        }
    };

    // `--layout point` on the ordinary lane: profile m/z in spectra_data, centroid m/z in spectra_peaks.
    let point = convert_fixture("point-mz", &["--layout", "point"]);
    assert_bss(&point, "spectra_data.parquet");
    assert_bss(&point, "spectra_peaks.parquet");

    let mut source = mzdata::MZReader::open_path(TINY).unwrap();
    let mut reader = MzPeakReader::new(&point).unwrap();
    assert_eq!(reader.len(), source.len());
    let bits = |v: &[f64]| v.iter().map(|x| x.to_bits()).collect::<Vec<_>>();
    for (i, spectrum) in source.iter().enumerate() {
        let want = spectrum.arrays.as_ref().and_then(|a| a.mzs().ok().map(|m| m.to_vec())).unwrap_or_default();
        // Profile m/z from spectra_data; a spectrum with none there is a centroid one, in spectra_peaks.
        let profile = reader
            .get_spectrum_arrays(i as u64)
            .unwrap()
            .and_then(|a| a.mzs().ok().map(|m| m.to_vec()))
            .unwrap_or_default();
        let got: Vec<f64> = if !profile.is_empty() {
            profile
        } else {
            match reader.get_spectrum_peaks_for(i as u64).unwrap() {
                Some(PeakDataLevel::Centroid(peaks)) => peaks.iter().map(|p| p.mz).collect(),
                _ => Vec::new(),
            }
        };
        assert_eq!(bits(&got), bits(&want), "spectrum {i}: m/z must read back bit-identical to the source");
    }
    let _ = std::fs::remove_file(&point);

    // The mzML `--tof-grid` lane builds its writer separately: the f64 column beside its gridded centroids.
    let swath = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/swath.api-sample-centroid.mzML.gz");
    let gridded = convert(swath, "tof-grid-mz", &["--tof-grid", "on"]);
    assert_bss(&gridded, "spectra_peaks.parquet");
    let _ = std::fs::remove_file(&gridded);

    // Chunk facets are outside the rule, so a chunked archive keeps its bytes.
    let chunked = convert_fixture("chunk-mz", &[]);
    for encodings in column_encodings(&chunked, "spectra_data.parquet", "chunk.mz_chunk_start") {
        assert!(encodings.iter().any(is_dictionary), "chunk.mz_chunk_start changed encoding: {encodings:?}");
    }
    let _ = std::fs::remove_file(&chunked);
}

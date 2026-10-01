//! Row groups of the signal facets are bounded by bytes as well as rows.
//!
//! Through 0.16 a signal row group was capped by its row count alone — and in the chunked layout a
//! row is a whole chunk, so a facet that compresses well stayed in ONE group however large: the
//! 16 MB flush of `spectra_data` measures the writer's compressed estimate, which a Shimadzu profile
//! facet never reached before its single group held 85 MiB of pages (validator:
//! `data_row_group_not_monolithic`), and the peak facet had no byte bound at all (8192 timsTOF grid
//! chunks: 270–460 MiB per group on PXD076703). Parquet decodes a whole row group for any row of
//! it, and the parallel peak encoder serialised on groups larger than its in-flight budget.
//!
//! `profile_and_centroid_facets_stay_under_the_byte_cap` writes, through the writer, ~90 MB of
//! profile and of centroid signal that compresses far past 4:1 and checks both facets' row groups
//! against the default cap and the validator's 64 MiB, and that spectra on either side of a
//! boundary read back intact. The CLI tests pin the paths only a conversion reaches: the serial and
//! parallel peak encoders cut the same row groups under a byte cap, and the mzPeak→mzPeak filter
//! lane re-groups within it, in the source's column encodings (a dictionary per group otherwise).

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::Command;

use mzdata::params::Unit;
use mzdata::prelude::*;
use mzdata::spectrum::bindata::{ArrayType, BinaryArrayMap, BinaryDataArrayType, DataArray};
use mzdata::spectrum::{Chromatogram, ChromatogramDescription, MultiLayerSpectrum, SignalContinuity, SpectrumDescription};
use mzpeak_prototyping::MzPeakReader;
use mzpeak_prototyping::chunk_series::ChunkingStrategy;
use mzpeak_prototyping::writer::{
    AbstractMzPeakWriter, ColumnEncoding, DEFAULT_ROW_GROUP_BYTES, DataColumnEncodings, MzPeakWriterType,
};
use parquet::basic::{Compression, Encoding, ZstdLevel};
use parquet::column::page::Page;
use parquet::file::metadata::{ParquetMetaData, SortingColumn};
use parquet::file::reader::{FileReader, SerializedFileReader};
use parquet::file::statistics::Statistics;

/// The validator's `data_row_group_not_monolithic` threshold on a group's uncompressed pages.
const VALIDATOR_MAX_BYTES: i64 = 64 * 1024 * 1024;

const SPECTRA: usize = 8;
const POINTS: usize = 1_000_000;

/// Spectrum `index`: `POINTS` points on a regular 0.0004-Th grid from 100 Th, intensity a step ramp.
/// Byte-stream-split, both compress well past 4:1 (as real profile data does), so the writer's
/// 16 MB compressed-size flush never cuts the facet and only a byte cap on the pages can.
fn spectrum(index: usize, continuity: SignalContinuity) -> MultiLayerSpectrum {
    let mut arrays = BinaryArrayMap::new();
    let mut mz = DataArray::wrap(&ArrayType::MZArray, BinaryDataArrayType::Float64, Vec::new());
    mz.update_buffer(&(0..POINTS).map(|i| 100.0 + 0.0004 * i as f64).collect::<Vec<_>>()).unwrap();
    mz.unit = Unit::MZ;
    arrays.add(mz);
    let mut intensity = DataArray::wrap(&ArrayType::IntensityArray, BinaryDataArrayType::Float32, Vec::new());
    intensity
        .update_buffer(&(0..POINTS).map(|i| (((i / 256) + index) % 4096) as f32 + 1.0).collect::<Vec<_>>())
        .unwrap();
    intensity.unit = Unit::DetectorCounts;
    arrays.add(intensity);
    let descr = SpectrumDescription {
        id: format!("scan={}", index + 1),
        index,
        ms_level: 1,
        signal_continuity: continuity,
        ..Default::default()
    };
    MultiLayerSpectrum::new(descr, Some(arrays), None, None)
}

fn write_archive(path: &Path) {
    let probe = spectrum(0, SignalContinuity::Profile);
    let basic = Some(ChunkingStrategy::Basic { chunk_size: 50.0 });
    let bss = DataColumnEncodings {
        mz_values: ColumnEncoding::ByteStreamSplit,
        intensity: ColumnEncoding::ByteStreamSplit,
        ion_mobility: ColumnEncoding::Writer,
    };
    let mut writer = MzPeakWriterType::<File>::builder()
        .chunked_encoding(basic.clone())
        .peaks_chunked_encoding(basic)
        .chromatogram_chunked_encoding(None)
        .data_column_encodings(bss)
        .compression(Compression::ZSTD(ZstdLevel::try_new(3).unwrap()))
        .sample_array_types_from_spectra(std::iter::once(probe.clone()))
        .sample_array_types_for_peaks_from_spectra(std::iter::once(probe))
        .build(File::create(path).unwrap(), false);
    // The first half profile (→ spectra_data), the second centroid (→ spectra_peaks).
    for i in 0..2 * SPECTRA {
        let continuity = if i < SPECTRA { SignalContinuity::Profile } else { SignalContinuity::Centroid };
        writer.write_spectrum(&spectrum(i, continuity)).unwrap();
    }
    let mut chrom = BinaryArrayMap::new();
    chrom.add(DataArray::wrap(&ArrayType::TimeArray, BinaryDataArrayType::Float64, Vec::new()));
    chrom.add(DataArray::wrap(&ArrayType::IntensityArray, BinaryDataArrayType::Float64, Vec::new()));
    writer.write_chromatogram(&Chromatogram::new(ChromatogramDescription::default(), chrom)).unwrap();
    writer.finish_parquet().unwrap().finish().unwrap();
}

/// `member` of `archive`, extracted to a temp file named after both.
fn extract(archive: &Path, member: &str) -> PathBuf {
    let mut zip = zip::ZipArchive::new(File::open(archive).unwrap()).unwrap();
    let stem = archive.file_stem().unwrap().to_string_lossy();
    let out = std::env::temp_dir().join(format!("mzpc-rgb-{}-{stem}-{member}", std::process::id()));
    let mut src = zip.by_name(member).unwrap_or_else(|_| panic!("{member} missing"));
    std::io::copy(&mut src, &mut File::create(&out).unwrap()).unwrap();
    out
}

fn facet_metadata(archive: &Path, member: &str) -> ParquetMetaData {
    let extracted = extract(archive, member);
    let md = SerializedFileReader::new(File::open(&extracted).unwrap()).unwrap().metadata().clone();
    let _ = std::fs::remove_file(&extracted);
    md
}

/// `(min, max)` spectrum index of each row group, from the column statistics.
fn spectrum_index_ranges(md: &ParquetMetaData) -> Vec<(u64, u64)> {
    md.row_groups()
        .iter()
        .map(|rg| {
            let col = rg
                .columns()
                .iter()
                .find(|c| c.column_path().string().ends_with("spectrum_index"))
                .expect("a spectrum_index column");
            match col.statistics() {
                Some(Statistics::Int64(s)) => (*s.min_opt().unwrap() as u64, *s.max_opt().unwrap() as u64),
                other => panic!("unexpected spectrum_index statistics {other:?}"),
            }
        })
        .collect()
}

#[test]
fn profile_and_centroid_facets_stay_under_the_byte_cap() {
    let out = std::env::temp_dir().join(format!("mzpc-rgb-{}-archive.mzpeak", std::process::id()));
    let _ = std::fs::remove_file(&out);
    write_archive(&out);

    for member in ["spectra_data.parquet", "spectra_peaks.parquet"] {
        let md = facet_metadata(&out, member);
        let sizes: Vec<i64> = md.row_groups().iter().map(|rg| rg.total_byte_size()).collect();
        let total: i64 = sizes.iter().sum();
        eprintln!("{member}: {} row groups, uncompressed {sizes:?} (total {total})", sizes.len());
        assert!(
            total > VALIDATOR_MAX_BYTES,
            "{member}: the fixture must hold more than one group's worth ({total} bytes)"
        );
        assert!(
            sizes.iter().all(|&s| s <= VALIDATOR_MAX_BYTES),
            "{member}: a row group exceeds the validator's 64 MiB ({sizes:?}) — capped by rows alone"
        );
        assert!(
            sizes.iter().all(|&s| s <= DEFAULT_ROW_GROUP_BYTES as i64),
            "{member}: a row group exceeds the writer's byte cap ({sizes:?})"
        );
        // The byte rule starts a new group rather than splitting a spectrum that fits one.
        let ranges = spectrum_index_ranges(&md);
        for w in ranges.windows(2) {
            assert!(w[0].1 < w[1].0, "{member}: a spectrum spans two row groups ({ranges:?})");
        }
    }

    // Spectra on both sides of the first boundary of each facet read back point for point
    // (basic chunks are lossless).
    let mut reader = MzPeakReader::new(&out).unwrap();
    for member in ["spectra_data.parquet", "spectra_peaks.parquet"] {
        let ranges = spectrum_index_ranges(&facet_metadata(&out, member));
        for index in [ranges[0].1, ranges[1].0] {
            let continuity = if (index as usize) < SPECTRA { SignalContinuity::Profile } else { SignalContinuity::Centroid };
            let want = spectrum(index as usize, continuity);
            let want = want.raw_arrays().unwrap();
            let got = if continuity == SignalContinuity::Profile {
                reader.get_spectrum_arrays(index).unwrap()
            } else {
                reader.get_spectrum_peak_arrays_for(index).unwrap()
            }
            .unwrap_or_else(|| panic!("{member}: spectrum {index} not found"));
            assert_eq!(got.mzs().unwrap().as_ref(), want.mzs().unwrap().as_ref(), "{member}: m/z of {index}");
            assert_eq!(
                got.intensities().unwrap().as_ref(),
                want.intensities().unwrap().as_ref(),
                "{member}: intensity of {index}"
            );
        }
    }
    let _ = std::fs::remove_file(&out);
}

const SWATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/swath.api-sample-centroid.mzML.gz");

fn convert(input: &Path, out: &Path, env: &[(&str, &str)]) {
    let _ = std::fs::remove_file(out);
    let status = Command::new(env!("CARGO_BIN_EXE_mzpeak-convert"))
        .arg(input)
        .arg("-o")
        .arg(out)
        .arg("--force")
        .arg("-q")
        .envs(env.iter().copied())
        .env_remove("MZPC_PARALLEL_ENCODE")
        .status()
        .expect("failed to run mzpeak-convert");
    assert!(status.success(), "conversion of {} failed: {status}", input.display());
}

/// Per row group: its row count and every column chunk's raw bytes.
fn row_group_bytes(facet: &Path) -> Vec<(i64, Vec<Vec<u8>>)> {
    use std::io::{Read, Seek, SeekFrom};
    let md = SerializedFileReader::new(File::open(facet).unwrap()).unwrap().metadata().clone();
    let mut f = File::open(facet).unwrap();
    md.row_groups()
        .iter()
        .map(|rg| {
            let chunks = rg
                .columns()
                .iter()
                .map(|c| {
                    let (start, len) = c.byte_range();
                    let mut buf = vec![0u8; len as usize];
                    f.seek(SeekFrom::Start(start)).unwrap();
                    f.read_exact(&mut buf).unwrap();
                    buf
                })
                .collect();
            (rg.num_rows(), chunks)
        })
        .collect()
}

/// The serial peak encoder (encrypted facets, `MZPC_PARALLEL_ENCODE=0`) and the parallel one cut
/// the facet with the same cutter, so under a byte cap that splits it they still agree byte for
/// byte, row group by row group.
#[test]
fn serial_and_parallel_peak_encoders_cut_the_same_row_groups() {
    let dir = std::env::temp_dir();
    let pid = std::process::id();
    let mut facets = Vec::new();
    for parallel in ["0", "1"] {
        let out = dir.join(format!("mzpc-rgb-{pid}-swath-parallel{parallel}.mzpeak"));
        // ~0.1 MiB groups: the fixture's peak facet is a few MiB of Arrow buffers.
        convert(Path::new(SWATH), &out, &[("MZPC_ROW_GROUP_MB", "0.1"), ("MZPC_PARALLEL_ENCODE", parallel)]);
        facets.push(extract(&out, "spectra_peaks.parquet"));
        let _ = std::fs::remove_file(&out);
    }
    let serial = row_group_bytes(&facets[0]);
    let parallel = row_group_bytes(&facets[1]);
    let rows: Vec<i64> = serial.iter().map(|g| g.0).collect();
    eprintln!("{} row groups, rows {rows:?}", rows.len());
    assert!(serial.len() > 2, "the byte cap did not split the peak facet ({rows:?})");
    assert_eq!(
        rows,
        parallel.iter().map(|g| g.0).collect::<Vec<_>>(),
        "serial and parallel encoders cut different row groups"
    );
    for (i, (s, p)) in serial.iter().zip(&parallel).enumerate() {
        assert!(s.1 == p.1, "row group {i}: serial and parallel column chunks differ");
    }
    for f in facets {
        let _ = std::fs::remove_file(f);
    }
}

/// A row group larger than the parallel encoder's whole in-flight budget used to be admitted only
/// once nothing else was in flight, so a run of them encoded on one worker. Here every group is
/// (0.1 MiB groups against a 4 KiB budget), and zstd-19 keeps each encode busy long enough that the
/// next groups are dispatched while it runs: several workers must be busy at once. Read from the
/// encoder's `MZPC_TIMING` report — the only place its concurrency is visible.
#[test]
fn oversized_row_groups_encode_on_several_workers() {
    let out = std::env::temp_dir().join(format!("mzpc-rgb-{}-oversized.mzpeak", std::process::id()));
    let _ = std::fs::remove_file(&out);
    let run = Command::new(env!("CARGO_BIN_EXE_mzpeak-convert"))
        .arg(SWATH)
        .arg("-o")
        .arg(&out)
        .args(["--force", "-q", "--zstd-level", "19"])
        .env("MZPC_TIMING", "1")
        .env("MZPC_ROW_GROUP_MB", "0.1")
        .env("MZPC_ENCODE_INFLIGHT_BYTES", "4096")
        .env("MZPC_ENCODE_THREADS", "4")
        .env_remove("MZPC_PARALLEL_ENCODE")
        .output()
        .expect("failed to run mzpeak-convert");
    assert!(run.status.success(), "conversion failed: {}", run.status);
    let stderr = String::from_utf8_lossy(&run.stderr);
    let report = stderr
        .lines()
        .find(|l| l.contains("parallel peak encode:") && l.contains("at once"))
        .unwrap_or_else(|| panic!("no encoder occupancy report in:\n{stderr}"));
    eprintln!("{report}");
    let at_once: usize = report
        .split("at most ")
        .nth(1)
        .and_then(|s| s.split_whitespace().next())
        .and_then(|n| n.parse().ok())
        .unwrap_or_else(|| panic!("unparsable report: {report}"));
    assert!(at_once >= 2, "oversized row groups were encoded one at a time: {report}");
    let _ = std::fs::remove_file(&out);
}

/// The filter lane re-encodes every facet it filters; it re-groups within the byte cap too, instead
/// of merging the input's groups up to parquet's million-row default.
#[test]
fn filter_lane_keeps_row_groups_under_the_byte_cap() {
    let dir = std::env::temp_dir();
    let pid = std::process::id();
    let src = dir.join(format!("mzpc-rgb-{pid}-filter-src.mzpeak"));
    let out = dir.join(format!("mzpc-rgb-{pid}-filter-out.mzpeak"));
    convert(Path::new(SWATH), &src, &[]);
    let before = facet_metadata(&src, "spectra_peaks.parquet");
    // Keep every spectrum: the filter still rewrites the per-spectrum facets.
    let _ = std::fs::remove_file(&out);
    let status = Command::new(env!("CARGO_BIN_EXE_mzpeak-convert"))
        .arg(&src)
        .args(["-o"])
        .arg(&out)
        .args(["--force", "-q", "--rt", "0-1000000"])
        .env("MZPC_ROW_GROUP_MB", "0.1")
        .status()
        .expect("failed to run mzpeak-convert");
    assert!(status.success(), "filter failed: {status}");
    let after = facet_metadata(&out, "spectra_peaks.parquet");
    let cap = (0.1 * 1024.0 * 1024.0) as i64;
    let sizes: Vec<i64> = after.row_groups().iter().map(|rg| rg.total_byte_size()).collect();
    eprintln!("filter: {} → {} row groups, uncompressed {sizes:?}", before.num_row_groups(), sizes.len());
    assert_eq!(before.file_metadata().num_rows(), after.file_metadata().num_rows(), "rows lost");
    assert!(sizes.len() > 2, "the filter lane re-grouped by rows alone ({sizes:?})");
    assert!(sizes.iter().all(|&s| s <= cap), "a filtered row group exceeds the byte cap ({sizes:?})");
    let _ = std::fs::remove_file(&src);
    let _ = std::fs::remove_file(&out);
}

/// Per column of `md`: whether any row group used the dictionary, and the value encodings the
/// non-dictionary columns chose (a dictionary column's fallback depends on its group sizes). RLE
/// and PLAIN are no choice: levels and the defaults, whose boolean form follows the writer version.
fn column_encodings(md: &ParquetMetaData) -> BTreeMap<String, (bool, BTreeSet<String>)> {
    let mut out: BTreeMap<String, (bool, BTreeSet<String>)> = BTreeMap::new();
    for rg in md.row_groups() {
        for c in rg.columns() {
            let e = out.entry(c.column_path().string()).or_default();
            for enc in c.encodings() {
                match enc {
                    Encoding::RLE_DICTIONARY | Encoding::PLAIN_DICTIONARY => e.0 = true,
                    Encoding::RLE | Encoding::PLAIN => {}
                    other => {
                        e.1.insert(format!("{other:?}"));
                    }
                }
            }
        }
    }
    for e in out.values_mut() {
        if e.0 {
            e.1.clear();
        }
    }
    out
}

/// The filter lane writes every column in the encodings the source used. Parquet's dictionary is
/// on by default and takes precedence over a column encoding, so its former fixed rules shipped
/// every column dictionary-encoded — byte-stream-split intensity and delta-packed spectrum indices
/// included — and with the byte cap splitting a facet each row group paid its own dictionary.
#[test]
fn filter_lane_keeps_the_source_column_encodings() {
    let dir = std::env::temp_dir();
    let pid = std::process::id();
    let src = dir.join(format!("mzpc-rgb-{pid}-enc-src.mzpeak"));
    let out = dir.join(format!("mzpc-rgb-{pid}-enc-out.mzpeak"));
    convert(Path::new(SWATH), &src, &[]);
    let _ = std::fs::remove_file(&out);
    let status = Command::new(env!("CARGO_BIN_EXE_mzpeak-convert"))
        .arg(&src)
        .args(["-o"])
        .arg(&out)
        .args(["--force", "-q", "--rt", "0-1000000"])
        .status()
        .expect("failed to run mzpeak-convert");
    assert!(status.success(), "filter failed: {status}");
    let members: Vec<String> = zip::ZipArchive::new(File::open(&src).unwrap())
        .unwrap()
        .file_names()
        .filter(|n| n.ends_with(".parquet"))
        .map(str::to_string)
        .collect();
    let peaks = column_encodings(&facet_metadata(&src, "spectra_peaks.parquet"));
    assert!(
        peaks.values().any(|(dict, values)| !dict && values.contains("BYTE_STREAM_SPLIT")),
        "the fixture's peak facet has no byte-stream-split column to keep: {peaks:?}"
    );
    for member in &members {
        let want = column_encodings(&facet_metadata(&src, member));
        let got = column_encodings(&facet_metadata(&out, member));
        for (column, enc) in &want {
            assert_eq!(got.get(column), Some(enc), "{member}: {column} (dictionary, value encodings)");
        }
    }
    let _ = std::fs::remove_file(&src);
    let _ = std::fs::remove_file(&out);
}

/// What a facet's footer records of its layout: the Parquet format version, the sorting columns,
/// and the columns that carry a bloom filter.
fn footer_layout(md: &ParquetMetaData) -> (i32, Vec<SortingColumn>, BTreeSet<String>) {
    let sorting = md.row_groups().first().and_then(|rg| rg.sorting_columns().cloned()).unwrap_or_default();
    let bloom = md
        .row_groups()
        .iter()
        .flat_map(|rg| rg.columns())
        .filter(|c| c.bloom_filter_offset().is_some())
        .map(|c| c.column_path().string())
        .collect();
    (md.file_metadata().version(), sorting, bloom)
}

/// The largest data page of each column of one facet: its values (rows, in a point facet) and its
/// uncompressed bytes.
fn largest_pages(archive: &Path, member: &str) -> BTreeMap<String, (u32, usize)> {
    let facet = extract(archive, member);
    let reader = SerializedFileReader::new(File::open(&facet).unwrap()).unwrap();
    let mut out: BTreeMap<String, (u32, usize)> = BTreeMap::new();
    for g in 0..reader.num_row_groups() {
        let group = reader.get_row_group(g).unwrap();
        for c in 0..group.num_columns() {
            let column = group.metadata().column(c).column_path().string();
            let mut pages = group.get_column_page_reader(c).unwrap();
            while let Some(page) = pages.get_next_page().unwrap() {
                let (values, bytes) = match page {
                    Page::DataPage { buf, num_values, .. } | Page::DataPageV2 { buf, num_values, .. } => (num_values, buf.len()),
                    Page::DictionaryPage { .. } => continue,
                };
                let largest = out.entry(column.clone()).or_default();
                *largest = (largest.0.max(values), largest.1.max(bytes));
            }
        }
    }
    let _ = std::fs::remove_file(&facet);
    out
}

/// The filter lane lays a facet out as its source was: the source's Parquet format version, sorting
/// columns and bloom filters (the converter writes none: the index column it asks one for is named
/// by a dotted string, which parquet takes as one path segment), and the page limits of the
/// converter's spectrum signal facets — a point facet's pages up to 1,048,576 rows (the 1 MiB page
/// still applies), a chunk facet's up to a quarter of parquet's 1 MiB page. With parquet's defaults
/// (format 1.0, pages of at most 20,000 rows, no sort order) a keep-everything filter grew the point
/// peak facet of QC01 by 16 %.
#[test]
fn filter_lane_keeps_the_source_layout() {
    let dir = std::env::temp_dir();
    let pid = std::process::id();
    let quarter_page = parquet::file::properties::DEFAULT_PAGE_SIZE / 4;
    for layout in ["chunked", "point"] {
        let src = dir.join(format!("mzpc-rgb-{pid}-layout-{layout}-src.mzpeak"));
        let out = dir.join(format!("mzpc-rgb-{pid}-layout-{layout}-out.mzpeak"));
        for (input, output, args) in [
            (Path::new(SWATH), &src, vec!["--layout", layout]),
            (src.as_path(), &out, vec!["--rt", "0-1000000"]),
        ] {
            let _ = std::fs::remove_file(output);
            let status = Command::new(env!("CARGO_BIN_EXE_mzpeak-convert"))
                .arg(input)
                .arg("-o")
                .arg(output)
                .args(["--force", "-q"])
                .args(&args)
                .status()
                .expect("failed to run mzpeak-convert");
            assert!(status.success(), "{} → {} failed: {status}", input.display(), output.display());
        }
        let members: Vec<String> = zip::ZipArchive::new(File::open(&src).unwrap())
            .unwrap()
            .file_names()
            .filter(|n| n.ends_with(".parquet"))
            .map(str::to_string)
            .collect();
        for member in &members {
            let want = footer_layout(&facet_metadata(&src, member));
            assert_eq!(footer_layout(&facet_metadata(&out, member)), want, "{layout} {member}: format version, sort order, bloom filters");
        }
        let peaks = facet_metadata(&src, "spectra_peaks.parquet");
        assert_eq!(peaks.file_metadata().version(), 2, "the converter writes format 2 facets");
        assert!(!footer_layout(&peaks).1.is_empty(), "the peak facet declares no sort order");

        let (src_pages, out_pages) = (largest_pages(&src, "spectra_peaks.parquet"), largest_pages(&out, "spectra_peaks.parquet"));
        eprintln!("{layout}: largest pages (values, bytes) source {src_pages:?}, filtered {out_pages:?}");
        if layout == "point" {
            // Parquet's default ends a page at 20,000 rows; the fixture's m/z and intensity pages
            // are bounded by the 1 MiB page instead.
            let long: Vec<&String> = src_pages.iter().filter(|(_, p)| p.0 > 20_000).map(|(c, _)| c).collect();
            assert!(!long.is_empty(), "no page of the source holds more than 20,000 rows: {src_pages:?}");
            for column in long {
                assert!(out_pages[column].0 > 20_000, "{column}: the filtered facet's pages end at 20,000 rows ({out_pages:?})");
            }
        } else {
            // A page ends once a write batch takes it past the limit, so it may overshoot by one batch.
            let limit = quarter_page + 32 * 1024;
            assert!(src_pages.values().any(|p| p.1 > quarter_page / 2), "the source's pages are too small to tell: {src_pages:?}");
            for (column, page) in &out_pages {
                assert!(page.1 <= limit, "{column}: a filtered chunk page of {} bytes, the source's limit is {quarter_page}", page.1);
            }
        }
        let _ = std::fs::remove_file(&src);
        let _ = std::fs::remove_file(&out);
    }
}

/// The encodings of the data pages of each column of one facet (dictionary pages left out).
fn data_page_encodings(archive: &Path, member: &str) -> BTreeMap<String, BTreeSet<String>> {
    let facet = extract(archive, member);
    let reader = SerializedFileReader::new(File::open(&facet).unwrap()).unwrap();
    let mut out: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for g in 0..reader.num_row_groups() {
        let group = reader.get_row_group(g).unwrap();
        for c in 0..group.num_columns() {
            let column = group.metadata().column(c).column_path().string();
            let mut pages = group.get_column_page_reader(c).unwrap();
            while let Some(page) = pages.get_next_page().unwrap() {
                if let Page::DataPage { encoding, .. } | Page::DataPageV2 { encoding, .. } = page {
                    out.entry(column.clone()).or_default().insert(format!("{encoding:?}"));
                }
            }
        }
    }
    let _ = std::fs::remove_file(&facet);
    out
}

/// The converter gives a signal facet with an ion-mobility column twice parquet's 1 MiB dictionary
/// page, and so does the filter lane's rewrite. 160,000 distinct 1/K0 values (1.28 MB) fit it: every
/// data page of the column is dictionary-encoded in the source and in the rewrite. Under parquet's
/// limit the rewrite fell back to plain pages part way through the column.
#[test]
fn filter_lane_keeps_the_ion_mobility_dictionary_page() {
    const N: usize = 160_000;
    let dir = std::env::temp_dir();
    let pid = std::process::id();
    let src = dir.join(format!("mzpc-rgb-{pid}-im-src.mzpeak"));
    let out = dir.join(format!("mzpc-rgb-{pid}-im-out.mzpeak"));

    let mut arrays = BinaryArrayMap::new();
    let mut mz = DataArray::wrap(&ArrayType::MZArray, BinaryDataArrayType::Float64, Vec::new());
    mz.update_buffer(&(0..N).map(|i| 100.0 + 0.01 * i as f64).collect::<Vec<_>>()).unwrap();
    arrays.add(mz);
    let mut intensity = DataArray::wrap(&ArrayType::IntensityArray, BinaryDataArrayType::Float32, Vec::new());
    intensity.update_buffer(vec![1.0f32; N].as_slice()).unwrap();
    arrays.add(intensity);
    let mut mobility =
        DataArray::wrap(&ArrayType::MeanInverseReducedIonMobilityArray, BinaryDataArrayType::Float64, Vec::new());
    mobility.update_buffer(&(0..N).map(|i| 0.6 + 1e-6 * i as f64).collect::<Vec<_>>()).unwrap();
    arrays.add(mobility);
    let descr = SpectrumDescription {
        id: "scan=1".into(),
        index: 0,
        ms_level: 1,
        signal_continuity: SignalContinuity::Centroid,
        ..Default::default()
    };
    let spectrum: MultiLayerSpectrum = MultiLayerSpectrum::new(descr, Some(arrays), None, None);
    let mut writer = MzPeakWriterType::<File>::builder()
        .chromatogram_chunked_encoding(None)
        .sample_array_types_from_spectra(std::iter::once(spectrum.clone()))
        .sample_array_types_for_peaks_from_spectra(std::iter::once(spectrum.clone()))
        .build(File::create(&src).unwrap(), false);
    writer.write_spectrum(&spectrum).unwrap();
    writer.finish_parquet().unwrap().finish().unwrap();

    let _ = std::fs::remove_file(&out);
    let status = Command::new(env!("CARGO_BIN_EXE_mzpeak-convert"))
        .arg(&src)
        .arg("-o")
        .arg(&out)
        .args(["--force", "-q", "--ms-level", "1"])
        .status()
        .expect("failed to run mzpeak-convert");
    assert!(status.success(), "filter failed: {status}");

    let mut checked = 0;
    for member in ["spectra_data.parquet", "spectra_peaks.parquet"] {
        let (want, got) = (data_page_encodings(&src, member), data_page_encodings(&out, member));
        for (column, pages) in want.iter().filter(|(c, _)| c.contains("ion_mobility")) {
            assert_eq!(pages.iter().collect::<Vec<_>>(), ["RLE_DICTIONARY"], "{member} {column}: the source's pages");
            assert_eq!(got.get(column), Some(pages), "{member} {column}: the rewrite's pages");
            checked += 1;
        }
    }
    assert!(checked > 0, "no ion-mobility column in the fixture");
    let _ = std::fs::remove_file(&src);
    let _ = std::fs::remove_file(&out);
}

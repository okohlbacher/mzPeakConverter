//! The mzPeak-input lane: `.mzpeak → .mzpeak` (src/filter.rs) and `.mzpeak → .mzML`
//! (`filter_mzpeak_to_mzml`). The rewrite lane shipped without a single test, and four defects sat
//! in it:
//!
//!   * every filter, even a pure `--sdrf` inject, refused an archive holding wavelength (UV/PDA)
//!     spectra with exit 1;
//!   * `--drop-aux` could delete a core facet and exit 0;
//!   * `--ms-level` / `--rt` defaulted a missing or retyped column (level 0 / NaN) and kept nothing;
//!   * `--rt` never refreshed `number_of_data_points` in the flat `chromatograms_metadata`;
//!   * an archive it wrote with `--ms-level` / `--rt` could not be read back.
//!
//! Each test converts its fixture into a scratch directory that belongs to that test alone.

use arrow::array::{Array, LargeStringArray, RecordBatch, StructArray, UInt8Array, UInt64Array};
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const TINY: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/tiny.pwiz.1.1.mzML");
const PDA_UV: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/pda_uv.pwiz.mzML");

/// A fresh directory for ONE test. The tests in a binary run in parallel under a single process id,
/// so a pid-only name let one test's cleanup delete another test's archive mid-run.
fn scratch(test: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("mzpc-filter-lane-{}-{test}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// `mzpeak-convert <input> -o <output> --force <extra…>`
fn mzpc(input: &Path, output: &Path, extra: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_mzpeak-convert"))
        .arg(input)
        .arg("-o")
        .arg(output)
        .arg("--force")
        .args(extra)
        .output()
        .expect("failed to run mzpeak-convert")
}

fn ok(r: &Output) {
    assert!(r.status.success(), "exit {:?}; stderr:\n{}", r.status.code(), String::from_utf8_lossy(&r.stderr));
}

/// Convert `fixture` into `dir/src.mzpeak`.
fn convert(fixture: &str, dir: &Path) -> PathBuf {
    let archive = dir.join("src.mzpeak");
    ok(&mzpc(Path::new(fixture), &archive, &[]));
    archive
}

fn member(archive: &Path, name: &str) -> Vec<u8> {
    let mut zip = zip::ZipArchive::new(File::open(archive).unwrap()).unwrap();
    let mut v = Vec::new();
    zip.by_name(name)
        .unwrap_or_else(|_| panic!("{name} missing from {}", archive.display()))
        .read_to_end(&mut v)
        .unwrap();
    v
}

fn table(archive: &Path, name: &str) -> RecordBatch {
    let b = ParquetRecordBatchReaderBuilder::try_new(bytes::Bytes::from(member(archive, name))).unwrap();
    let schema = b.schema().clone();
    let batches: Vec<_> = b.build().unwrap().map(Result::unwrap).collect();
    arrow::compute::concat_batches(&schema, &batches).unwrap()
}

fn footer(archive: &Path, name: &str, key: &str) -> Option<String> {
    let b = ParquetRecordBatchReaderBuilder::try_new(bytes::Bytes::from(member(archive, name))).unwrap();
    b.metadata().file_metadata().key_value_metadata()?.iter().find(|kv| kv.key == key)?.value.clone()
}

fn column<T: From<arrow::array::ArrayData>>(t: &RecordBatch, name: &str) -> T {
    T::from(t.column_by_name(name).unwrap_or_else(|| panic!("no `{name}` in {:?}", t.schema())).to_data())
}

/// (a) `--ms-level 2` keeps the one MS2 spectrum and nulls its reference to the MS1 it came from.
#[test]
fn ms_level_keeps_matching_spectra_and_nulls_dropped_parent_refs() {
    let dir = scratch("ms_level");
    let src = convert(TINY, &dir);
    let out = dir.join("f.mzpeak");
    ok(&mzpc(&src, &out, &["--ms-level", "2"]));

    let meta = table(&out, "spectra_metadata.parquet");
    assert_eq!(meta.num_rows(), 1);
    assert_eq!(column::<UInt8Array>(&meta, "ms_level").value(0), 2);

    let precursors = table(&out, "spectra_metadata_precursors.parquet");
    assert_eq!(precursors.num_rows(), 1);
    assert!(column::<UInt64Array>(&precursors, "precursor_index").is_null(0), "the MS1 parent was filtered out");
    let _ = std::fs::remove_dir_all(&dir);
}

/// (b) `--rt` keeps the spectra in the window, truncates the chromatogram traces to it, and rewrites
/// each chromatogram's `number_of_data_points` to what is left.
#[test]
fn rt_window_truncates_chromatograms_and_refreshes_point_counts() {
    let dir = scratch("rt_window");
    let src = convert(TINY, &dir);
    let out = dir.join("f.mzpeak");
    ok(&mzpc(&src, &out, &["--rt", "0-0.0001"]));

    assert_eq!(table(&out, "spectra_metadata.parquet").num_rows(), 1);

    // What the window must leave, counted in the source archive rather than written down: which
    // traces start at t = 0 depends on what the converter carries (tiny's own `sic` came back with
    // the Latin-1 transcode fix).
    let src_point: StructArray = column(&table(&src, "chromatograms_data.parquet"), "point");
    let time = arrow::compute::cast(src_point.column_by_name("time").unwrap(), &arrow::datatypes::DataType::Float64).unwrap();
    let time = time.as_any().downcast_ref::<arrow::array::Float64Array>().unwrap();
    let want = (0..time.len()).filter(|&r| (0.0..=0.0001).contains(&time.value(r))).count() as u64;
    assert!(want > 0, "the source holds no chromatogram point inside the window");

    let data = table(&out, "chromatograms_data.parquet");
    let point: StructArray = column(&data, "point");
    let idx = UInt64Array::from(point.column_by_name("chromatogram_index").unwrap().to_data());
    let mut left: HashMap<u64, u64> = HashMap::new();
    for r in 0..idx.len() {
        *left.entry(idx.value(r)).or_default() += 1;
    }
    assert_eq!(left.values().sum::<u64>(), want, "points left in the window: {left:?}");

    let meta = table(&out, "chromatograms_metadata.parquet");
    let (index, n) = (column::<UInt64Array>(&meta, "index"), column::<UInt64Array>(&meta, "number_of_data_points"));
    for r in 0..meta.num_rows() {
        let c = index.value(r);
        assert_eq!(n.value(r), left.get(&c).copied().unwrap_or(0), "chromatogram {c}: stale number_of_data_points");
    }
    let total = footer(&out, "chromatograms_metadata.parquet", "chromatogram_data_point_count");
    assert_eq!(total, Some(want.to_string()), "the metadata footer total follows the truncation");
    let _ = std::fs::remove_dir_all(&dir);
}

/// (c) The same two filters through `-o f.mzML`.
#[test]
fn mzml_output_applies_the_same_filters() {
    let dir = scratch("mzml");
    let src = convert(TINY, &dir);
    let out = dir.join("f.mzML");
    for args in [["--ms-level", "2"], ["--rt", "0-0.0001"]] {
        ok(&mzpc(&src, &out, &args));
        let xml = std::fs::read_to_string(&out).unwrap();
        assert_eq!(xml.matches("<spectrum ").count(), 1, "{args:?}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// A filtered archive keeps each survivor's original, now sparse, `index`. Reading one back aborted
/// (the reader sized its per-spectrum tables by row count), and the export walked `0..len`, which
/// asks for spectra that are gone. Each archive must export exactly the spectra it holds.
#[test]
fn filtered_archives_read_back() {
    let dir = scratch("read_back");
    let src = convert(TINY, &dir);
    let (out, mzml) = (dir.join("f.mzpeak"), dir.join("f.mzML"));
    for args in [["--ms-level", "1"], ["--ms-level", "2"], ["--rt", "0-1"], ["--rt", "0-0.0001"]] {
        ok(&mzpc(&src, &out, &args));
        let meta = table(&out, "spectra_metadata.parquet");
        let (index, id) = (column::<UInt64Array>(&meta, "index"), column::<LargeStringArray>(&meta, "id"));
        let mut want: Vec<(u64, String)> = (0..meta.num_rows()).map(|r| (index.value(r), id.value(r).to_string())).collect();
        want.sort();
        let want: Vec<String> = want.into_iter().map(|(_, id)| id).collect();

        ok(&mzpc(&out, &mzml, &[]));
        let xml = std::fs::read_to_string(&mzml).unwrap();
        let got: Vec<&str> = xml.split("<spectrum id=\"").skip(1).map(|s| s.split('"').next().unwrap()).collect();
        assert_eq!(got, want, "{args:?}: the export must hold exactly the archive's spectra");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// A rewrite keeps the survivors' original indices, so its data facets are sparse as well: their
/// `spectrum_count` is one past the largest index left, the bound a reader iterates to, not the
/// number of spectra left. On tiny, spectrum 1 is the profile spectrum in spectra_data, and
/// spectra_peaks holds 0 and 3 (2 is an empty centroid spectrum).
#[test]
fn rewritten_data_facets_declare_an_index_bound() {
    let dir = scratch("count_bound");
    let src = convert(TINY, &dir);
    let out = dir.join("f.mzpeak");
    for (args, data, peaks) in [(["--ms-level", "1"], "0", "4"), (["--rt", "0-1"], "0", "4"), (["--ms-level", "2"], "2", "0")] {
        ok(&mzpc(&src, &out, &args));
        assert_eq!(footer(&out, "spectra_data.parquet", "spectrum_count").as_deref(), Some(data), "{args:?}: spectra_data");
        assert_eq!(footer(&out, "spectra_peaks.parquet", "spectrum_count").as_deref(), Some(peaks), "{args:?}: spectra_peaks");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// A rewritten facet must not embed the pre-filter counts in `ARROW:schema`. arrow-rs folds the source
/// footer into the schema it reads, the rewrite's writer serialised that schema, and Arrow C++ and
/// pyarrow return the embedded metadata: after `--ms-level 1`, `spectra_data` said `spectrum_count=1`
/// on 0 rows.
#[test]
fn rewrite_embeds_no_stale_counts_in_the_arrow_schema() {
    let dir = scratch("arrow_schema");
    let src = convert(TINY, &dir);
    let out = dir.join("f.mzpeak");
    ok(&mzpc(&src, &out, &["--ms-level", "1"]));
    let names: Vec<String> = zip::ZipArchive::new(File::open(&out).unwrap())
        .unwrap()
        .file_names()
        .filter(|n| n.ends_with(".parquet"))
        .map(str::to_string)
        .collect();
    for name in names {
        let b = ParquetRecordBatchReaderBuilder::try_new(bytes::Bytes::from(member(&out, &name))).unwrap();
        let meta = b.metadata().file_metadata();
        let Some(embedded) = meta.key_value_metadata().and_then(|kvs| kvs.iter().find(|kv| kv.key == "ARROW:schema")).cloned() else {
            continue;
        };
        // Given only the ARROW:schema entry, the schema's metadata is exactly what that entry embeds.
        let schema = parquet::arrow::parquet_to_arrow_schema(meta.schema_descr(), Some(&vec![embedded])).unwrap();
        let counts: Vec<&String> = schema.metadata().keys().filter(|k| k.ends_with("_count")).collect();
        assert!(counts.is_empty(), "{name} embeds {counts:?} in ARROW:schema");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// (d) `--rt` parsing, driven through the CLI (the crate has no library target to call into): an
/// omitted bound is open, and a reversed or non-numeric range exits 1 without writing anything. The
/// bounds are read back from the filter's data-processing entry. `--rt=` because clap reads a bare
/// `-30` as a flag.
#[test]
fn rt_parses_open_bounds_and_refuses_bad_ranges() {
    let dir = scratch("parse_rt");
    let src = convert(TINY, &dir);
    let out = dir.join("f.mzpeak");
    // tiny's spectra sit at 0.0, 0.70, 5.89 and 5.99 min.
    for (arg, recorded, kept) in [("10-", "rt=10-inf", 0), ("-30", "rt=-inf-30", 4)] {
        ok(&mzpc(&src, &out, &[&format!("--rt={arg}")]));
        let index = String::from_utf8(member(&out, "mzpeak_index.json")).unwrap();
        assert!(index.contains(recorded), "--rt {arg}: expected `{recorded}` in the index:\n{index}");
        assert_eq!(table(&out, "spectra_metadata.parquet").num_rows(), kept, "--rt {arg}");
    }
    for arg in ["5-1", "a-b"] {
        let _ = std::fs::remove_file(&out);
        let r = mzpc(&src, &out, &[&format!("--rt={arg}")]);
        let stderr = String::from_utf8_lossy(&r.stderr);
        assert_eq!(r.status.code(), Some(1), "--rt {arg}; stderr:\n{stderr}");
        assert!(stderr.contains("--rt"), "--rt {arg}: the error must name the flag; stderr:\n{stderr}");
        assert!(!out.exists(), "--rt {arg} wrote output");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// Item 1: an archive with wavelength spectra filters by MS level and takes an SDRF. `--ms-level`
/// leaves the wavelength (UV/PDA) spectra out, facets and all, as the mzML export does: they have no
/// MS level. Their facets used to fail classification and abort every filter.
#[test]
fn wavelength_archive_filters_by_ms_level_and_takes_an_sdrf() {
    let dir = scratch("wavelength");
    let src = convert(PDA_UV, &dir);
    assert_eq!(table(&src, "wavelength_spectra_metadata.parquet").num_rows(), 8, "the fixture holds 8 UV spectra");

    let sdrf = dir.join("s.tsv");
    std::fs::write(&sdrf, "source name\tcomment[data file]\nsample 1\tpda_uv.raw\n").unwrap();
    let out = dir.join("f.mzpeak");
    let r = mzpc(&src, &out, &["--ms-level", "1", "--sdrf", sdrf.to_str().unwrap()]);
    ok(&r);

    let meta = table(&out, "spectra_metadata.parquet");
    assert_eq!(meta.num_rows(), 1);
    assert_eq!(column::<UInt8Array>(&meta, "ms_level").value(0), 1);
    let names: Vec<String> = zip::ZipArchive::new(File::open(&out).unwrap()).unwrap().file_names().map(str::to_string).collect();
    assert!(names.iter().all(|n| !n.starts_with("wavelength_spectra")), "UV facets survived --ms-level: {names:?}");
    assert!(String::from_utf8_lossy(&r.stderr).contains("leaves out the 8 wavelength"), "{}", String::from_utf8_lossy(&r.stderr));
    assert_eq!(member(&out, "sample_metadata/sdrf.tsv"), std::fs::read(&sdrf).unwrap());
    let _ = std::fs::remove_dir_all(&dir);
}

/// `--rt` keeps the wavelength spectra inside the window in all three of their facets, and their
/// footer counts follow: the entity count one past the largest index left, as for mass spectra.
#[test]
fn an_rt_archive_filter_keeps_the_wavelength_spectra_in_the_window() {
    let dir = scratch("wavelength-rt");
    let src = convert(PDA_UV, &dir);
    let out = dir.join("f.mzpeak");
    ok(&mzpc(&src, &out, &["--rt", "0.003-0.0055"]));
    let meta = table(&out, "wavelength_spectra_metadata.parquet");
    assert_eq!(column::<UInt64Array>(&meta, "index").values().as_ref(), &[4u64, 5, 6], "scans 5, 6 and 7 lie in the window");
    assert_eq!(table(&out, "wavelength_spectra_metadata_scans.parquet").num_rows(), 3);
    assert_eq!(table(&out, "wavelength_spectra_data.parquet").num_rows(), 3 * 191);
    let count = |name: &str, key: &str| footer(&out, name, key);
    assert_eq!(count("wavelength_spectra_metadata.parquet", "wavelength_spectrum_count").as_deref(), Some("3"));
    assert_eq!(count("wavelength_spectra_metadata.parquet", "wavelength_spectrum_data_point_count").as_deref(), Some("573"));
    assert_eq!(count("wavelength_spectra_data.parquet", "wavelength_spectrum_count").as_deref(), Some("7"));
    assert_eq!(count("wavelength_spectra_data.parquet", "wavelength_spectrum_data_point_count").as_deref(), Some("573"));
    let _ = std::fs::remove_dir_all(&dir);
}

/// Item 2: `--drop-aux` must not take a core facet with it — not by name, not by a careless glob.
#[test]
fn drop_aux_refuses_to_remove_a_core_facet() {
    let dir = scratch("drop_core");
    let src = convert(TINY, &dir);
    let out = dir.join("f.mzpeak");
    for glob in ["spectra_data.parquet", "spectra_metadata_precursors.parquet", "chromatograms_data.parquet", "chromatograms_metadata.parquet", "*.parquet"] {
        let r = mzpc(&src, &out, &["--drop-aux", glob]);
        let stderr = String::from_utf8_lossy(&r.stderr);
        assert_eq!(r.status.code(), Some(1), "--drop-aux {glob} must be refused; stderr:\n{stderr}");
        assert!(!out.exists() && !dir.join("f.mzpeak.tmp").exists(), "--drop-aux {glob} wrote output");
    }
    // The refusal is about core facets, not the flag: `--no-vendor` (a `vendor*` drop) still runs.
    ok(&mzpc(&src, &out, &["--no-vendor"]));
    // Wavelength (UV/PDA) facets reference only each other: stripping all of them stays allowed.
    let uv_dir = scratch("drop_uv");
    let uv = convert(PDA_UV, &uv_dir);
    let stripped = uv_dir.join("f.mzpeak");
    ok(&mzpc(&uv, &stripped, &["--drop-aux", "wavelength_spectra*"]));
    let names: Vec<String> = zip::ZipArchive::new(File::open(&stripped).unwrap()).unwrap().file_names().map(str::to_string).collect();
    assert!(names.iter().all(|n| !n.starts_with("wavelength_spectra")), "UV facets survived: {names:?}");
    let _ = std::fs::remove_dir_all(&uv_dir);
    let _ = std::fs::remove_dir_all(&dir);
}

// ── the wavelength (UV/PDA) spectra of an archive → mzML export ────────────────────────────────────

/// Each `<spectrum ...>` element of an mzML, up to its `</spectrum>`.
fn spectrum_blocks(xml: &str) -> Vec<&str> {
    xml.match_indices("<spectrum ").map(|(at, _)| &xml[at..at + xml[at..].find("</spectrum>").unwrap()]).collect()
}

/// The ids of an mzML's spectra, in document order.
fn spectrum_ids(mzml: &Path) -> Vec<String> {
    let xml = std::fs::read_to_string(mzml).unwrap();
    spectrum_blocks(&xml)
        .iter()
        .map(|b| b.split(" id=\"").nth(1).and_then(|v| v.split('"').next()).unwrap().to_string())
        .collect()
}

/// Each wavelength spectrum's element of an mzML, up to its arrays.
fn wavelength_heads(mzml: &Path) -> Vec<String> {
    let xml = std::fs::read_to_string(mzml).unwrap();
    spectrum_blocks(&xml)
        .into_iter()
        .filter(|b| b.contains(r#"accession="MS:1000617""#))
        .map(|b| b[..b.find("<binaryDataArrayList").unwrap()].to_string())
        .collect()
}

/// The source's spectrum ids, wavelength spectra included, in its order.
fn pda_source_ids() -> Vec<String> {
    spectrum_ids(Path::new(PDA_UV))
}

/// The export read the mass-spectrum facets only: a PDA run's archive came out without its UV spectra
/// (8 of the fixture's 10 spectra; 520 of the corpus TOFsulfas file's 732), and nothing said so. They
/// come back in time order, which for this fixture is also its source order.
#[test]
fn the_mzml_export_carries_the_wavelength_spectra_in_time_order() {
    let dir = scratch("uv-export");
    let src = convert(PDA_UV, &dir);
    let out = dir.join("f.mzML");
    ok(&mzpc(&src, &out, &[]));
    assert_eq!(spectrum_ids(&out), pda_source_ids());
    assert!(std::fs::read_to_string(&out).unwrap().contains(r#"<spectrumList count="10""#));
    assert_eq!(wavelength_heads(&out).len(), 8);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_exported_wavelength_arrays_are_the_sources() {
    use mzdata::prelude::*;
    use mzdata::spectrum::ArrayType;
    let dir = scratch("uv-arrays");
    let src = convert(PDA_UV, &dir);
    let out = dir.join("f.mzML");
    ok(&mzpc(&src, &out, &[]));
    type Uv = (Vec<f32>, Vec<f32>, f64);
    let read = |path: &Path| -> std::collections::BTreeMap<String, Uv> {
        mzdata::io::mzml::MzMLReader::open_path(path)
            .unwrap()
            .filter(|s| s.spectrum_type().is_some_and(|t| t.default_main_axis() == ArrayType::WavelengthArray))
            .map(|s| {
                let arrays = s.raw_arrays().unwrap();
                let wavelengths = arrays.get(&ArrayType::WavelengthArray).unwrap().to_f32().unwrap().to_vec();
                (s.id().to_string(), (wavelengths, arrays.intensities().unwrap().to_vec(), s.start_time()))
            })
            .collect()
    };
    let (source, exported) = (read(Path::new(PDA_UV)), read(&out));
    assert_eq!(source.len(), 8);
    assert_eq!(source.keys().collect::<Vec<_>>(), exported.keys().collect::<Vec<_>>());
    for (id, (wavelengths, intensities, time)) in &source {
        let (w, i, t) = &exported[id];
        assert_eq!((w, i), (wavelengths, intensities), "{id}: arrays differ");
        assert!((t - time).abs() < 1e-6, "{id}: time {t} vs {time}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// mzdata's writer sums every spectrum it writes into its TIC and base-peak chromatograms, so a PDA
/// run's absorbance, negative values included, landed in the mass spectrometer's summary: on `--to mzml`
/// from the source (10 points, -4389 among them) and on the export. The source's own TIC (2,360 points)
/// is kept as it is on both routes, so the summed one is the base-peak chromatogram alone — the
/// writer's `BIC` on the direct route, the archive's MS1-summed `BPC` on the export, where the writer
/// adds nothing.
#[test]
fn the_mzml_tic_sums_the_mass_spectra_only() {
    use mzdata::prelude::*;
    let dir = scratch("uv-tic");
    let src = convert(PDA_UV, &dir);
    let export = dir.join("export.mzML");
    ok(&mzpc(&src, &export, &[]));
    let direct = dir.join("direct.mzML");
    ok(&mzpc(Path::new(PDA_UV), &direct, &[]));
    for (mzml, bpc, points) in [(&export, "BPC", 1), (&direct, "BIC", 2)] {
        let mut reader = mzdata::io::mzml::MzMLReader::open_path(mzml).unwrap();
        let tic = reader.get_chromatogram_by_id("TIC").unwrap_or_else(|| panic!("{}: no TIC", mzml.display()));
        assert_eq!(tic.intensity().unwrap().len(), 2360, "{}: the source's TIC, kept as it is", mzml.display());
        let chrom = reader.get_chromatogram_by_id(bpc).unwrap_or_else(|| panic!("{}: no {bpc}", mzml.display()));
        let intensity = chrom.intensity().unwrap();
        assert_eq!(intensity.len(), points, "{}: {bpc} has a point per mass spectrum summed, UV excluded", mzml.display());
        assert!(intensity.iter().all(|&v| v >= 0.0), "{}: {bpc} {intensity:?}", mzml.display());
        assert!(reader.get_chromatogram_by_id(if bpc == "BPC" { "BIC" } else { "BPC" }).is_none(), "{}: one base-peak chromatogram", mzml.display());
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// What mzdata's writer states for a wavelength spectrum that it does not have — `ms level` 0,
/// `positive scan`, a second type term, an `ion injection time` of 0 — is blanked, on both lanes. The
/// export also leaves out the summaries the archive computed from the arrays.
#[test]
fn an_exported_wavelength_spectrum_states_only_what_it_has() {
    let dir = scratch("uv-terms");
    let src = convert(PDA_UV, &dir);
    let export = dir.join("export.mzML");
    ok(&mzpc(&src, &export, &[]));
    let direct = dir.join("direct.mzML");
    ok(&mzpc(Path::new(PDA_UV), &direct, &[]));
    for (mzml, from_archive) in [(&export, true), (&direct, false)] {
        let heads = wavelength_heads(mzml);
        assert_eq!(heads.len(), 8, "{}", mzml.display());
        for head in heads {
            assert_eq!(head.matches(r#"accession="MS:1000804""#).count(), 1, "{head}");
            for invented in [r#"name="ms level""#, r#"name="positive scan""#, r#"name="ion injection time""#] {
                assert!(!head.contains(invented), "{}: {invented} in {head}", mzml.display());
            }
            if from_archive {
                for computed in ["MS:1000285", "MS:1000505", "MS:1000504", "MS:1003812"] {
                    assert!(!head.contains(computed), "{computed} in {head}");
                }
            }
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// A wavelength spectrum has no MS level: in a search engine's MS2 slice it would read as MS1 profile
/// data with wavelengths for m/z. The export leaves them out and says so, as filtering into an archive
/// does (`wavelength_archive_filters_by_ms_level_and_takes_an_sdrf`).
#[test]
fn an_ms_level_export_leaves_the_wavelength_spectra_out_and_says_so() {
    let dir = scratch("uv-ms-level");
    let src = convert(PDA_UV, &dir);
    let out = dir.join("f.mzML");
    let r = mzpc(&src, &out, &["--ms-level", "2"]);
    ok(&r);
    assert_eq!(spectrum_ids(&out), ["function=2 process=0 scan=1"]);
    assert!(String::from_utf8_lossy(&r.stderr).contains("leaves out the 8 wavelength"), "{}", String::from_utf8_lossy(&r.stderr));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_rt_export_keeps_the_wavelength_spectra_in_the_window() {
    let dir = scratch("uv-rt");
    let src = convert(PDA_UV, &dir);
    let out = dir.join("f.mzML");
    ok(&mzpc(&src, &out, &["--rt", "0.003-0.0055"]));
    assert_eq!(
        spectrum_ids(&out),
        ["function=3 process=0 scan=5", "function=3 process=0 scan=6", "function=3 process=0 scan=7", "function=2 process=0 scan=1"]
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// `MZPC_MAX_SPECTRA` takes the first spectra of the export's order, a wavelength spectrum counting as
/// one, as it does in every import lane.
#[test]
fn the_spectrum_cap_counts_wavelength_spectra() {
    let dir = scratch("uv-cap");
    let src = convert(PDA_UV, &dir);
    let out = dir.join("f.mzML");
    let r = Command::new(env!("CARGO_BIN_EXE_mzpeak-convert"))
        .arg(&src)
        .arg("-o")
        .arg(&out)
        .arg("--force")
        .env("MZPC_MAX_SPECTRA", "5")
        .output()
        .unwrap();
    ok(&r);
    assert_eq!(spectrum_ids(&out), pda_source_ids()[..5]);
    let _ = std::fs::remove_dir_all(&dir);
}

/// archive → mzML → archive keeps every wavelength spectrum, and a second export still states each
/// type term once: the duplicate the reader hands over does not grow with each round trip.
#[test]
fn the_wavelength_facet_survives_an_mzml_round_trip() {
    let dir = scratch("uv-round-trip");
    let src = convert(PDA_UV, &dir);
    let mzml = dir.join("f.mzML");
    ok(&mzpc(&src, &mzml, &[]));
    let back = dir.join("back.mzpeak");
    ok(&mzpc(&mzml, &back, &[]));
    assert_eq!(table(&back, "wavelength_spectra_metadata.parquet").num_rows(), 8);
    assert_eq!(table(&back, "wavelength_spectra_data.parquet").num_rows(), 1528);
    let again = dir.join("again.mzML");
    ok(&mzpc(&back, &again, &[]));
    assert_eq!(spectrum_ids(&again), pda_source_ids());
    for head in wavelength_heads(&again) {
        assert_eq!(head.matches(r#"accession="MS:1000804""#).count(), 1, "{head}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// `--drop-aux` can take one wavelength member and leave the others; the reader then panicked on the
/// arrays it could not find.
#[test]
fn an_archive_without_its_wavelength_arrays_still_exports() {
    let dir = scratch("uv-no-data");
    let src = convert(PDA_UV, &dir);
    let stripped = dir.join("no-data.mzpeak");
    ok(&mzpc(&src, &stripped, &["--drop-aux", "wavelength_spectra_data.parquet"]));
    let out = dir.join("f.mzML");
    let r = mzpc(&stripped, &out, &[]);
    ok(&r);
    assert_eq!(spectrum_ids(&out), ["function=1 process=0 scan=1", "function=2 process=0 scan=1"]);
    assert!(String::from_utf8_lossy(&r.stderr).contains("no wavelength_spectra_data.parquet"), "{}", String::from_utf8_lossy(&r.stderr));
    let _ = std::fs::remove_dir_all(&dir);
}

/// MS:1000789 and MS:1000790 are mass spectra, children of MS1 and MSn spectrum. mzdata's
/// `is_mass_spectrum` tests direct parents only, so a guard keyed on it would take them out of the
/// writer's summaries — the base-peak one here, the source carrying its own TIC.
#[test]
fn mass_spectra_of_a_child_type_stay_in_the_tic() {
    use mzdata::prelude::*;
    let dir = scratch("child-types");
    let src = std::fs::read_to_string(PDA_UV).unwrap();
    // The fixture types its MS2 spectrum only (the MS1 is typed by its level): make that one a
    // time-delayed fragmentation spectrum.
    let retyped = src.replacen(
        r#"accession="MS:1000580" name="MSn spectrum""#,
        r#"accession="MS:1000790" name="time-delayed fragmentation spectrum""#,
        1,
    );
    assert_ne!(retyped, src, "the fixture's MSn spectrum term moved");
    let input = dir.join("child-types.mzML");
    std::fs::write(&input, retyped).unwrap();
    let out = dir.join("out.mzML");
    ok(&mzpc(&input, &out, &[]));
    let mut reader = mzdata::io::mzml::MzMLReader::open_path(&out).unwrap();
    assert_eq!(reader.get_chromatogram_by_id("BIC").unwrap().intensity().unwrap().len(), 2);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Filtering straight to mzML, and filtering into an archive and exporting that, write the same
/// spectra: the two routes take the same rules for wavelength spectra.
#[test]
fn the_direct_and_the_two_step_export_agree_on_wavelength_spectra() {
    let dir = scratch("uv-two-routes");
    let src = convert(PDA_UV, &dir);
    for (tag, args) in [("ms-level", ["--ms-level", "2"]), ("rt", ["--rt", "0.003-0.0055"])] {
        let direct = dir.join(format!("{tag}.direct.mzML"));
        ok(&mzpc(&src, &direct, &args));
        let archive = dir.join(format!("{tag}.mzpeak"));
        ok(&mzpc(&src, &archive, &args));
        let two_step = dir.join(format!("{tag}.two-step.mzML"));
        ok(&mzpc(&archive, &two_step, &[]));
        assert_eq!(spectrum_ids(&direct), spectrum_ids(&two_step), "{tag}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// The vendored reader never read the wavelength scans facet, so every exported UV spectrum lost what
/// its scan states: here each one's `preset scan configuration` 3.
#[test]
fn the_export_keeps_what_a_wavelength_scan_states() {
    let dir = scratch("uv-scan-values");
    let src = convert(PDA_UV, &dir);
    let out = dir.join("f.mzML");
    ok(&mzpc(&src, &out, &[]));
    let heads = wavelength_heads(&out);
    assert_eq!(heads.len(), 8);
    for head in heads {
        let scan = &head[head.find("<scan ").or_else(|| head.find("<scan>")).expect("a scan")..];
        assert!(scan.contains(r#"accession="MS:1000616""#) && scan.contains(r#"value="3""#), "{head}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// Without its scans facet (`--drop-aux` can take it) a wavelength spectrum keeps the time the
/// metadata facet states.
#[test]
fn a_wavelength_spectrum_without_its_scan_keeps_its_time() {
    use mzdata::prelude::*;
    let dir = scratch("uv-no-scans");
    let src = convert(PDA_UV, &dir);
    let stripped = dir.join("no-scans.mzpeak");
    ok(&mzpc(&src, &stripped, &["--drop-aux", "wavelength_spectra_metadata_scans.parquet"]));
    let out = dir.join("f.mzML");
    ok(&mzpc(&stripped, &out, &[]));
    let times = |path: &Path| -> Vec<(String, f64)> {
        mzdata::io::mzml::MzMLReader::open_path(path).unwrap().map(|s| (s.id().to_string(), s.start_time())).collect()
    };
    let (source, exported) = (times(Path::new(PDA_UV)), times(&out));
    assert_eq!(source.len(), exported.len());
    for ((id, t), (exported_id, e)) in source.iter().zip(&exported) {
        assert_eq!(id, exported_id);
        assert!((t - e).abs() < 1e-6, "{id}: {e} vs {t}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// A wavelength range the source states wins over the one the archive computed from the arrays, and
/// a spectrum that states none gains none.
#[test]
fn a_stated_wavelength_range_wins_over_the_computed_one() {
    let dir = scratch("uv-stated-range");
    let text = std::fs::read_to_string(PDA_UV).unwrap();
    let at = text.find(r#"id="function=3 process=0 scan=5""#).unwrap();
    let term = r#"<cvParam cvRef="MS" accession="MS:1000804" name="electromagnetic radiation spectrum" value=""/>"#;
    let end = at + text[at..].find(term).unwrap() + term.len();
    let stated = concat!(
        "\n        <cvParam cvRef=\"MS\" accession=\"MS:1000619\" name=\"lowest observed wavelength\" value=\"190\" unitCvRef=\"UO\" unitAccession=\"UO:0000018\" unitName=\"nanometer\"/>",
        "\n        <cvParam cvRef=\"MS\" accession=\"MS:1000618\" name=\"highest observed wavelength\" value=\"800\" unitCvRef=\"UO\" unitAccession=\"UO:0000018\" unitName=\"nanometer\"/>"
    );
    let input = dir.join("stated-range.mzML");
    std::fs::write(&input, format!("{}{stated}{}", &text[..end], &text[end..])).unwrap();
    let archive = dir.join("stated-range.mzpeak");
    ok(&mzpc(&input, &archive, &[]));
    let out = dir.join("f.mzML");
    ok(&mzpc(&archive, &out, &[]));
    for head in wavelength_heads(&out) {
        if head.contains(r#"id="function=3 process=0 scan=5""#) {
            assert_eq!((head.matches("MS:1000619").count(), head.matches("MS:1000618").count()), (1, 1), "{head}");
            assert!(head.contains(r#"value="190"#) && head.contains(r#"value="800"#), "the stated range was replaced: {head}");
        } else {
            assert!(!head.contains("MS:1000619") && !head.contains("MS:1000618"), "a computed range was added: {head}");
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

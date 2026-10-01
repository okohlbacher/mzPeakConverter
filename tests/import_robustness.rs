//! What an mzML, imzML or Thermo import must survive and must not lose: the import findings of the
//! 0.17.0-rc.1 corpus audit, each on the smallest input that shows it.
//!
//! * a header value that reads as NaN or infinity aborted the conversion (exit 134, no archive);
//! * a source file's SHA-1 of decimal digits became a number;
//! * a run `startTimeStamp` without a UTC offset was dropped with an ERROR line and no trace;
//! * a spectrum's `sourceFileRef` was not stored;
//! * every source processing method gained an invented `file format conversion` term;
//! * a device trace written as an `intensity array` in pascal was stored and exported as counts
//!   (and, moved out of the way, must not take an ion current in counts per second with it, nor
//!   leave the facet without an `intensity` column; a unit mzdata does not know is read back);
//! * an imaging run that states no scan start time got a TIC and a base-peak chromatogram with
//!   every point at time 0, and `provenance.time` said "as stated" when one spectrum of nine states
//!   one;
//! * an `.ibd` that does not begin with the imzML's UUID left nothing in the archive;
//! * a pixel size in millimetres gave the marker no `pixel_size_um`, a zero or negative one did;
//! * a path that does not exist was reported as a missing .NET framework;
//! * a Thermo run without MS1 named scan 1 as the precursor spectrum of every scan, scan 1 included.
//!
//! `MZPC_TEST_BINARY=/path/to/another/mzpeak-convert` runs the same assertions against another
//! build (the release before the fix, to see them fail).

use arrow::array::{Array, ArrayRef, AsArray, LargeListArray, LargeStringArray, ListArray, RecordBatch, StringArray};
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

#[path = "common/corpus.rs"]
mod corpus;

const TINY: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/tiny.pwiz.1.1.mzML");
const IMZML: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/imaging/Synthetic_DeclaredGrid.imzML");
const SMALL_RAW: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/small.RAW");

fn bin() -> PathBuf {
    std::env::var_os("MZPC_TEST_BINARY").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_mzpeak-convert")))
}

fn scratch(test: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("mzpc-import-{}-{test}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Run the converter; exit code and log.
fn run(args: &[&std::ffi::OsStr], envs: &[(&str, &str)]) -> (Option<i32>, String) {
    let mut cmd = Command::new(bin());
    cmd.args(args).env_remove("RUST_LOG").env_remove("MZPC_MAX_SPECTRA");
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let r = cmd.output().expect("failed to run mzpeak-convert");
    (r.status.code(), String::from_utf8_lossy(&r.stderr).into_owned())
}

/// Convert `input` to `dir/<name>`; the output and the run's log.
fn convert(input: &Path, dir: &Path, name: &str) -> (PathBuf, String) {
    convert_with(input, dir, name, &[])
}

fn convert_with(input: &Path, dir: &Path, name: &str, envs: &[(&str, &str)]) -> (PathBuf, String) {
    let out = dir.join(name);
    let (code, log) = run(&[input.as_os_str(), "-o".as_ref(), out.as_os_str(), "--force".as_ref()], envs);
    assert_eq!(code, Some(0), "converting {} failed; stderr:\n{log}", input.display());
    (out, log)
}

/// `source` with each `(from, to)` replaced exactly once, written as `dir/name`.
fn variant(source: &str, dir: &Path, name: &str, edits: &[(&str, &str)]) -> PathBuf {
    let mut text = std::fs::read_to_string(source).unwrap();
    for (from, to) in edits {
        assert_eq!(text.matches(from).count(), 1, "{from:?} must occur exactly once in {source}");
        text = text.replace(from, to);
    }
    let path = dir.join(name);
    std::fs::write(&path, text).unwrap();
    path
}

/// The mzML fixture with each `(from, to)` replaced exactly once within its `<chromatogramList>`
/// (the spectra hold the same arrays as its TIC), written as `dir/name`.
fn chromatogram_variant(dir: &Path, name: &str, edits: &[(&str, &str)]) -> PathBuf {
    let text = std::fs::read_to_string(TINY).unwrap();
    let (head, mut list) = text.split_at(text.find("<chromatogramList").unwrap());
    let mut edited;
    for (from, to) in edits {
        assert_eq!(list.matches(from).count(), 1, "{from:?} must occur exactly once in the chromatogram list");
        edited = list.replace(from, to);
        list = &edited;
    }
    let path = dir.join(name);
    std::fs::write(&path, format!("{head}{list}")).unwrap();
    path
}

/// The synthetic imzML with `edits`, and its `.ibd` beside it.
fn imzml_variant(dir: &Path, name: &str, edits: &[(&str, &str)]) -> PathBuf {
    let path = variant(IMZML, dir, &format!("{name}.imzML"), edits);
    std::fs::copy(Path::new(IMZML).with_extension("ibd"), dir.join(format!("{name}.ibd"))).unwrap();
    path
}

fn member(archive: &Path, name: &str) -> Vec<u8> {
    let mut zip = zip::ZipArchive::new(File::open(archive).unwrap()).unwrap();
    let mut v = Vec::new();
    zip.by_name(name).unwrap_or_else(|_| panic!("{name} missing")).read_to_end(&mut v).unwrap();
    v
}

fn metadata(archive: &Path) -> serde_json::Value {
    serde_json::from_slice::<serde_json::Value>(&member(archive, "mzpeak_index.json")).unwrap()["metadata"].clone()
}

fn batches(archive: &Path, facet: &str) -> Vec<RecordBatch> {
    let b = ParquetRecordBatchReaderBuilder::try_new(bytes::Bytes::from(member(archive, facet))).unwrap();
    b.build().unwrap().map(Result::unwrap).collect()
}

fn text(a: &ArrayRef, i: usize) -> Option<String> {
    if a.is_null(i) {
        return None;
    }
    Some(match a.as_any().downcast_ref::<StringArray>() {
        Some(s) => s.value(i).to_string(),
        None => a.as_any().downcast_ref::<LargeStringArray>().expect("a string column").value(i).to_string(),
    })
}

fn list_item(a: &ArrayRef, i: usize) -> ArrayRef {
    match a.as_any().downcast_ref::<LargeListArray>() {
        Some(l) => l.value(i),
        None => a.as_any().downcast_ref::<ListArray>().expect("a list column").value(i),
    }
}

/// A facet's string column, nulls as `None`.
fn strings(archive: &Path, facet: &str, column: &str) -> Vec<Option<String>> {
    batches(archive, facet)
        .iter()
        .flat_map(|b| {
            let c = b.column_by_name(column).unwrap_or_else(|| panic!("{facet} has no {column}"));
            (0..c.len()).map(|i| text(c, i)).collect::<Vec<_>>()
        })
        .collect()
}

/// Per spectrum of `spectra_metadata`: its id and the string value of its parameter `name`.
fn spectrum_parameter(archive: &Path, name: &str) -> Vec<(String, Option<String>)> {
    parameter(archive, "spectra_metadata.parquet", name)
}

/// Per row of a metadata facet: its id and the string value of its parameter `name`.
fn parameter(archive: &Path, facet: &str, name: &str) -> Vec<(String, Option<String>)> {
    let mut out = Vec::new();
    for b in batches(archive, facet) {
        let (ids, params) = (b.column_by_name("id").unwrap(), b.column_by_name("parameters").unwrap());
        for i in 0..b.num_rows() {
            let item = list_item(params, i);
            let st = item.as_struct();
            let names = st.column_by_name("name").unwrap();
            let values = st.column_by_name("value").unwrap().as_struct().column_by_name("string").unwrap();
            let value = (0..st.len()).find(|&k| text(names, k).as_deref() == Some(name)).and_then(|k| text(values, k));
            out.push((text(ids, i).unwrap(), value));
        }
    }
    out
}

fn declared(m: &serde_json::Value, entry: &str) -> bool {
    m["transformations"].as_array().unwrap().iter().any(|t| t == entry)
}

fn sha1s(m: &serde_json::Value) -> Vec<(String, serde_json::Value)> {
    m["file_description"]["source_files"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|sf| {
            let id = sf["id"].as_str().unwrap().to_string();
            sf["parameters"].as_array().unwrap().iter().filter(|p| p["accession"] == "MS:1000569").map(move |p| (id.clone(), p["value"].clone())).collect::<Vec<_>>()
        })
        .collect()
}

/// JSON has no NaN and no infinity, and the writer unwrapped the conversion of every float: a
/// userParam whose text is `NaN` or `Inf`, or a digest of digits with an exponent beyond f64,
/// aborted the run (exit 134, nothing written). Such a value is stored as text.
#[test]
fn a_header_value_that_reads_as_nan_or_infinity_does_not_abort() {
    let dir = scratch("nonfinite");
    let input = variant(
        TINY,
        &dir,
        "nonfinite.mzML",
        &[
            ("name=\"Sample 1\">\n", "name=\"Sample 1\">\n        <userParam name=\"concentration\" value=\"NaN\"/>\n        <userParam name=\"operator\" value=\"Inf\"/>\n"),
            ("value=\"2345678901234567890123456789012345678901\"", "value=\"1234567890123456789012345678901234e99999\""),
        ],
    );
    let (archive, _) = convert(&input, &dir, "out.mzpeak");
    let m = metadata(&archive);
    let sample = m["sample_list"][0]["parameters"].as_array().unwrap();
    let value = |name: &str| sample.iter().find(|p| p["name"] == name).unwrap_or_else(|| panic!("{name} missing: {sample:?}"))["value"].clone();
    assert_eq!(value("concentration"), "NaN");
    assert_eq!(value("operator"), "inf", "infinity as Rust prints it");
    assert!(sha1s(&m).contains(&("tiny.wiff".to_string(), "1234567890123456789012345678901234e99999".into())), "{:?}", sha1s(&m));
    let _ = std::fs::remove_dir_all(&dir);
}

/// The fixture's three digests are 40 decimal digits each: mzdata reads each as a float, and the
/// archive held `1.2345678901234568e39`. They are the text the header states, on the archive lane
/// and in the direct mzML export.
#[test]
fn a_source_file_sha1_of_digits_stays_the_text_the_header_states() {
    let dir = scratch("sha1");
    let (archive, _) = convert(Path::new(TINY), &dir, "out.mzpeak");
    assert_eq!(
        sha1s(&metadata(&archive)),
        [
            ("tiny1.yep".to_string(), serde_json::json!("1234567890123456789012345678901234567890")),
            ("tiny.wiff".to_string(), serde_json::json!("2345678901234567890123456789012345678901")),
            ("sf_parameters".to_string(), serde_json::json!("3456789012345678901234567890123456789012")),
        ]
    );
    let (mzml, _) = convert(Path::new(TINY), &dir, "out.mzML");
    let text = std::fs::read_to_string(&mzml).unwrap();
    for digest in ["1234567890123456789012345678901234567890", "2345678901234567890123456789012345678901", "3456789012345678901234567890123456789012"] {
        assert!(text.contains(&format!("accession=\"MS:1000569\" cvRef=\"MS\" name=\"SHA-1\" value=\"{digest}\"")), "{digest} not written as stated");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// `startTimeStamp="2007-06-27T15:23:45.00035"` has no UTC offset: `run.start_time` cannot hold it,
/// and it is kept as the `acquisition_time` block the Waters and SciEX lanes write. A stamp with an
/// offset is `run.start_time`, with no block.
#[test]
fn a_start_time_stamp_without_an_offset_is_kept_as_acquisition_time() {
    let dir = scratch("stamp");
    let (archive, _) = convert(Path::new(TINY), &dir, "out.mzpeak");
    let m = metadata(&archive);
    assert!(m["run"]["start_time"].is_null(), "{}", m["run"]);
    let block = &m["acquisition_time"];
    assert_eq!(block["wall_clock"], "2007-06-27T15:23:45.000350", "{block}");
    assert_eq!(block["zone"], "unstated");
    assert_eq!(block["source"], "mzML run startTimeStamp");

    let zoned = variant(TINY, &dir, "zoned.mzML", &[("startTimeStamp=\"2007-06-27T15:23:45.00035\"", "startTimeStamp=\"2007-06-27T15:23:45.00035+02:00\"")]);
    let m = metadata(&convert(&zoned, &dir, "zoned.mzpeak").0);
    assert_eq!(m["run"]["start_time"], "2007-06-27T15:23:45.000350+02:00");
    assert!(m.get("acquisition_time").is_none(), "{}", m["acquisition_time"]);

    // No date-time at all: kept as stated, with no wall clock a reader could take for one.
    let odd = variant(TINY, &dir, "odd.mzML", &[("startTimeStamp=\"2007-06-27T15:23:45.00035\"", "startTimeStamp=\"June 27th\"")]);
    let m = metadata(&convert(&odd, &dir, "odd.mzpeak").0);
    assert_eq!(m["acquisition_time"]["stated"], "June 27th");
    assert!(m["acquisition_time"].get("wall_clock").is_none() && m["run"]["start_time"].is_null());
    assert!(m["acquisition_time"].get("zone").is_none(), "text that was not read states no zone, nor the lack of one: {}", m["acquisition_time"]);

    // An offset without its colon (ISO 8601's basic form) is the offset it states.
    let basic = variant(TINY, &dir, "basic.mzML", &[("startTimeStamp=\"2007-06-27T15:23:45.00035\"", "startTimeStamp=\"2007-06-27T15:23:45.00035+0200\"")]);
    let m = metadata(&convert(&basic, &dir, "basic.mzpeak").0);
    assert_eq!(m["run"]["start_time"], "2007-06-27T15:23:45.000350+02:00");
    assert!(m.get("acquisition_time").is_none(), "{}", m["acquisition_time"]);
    let _ = std::fs::remove_dir_all(&dir);
}

/// The fixture's fourth spectrum states `sourceFileRef="tiny.wiff"`, which mzdata does not read:
/// it is the spectrum parameter `sourceFileRef`, and a `userParam` of the direct mzML export. One
/// that names no source file is dropped and declared like the other references.
#[test]
fn a_spectrum_s_source_file_reference_is_stored() {
    let dir = scratch("sourcefileref");
    let (archive, _) = convert(Path::new(TINY), &dir, "out.mzpeak");
    let refs: Vec<Option<String>> = spectrum_parameter(&archive, "sourceFileRef").into_iter().map(|(_, v)| v).collect();
    assert_eq!(refs, [None, None, None, Some("tiny.wiff".to_string())]);
    assert!(!declared(&metadata(&archive), "mzml:dangling-reference-dropped"));

    let (mzml, _) = convert(Path::new(TINY), &dir, "out.mzML");
    let text = std::fs::read_to_string(&mzml).unwrap();
    assert_eq!(text.matches("name=\"sourceFileRef\"").count(), 1);
    let at = text.find("name=\"sourceFileRef\" value=\"tiny.wiff\"").expect("the userParam");
    assert!(text[..at].ends_with("<userParam type=\"xsd:string\" "), "{}", &text[at - 40..at]);
    let spectrum = text[..at].rfind("<spectrum ").unwrap();
    assert!(text[spectrum..at].contains("id=\"sample=1 period=1 cycle=22 experiment=1\""), "on the spectrum that states it");
    // The export of the archive states it too, and lists the file it names: the archive's own
    // source files are in its `sourceFileList`, so the id resolves within the exported mzML.
    let (back, _) = convert(&archive, &dir, "back.mzML");
    let text = std::fs::read_to_string(&back).unwrap();
    assert_eq!(text.matches("name=\"sourceFileRef\" value=\"tiny.wiff\"").count(), 1);
    assert_eq!(text.matches("<sourceFile id=\"tiny.wiff\"").count(), 1, "the file the parameter names is listed");

    let ghost = variant(TINY, &dir, "ghost.mzML", &[("sourceFileRef=\"tiny.wiff\"", "sourceFileRef=\"ghost\"")]);
    let (archive, log) = convert(&ghost, &dir, "ghost.mzpeak");
    assert!(spectrum_parameter(&archive, "sourceFileRef").iter().all(|(_, v)| v.is_none()));
    assert!(declared(&metadata(&archive), "mzml:dangling-reference-dropped"));
    assert!(log.contains("1 sourceFileRef (ghost)"), "{log}");
    // The direct mzML export runs the same check: no parameter, and its one warning counts it.
    let (mzml, log) = convert(&ghost, &dir, "ghost.out.mzML");
    assert!(!std::fs::read_to_string(&mzml).unwrap().contains("name=\"sourceFileRef\""));
    assert!(log.contains("1 sourceFileRef (ghost)"), "{log}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A source's processing method keeps the terms it states: `file format conversion` (MS:1000530)
/// is added only where no term of the method is a data transformation, which the spec's
/// `processingmethod_must` rule requires one of.
#[test]
fn a_source_processing_method_gains_no_term_it_does_not_state() {
    let dir = scratch("methods");
    let accessions = |m: &serde_json::Value, id: &str| -> Vec<String> {
        let dp = m["data_processing_method_list"].as_array().unwrap().iter().find(|dp| dp["id"] == id).unwrap_or_else(|| panic!("{id} missing"));
        dp["methods"][0]["parameters"].as_array().unwrap().iter().filter_map(|p| p["accession"].as_str()).map(str::to_string).collect()
    };
    let m = metadata(&convert(Path::new(TINY), &dir, "out.mzpeak").0);
    assert_eq!(accessions(&m, "CompassXtract_x0020_processing"), ["MS:1000033", "MS:1000034", "MS:1000035"]);
    assert_eq!(accessions(&m, "pwiz_processing"), ["MS:1000544"]);

    // No CV term at all, and a CV term that is no data transformation: the rule's term is added.
    const STATED: &str = "<cvParam cvRef=\"MS\" accession=\"MS:1000544\" name=\"Conversion to mzML\" value=\"\"/>";
    for (name, replacement, expected) in [
        ("bare", "<userParam name=\"note\" value=\"x\"/>", vec!["MS:1000530"]),
        ("other", "<cvParam cvRef=\"MS\" accession=\"MS:1000747\" name=\"completion time\" value=\"2007-06-27\"/>", vec!["MS:1000747", "MS:1000530"]),
    ] {
        let input = variant(TINY, &dir, &format!("{name}.mzML"), &[(STATED, replacement)]);
        let m = metadata(&convert(&input, &dir, &format!("{name}.mzpeak")).0);
        assert_eq!(accessions(&m, "pwiz_processing"), expected, "{name}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// ProteoWizard writes a device trace as an `intensity array` in the trace's unit. The facet's one
/// `intensity` column is in detector counts and mzdata's mzML writer states counts for every
/// intensity array, so the unit was lost on the way in and out. Such an array is stored as the
/// chromatogram's auxiliary array, as the native Bruker lanes store the same traces, and both
/// exports state its unit.
#[test]
fn a_device_trace_keeps_its_unit_in_the_archive_and_in_both_exports() {
    let dir = scratch("traces");
    let trace = |name: &str, kind: &str, unit: &str| variant(TINY, &dir, name, &[(SIC_TYPE, kind), (SIC_COUNTS, &SIC_COUNTS.replace(COUNTS_UNIT, unit))]);
    let pressure = trace(
        "pressure.mzML",
        "<cvParam cvRef=\"MS\" accession=\"MS:1003019\" name=\"pressure chromatogram\" value=\"\"/>",
        "unitCvRef=\"UO\" unitAccession=\"UO:0000110\" unitName=\"pascal\"",
    );
    let (archive, _) = convert(&pressure, &dir, "pressure.mzpeak");
    let aux: Vec<i64> = batches(&archive, "chromatograms_metadata.parquet")
        .iter()
        .flat_map(|b| {
            let c = arrow::compute::cast(b.column_by_name("number_of_auxiliary_arrays").unwrap(), &arrow::datatypes::DataType::Int64).unwrap();
            c.as_primitive::<arrow::datatypes::Int64Type>().iter().map(|v| v.unwrap_or(0)).collect::<Vec<_>>()
        })
        .collect();
    // The base-peak chromatogram the source lacks is synthesized and leads the facet.
    let ids = |archive: &Path| -> Vec<String> { strings(archive, "chromatograms_metadata.parquet", "id").into_iter().flatten().collect() };
    assert_eq!(ids(&archive), ["BPC", "tic", "sic"]);
    assert_eq!(aux, [0, 0, 1], "the pressure trace's values are its auxiliary array");

    let stated = |mzml: &Path| -> (usize, usize) {
        let text = std::fs::read_to_string(mzml).unwrap();
        let chromatograms = &text[text.find("<chromatogramList").unwrap()..];
        (
            chromatograms.matches("accession=\"MS:1000515\"").count(),
            chromatograms.matches("accession=\"MS:1000821\" cvRef=\"MS\" name=\"pressure array\" unitCvRef=\"UO\" unitAccession=\"UO:0000110\"").count(),
        )
    };
    assert_eq!(stated(&convert(&archive, &dir, "pressure.archive.mzML").0), (2, 1), "archive export: the TIC's and the BPC's intensity, the trace's pressure in pascal");
    assert_eq!(stated(&convert(&pressure, &dir, "pressure.direct.mzML").0), (2, 1), "direct export");

    // A unit with no array type of its own (a UV trace in absorbance units): a non-standard array
    // named after the chromatogram, in that unit.
    let uv = trace(
        "uv.mzML",
        "<cvParam cvRef=\"MS\" accession=\"MS:1000812\" name=\"absorption chromatogram\" value=\"\"/>",
        "unitCvRef=\"UO\" unitAccession=\"UO:0000269\" unitName=\"absorbance unit\"",
    );
    let (archive, _) = convert(&uv, &dir, "uv.mzpeak");
    for (route, mzml) in [("archive", convert(&archive, &dir, "uv.archive.mzML").0), ("direct", convert(&uv, &dir, "uv.direct.mzML").0)] {
        let text = std::fs::read_to_string(&mzml).unwrap();
        let chromatograms = &text[text.find("<chromatogramList").unwrap()..];
        assert_eq!(chromatograms.matches("accession=\"MS:1000515\"").count(), 2, "{route}: the TIC and the BPC");
        let at = chromatograms.find("accession=\"MS:1000786\"").unwrap_or_else(|| panic!("{route}: no non-standard array"));
        let tag = &chromatograms[at..at + chromatograms[at..].find("/>").unwrap()];
        assert!(tag.contains("unitAccession=\"UO:0000269\"") && tag.contains("value=\"sic\""), "{route}: {tag}");
    }

    // Counts stay the intensity: the fixture as it is has no auxiliary array.
    let (archive, _) = convert(Path::new(TINY), &dir, "counts.mzpeak");
    assert_eq!(ids(&archive), ["BPC", "tic", "sic"]);
    assert!(batches(&archive, "chromatograms_metadata.parquet").iter().all(|b| {
        let c = arrow::compute::cast(b.column_by_name("number_of_auxiliary_arrays").unwrap(), &arrow::datatypes::DataType::Int64).unwrap();
        c.as_primitive::<arrow::datatypes::Int64Type>().iter().all(|v| v.unwrap_or(0) == 0)
    }));
    let _ = std::fs::remove_dir_all(&dir);
}

/// The fixture's two chromatogram intensity arrays, each with the start of its `<binary>`: the TIC's
/// and the SIC's.
const TIC_COUNTS: &str = "<cvParam cvRef=\"MS\" accession=\"MS:1000515\" name=\"intensity array\" value=\"\" unitCvRef=\"MS\" unitAccession=\"MS:1000131\" unitName=\"number of counts\"/>\n              <binary>AAAAAAAALkAAAAAAAAAsQ";
const SIC_COUNTS: &str = "<cvParam cvRef=\"MS\" accession=\"MS:1000515\" name=\"intensity array\" value=\"\" unitCvRef=\"MS\" unitAccession=\"MS:1000131\" unitName=\"number of counts\"/>\n              <binary>AAAAAAAAJEAAAAAAAAAiQ";
const COUNTS_UNIT: &str = "unitCvRef=\"MS\" unitAccession=\"MS:1000131\" unitName=\"number of counts\"";
const TIC_TYPE: &str = "<cvParam cvRef=\"MS\" accession=\"MS:1000235\" name=\"total ion current chromatogram\" value=\"\"/>\n          <binaryDataArrayList";
const SIC_TYPE: &str = "<cvParam cvRef=\"MS\" accession=\"MS:1000627\" name=\"selected ion current chromatogram\" value=\"\"/>";

/// Per chromatogram: id, how many of its points have a value in the facet's `intensity` column,
/// and its number of auxiliary arrays.
fn chromatogram_values(archive: &Path) -> Vec<(String, usize, i64)> {
    let int64 = |c: &ArrayRef| -> Vec<i64> {
        let c = arrow::compute::cast(c, &arrow::datatypes::DataType::Int64).unwrap();
        c.as_primitive::<arrow::datatypes::Int64Type>().iter().map(|v| v.unwrap_or(0)).collect()
    };
    let mut rows: Vec<(String, usize, i64)> = Vec::new();
    for b in batches(archive, "chromatograms_metadata.parquet") {
        let (ids, aux) = (b.column_by_name("id").unwrap(), int64(b.column_by_name("number_of_auxiliary_arrays").unwrap()));
        rows.extend((0..b.num_rows()).map(|i| (text(ids, i).unwrap_or_default(), 0, aux[i])));
    }
    for b in batches(archive, "chromatograms_data.parquet") {
        let point = b.column_by_name("point").unwrap().as_struct();
        let index = int64(point.column_by_name("chromatogram_index").unwrap());
        let Some(intensity) = point.column_by_name("intensity") else { continue };
        for (k, &i) in index.iter().enumerate() {
            rows[i as usize].1 += usize::from(!intensity.is_null(k));
        }
    }
    rows
}

/// What a chromatogram list of an mzML export states: how many `intensity array`s, how many
/// non-standard arrays, and the values of the `intensity array unit` userParams.
fn exported_chromatogram_units(mzml: &Path) -> (usize, usize, Vec<String>) {
    let text = std::fs::read_to_string(mzml).unwrap();
    let chromatograms = &text[text.find("<chromatogramList").unwrap()..];
    let units = chromatograms
        .split("name=\"intensity array unit\" value=\"")
        .skip(1)
        .map(|rest| rest[..rest.find('"').unwrap()].to_string())
        .collect();
    (chromatograms.matches("accession=\"MS:1000515\"").count(), chromatograms.matches("accession=\"MS:1000786\"").count(), units)
}

/// PSI-MS allows an `intensity array` in counts per second (MS:1000814) or percent of base peak.
/// Moved out of the `intensity` column like a device trace, such a TIC had no intensity left — in
/// the archive (15 points, none with a value) or in either export — and none was synthesized in
/// its place. An intensity stays the intensity, in every unit an ion current states; the unit is
/// the chromatogram's `intensity array unit` parameter, and the archive declares it.
#[test]
fn an_ion_current_in_another_intensity_unit_stays_the_intensity() {
    let dir = scratch("ion-current-units");
    const CPS: &str = "unitCvRef=\"MS\" unitAccession=\"MS:1000814\" unitName=\"counts per second\"";
    let cps = chromatogram_variant(&dir, "cps.mzML", &[(TIC_COUNTS, &TIC_COUNTS.replace(COUNTS_UNIT, CPS)), (SIC_COUNTS, &SIC_COUNTS.replace(COUNTS_UNIT, CPS))]);
    let (archive, _) = convert(&cps, &dir, "cps.mzpeak");
    assert_eq!(chromatogram_values(&archive), [("BPC".to_string(), 3, 0), ("tic".to_string(), 15, 0), ("sic".to_string(), 10, 0)]);
    let unit = |archive: &Path| -> Vec<Option<String>> { parameter(archive, "chromatograms_metadata.parquet", "intensity array unit").into_iter().map(|(_, v)| v).collect() };
    assert_eq!(unit(&archive), [None, Some("MS:1000814".to_string()), Some("MS:1000814".to_string())]);
    assert!(declared(&metadata(&archive), "mzml:chromatogram-intensity-unit-as-parameter"));
    let stated = vec!["MS:1000814".to_string(); 2];
    assert_eq!(exported_chromatogram_units(&convert(&archive, &dir, "cps.archive.mzML").0), (3, 0, stated.clone()), "archive export");
    let (direct, log) = convert(&cps, &dir, "cps.direct.mzML");
    assert_eq!(exported_chromatogram_units(&direct), (3, 0, stated), "direct export");
    assert!(log.contains("2 chromatogram(s) (\"tic\" first) state an intensity in another unit"), "{log}");

    // A pressure in pascal on a selected ion current chromatogram: an ion current keeps its
    // intensity whatever unit the array states.
    const PASCAL: &str = "unitCvRef=\"UO\" unitAccession=\"UO:0000110\" unitName=\"pascal\"";
    let odd = variant(TINY, &dir, "odd.mzML", &[(SIC_COUNTS, &SIC_COUNTS.replace(COUNTS_UNIT, PASCAL))]);
    let (archive, _) = convert(&odd, &dir, "odd.mzpeak");
    assert_eq!(chromatogram_values(&archive)[2], ("sic".to_string(), 10, 0));
    assert_eq!(unit(&archive), [None, None, Some("UO:0000110".to_string())]);

    // Counts are counts: no parameter, nothing declared.
    let (archive, _) = convert(Path::new(TINY), &dir, "counts.mzpeak");
    assert_eq!(unit(&archive), [None, None, None]);
    assert!(!declared(&metadata(&archive), "mzml:chromatogram-intensity-unit-as-parameter"));
    let _ = std::fs::remove_dir_all(&dir);
}

/// mzdata reads a unit it has no `Unit` for as no unit at all: an `intensity array` in
/// `UO:0000095` was stored in the counts column and exported as detector counts, with nothing said.
/// The accession is read back from the source: a trace is stored as a device trace is, with the
/// unit as the chromatogram's parameter (the array cannot state it); an ion current stays the
/// intensity, with the same parameter.
#[test]
fn a_chromatogram_unit_mzdata_does_not_know_is_read_back_from_the_source() {
    let dir = scratch("unknown-unit");
    const VOLUME: &str = "unitCvRef=\"UO\" unitAccession=\"UO:0000095\" unitName=\"volume unit\"";
    let unit = |archive: &Path| -> Vec<Option<String>> { parameter(archive, "chromatograms_metadata.parquet", "intensity array unit").into_iter().map(|(_, v)| v).collect() };
    let sic = variant(TINY, &dir, "sic.mzML", &[(SIC_COUNTS, &SIC_COUNTS.replace(COUNTS_UNIT, VOLUME))]);
    let (archive, log) = convert(&sic, &dir, "sic.mzpeak");
    assert_eq!(chromatogram_values(&archive)[2], ("sic".to_string(), 10, 0));
    assert_eq!(unit(&archive), [None, None, Some("UO:0000095".to_string())]);
    assert_eq!(log.matches("a unit mzdata does not know (UO:0000095)").count(), 1, "{log}");
    assert!(declared(&metadata(&archive), "mzml:chromatogram-intensity-unit-as-parameter"));

    const UV: &str = "<cvParam cvRef=\"MS\" accession=\"MS:1000811\" name=\"electromagnetic radiation chromatogram\" value=\"\"/>";
    let trace = variant(TINY, &dir, "trace.mzML", &[(SIC_TYPE, UV), (SIC_COUNTS, &SIC_COUNTS.replace(COUNTS_UNIT, VOLUME))]);
    let (archive, _) = convert(&trace, &dir, "trace.mzpeak");
    assert_eq!(chromatogram_values(&archive)[2], ("sic".to_string(), 0, 1), "its values are its auxiliary array");
    assert_eq!(unit(&archive), [None, None, Some("UO:0000095".to_string())]);
    assert!(!declared(&metadata(&archive), "mzml:chromatogram-intensity-unit-as-parameter"), "no stored intensity is in another unit");
    for (route, mzml) in [("archive", convert(&archive, &dir, "trace.archive.mzML").0), ("direct", convert(&trace, &dir, "trace.direct.mzML").0)] {
        assert_eq!(exported_chromatogram_units(&mzml), (2, 1, vec!["UO:0000095".to_string()]), "{route}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// The chromatogram facet's schema is sampled from the source's first chromatograms. When they are
/// all device traces, none keeps an `intensity array`, and the facet had no `intensity` column:
/// the TIC and base-peak chromatogram synthesized after them stored their intensity as a per-row
/// auxiliary array with no unit.
#[test]
fn a_source_of_device_traces_alone_leaves_the_intensity_column_in_place() {
    let dir = scratch("traces-alone");
    let traces = chromatogram_variant(
        &dir,
        "traces.mzML",
        &[
            (TIC_TYPE, &TIC_TYPE.replace("accession=\"MS:1000235\" name=\"total ion current chromatogram\"", "accession=\"MS:1003019\" name=\"pressure chromatogram\"")),
            (SIC_TYPE, "<cvParam cvRef=\"MS\" accession=\"MS:1003020\" name=\"flow rate chromatogram\" value=\"\"/>"),
            (TIC_COUNTS, &TIC_COUNTS.replace(COUNTS_UNIT, "unitCvRef=\"UO\" unitAccession=\"UO:0000110\" unitName=\"pascal\"")),
            (SIC_COUNTS, &SIC_COUNTS.replace(COUNTS_UNIT, "unitCvRef=\"UO\" unitAccession=\"UO:0000271\" unitName=\"microliters per minute\"")),
        ],
    );
    let (archive, _) = convert(&traces, &dir, "traces.mzpeak");
    assert_eq!(
        chromatogram_values(&archive),
        [("TIC".to_string(), 3, 0), ("BPC".to_string(), 3, 0), ("tic".to_string(), 0, 1), ("sic".to_string(), 0, 1)],
        "the synthesized pair in the intensity column, each trace in its auxiliary array"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The synthetic imzML states no scan start time: mzdata stores 0 for each, and a TIC and a
/// base-peak chromatogram summed over its spectra had every point at time 0. None is synthesized;
/// the facet holds the one placeholder row (empty id, no type, no points) the readers need. With a
/// time on one spectrum of nine the chromatograms are back and the marker says how many state one.
#[test]
fn a_run_that_states_no_scan_time_gets_no_synthesized_chromatogram() {
    let dir = scratch("timeless");
    let (archive, _) = convert(Path::new(IMZML), &dir, "out.mzpeak");
    assert_eq!(strings(&archive, "chromatograms_metadata.parquet", "id"), [Some(String::new())], "the placeholder alone");
    assert_eq!(batches(&archive, "chromatograms_data.parquet").iter().map(RecordBatch::num_rows).sum::<usize>(), 0);
    assert_eq!(metadata(&archive)["imaging"]["provenance"]["time"], "not stated by the source; index is the source list order");

    const POSITION: &str = "<cvParam cvRef=\"IMS\" accession=\"IMS:1000050\" name=\"position x\" value=\"1\"/>\n            <cvParam cvRef=\"IMS\" accession=\"IMS:1000051\" name=\"position y\" value=\"1\"/>";
    let timed = format!("{POSITION}\n            <cvParam cvRef=\"MS\" accession=\"MS:1000016\" name=\"scan start time\" value=\"12\" unitCvRef=\"UO\" unitAccession=\"UO:0000010\" unitName=\"second\"/>");
    let one = imzml_variant(&dir, "one", &[(POSITION, &timed)]);
    let (archive, _) = convert(&one, &dir, "one.mzpeak");
    assert_eq!(metadata(&archive)["imaging"]["provenance"]["time"], "stated on 1 of 9 spectra; the others are stored as 0");
    assert_eq!(strings(&archive, "chromatograms_metadata.parquet", "id"), [Some("TIC".to_string()), Some("BPC".to_string())]);
    let _ = std::fs::remove_dir_all(&dir);
}

/// The `.ibd` begins with its UUID, and the imzML states the same one: the pairing check the imzML
/// specification gives. A mismatch was mzdata's log line only; it is recorded and declared, and the
/// conversion goes on (as for a checksum mismatch).
#[test]
fn an_ibd_that_does_not_begin_with_the_stated_uuid_is_recorded_and_declared() {
    let dir = scratch("uuid");
    let (archive, _) = convert(Path::new(IMZML), &dir, "out.mzpeak");
    let m = metadata(&archive);
    assert_eq!(m["imaging"]["provenance"]["ibd_uuid"], "verified", "{}", m["imaging"]);
    assert!(!declared(&m, "imzml:ibd-uuid-mismatch"));

    let other = imzml_variant(&dir, "other", &[("{1a2b3c4d-5e6f-7081-9203-b4c5d6e7f8a9}", "{00000000-5e6f-7081-9203-b4c5d6e7f8a9}")]);
    let (archive, log) = convert(&other, &dir, "other.mzpeak");
    let m = metadata(&archive);
    let provenance = &m["imaging"]["provenance"];
    assert_eq!(provenance["ibd_uuid"], "mismatch", "{provenance}");
    assert_eq!(provenance["ibd_uuid_found"], "1a2b3c4d5e6f70819203b4c5d6e7f8a9");
    assert!(declared(&m, "imzml:ibd-uuid-mismatch"), "{}", m["transformations"]);
    assert!(log.contains("declared as imzml:ibd-uuid-mismatch"), "{log}");
    // The stated identifier stays as the header states it.
    let stated = m["file_description"]["contents"].as_array().unwrap().iter().find(|p| p["accession"] == "IMS:1000080").unwrap();
    assert_eq!(stated["value"], "{00000000-5e6f-7081-9203-b4c5d6e7f8a9}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// `metadata.imaging.pixel_size_um` is in micrometres whatever length unit the header states the
/// pixel size in; a size that is not positive is no pixel size, and is dropped and declared.
#[test]
fn the_marker_s_pixel_size_is_in_micrometres_and_positive() {
    let dir = scratch("pixelsize");
    const X: &str = "name=\"pixel size x\" value=\"100.0\" unitCvRef=\"UO\" unitAccession=\"UO:0000017\" unitName=\"micrometer\"";
    const Y: &str = "name=\"pixel size y\" value=\"100.0\" unitCvRef=\"UO\" unitAccession=\"UO:0000017\" unitName=\"micrometer\"";
    let stated = |name: &str, x: &str, y: &str, unit: &str| {
        let unit = format!("unitCvRef=\"UO\" {unit}");
        imzml_variant(&dir, name, &[(X, &format!("name=\"pixel size x\" value=\"{x}\" {unit}")), (Y, &format!("name=\"pixel size y\" value=\"{y}\" {unit}"))])
    };
    for (name, value, unit) in [
        ("mm", "0.1", "unitAccession=\"UO:0000016\" unitName=\"millimeter\""),
        ("nm", "100000", "unitAccession=\"UO:0000018\" unitName=\"nanometer\""),
    ] {
        let m = metadata(&convert(&stated(name, value, value, unit), &dir, &format!("{name}.mzpeak")).0);
        assert_eq!(m["imaging"]["pixel_size_um"], serde_json::json!({"x": 100.0, "y": 100.0}), "{name}: {}", m["imaging"]);
        // The scan settings keep the value and the unit as stated.
        let grid = m["scan_settings_list"][0]["parameters"].as_array().unwrap();
        let x = grid.iter().find(|p| p["accession"] == "IMS:1000046").unwrap();
        assert_eq!((x["value"].as_f64(), x["unit"].as_str()), (value.parse().ok(), unit.split('"').nth(1)), "{name}");
    }

    let m = metadata(&convert(&stated("zero", "0", "-100", "unitAccession=\"UO:0000017\" unitName=\"micrometer\""), &dir, "zero.mzpeak").0);
    assert!(m["imaging"].get("pixel_size_um").is_none(), "{}", m["imaging"]);
    assert!(declared(&m, "imzml:pixel-size-dropped"), "{}", m["transformations"]);
    assert_eq!(m["imaging_pixel_size"][0]["case"], "x and y not both positive");
    let grid = m["scan_settings_list"][0]["parameters"].as_array().unwrap();
    assert!(!grid.iter().any(|p| p["accession"] == "IMS:1000046" || p["accession"] == "IMS:1000047"), "{grid:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A path that names nothing is an error about the path, on every lane: a missing `.raw` went to
/// the Thermo reader, whose .NET host failed to start and reported a missing framework.
#[test]
fn a_missing_input_is_reported_as_missing() {
    let dir = scratch("missing");
    for name in ["nothing.raw", "nothing.mzML", "nothing.d"] {
        let input = dir.join(name);
        for output in ["out.mzpeak", "out.mzML"] {
            let out = dir.join(output);
            let (code, log) = run(&[input.as_os_str(), "-o".as_ref(), out.as_os_str()], &[]);
            assert_eq!(code, Some(1), "{name} -> {output}: {log}");
            let error = log.lines().find(|l| l.starts_with("error:")).unwrap_or_else(|| panic!("{name}: no error line in {log}"));
            assert!(error.starts_with(&format!("error: input {}: ", input.display())), "{error}");
            assert!(!log.contains("framework") && !out.exists(), "{log}");
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// The precursor references of an archive's `spectra_metadata_precursors`, nulls as `None`.
fn precursor_ids(archive: &Path) -> Vec<Option<String>> {
    strings(archive, "spectra_metadata_precursors.parquet", "precursor_id")
}

/// A data-dependent Thermo run keeps every precursor reference: an MS2 spectrum's parent is an MS1
/// spectrum, and nothing is declared.
#[test]
fn a_thermo_run_with_ms1_keeps_its_precursor_references() {
    let dir = scratch("thermo-dda");
    let (archive, _) = convert(Path::new(SMALL_RAW), &dir, "small.mzpeak");
    let ids = precursor_ids(&archive);
    assert!(!ids.is_empty() && ids.iter().all(Option::is_some), "{ids:?}");
    assert!(!declared(&metadata(&archive), "thermo:invalid-precursor-reference-dropped"));
    let _ = std::fs::remove_dir_all(&dir);
}

/// PXD057269's SRM run has no MS1 spectrum: the reader library reports parent index 0 for every
/// scan and mzdata names scan 1 — an MS2 spectrum, and for scan 1 itself — as each one's precursor
/// spectrum. The reference is cleared and declared, in the archive and in the mzML export.
#[test]
fn a_thermo_run_without_ms1_names_no_precursor_spectrum() {
    let Some(raw) = corpus::corpus_path("general-ms/PXD057269/LD401_001fmol_r1.raw") else { return };
    let dir = scratch("thermo-srm");
    let cap = [("MZPC_MAX_SPECTRA", "50")];
    let (archive, log) = convert_with(&raw, &dir, "srm.mzpeak", &cap);
    let ids = precursor_ids(&archive);
    assert_eq!(ids.len(), 50);
    assert!(ids.iter().all(Option::is_none), "{:?}", &ids[..3]);
    assert!(declared(&metadata(&archive), "thermo:invalid-precursor-reference-dropped"));
    assert!(log.contains("50 Thermo precursor reference(s)"), "{log}");

    let (mzml, log) = convert_with(&raw, &dir, "srm.mzML", &cap);
    let text = std::fs::read_to_string(&mzml).unwrap();
    assert_eq!(text.matches("<precursor>").count() + text.matches("<precursor ").count(), 50);
    assert_eq!(text.matches("<precursor spectrumRef=").count(), 0, "no precursor names a spectrum");
    assert!(log.contains("50 Thermo precursor reference(s)"), "{log}");
    // The archive's export states the same: each spectrum its precursor, none a spectrum it came
    // from, and no list without a member.
    let (back, _) = convert(&archive, &dir, "srm.back.mzML");
    let text = std::fs::read_to_string(&back).unwrap();
    assert_eq!(text.matches("<precursor>").count() + text.matches("<precursor ").count(), 50);
    assert_eq!(text.matches("<precursor spectrumRef=").count(), 0, "no precursor names a spectrum");
    assert_eq!(text.matches("<precursorList count=\"1\">").count(), 50);
    assert_eq!(text.matches("<precursorList count=\"0\">").count(), 0);
    let _ = std::fs::remove_dir_all(&dir);
}

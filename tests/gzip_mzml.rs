//! `.mzML.gz` in both directions.
//!
//! Three support tables advertised gzipped mzML as an input from the first release, and it never
//! opened: mzdata's `open_path` refuses a gzip stream outright ("Gzipped files are not supported
//! with this method"), and nothing in the converter looked for the magic. The 2026-09-04 review
//! found it (Fable, lens Q4); this pins the fix.
//!
//! Input is handled by decompressing to a temp copy BEFORE the Latin-1 transcode and param-group
//! sanitize stages (both sniff XML bytes, which do not exist until the stream is inflated), so every
//! downstream lane sees exactly what it would see for the plain file — asserted here facet by facet.
//! Output is a streaming `GzEncoder` sink chosen by the `.gz` suffix, and the format inference looks
//! through that suffix so `-o x.mzML.gz` is an mzML request rather than an mzPeak archive with an
//! odd name. Decompression is decided by the `1f 8b` magic, not the extension.

use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use parquet::file::reader::{FileReader, SerializedFileReader};

const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/tiny.pwiz.1.1.mzML");

fn tmp(name: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("mzpc-gz-{}-{name}", std::process::id()));
    let _ = std::fs::remove_file(&p);
    p
}

fn run(args: &[&Path]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_mzpeak-convert"))
        .args(args)
        .arg("--force")
        .output()
        .expect("failed to run mzpeak-convert")
}

/// A gzipped copy of the fixture under a per-TEST name: cargo runs these tests in parallel inside
/// one process, so a shared temp name lets one test's cleanup delete another test's input mid-run.
fn gzip_fixture(tag: &str) -> PathBuf {
    let gz = tmp(&format!("{tag}-tiny.mzML.gz"));
    let mut enc = flate2::write::GzEncoder::new(File::create(&gz).unwrap(), flate2::Compression::default());
    enc.write_all(&std::fs::read(FIXTURE).unwrap()).unwrap();
    enc.finish().unwrap();
    gz
}

/// Every Parquet member of the archive as `(name, raw bytes)`, sorted by name.
fn facets(archive: &Path) -> Vec<(String, Vec<u8>)> {
    let mut zip = zip::ZipArchive::new(File::open(archive).unwrap()).unwrap();
    let mut out = Vec::new();
    for i in 0..zip.len() {
        let mut f = zip.by_index(i).unwrap();
        if !f.name().ends_with(".parquet") {
            continue;
        }
        let mut bytes = Vec::new();
        f.read_to_end(&mut bytes).unwrap();
        out.push((f.name().to_string(), bytes));
    }
    out.sort();
    out
}

fn num_rows(bytes: &[u8]) -> i64 {
    let p = std::env::temp_dir().join(format!("mzpc-gz-{}-rows.parquet", std::process::id()));
    std::fs::write(&p, bytes).unwrap();
    let n = SerializedFileReader::new(File::open(&p).unwrap()).unwrap().metadata().file_metadata().num_rows();
    let _ = std::fs::remove_file(&p);
    n
}

/// Every row of a Parquet member, decoded through the arrow reader into one batch per row group
/// and rendered column by column with `{:?}` — a representation that is equal exactly when the
/// stored VALUES are equal, whatever the physical layout did.
fn decode(bytes: &[u8]) -> Vec<String> {
    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
    // Through a temp file rather than an in-memory `Bytes`, so this test needs no crate beyond the
    // ones the rest of the suite already uses.
    let p = std::env::temp_dir().join(format!("mzpc-gz-{}-decode-{}.parquet", std::process::id(), bytes.len()));
    std::fs::write(&p, bytes).unwrap();
    let reader = ParquetRecordBatchReaderBuilder::try_new(File::open(&p).unwrap())
        .unwrap()
        .build()
        .unwrap();
    let _ = std::fs::remove_file(&p);
    let mut out = Vec::new();
    for batch in reader {
        let batch = batch.unwrap();
        for (i, col) in batch.columns().iter().enumerate() {
            out.push(format!("{}={:?}", batch.schema().field(i).name(), col));
        }
    }
    out
}

#[test]
fn gzipped_mzml_converts_to_the_same_archive_as_the_plain_file() {
    let gz = gzip_fixture("convert");
    let plain_out = tmp("plain.mzpeak");
    let gz_out = tmp("fromgz.mzpeak");

    let a = run(&[Path::new(FIXTURE), Path::new("-o"), &plain_out]);
    assert!(a.status.success(), "plain conversion failed: {}", String::from_utf8_lossy(&a.stderr));
    let b = run(&[&gz, Path::new("-o"), &gz_out]);
    assert!(b.status.success(), "gz conversion failed: {}", String::from_utf8_lossy(&b.stderr));
    assert!(
        String::from_utf8_lossy(&b.stderr).contains("gzip-compressed"),
        "the gunzip stage must announce itself so a user can tell which copy was read"
    );

    let (fa, fb) = (facets(&plain_out), facets(&gz_out));
    assert_eq!(
        fa.iter().map(|(n, _)| n).collect::<Vec<_>>(),
        fb.iter().map(|(n, _)| n).collect::<Vec<_>>(),
        "same set of facets"
    );
    for ((name, pa), (_, pb)) in fa.iter().zip(&fb) {
        assert_eq!(num_rows(pa), num_rows(pb), "row count differs in {name}");
        // Compare VALUES, not bytes: two Parquet files holding identical columns are not byte-equal
        // (footer metadata, page boundaries and dictionary state all vary run to run). Decode both
        // through the arrow reader and compare the concatenated batches.
        assert_eq!(
            decode(pa),
            decode(pb),
            "facet {name}: decoded columns differ between plain and gunzipped input"
        );
    }

    let _ = std::fs::remove_file(&gz);
    let _ = std::fs::remove_file(&plain_out);
    let _ = std::fs::remove_file(&gz_out);
}

#[test]
fn mzml_export_to_a_gz_name_is_gzip_and_reparses() {
    let out = tmp("export.mzML.gz");
    let r = run(&[Path::new(FIXTURE), Path::new("-o"), &out]);
    assert!(r.status.success(), "export failed: {}", String::from_utf8_lossy(&r.stderr));

    let mut magic = [0u8; 2];
    File::open(&out).unwrap().read_exact(&mut magic).unwrap();
    assert_eq!(magic, [0x1f, 0x8b], "output named .gz must be a gzip stream, not an mzPeak archive");

    // Inflate and hand the XML back to the converter's own inspection: it must parse as mzML with
    // the fixture's four spectra, which proves the document was CLOSED (index + checksum written)
    // before the gzip trailer — both happen on drop, and this is the check that they happened.
    let plain = tmp("export.mzML");
    let mut dec = flate2::read::GzDecoder::new(File::open(&out).unwrap());
    let mut xml = Vec::new();
    dec.read_to_end(&mut xml).expect("gzip stream must be complete (trailer present)");
    assert!(
        String::from_utf8_lossy(&xml).trim_end().ends_with("</indexedmzML>"),
        "mzML document not closed before the gzip trailer"
    );
    std::fs::write(&plain, &xml).unwrap();
    let inspect = Command::new(env!("CARGO_BIN_EXE_mzpeak-convert")).arg(&plain).output().unwrap();
    let text = String::from_utf8_lossy(&inspect.stdout);
    assert!(inspect.status.success(), "inspect failed: {}", String::from_utf8_lossy(&inspect.stderr));
    assert!(text.contains("format:        mzML"), "{text}");
    assert!(text.contains("spectra:       4"), "{text}");

    let _ = std::fs::remove_file(&out);
    let _ = std::fs::remove_file(&plain);
}

#[test]
fn inspecting_a_gzipped_mzml_works_without_an_output() {
    let gz = gzip_fixture("inspect");
    let r = Command::new(env!("CARGO_BIN_EXE_mzpeak-convert")).arg(&gz).output().unwrap();
    let text = String::from_utf8_lossy(&r.stdout);
    assert!(r.status.success(), "{}", String::from_utf8_lossy(&r.stderr));
    assert!(text.contains("format:        mzML") && text.contains("spectra:       4"), "{text}");
    let _ = std::fs::remove_file(&gz);
}

#[test]
fn gz_suffix_is_decided_by_magic_not_name() {
    // A file called .gz that is NOT gzip must reach the reader untouched and fail on ITS terms —
    // the converter must not "decompress" plain XML and must not mask the real error.
    let fake = tmp("notreally.mzML.gz");
    std::fs::copy(FIXTURE, &fake).unwrap();
    let out = tmp("fake.mzpeak");
    let r = run(&[&fake, Path::new("-o"), &out]);
    let err = String::from_utf8_lossy(&r.stderr);
    assert!(!err.contains("gzip-compressed"), "must not claim to decompress a non-gzip file: {err}");
    // Plain XML under a .gz name is still valid mzML: the reader infers from content, so it converts.
    assert!(r.status.success(), "a plain mzML under a .gz name should still convert: {err}");
    let _ = std::fs::remove_file(&fake);
    let _ = std::fs::remove_file(&out);
}

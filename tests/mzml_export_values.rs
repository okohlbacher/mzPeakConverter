//! What an mzML export states per spectrum and chromatogram, value by value, on both routes
//! (`source -o x.mzML` and `archive.mzpeak -o x.mzML`). Through 0.17.0-rc.1:
//!
//!   * a spectrum whose polarity nothing states was exported as `positive scan`, with a warning per
//!     spectrum (all 1,196 spectra of a negative-mode imaging run);
//!   * every scan stated an `ion injection time`, every selected ion a `peak intensity` and every
//!     activation a `collision energy`, 0 where the source or the archive holds none (201 and 186
//!     such zeros on ProteoWizard's `swath.api-sample-centroid.mzML`, which states neither term),
//!     and a scan that states no time a `scan start time` of 0 (every pixel of an imaging run);
//!   * the direct mzML → mzML lane wrote mzdata's peak list for a centroid spectrum: m/z and a 32-bit
//!     intensity, and none of the spectrum's other arrays (the per-peak 1/K0 of a combineIMS file);
//!   * an array of length 0 was written as the zlib stream of nothing, which OpenMS 3.5 cannot
//!     inflate to an integer array (five corpus archives' exports did not load), an empty spectrum of
//!     an archive as `<binaryDataArrayList count="0">`, and an empty Thermo scan with an observed m/z
//!     range of inf to -inf;
//!   * a spectrum without a precursor got `<precursorList count="0">`, and a chromatogram's precursor
//!     and product the lists the schema has for spectra only;
//!   * the base-peak chromatogram summed for a source that has none was `BIC` over every spectrum on
//!     the direct route and `BPC` over the MS1 spectra in the archive, and a run without an MS1
//!     spectrum got a pair summed over whatever it held;
//!   * every `<offset>` of the index pointed at the line break before its element, and the
//!     `<fileChecksum>` was not the SHA-1 of the file.
//!
//! The fixtures stand in for the corpus units the corpus-gated tests run on.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use mzdata::prelude::*;
use mzdata::spectrum::{BinaryDataArrayType, Chromatogram, ScanPolarity};

#[path = "common/corpus.rs"]
mod corpus;

const TINY: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/tiny.pwiz.1.1.mzML");
const SWATH_GZ: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/swath.api-sample-centroid.mzML.gz");
const SMALL_RAW: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/small.RAW");
const PASEF: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/pasef_combineims_centroid.pwiz.mzML");
/// Nine pixels that state `ms level` 1 and no `scan start time`.
const IMAGING: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/imaging/Synthetic_DeclaredGrid.imzML");
/// Two MS2 spectra and no chromatogram.
const MS2_ONLY: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/dangling_refs.mzML");
/// The zlib stream of zero bytes, base64-encoded: what mzdata's writer prints for an empty array.
const ZLIB_OF_NOTHING: &str = "eNoDAAAAAAE=";

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("mzpc-export-values-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Run the converter and return its log (stderr).
fn convert(input: &Path, output: &Path, extra: &[&str], envs: &[(&str, &str)]) -> String {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_mzpeak-convert"));
    cmd.arg(input).arg("-o").arg(output).arg("--force").args(extra);
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("failed to run mzpeak-convert");
    let log = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(out.status.success(), "mzpeak-convert {} -o {} {extra:?} failed: {}\n{log}", input.display(), output.display(), out.status);
    log
}

/// Both exports of `source`: the direct one, and the one of the archive converted from it. Returns
/// `[(route, mzML text, the export's log)]`.
fn both_routes(source: &Path, dir: &Path, envs: &[(&str, &str)]) -> Vec<(&'static str, String, String)> {
    let direct = dir.join("direct.mzML");
    let direct_log = convert(source, &direct, &[], envs);
    let archive = dir.join("archive.mzpeak");
    convert(source, &archive, &[], envs);
    let export = dir.join("export.mzML");
    let export_log = convert(&archive, &export, &[], envs);
    vec![
        ("direct", std::fs::read_to_string(&direct).unwrap(), direct_log),
        ("archive", std::fs::read_to_string(&export).unwrap(), export_log),
    ]
}

fn count(text: &str, needle: &str) -> usize {
    text.matches(needle).count()
}

/// How many cvParams of `accession` state the value 0.
fn zeros(text: &str, accession: &str) -> usize {
    let key = format!("accession=\"{accession}\"");
    text.match_indices(&key)
        .filter(|(at, _)| {
            let element = &text[*at..*at + text[*at..].find("/>").unwrap()];
            element.split_once(" value=\"").is_some_and(|(_, v)| v[..v.find('"').unwrap()].parse::<f64>() == Ok(0.0))
        })
        .count()
}

fn gunzip(gz: &Path) -> String {
    let mut text = String::new();
    std::io::Read::read_to_string(&mut flate2::read::GzDecoder::new(std::fs::File::open(gz).unwrap()), &mut text).unwrap();
    text
}

/// Each `<tag ...>` element of an mzML's text, up to its closing tag.
fn elements<'a>(xml: &'a str, tag: &str) -> Vec<&'a str> {
    let (open, close) = (format!("<{tag} "), format!("</{tag}>"));
    xml.match_indices(&open).map(|(at, _)| &xml[at..at + xml[at..].find(&close).unwrap()]).collect()
}

fn chromatograms(mzml: &str) -> Vec<Chromatogram> {
    let mut reader = mzdata::io::mzml::MzMLReader::new_indexed(std::io::Cursor::new(mzml.as_bytes().to_vec()));
    (0..reader.count_chromatograms()).map(|i| reader.get_chromatogram_by_index(i).unwrap()).collect()
}

/// What no part of an export may hold, whatever its source: the marks of [`mzml_unstated`], the
/// lists the schema does not have, an empty array written as a compressed nothing; and what its
/// last lines must: an index of the elements' own positions and the file's checksum.
fn assert_well_formed(route: &str, mzml: &str, log: &str) {
    for gone in [
        "value=\"NaN\"",
        "polarity not stated",
        "<precursorList count=\"0\">",
        "<selectedIonList count=\"0\">",
        "<binaryDataArrayList count=\"0\">",
        ZLIB_OF_NOTHING,
        "value=\"inf\"",
        "value=\"-inf\"",
    ] {
        assert_eq!(count(mzml, gone), 0, "{route}: {gone}");
    }
    for chromatogram in elements(mzml, "chromatogram") {
        assert!(!chromatogram.contains("<precursorList") && !chromatogram.contains("<productList"), "{route}: {}", &chromatogram[..200]);
    }
    assert_eq!(count(log, "Could not determine scan polarity"), 0, "{route}: a warning per spectrum\n{log}");
    // The index points at its elements, and the checksum is the file's.
    let mut indexed = 0;
    for (at, open) in mzml.match_indices("<offset idRef=\"") {
        let rest = &mzml[at + open.len()..];
        let (id, rest) = rest.split_once("\">").unwrap();
        let offset: usize = rest[..rest.find('<').unwrap()].parse().unwrap();
        let element = &mzml[offset..];
        assert!(element.starts_with("<spectrum ") || element.starts_with("<chromatogram "), "{route}: {id} at {offset}: {:?}", &element[..20]);
        assert!(element[..element.find('>').unwrap()].contains(&format!("id=\"{id}\"")), "{route}: {id} at {offset}");
        indexed += 1;
    }
    assert_eq!(indexed, count(mzml, "<spectrum ") + count(mzml, "<chromatogram "), "{route}: an offset per element");
    let list: usize = mzml.split_once("<indexListOffset>").unwrap().1.split_once('<').unwrap().0.parse().unwrap();
    assert!(mzml[list..].starts_with("<indexList "), "{route}: {:?}", &mzml[list..list + 20]);
    let checked = mzml.find("<fileChecksum>").unwrap() + "<fileChecksum>".len();
    let sha1: String = <sha1::Sha1 as sha1::Digest>::digest(&mzml.as_bytes()[..checked]).iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(&mzml[checked..checked + 40], sha1, "{route}: the SHA-1 of the file up to and including <fileChecksum>");
}

/// ProteoWizard's SCIEX file states no `ion injection time` and no `peak intensity`, and a
/// `collision energy` on each of its 186 MS2 spectra: so does each export. Through rc.1 both
/// exports stated 201 injection times and 186 peak intensities, every one 0.
#[test]
fn an_export_states_no_zero_its_source_does_not() {
    let dir = scratch("swath");
    let source = gunzip(Path::new(SWATH_GZ));
    assert_eq!((count(&source, "MS:1000927"), count(&source, "MS:1000042"), count(&source, "\"MS:1000045\"")), (0, 0, 186), "the fixture changed");
    let (positive, spectra) = (count(&source, "MS:1000130"), count(&source, "<spectrum "));
    assert_eq!((positive, spectra), (201, 201));
    for (route, mzml, log) in both_routes(Path::new(SWATH_GZ), &dir, &[]) {
        assert_eq!(count(&mzml, "<spectrum "), 201, "{route}");
        assert_eq!(count(&mzml, "MS:1000927"), 0, "{route}: ion injection time");
        assert_eq!(count(&mzml, "MS:1000042"), 0, "{route}: peak intensity");
        assert_eq!(count(&mzml, "\"MS:1000045\""), 186, "{route}: collision energy");
        assert_eq!(zeros(&mzml, "MS:1000045"), 0, "{route}");
        assert_eq!(count(&mzml, "MS:1000130"), 201, "{route}: the stated polarity stays");
        assert_eq!(count(&log, "state no polarity"), 0, "{route}: {log}");
        assert_well_formed(route, &mzml, &log);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// A Thermo `.raw`: mzdata's reader knows each scan's injection time (48 of them, none 0) and no
/// selected ion's intensity, so the export states the first and not the second — 34 `peak intensity`
/// of 0 through rc.1, on both routes — and its 14 MS1 spectra hold no empty precursor list.
#[test]
fn a_thermo_export_states_what_the_reader_knows() {
    let dir = scratch("thermo");
    for (route, mzml, log) in both_routes(Path::new(SMALL_RAW), &dir, &[]) {
        assert_eq!(count(&mzml, "<spectrum "), 48, "{route}");
        assert_eq!((count(&mzml, "MS:1000927"), zeros(&mzml, "MS:1000927")), (48, 0), "{route}: ion injection time");
        assert_eq!(count(&mzml, "MS:1000042"), 0, "{route}: peak intensity");
        assert_eq!((count(&mzml, "\"MS:1000045\""), zeros(&mzml, "MS:1000045")), (34, 0), "{route}: collision energy");
        assert_eq!(count(&mzml, "<precursorList count=\"1\">"), 34, "{route}");
        assert_eq!(count(&mzml, "MS:1000130"), 48, "{route}: the reader's polarity");
        assert_well_formed(route, &mzml, &log);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// A source that states no polarity is exported without one, and the run says so once. mzdata's
/// writer states `positive scan` for such a spectrum and warns once per spectrum.
#[test]
fn a_spectrum_without_a_polarity_is_exported_without_one() {
    let dir = scratch("polarity");
    let tiny = std::fs::read_to_string(TINY).unwrap();
    // The fixture states it in the two param groups its spectra refer to, and on its chromatograms.
    let stated: Vec<&str> = tiny.lines().filter(|l| l.contains("MS:1000130")).collect();
    assert_eq!(stated.len(), 2, "the fixture changed");
    for (route, mzml, log) in both_routes(Path::new(TINY), &dir, &[]) {
        assert_eq!(count(&mzml, "MS:1000130"), 4, "{route}: every spectrum's stated polarity");
        assert_eq!(count(&log, "state no polarity"), 0, "{route}: {log}");
    }
    let unstated = dir.join("no-polarity.mzML");
    std::fs::write(&unstated, tiny.lines().filter(|l| !l.contains("MS:1000130")).collect::<Vec<_>>().join("\n")).unwrap();
    for (route, mzml, log) in both_routes(&unstated, &dir, &[]) {
        assert_eq!(count(&mzml, "<spectrum "), 4, "{route}");
        assert_eq!(count(&mzml, "MS:1000130") + count(&mzml, "MS:1000129"), 0, "{route}: a polarity nobody stated");
        assert_eq!(count(&log, "4 of 4 spectra state no polarity"), 1, "{route}: one line for the run\n{log}");
        assert_well_formed(route, &mzml, &log);
        let mut reader = mzdata::io::mzml::MzMLReader::new_indexed(std::io::Cursor::new(mzml.into_bytes()));
        assert!(reader.iter().all(|s| s.description().polarity == ScanPolarity::Unknown), "{route}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// A 0 the source states in its own text is a statement: the direct export keeps it. An archive
/// stores a 0 of these terms as null, so its export states none (the manual's "Different by design").
#[test]
fn a_zero_the_source_states_itself_is_kept_by_the_direct_export() {
    let dir = scratch("stated-zero");
    let tiny = std::fs::read_to_string(TINY).unwrap();
    let energy = r#"name="collision energy" value="35""#;
    assert_eq!(count(&tiny, energy), 1, "the fixture changed");
    let source = dir.join("zero-energy.mzML");
    std::fs::write(&source, tiny.replace(energy, r#"name="collision energy" value="0""#)).unwrap();
    let routes = both_routes(&source, &dir, &[]);
    let energies = |mzml: &str| -> Vec<usize> { elements(mzml, "spectrum").iter().map(|s| zeros(s, "MS:1000045")).collect() };
    assert_eq!(energies(&routes[0].1), [0, 1, 0, 0], "direct: the stated 0, on the spectrum that states it");
    assert!(routes[0].2.contains("state a zero"), "{}", routes[0].2);
    assert_eq!(energies(&routes[1].1), [0, 0, 0, 0], "archive: holds no energy for that spectrum");
    for (route, mzml, log) in &routes {
        assert_eq!(count(mzml, "MS:1000927"), 0, "{route}: the fixture states no injection time");
        assert_eq!((count(mzml, "MS:1000042"), zeros(mzml, "MS:1000042")), (1, 0), "{route}: the one stated peak intensity");
        assert_well_formed(route, mzml, log);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// Every array of a source spectrum, by name: its type and its values as 64-bit floats.
fn arrays_of(mzml: &Path) -> Vec<BTreeMap<String, (BinaryDataArrayType, Vec<f64>)>> {
    let mut reader = mzdata::io::mzml::MzMLReader::open_path(mzml).unwrap();
    reader
        .iter()
        .map(|s| {
            let arrays = s.arrays.as_ref().expect("the reader keeps a spectrum's arrays");
            arrays.iter().map(|(name, a)| (format!("{name:?}"), (a.dtype, a.to_f64().unwrap().to_vec()))).collect()
        })
        .collect()
}

/// The direct mzML → mzML lane writes the arrays the source spectrum holds, in their own types:
/// the per-peak 1/K0 of a combineIMS centroid spectrum (MS:1003006) and its 64-bit intensities.
/// Through rc.1 it wrote mzdata's peak list: m/z, a 32-bit intensity, and no third array.
#[test]
fn the_direct_mzml_lane_writes_the_sources_arrays() {
    let dir = scratch("arrays");
    let out = dir.join("direct.mzML");
    convert(Path::new(PASEF), &out, &[], &[]);
    let (source, export) = (std::fs::read_to_string(PASEF).unwrap(), std::fs::read_to_string(&out).unwrap());
    assert_eq!((count(&source, "MS:1003006"), count(&export, "MS:1003006")), (1, 1), "mean inverse reduced ion mobility array");
    assert_eq!((count(&source, "MS:1000521"), count(&export, "MS:1000521")), (0, 0), "32-bit float arrays");
    let (want, got) = (arrays_of(Path::new(PASEF)), arrays_of(&out));
    assert_eq!(want[0].len(), 3);
    assert_eq!(want[0]["MeanInverseReducedIonMobilityArray"].1.len(), 1391);
    assert!(want == got, "the export's arrays are not the source's, bit for bit");
    let _ = std::fs::remove_dir_all(&dir);
}

/// An archive that stores 64-bit intensities in its peak facet — a `--lossless` one, which keeps a
/// centroid spectrum's own arrays — exports them as stored. The reader's peak list holds a 32-bit
/// float, so the export of an archive holding 15.1 wrote 15.100000381 (0.17.0-rc.1, and each wave-4
/// unit on its own). A default archive stores such a spectrum's float32 peak-set values and says so
/// when it is converted (`intensity-f32-rounding`); its export writes what it stores, as the
/// 32-bit floats of the peak column.
#[test]
fn a_lossless_archive_exports_its_64_bit_intensities() {
    // The fixture's first intensity array (15.0, 14.0, … as 64-bit floats), and the same plus 0.1.
    const STATED: &str = "AAAAAAAALkAAAAAAAAAsQAAAAAAAACpAAAAAAAAAKEAAAAAAAAAmQAAAAAAAACRAAAAAAAAAIkAAAAAAAAAgQAAAAAAAABxAAAAAAAAAGEAAAAAAAAAUQAAAAAAAABBAAAAAAAAACEAAAAAAAAAAQAAAAAAAAPA/";
    const TENTHS: &str = "MzMzMzMzLkAzMzMzMzMsQDMzMzMzMypAMzMzMzMzKEAzMzMzMzMmQDMzMzMzMyRAMzMzMzMzIkAzMzMzMzMgQGZmZmZmZhxAZmZmZmZmGEBmZmZmZmYUQGZmZmZmZhBAzczMzMzMCEDNzMzMzMwAQJqZmZmZmfE/";
    let dir = scratch("wide-intensity");
    let tiny = std::fs::read_to_string(TINY).unwrap();
    assert!(tiny.contains(STATED), "the fixture changed");
    let source = dir.join("tenths.mzML");
    std::fs::write(&source, tiny.replacen(STATED, TENTHS, 1)).unwrap();
    let want = arrays_of(&source);
    assert_eq!(want[0]["IntensityArray"].0, BinaryDataArrayType::Float64);
    assert_eq!(want[0]["IntensityArray"].1[0], 15.1);

    let (lossless, export) = (dir.join("lossless.mzpeak"), dir.join("lossless.mzML"));
    convert(&source, &lossless, &["--lossless"], &[]);
    let log = convert(&lossless, &export, &[], &[]);
    let got = arrays_of(&export);
    for at in [0, 3] {
        assert!(want[at]["IntensityArray"] == got[at]["IntensityArray"], "spectrum {at}: {:?}", got[at]["IntensityArray"]);
        assert!(want[at]["MZArray"] == got[at]["MZArray"], "spectrum {at}: m/z");
    }
    assert_well_formed("lossless archive", &std::fs::read_to_string(&export).unwrap(), &log);

    let (archive, export) = (dir.join("default.mzpeak"), dir.join("default.mzML"));
    let log = convert(&source, &archive, &[], &[]);
    assert!(log.contains("15 intensities are stored as the nearest float32") && log.contains("intensity-f32-rounding"), "{log}");
    convert(&archive, &export, &[], &[]);
    let got = arrays_of(&export);
    // The peak facet's column is typed from the centroid spectra alone (scan=19's float32 peak
    // set), not from the fixture's profile scan=20 with 64-bit intensities, which is stored in the
    // data facet: a float32, holding the rounded values, which the export writes as 32-bit floats.
    assert_eq!(got[0]["IntensityArray"], (BinaryDataArrayType::Float32, want[0]["IntensityArray"].1.iter().map(|v| *v as f32 as f64).collect()));
    let _ = std::fs::remove_dir_all(&dir);
}

/// A scan that states no start time is exported without one. mzdata's model holds 0 for it and its
/// writer prints that 0: through rc.1 the fixture's `scan=21`, which states no time, was exported
/// with `scan start time` 0, and so was every pixel of an imaging run. The direct export reads which
/// spectra state a time; an archive stores 0 for such a scan and only an imaging archive says that
/// no time was stated (`imaging.provenance.time`), so its export states none, while the export of
/// any other archive still writes the stored 0 (the manual's "Different by design").
#[test]
fn a_scan_without_a_start_time_is_exported_without_one() {
    let dir = scratch("start-time");
    let tiny = std::fs::read_to_string(TINY).unwrap();
    assert_eq!((count(&tiny, "<spectrum "), count(&tiny, "MS:1000016")), (4, 3), "the fixture changed");
    let times = |mzml: &str| -> Vec<(usize, usize)> {
        elements(mzml, "spectrum").iter().map(|s| (count(s, "MS:1000016"), zeros(s, "MS:1000016"))).collect()
    };
    let routes = both_routes(Path::new(TINY), &dir, &[]);
    assert_eq!(times(&routes[0].1), [(1, 0), (1, 0), (0, 0), (1, 0)], "direct: scan=21 states no time");
    assert_eq!(times(&routes[1].1), [(1, 0), (1, 0), (1, 1), (1, 0)], "archive: the 0 it stores for scan=21");
    let mut reader = mzdata::io::mzml::MzMLReader::new_indexed(std::io::Cursor::new(routes[0].1.clone().into_bytes()));
    let read: Vec<f64> = reader.iter().map(|s| s.start_time()).collect();
    assert_eq!(read[2], 0.0, "no time reads as mzdata's default");
    assert!(read[0] > 5.0 && read[3] > 0.7, "{read:?}");
    // A 0 the source writes itself is a statement, and stays.
    let stated = r#"name="scan start time" value="5.8905000000000003""#;
    assert_eq!(count(&tiny, stated), 1, "the fixture changed");
    let source = dir.join("zero-time.mzML");
    std::fs::write(&source, tiny.replace(stated, r#"name="scan start time" value="0""#)).unwrap();
    let routes = both_routes(&source, &dir, &[]);
    assert_eq!(times(&routes[0].1), [(1, 1), (1, 0), (0, 0), (1, 0)], "direct: the stated 0, and no other");
    for (route, mzml, log) in &routes {
        assert_well_formed(route, mzml, log);
    }
    // An imaging run that states no time at all: none on either route.
    let imzml = std::fs::read_to_string(IMAGING).unwrap();
    assert_eq!((count(&imzml, "<spectrum "), count(&imzml, "MS:1000016")), (9, 0), "the fixture changed");
    for (route, mzml, log) in both_routes(Path::new(IMAGING), &dir, &[]) {
        assert_eq!((count(&mzml, "<spectrum "), count(&mzml, "MS:1000016")), (9, 0), "{route}: a time nobody stated");
        assert_eq!(count(&mzml, "accession=\"IMS:1000050\""), 9, "{route}: the scans keep what they state");
        // Its nine pixels state `ms level` 1: nothing is summed over spectra that state no time,
        // as a conversion synthesizes no pair into the archive of such a run.
        assert_eq!(count(&imzml, "name=\"ms level\" value=\"1\""), 1, "the fixture changed");
        assert_eq!((count(&mzml, "<chromatogram"), count(&mzml, "<indexList count=\"1\">")), (0, 1), "{route}: a trace at time 0 nobody stated");
        assert_well_formed(route, &mzml, &log);
    }
    // Where some spectra state a time and others none (the fixture: 3 of 4), every MS1 spectrum is
    // summed, the untimed at 0, as in the archive: the two routes hold the same trace.
    let routes = both_routes(Path::new(TINY), &dir, &[]);
    let traces: Vec<Vec<(String, Vec<f64>)>> = routes
        .iter()
        .map(|(_, mzml, _)| chromatograms(mzml).iter().filter(|c| c.id() == "BPC").map(|c| (c.id().to_string(), c.time().unwrap().to_vec())).collect())
        .collect();
    assert!(!traces[0].is_empty() && traces[0][0].1.iter().all(|t| t.is_finite()), "{:?}", traces[0]);
    assert_eq!(traces[0], traces[1], "direct and archive");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Nothing is summed over a run without an MS1 spectrum, and a run left without a chromatogram has
/// no `<chromatogramList>` and no chromatogram index: the schema lets a run go without the list,
/// not the list without a member. Through rc.1 the two MS2 spectra of this fixture got a `TIC` and
/// a `BIC` summed over them on the direct route, a `TIC` on the archive's.
#[test]
fn nothing_is_summed_over_a_run_without_an_ms1_spectrum() {
    let dir = scratch("no-ms1");
    let source = std::fs::read_to_string(MS2_ONLY).unwrap();
    assert_eq!((count(&source, "<spectrum "), count(&source, "name=\"ms level\" value=\"2\""), count(&source, "<chromatogram")), (2, 2, 0), "the fixture changed");
    for (route, mzml, log) in both_routes(Path::new(MS2_ONLY), &dir, &[]) {
        assert_eq!((count(&mzml, "<spectrum "), count(&mzml, "<chromatogram")), (2, 0), "{route}: {}", &mzml[mzml.find("</spectrumList>").unwrap()..]);
        assert_eq!((count(&mzml, "<indexList count=\"1\">"), count(&mzml, "<index name=")), (1, 1), "{route}");
        assert_well_formed(route, &mzml, &log);
        assert!(chromatograms(&mzml).is_empty(), "{route}");
        let mut reader = mzdata::io::mzml::MzMLReader::new_indexed(std::io::Cursor::new(mzml.into_bytes()));
        assert_eq!(reader.iter().count(), 2, "{route}");
        if let Some(loads) = openms_loads(&dir.join(if route == "direct" { "direct.mzML" } else { "export.mzML" })) {
            assert!(loads, "{route}: OpenMS FileInfo does not load the export");
        }
    }
    // The export converts back, to an archive with the two spectra and no chromatogram of its own.
    let back = dir.join("back.mzML");
    convert(&dir.join("direct.mzML"), &dir.join("back.mzpeak"), &[], &[]);
    convert(&dir.join("back.mzpeak"), &back, &[], &[]);
    let back = std::fs::read_to_string(&back).unwrap();
    assert_eq!((count(&back, "<spectrum "), count(&back, "<chromatogram")), (2, 0));
    let _ = std::fs::remove_dir_all(&dir);
}

/// The fixture's third spectrum (`scan=21`) has no point. Both routes write it with an m/z and an
/// intensity array of length 0, each an empty `<binary>` declared `no compression` (an empty string
/// is no zlib stream); through rc.1 the archive's export had `<binaryDataArrayList count="0">`,
/// which the schema does not allow. The arrays of every other spectrum keep their compression.
#[test]
fn an_empty_spectrum_is_written_with_two_empty_arrays() {
    let dir = scratch("empty-spectrum");
    for (route, mzml, log) in both_routes(Path::new(TINY), &dir, &[]) {
        let spectra = elements(&mzml, "spectrum");
        let empty = spectra.iter().find(|s| s.contains("id=\"scan=21\"")).unwrap();
        assert!(empty.contains("defaultArrayLength=\"0\"") && empty.contains("<binaryDataArrayList count=\"2\">"), "{route}: {empty}");
        assert_eq!((count(empty, "encodedLength=\"0\""), count(empty, "<binary></binary>")), (2, 2), "{route}: {empty}");
        assert!(empty.contains("MS:1000514") && empty.contains("MS:1000515"), "{route}: an m/z and an intensity array");
        assert_eq!((count(empty, "MS:1000576"), count(empty, "MS:1000574")), (2, 0), "{route}: declared uncompressed: {empty}");
        for full in spectra.iter().filter(|s| !s.contains("id=\"scan=21\"")) {
            assert_eq!((count(full, "MS:1000576"), count(full, "MS:1000574")), (0, 2), "{route}: {}", &full[..120]);
        }
        assert_well_formed(route, &mzml, &log);
        let mut reader = mzdata::io::mzml::MzMLReader::new_indexed(std::io::Cursor::new(mzml.into_bytes()));
        let points: Vec<usize> = reader.iter().map(|s| s.peaks().len()).collect();
        assert_eq!(points, [15, 10, 0, 15], "{route}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// OpenMS, when one is at hand (`MZPC_OPENMS_FILEINFO`, or the environment the corpus audit used):
/// whether its `FileInfo` loads `mzml`. `None` without one.
fn openms_loads(mzml: &Path) -> Option<bool> {
    let file_info = std::env::var_os("MZPC_OPENMS_FILEINFO")
        .map(PathBuf::from)
        .or_else(|| std::env::home_dir().map(|h| h.join("anaconda3/envs/fastag-diag/bin/FileInfo")))
        .filter(|p| p.exists())?;
    let out = Command::new(file_info).arg("-in").arg(mzml).output().ok()?;
    Some(out.status.success())
}

/// A chromatogram without a point — here the archive's three (the fixture's two and the
/// synthesized base-peak one), cut by an `--rt` window that holds nothing — is written with empty
/// payloads, not with the zlib stream of nothing that OpenMS 3.5 refuses in an integer array.
#[test]
fn a_chromatogram_without_a_point_has_empty_payloads() {
    let dir = scratch("empty-chromatogram");
    let archive = dir.join("tiny.mzpeak");
    convert(Path::new(TINY), &archive, &[], &[]);
    let export = dir.join("export.mzML");
    let log = convert(&archive, &export, &["--rt", "100-200"], &[]);
    let mzml = std::fs::read_to_string(&export).unwrap();
    assert_eq!(count(&mzml, "<spectrum "), 0);
    let chroms = elements(&mzml, "chromatogram");
    assert_eq!(chroms.len(), 3, "tic, sic and BPC: {mzml}");
    for c in &chroms {
        assert!(c.contains("defaultArrayLength=\"0\""), "{c}");
        assert_eq!(count(c, "encodedLength=\"0\""), count(c, "<binaryDataArray "), "{c}");
        assert_eq!((count(c, "MS:1000576"), count(c, "MS:1000574")), (count(c, "<binaryDataArray "), 0), "declared uncompressed: {c}");
    }
    assert_well_formed("archive --rt", &mzml, &log);
    assert!(chromatograms(&mzml).iter().all(|c| c.time().unwrap().is_empty()));
    if let Some(loads) = openms_loads(&export) {
        assert!(loads, "OpenMS FileInfo does not load {}", export.display());
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// The base-peak chromatogram a source lacks is summed the same way on both routes: `BPC`, a point
/// per MS1 spectrum, from the signal written. Through rc.1 the direct export's was `BIC` with a
/// point for each of the 201 spectra, the archive's `BPC` with the 15 MS1 ones.
#[test]
fn the_summed_base_peak_chromatogram_is_the_same_on_both_routes() {
    let dir = scratch("bpc");
    let routes = both_routes(Path::new(SWATH_GZ), &dir, &[]);
    let summed: Vec<(Vec<f64>, Vec<f32>)> = routes
        .iter()
        .map(|(route, mzml, _)| {
            let chroms = chromatograms(mzml);
            let mut ids: Vec<&str> = chroms.iter().map(|c| c.id()).collect();
            ids.sort_unstable();
            assert_eq!(ids, ["BPC", "TIC"], "{route}");
            let tic = chroms.iter().find(|c| c.id() == "TIC").unwrap();
            assert_eq!(tic.time().unwrap().len(), 616, "{route}: the source's own TIC, as it is");
            let bpc = chroms.iter().find(|c| c.id() == "BPC").unwrap();
            (bpc.time().unwrap().to_vec(), bpc.intensity().unwrap().to_vec())
        })
        .collect();
    assert_eq!((summed[0].0.len(), summed[1].0.len()), (15, 15), "a point per MS1 spectrum");
    // The archive holds a spectrum's time as a 32-bit float.
    for (d, a) in summed[0].0.iter().zip(&summed[1].0) {
        assert!((d - a).abs() <= 1e-6, "time: direct {d} vs archive {a}");
    }
    for (d, a) in summed[0].1.iter().zip(&summed[1].1) {
        assert!((d - a).abs() <= d.abs() * 1e-6, "intensity: direct {d} vs archive {a}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// The index and the checksum are those of the document, compressed or not: a `.mzML.gz` holds the
/// same bytes as the `.mzML`, offsets counted in the uncompressed text. mzdata's writer records each
/// offset at the line break before the element and takes the checksum before its last bytes are out.
#[test]
fn a_gzipped_export_has_the_same_index_and_checksum() {
    let dir = scratch("gz-index");
    let (plain, gz) = (dir.join("out.mzML"), dir.join("out.mzML.gz"));
    let log = convert(Path::new(TINY), &plain, &[], &[]);
    convert(Path::new(TINY), &gz, &[], &[]);
    let text = std::fs::read_to_string(&plain).unwrap();
    assert_eq!(count(&text, "<offset idRef="), 4 + 3, "four spectra; tic, sic and BPC");
    assert_well_formed("direct", &text, &log);
    // The two differ in the command line the export records, and so in the checksum.
    let unzipped = gunzip(&gz);
    assert_well_formed("direct .gz", &unzipped, &log);
    let index = |doc: &str| doc[doc.find("<indexList ").unwrap()..doc.find("<fileChecksum>").unwrap()].to_string();
    assert_eq!(index(&text).lines().count(), index(&unzipped).lines().count());
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- the corpus units the fixtures stand in for ----

/// ProteoWizard's combineIMS centroid export of a PASEF run: all 15 spectra keep their 1/K0 array
/// and their 64-bit intensities on the direct lane (0 and 15 narrowed through rc.1).
#[test]
fn corpus_hela_combine_ims_keeps_its_mobility_arrays() {
    let Some(source) = corpus::corpus_path("pwiz-examples/Bruker/Bruker/Reader_Bruker_Test.data/Hela_QC_PASEF_Slot1-first-6-frames-combineIMS-centroid.mzML") else { return };
    let dir = scratch("hela");
    let text = std::fs::read_to_string(&source).unwrap();
    let want = (count(&text, "MS:1003006"), count(&text, "MS:1000523"), count(&text, "MS:1000521"));
    assert_eq!(want, (15, 49, 0), "the corpus unit changed");
    for (route, mzml, log) in both_routes(&source, &dir, &[]) {
        assert_eq!((count(&mzml, "MS:1003006"), count(&mzml, "MS:1000523"), count(&mzml, "MS:1000521")), want, "{route}");
        assert_eq!(count(&mzml, "MS:1000927"), count(&text, "MS:1000927"), "{route}: ion injection time");
        assert_well_formed(route, &mzml, &log);
    }
    assert!(arrays_of(&source) == arrays_of(&dir.join("direct.mzML")), "the direct export's arrays are not the source's");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Three imaging runs whose imzML states no polarity and no injection time (one of them negative
/// mode by its name): neither does an export, on either route. Two of them state no scan start
/// time either, and one of those `ms level` 0 on every pixel, through a param group: the exports
/// state no time for the two, and sum no TIC or base-peak chromatogram over either (through rc.1:
/// a `scan start time` of 0 on every pixel, and a pair of 25 points at time 0) — over the one for
/// want of an MS1 spectrum, over the other because no spectrum states a time, which is why a
/// conversion synthesizes none into its archive either. The first 25 pixels of each.
#[test]
fn corpus_imaging_exports_invent_no_polarity() {
    for (unit, timed, ms1) in [
        ("imzml-examples/zenodo-LA-ESI/imzML_LA-ESI/180817_NEG_Thaliana_Leaf_bottom_1_0841.imzML", false, true),
        ("imzml-examples/zenodo-LTP/imzML_LTP/ltpmsi-chilli.imzML", true, true),
        ("imzml-examples/zenodo-18187395-GBM-multimodal/imzml/Test_P15_r2.imzML", false, false),
    ] {
        let Some(source) = corpus::corpus_path(unit) else { continue };
        let text = std::fs::read_to_string(&source).unwrap();
        assert_eq!(count(&text, "MS:1000130") + count(&text, "MS:1000129") + count(&text, "MS:1000927"), 0, "{unit} changed");
        assert_eq!(count(&text, "MS:1000016") > 0, timed, "{unit} changed");
        assert_eq!(count(&text, "name=\"ms level\" value=\"0\"") == 0, ms1, "{unit} changed");
        let dir = scratch("imaging");
        for (route, mzml, log) in both_routes(&source, &dir, &[("MZPC_MAX_SPECTRA", "25")]) {
            assert_eq!(count(&mzml, "<spectrum "), 25, "{unit} {route}");
            assert_eq!(count(&mzml, "MS:1000130") + count(&mzml, "MS:1000129"), 0, "{unit} {route}: a polarity nobody stated");
            assert_eq!(count(&mzml, "MS:1000927"), 0, "{unit} {route}: ion injection time");
            assert_eq!(count(&mzml, "MS:1000016"), if timed { 25 } else { 0 }, "{unit} {route}: scan start time");
            let summed: Vec<String> = chromatograms(&mzml).iter().map(|c| format!("{}:{}", c.id(), c.time().unwrap().len())).collect();
            assert_eq!(summed, if ms1 && timed { vec!["TIC:25", "BPC:25"] } else { Vec::new() }, "{unit} {route}");
            assert_eq!(count(&mzml, "<chromatogramList"), usize::from(ms1 && timed), "{unit} {route}");
            assert_eq!(count(&log, "25 of 25 spectra state no polarity"), 1, "{unit} {route}\n{log}");
            assert_well_formed(route, &mzml, &log);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// A Thermo SRM run: no injection time, no selected-ion intensity and no collision energy is known
/// for any spectrum, and the archive holds nulls. Through rc.1 both exports stated a 0 for each of
/// the three on every spectrum (9,600 each). The first 200 spectra, from the `.raw` and its archive.
#[test]
fn corpus_srm_exports_invent_no_zeros() {
    let Some(raw) = corpus::corpus_path("general-ms/PXD057269/LD401_001fmol_r1.raw") else { return };
    let dir = scratch("ld401");
    for (route, mzml, log) in both_routes(&raw, &dir, &[("MZPC_MAX_SPECTRA", "200")]) {
        assert_eq!(count(&mzml, "<spectrum "), 200, "{route}");
        for (term, accession) in [("ion injection time", "MS:1000927"), ("peak intensity", "MS:1000042"), ("collision energy", "\"MS:1000045\"")] {
            assert_eq!(count(&mzml, accession), 0, "{route}: {term}");
        }
        assert_eq!(count(&mzml, "<precursorList count=\"1\">"), 200, "{route}: the precursors stay");
        assert_well_formed(route, &mzml, &log);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// 285 of this Thermo run's 2,255 scans hold no peak. Both routes write each with two arrays of
/// length 0 and no observed m/z range; through rc.1 the direct export stated `lowest observed m/z`
/// inf and `highest observed m/z` -inf, and the archive's an empty array list.
#[test]
fn corpus_thermo_empty_scans_are_written_valid() {
    let Some(raw) = corpus::corpus_path("general-ms/PXD018751/SZB8102938.RAW") else { return };
    let dir = scratch("szb");
    for (route, mzml, log) in both_routes(&raw, &dir, &[]) {
        let spectra = elements(&mzml, "spectrum");
        assert_eq!(spectra.len(), 2255, "{route}");
        let empty: Vec<&&str> = spectra.iter().filter(|s| s.contains("defaultArrayLength=\"0\"")).collect();
        assert_eq!(empty.len(), 285, "{route}");
        for s in empty {
            assert!(s.contains("<binaryDataArrayList count=\"2\">") && count(s, "<binary></binary>") == 2, "{route}: {s}");
            assert!(!s.contains("MS:1000528") && !s.contains("MS:1000527"), "{route}: an observed range of no peak");
        }
        assert_eq!(count(&mzml, "<precursorList count=\"1\">"), 1622, "{route}");
        assert_well_formed(route, &mzml, &log);
        if let Some(loads) = openms_loads(&dir.join(if route == "direct" { "direct.mzML" } else { "export.mzML" })) {
            assert!(loads, "{route}: OpenMS FileInfo does not load the export");
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// The five corpus units whose archive holds a chromatogram without a point beside a 64-bit
/// integer `ms level` array: OpenMS 3.5 refused each export (`Decompression error?`). Converted here
/// from their sources; every export is free of the zlib stream of nothing, and loads in OpenMS when
/// one is at hand.
#[test]
fn corpus_empty_chromatograms_load_in_openms() {
    for unit in [
        "pwiz-examples/ABI/ABI/Reader_ABI_Test.data/PressureTrace1-6500SysSuit1269-globalChromatogramsAreMs1Only.mzML",
        "pwiz-examples/Bruker/Bruker/Reader_Bruker_Test.data/20percLaser_100fold_1_0_H6_MS-ms2-centroid.mzML",
        "pwiz-examples/Bruker/Bruker/Reader_Bruker_Test.data/ThyroglobMRM000003-combineIMS-ms1-centroid.mzML",
        "pwiz-examples/Bruker/Bruker/Reader_Bruker_Test.data/ThyroglobMRM000003-ms1-centroid.mzML",
        "pwiz-examples/Waters/Waters/Reader_Waters_Test.data/HDMRM_Short_noLM-globalChromatogramsAreMs1Only.mzML",
    ] {
        let Some(source) = corpus::corpus_path(unit) else { continue };
        let dir = scratch("empty-chrom-corpus");
        for (route, mzml, log) in both_routes(&source, &dir, &[]) {
            assert!(mzml.contains("<chromatogram ") && mzml.contains("defaultArrayLength=\"0\""), "{unit} {route}: an empty chromatogram");
            assert_well_formed(route, &mzml, &log);
            if let Some(loads) = openms_loads(&dir.join(if route == "direct" { "direct.mzML" } else { "export.mzML" })) {
                assert!(loads, "{unit} {route}: OpenMS FileInfo does not load the export");
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}

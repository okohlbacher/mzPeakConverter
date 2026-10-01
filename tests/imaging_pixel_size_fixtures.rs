//! The five pixel-size imzML files of HUPO-PSI/mzPeak-specification#23 (`tests/fixtures/imaging/thyra`,
//! from the Thyra repository, MIT; see its README.txt): one 3 × 2 acquisition of 50 µm pixels, its
//! pixel size stated the five ways public imzML headers state it. `pixel_size_expected.json`, written
//! by the files' author, says what a reader can conclude from each header: the size on both axes or
//! none, and how it was settled. The archive must agree — `metadata.imaging.pixel_size_um` with the
//! size, `metadata.imaging.pixel_size_source` with his word for the way (the converter's key since
//! 0.17.0; proposed for spec PR #25 with three more values), and its `transformations` with what
//! the rule did.
//!
//! Through 0.16.0 the area file (`IMS:1000046` alone, "pixel size", 2500) was written with a 50 µm x
//! size only and no `pixel_size_um`, although the vocabulary defines a lone `IMS:1000046` as the y
//! size too.

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/imaging/thyra");

/// The pixel-size transformations the imzML lane declares (`src/imaging.rs`).
const UNIT_ASSUMED: &str = "imzml:pixel-size-unit-assumed-um";
const AREA_TO_LENGTH: &str = "imzml:pixel-size-area-to-length";
const DROPPED: &str = "imzml:pixel-size-dropped";

fn metadata(archive: &Path) -> serde_json::Value {
    let mut zip = zip::ZipArchive::new(File::open(archive).unwrap()).unwrap();
    let mut v = Vec::new();
    zip.by_name("mzpeak_index.json").unwrap().read_to_end(&mut v).unwrap();
    serde_json::from_slice::<serde_json::Value>(&v).unwrap()["metadata"].clone()
}

/// Convert `tests/fixtures/imaging/thyra/pixel_size_<case>.imzML`; the archive's index metadata.
fn convert(case: &str, dir: &Path) -> serde_json::Value {
    let out = dir.join(format!("{case}.mzpeak"));
    let r = Command::new(env!("CARGO_BIN_EXE_mzpeak-convert"))
        .arg(Path::new(FIXTURES).join(format!("pixel_size_{case}.imzML")))
        .arg("-o")
        .arg(&out)
        .arg("--force")
        .env_remove("RUST_LOG")
        .output()
        .expect("failed to run mzpeak-convert");
    assert!(r.status.success(), "{case}: exit {:?}; stderr:\n{}", r.status.code(), String::from_utf8_lossy(&r.stderr));
    metadata(&out)
}

/// The pixel-size params of the grid entry: `(accession, name, value, unit)`.
fn pixel_sizes(m: &serde_json::Value) -> Vec<(String, String, f64, String)> {
    m["scan_settings_list"][0]["parameters"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|p| p["accession"] == "IMS:1000046" || p["accession"] == "IMS:1000047")
        .map(|p| {
            let text = |k: &str| p[k].as_str().unwrap_or_default().to_string();
            (text("accession"), text("name"), p["value"].as_f64().unwrap(), text("unit"))
        })
        .collect()
}

#[test]
fn the_archives_agree_with_pixel_size_expected_json() {
    let expected: serde_json::Value =
        serde_json::from_slice(&std::fs::read(Path::new(FIXTURES).join("pixel_size_expected.json")).unwrap()).unwrap();
    let expected = expected.as_object().unwrap();
    assert_eq!(expected.len(), 5, "the five files");
    let dir: PathBuf = std::env::temp_dir().join(format!("mzpc-thyra-pixel-size-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    for (case, want) in expected {
        let m = convert(case, &dir);
        let img = &m["imaging"];
        assert_eq!((&img["is_imaging"], &img["pixel_count"]), (&serde_json::json!(true), &serde_json::json!({"x": 3, "y": 2})), "{case}: {img:#}");

        // `pixel_size_um`: his `[x, y]` is the marker's `{x, y}`; his null is its absence, with no
        // pixel size left in the scan settings either.
        let sizes = pixel_sizes(&m);
        match want["pixel_size_um"].as_array() {
            Some(xy) => {
                let (x, y) = (xy[0].as_f64().unwrap(), xy[1].as_f64().unwrap());
                assert_eq!(img["pixel_size_um"], serde_json::json!({"x": x, "y": y}), "{case}: {img:#}");
                let stated = |acc: &str, v: f64| (sizes.iter().find(|s| s.0 == acc).map(|s| (s.2, s.3.as_str())), Some((v, "UO:0000017")));
                let ((got_x, want_x), (got_y, want_y)) = (stated("IMS:1000046", x), stated("IMS:1000047", y));
                assert_eq!((got_x, got_y), (want_x, want_y), "{case}: the scan settings say the same: {sizes:?}");
            }
            None => {
                assert!(want["pixel_size_um"].is_null(), "{case}: {want:#}");
                assert!(img.get("pixel_size_um").is_none() && sizes.is_empty(), "{case}: {img:#} {sizes:?}");
            }
        }

        // `pixel_size_source`: his word in the marker, and the archive's own account of it — the
        // transformation declared, the case of the rule in the `imaging_pixel_size` block, and the
        // marker's provenance line.
        assert_eq!(img["pixel_size_source"], want["pixel_size_source"], "{case}: {img:#}");
        let declared: Vec<&str> = m["transformations"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| t.as_str())
            .filter(|t| t.starts_with("imzml:pixel-size"))
            .collect();
        let rule = m["imaging_pixel_size"][0]["case"].as_str();
        let provenance = img["provenance"]["pixel_size"].as_str().unwrap();
        match want["pixel_size_source"].as_str().unwrap() {
            "declared" => {
                assert!(declared.is_empty() && m.get("imaging_pixel_size").is_none(), "{case}: {declared:?}");
                assert_eq!(provenance, "as stated", "{case}");
            }
            "unit_assumed" => {
                assert_eq!(declared, [UNIT_ASSUMED], "{case}");
                assert_eq!(rule, Some("x and y without a unit: micrometre assumed"), "{case}");
            }
            "derived_from_area" => {
                assert_eq!(declared, [AREA_TO_LENGTH], "{case}");
                assert_eq!(rule, Some("one value: an area (√value × count = extent)"), "{case}");
                // The lone x is written as both sizes, under the vocabulary's names.
                let names: Vec<&str> = sizes.iter().map(|s| s.1.as_str()).collect();
                assert_eq!(names, ["pixel size (x)", "pixel size y"], "{case}");
            }
            "unknown" => {
                assert_eq!(declared, [DROPPED], "{case}");
                // Neither file states an extent: nothing was tested, and the block says what is missing.
                assert_eq!(rule, Some("one value, untestable"), "{case}");
                let detail = m["imaging_pixel_size"][0]["detail"].as_str().unwrap();
                assert!(detail.ends_with("no max dimension (IMS:1000044/45) to test it against"), "{case}: {detail}");
            }
            other => panic!("{case}: pixel_size_source {other:?} is not one of the four the README names"),
        }
        if !declared.is_empty() {
            assert_eq!(provenance, "checked, see imaging_pixel_size", "{case}");
        }

        // Each `.ibd` matches the SHA-1 its header states, and no file states a scan start time.
        assert_eq!(img["provenance"]["ibd_checksum"], "verified", "{case}");
        assert_eq!(img["provenance"]["time"], "not stated by the source; index is the source list order", "{case}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

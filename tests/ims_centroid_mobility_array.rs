//! A CENTROID spectrum with a per-peak ion-mobility array must keep it (2026-09-09 defect).
//!
//! Fixture: `tests/data/pasef_combineims_centroid.pwiz.mzML` — ProteoWizard's
//! `Reader_Bruker_Test.data` PASEF frame 6, combined across its 100 IMS scans and centroided
//! (`--combineIonMobilitySpectra`): one MS1 spectrum of 1391 peaks with m/z, intensity and an
//! MS:1003006 `mean inverse reduced ion mobility array`.
//!
//! mzdata's mzML reader builds a `CentroidPeak` set eagerly for a centroid spectrum, and the
//! writer followed `peaks()` for both the peak-facet schema and the peak rows. That set holds only
//! m/z + intensity, so the mobility array left behind in `raw_arrays()` reached neither a column
//! nor `auxiliary_arrays`: it was gone, silently (`spectrum_array_index` listed m/z + intensity,
//! `number_of_auxiliary_arrays == 0`). Pinned here for the chunked (default) and point layouts:
//! the peak facet declares an ion-mobility column, and the values read back bit-identical.

use std::path::{Path, PathBuf};
use std::process::Command;

use mzdata::prelude::*;
use mzdata::spectrum::ArrayType;
use mzpeak_prototyping::MzPeakReader;

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/pasef_combineims_centroid.pwiz.mzML")
}

fn scratch(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("mzpc-imscentroid-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn source_mobility() -> Vec<f64> {
    let mut reader = mzdata::MZReader::open_path(fixture()).expect("opening the fixture");
    let spectra: Vec<_> = reader.iter().collect();
    assert_eq!(spectra.len(), 1, "fixture is one combined PASEF frame");
    spectra[0]
        .arrays
        .as_ref()
        .unwrap()
        .get(&ArrayType::MeanInverseReducedIonMobilityArray)
        .expect("fixture must carry MS:1003006")
        .to_f64()
        .unwrap()
        .to_vec()
}

fn mobility_array_survives(layout: &str) {
    let dir = scratch(layout);
    let out = dir.join("out.mzpeak");
    let st = Command::new(env!("CARGO_BIN_EXE_mzpeak-convert"))
        .arg(fixture())
        .args(["-o", out.to_str().unwrap(), "-q", "--layout", layout])
        .status()
        .expect("failed to run mzpeak-convert");
    assert!(st.success(), "conversion failed: {st}");

    let want = source_mobility();
    assert_eq!(want.len(), 1391);

    let mut reader = MzPeakReader::new(&out).unwrap();
    // (a) The peak facet DECLARES the mobility column, in this layout's struct.
    let index = reader.metadata.peak_array_indices().expect("peaks facet array index");
    let im = index
        .iter()
        .find(|e| e.array_type.is_ion_mobility())
        .unwrap_or_else(|| {
            panic!(
                "spectrum_array_index of the peaks facet has no ion-mobility entry: {:?}",
                index.iter().map(|e| e.path.clone()).collect::<Vec<_>>()
            )
        });
    assert_eq!(im.array_type, ArrayType::MeanInverseReducedIonMobilityArray);
    let prefix = if layout == "chunked" { "chunk." } else { "point." };
    assert!(im.path.starts_with(prefix), "{layout} layout stores {}", im.path);

    // (b) The values come back, per peak, bit-identical to the source.
    let arrays = reader
        .get_spectrum_peak_arrays_for(0)
        .unwrap()
        .expect("spectrum 0 has peak arrays");
    let got = arrays
        .get(&ArrayType::MeanInverseReducedIonMobilityArray)
        .expect("readback carries the mobility array")
        .to_f64()
        .unwrap();
    assert_eq!(got.len(), want.len());
    assert_eq!(arrays.mzs().unwrap().len(), want.len(), "one mobility per peak");
    for (i, (g, w)) in got.iter().zip(&want).enumerate() {
        assert_eq!(g.to_bits(), w.to_bits(), "peak {i}: {g} != source {w}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_centroid_ims_spectrum_keeps_its_mobility_array_in_the_chunked_peaks_facet() {
    mobility_array_survives("chunked");
}

#[test]
fn a_centroid_ims_spectrum_keeps_its_mobility_array_in_the_point_peaks_facet() {
    mobility_array_survives("point");
}

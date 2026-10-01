//! Fidelity controls and their declaration, through the real binary on imzML files this test
//! writes itself, so the source arrays are known to the last bit:
//!
//! * the `fidelity` index block states what the conversion did to the signal — points read against
//!   points stored, the binary types the file declares against the stored column types, and a
//!   numpress bound that holds for every decoded m/z;
//! * `--keep-zero-runs` (and `MZPC_KEEP_ZERO_RUNS`) stores every profile point, declares no
//!   `zero-run-mask`, and gives the pixels of a continuous-mode run one decoded m/z axis;
//! * `--lossless` writes an archive whose stored arrays equal the source arrays bit for bit, at
//!   the declared types, or fails and writes nothing;
//! * `--no-numpress` (delta) is NOT bit-exact on sparse 64-bit m/z, and the block says where;
//! * the `.mzpeak` filter carries the block, or drops it when it removes spectra.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use arrow::array::{Array, AsArray};
use arrow::datatypes::{DataType, Float32Type, Float64Type, UInt64Type};
use mzpeak_prototyping::MzPeakReader;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use serde_json::Value;

fn scratch(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("mzpc-fidelity-it-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// `mzpeak-convert <input> -o <output> -q <args…>`, with `MZPC_KEEP_ZERO_RUNS` set when `env`.
fn run(input: &Path, output: &Path, args: &[&str], env: bool) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_mzpeak-convert"));
    cmd.arg(input).arg("-o").arg(output).arg("-q").args(args).env_remove("MZPC_KEEP_ZERO_RUNS");
    if env {
        cmd.env("MZPC_KEEP_ZERO_RUNS", "1");
    }
    cmd.output().expect("failed to run mzpeak-convert")
}

fn convert(input: &Path, output: &Path, args: &[&str]) {
    let r = run(input, output, args, false);
    assert!(r.status.success(), "{args:?} failed: {}", String::from_utf8_lossy(&r.stderr));
}

/// The run must fail, say `needle`, and leave neither the output nor its temporary behind.
fn refused(input: &Path, output: &Path, args: &[&str], needle: &str) {
    let r = run(input, output, args, false);
    let err = String::from_utf8_lossy(&r.stderr);
    assert!(!r.status.success(), "{args:?} was accepted");
    assert!(err.contains(needle), "{args:?}: expected {needle:?} in:\n{err}");
    assert!(!output.exists(), "{args:?} left {} behind", output.display());
    assert!(!output.with_extension("mzpeak.tmp").exists(), "{args:?} left a temporary behind");
}

fn member(archive: &Path, name: &str) -> Vec<u8> {
    let mut zip = zip::ZipArchive::new(std::fs::File::open(archive).unwrap()).unwrap();
    let mut e = zip.by_name(name).unwrap_or_else(|_| panic!("{name} missing from the archive"));
    let mut buf = Vec::new();
    std::io::Read::read_to_end(&mut e, &mut buf).unwrap();
    buf
}

fn metadata(archive: &Path) -> Value {
    serde_json::from_slice::<Value>(&member(archive, "mzpeak_index.json")).unwrap()["metadata"].clone()
}

fn transformations(archive: &Path) -> Vec<String> {
    metadata(archive)["transformations"].as_array().unwrap().iter().map(|v| v.as_str().unwrap().to_string()).collect()
}

/// A source array at its declared binary type.
#[derive(Clone, Debug, PartialEq)]
enum Arr {
    F32(Vec<f32>),
    F64(Vec<f64>),
}

impl Arr {
    fn len(&self) -> usize {
        match self {
            Arr::F32(v) => v.len(),
            Arr::F64(v) => v.len(),
        }
    }

    fn bytes(&self) -> Vec<u8> {
        match self {
            Arr::F32(v) => v.iter().flat_map(|x| x.to_le_bytes()).collect(),
            Arr::F64(v) => v.iter().flat_map(|x| x.to_le_bytes()).collect(),
        }
    }

    fn as_f64(&self) -> Vec<f64> {
        match self {
            Arr::F32(v) => v.iter().map(|x| f64::from(*x)).collect(),
            Arr::F64(v) => v.clone(),
        }
    }

    /// `(accession, name)` of the mzML binary data type.
    fn term(&self) -> (&'static str, &'static str) {
        match self {
            Arr::F32(_) => ("MS:1000521", "32-bit float"),
            Arr::F64(_) => ("MS:1000523", "64-bit float"),
        }
    }

    fn type_name(&self) -> &'static str {
        match self {
            Arr::F32(_) => "float32",
            Arr::F64(_) => "float64",
        }
    }
}

/// A tiny deterministic generator in [0, 1).
struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> f64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (self.0 >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// Write `<dir>/<stem>.imzML` + `.ibd` holding `spectra` (m/z, intensity), one pixel each, declared
/// `profile` or centroid. `shared_axis`: continuous mode, every pixel pointing at the first
/// spectrum's m/z array, as a continuous imzML stores it.
fn write_imzml(dir: &Path, stem: &str, spectra: &[(Arr, Arr)], profile: bool, shared_axis: bool) -> PathBuf {
    const UUID: [u8; 16] = [0x1a, 0x2b, 0x3c, 0x4d, 0x5e, 0x6f, 0x70, 0x81, 0x92, 0x03, 0xb4, 0xc5, 0xd6, 0xe7, 0xf8, 0xa9];
    let mut ibd = UUID.to_vec();
    let mut list = String::new();
    let mut axis: Option<(usize, usize)> = None;
    let array = |group: &str, n: usize, offset: usize, bytes: usize| {
        format!(
            r#"          <binaryDataArray encodedLength="0"><referenceableParamGroupRef ref="{group}"/>
            <cvParam cvRef="IMS" accession="IMS:1000103" name="external array length" value="{n}"/>
            <cvParam cvRef="IMS" accession="IMS:1000102" name="external offset" value="{offset}"/>
            <cvParam cvRef="IMS" accession="IMS:1000104" name="external encoded length" value="{bytes}"/><binary/></binaryDataArray>
"#
        )
    };
    for (i, (mz, intensity)) in spectra.iter().enumerate() {
        assert_eq!(mz.len(), intensity.len());
        let (mz_at, mz_bytes) = match axis {
            Some(at) if shared_axis => at,
            _ => {
                let at = (ibd.len(), mz.bytes().len());
                ibd.extend(mz.bytes());
                axis = Some(at);
                at
            }
        };
        let int_at = ibd.len();
        ibd.extend(intensity.bytes());
        list.push_str(&format!(
            r#"      <spectrum id="Scan={}" defaultArrayLength="0" index="{i}">
        <referenceableParamGroupRef ref="spectrum1"/>
        <scanList count="1"><cvParam cvRef="MS" accession="MS:1000795" name="no combination"/>
          <scan><cvParam cvRef="IMS" accession="IMS:1000050" name="position x" value="{}"/><cvParam cvRef="IMS" accession="IMS:1000051" name="position y" value="{}"/></scan>
        </scanList>
        <binaryDataArrayList count="2">
{}{}        </binaryDataArrayList>
      </spectrum>
"#,
            i + 1,
            i % 3 + 1,
            i / 3 + 1,
            array("mzArray", mz.len(), mz_at, mz_bytes),
            array("intensityArray", intensity.len(), int_at, intensity.bytes().len()),
        ));
    }
    let (mz, intensity) = &spectra[0];
    let kind = if profile { ("MS:1000128", "profile spectrum") } else { ("MS:1000127", "centroid spectrum") };
    let mode = if shared_axis { ("IMS:1000030", "continuous") } else { ("IMS:1000031", "processed") };
    let uuid: String = UUID.iter().map(|b| format!("{b:02x}")).collect();
    let doc = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<mzML xmlns="http://psi.hupo.org/ms/mzml" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" xsi:schemaLocation="http://psi.hupo.org/ms/mzml http://psidev.info/files/ms/mzML/xsd/mzML1.1.0.xsd" version="1.1">
  <cvList count="3">
    <cv URI="https://raw.githubusercontent.com/hupo-psi/psi-ms-cv/master/psi-ms.obo" fullName="PSI-MS" id="MS" version="4.1.0"/>
    <cv URI="https://raw.githubusercontent.com/imzML/imzML/master/imagingMS.obo" fullName="Imaging MS" id="IMS" version="1.1.0"/>
    <cv URI="http://ontologies.berkeleybop.org/uo.obo" fullName="Units" id="UO" version="releases/2017-09-25"/>
  </cvList>
  <fileDescription><fileContent>
      <cvParam cvRef="MS" accession="MS:1000579" name="MS1 spectrum"/>
      <cvParam cvRef="MS" accession="{k0}" name="{k1}"/>
      <cvParam cvRef="IMS" accession="IMS:1000080" name="universally unique identifier" value="{{{uuid}}}"/>
      <cvParam cvRef="IMS" accession="{m0}" name="{m1}"/>
  </fileContent></fileDescription>
  <referenceableParamGroupList count="3">
    <referenceableParamGroup id="mzArray">
      <cvParam cvRef="MS" accession="MS:1000576" name="no compression"/>
      <cvParam cvRef="MS" accession="MS:1000514" name="m/z array" unitCvRef="MS" unitAccession="MS:1000040" unitName="m/z"/>
      <cvParam cvRef="IMS" accession="IMS:1000101" name="external data" value="true"/>
      <cvParam cvRef="MS" accession="{mz0}" name="{mz1}"/>
    </referenceableParamGroup>
    <referenceableParamGroup id="intensityArray">
      <cvParam cvRef="MS" accession="MS:1000576" name="no compression"/>
      <cvParam cvRef="MS" accession="MS:1000515" name="intensity array" unitCvRef="MS" unitAccession="MS:1000131" unitName="number of detector counts"/>
      <cvParam cvRef="IMS" accession="IMS:1000101" name="external data" value="true"/>
      <cvParam cvRef="MS" accession="{i0}" name="{i1}"/>
    </referenceableParamGroup>
    <referenceableParamGroup id="spectrum1">
      <cvParam cvRef="MS" accession="MS:1000579" name="MS1 spectrum"/>
      <cvParam cvRef="MS" accession="MS:1000511" name="ms level" value="1"/>
      <cvParam cvRef="MS" accession="{k0}" name="{k1}"/>
    </referenceableParamGroup>
  </referenceableParamGroupList>
  <softwareList count="1"><software id="gen" version="1"><cvParam cvRef="MS" accession="MS:1000799" name="custom unreleased software tool" value="gen"/></software></softwareList>
  <scanSettingsList count="1"><scanSettings id="s1">
      <cvParam cvRef="IMS" accession="IMS:1000042" name="max count of pixels x" value="3"/>
      <cvParam cvRef="IMS" accession="IMS:1000043" name="max count of pixels y" value="{rows}"/>
  </scanSettings></scanSettingsList>
  <instrumentConfigurationList count="1"><instrumentConfiguration id="IC1"><cvParam cvRef="MS" accession="MS:1000031" name="instrument model"/></instrumentConfiguration></instrumentConfigurationList>
  <dataProcessingList count="1"><dataProcessing id="dp1"><processingMethod order="1" softwareRef="gen"><cvParam cvRef="MS" accession="MS:1000544" name="Conversion to mzML"/></processingMethod></dataProcessing></dataProcessingList>
  <run id="r" defaultInstrumentConfigurationRef="IC1">
    <spectrumList count="{n}" defaultDataProcessingRef="dp1">
{list}    </spectrumList>
  </run>
</mzML>
"#,
        k0 = kind.0,
        k1 = kind.1,
        m0 = mode.0,
        m1 = mode.1,
        mz0 = mz.term().0,
        mz1 = mz.term().1,
        i0 = intensity.term().0,
        i1 = intensity.term().1,
        rows = spectra.len().div_ceil(3),
        n = spectra.len(),
    );
    std::fs::write(dir.join(format!("{stem}.ibd")), ibd).unwrap();
    let path = dir.join(format!("{stem}.imzML"));
    std::fs::write(&path, doc).unwrap();
    path
}

/// Low-mass 64-bit m/z as a ToF-SIMS or GC-EI spectrum has them (H, C, Na, K, then a sparse list):
/// neighbours more than a factor of two apart, which is where delta is not exact.
fn sparse_low_mass(rng: &mut Lcg, n: usize) -> Vec<f64> {
    let mut mz = vec![1.00782503207, 12.0, 22.98976928, 38.96370668];
    mz.extend((4..n).map(|_| 40.0 + 900.0 * rng.next()));
    mz.sort_by(f64::total_cmp);
    mz
}

/// Intensities that no f32 holds exactly, with two zero runs when `zero_runs`.
fn intensities(rng: &mut Lcg, n: usize, zero_runs: bool) -> Vec<f64> {
    (0..n).map(|i| if zero_runs && ((n / 8..n / 3).contains(&i) || (n / 2..n / 2 + n / 6).contains(&i)) { 0.0 } else { 1e5 * rng.next() + 0.123456789 }).collect()
}

fn f32s(v: &[f64]) -> Vec<f32> {
    v.iter().map(|x| *x as f32).collect()
}

/// One facet of a point-layout archive as stored: the column types, and per spectrum index the
/// values at those types.
fn stored_points(archive: &Path, name: &str) -> (DataType, DataType, BTreeMap<u64, (Arr, Arr)>) {
    let reader = ParquetRecordBatchReaderBuilder::try_new(bytes::Bytes::from(member(archive, name))).unwrap().build().unwrap();
    let mut types = None;
    let mut out: BTreeMap<u64, (Arr, Arr)> = BTreeMap::new();
    let column = |c: &dyn Array| match c.data_type() {
        DataType::Float32 => Arr::F32(c.as_primitive::<Float32Type>().values().to_vec()),
        DataType::Float64 => Arr::F64(c.as_primitive::<Float64Type>().values().to_vec()),
        other => panic!("{name}: a {other} signal column"),
    };
    for batch in reader {
        let batch = batch.unwrap();
        let point = batch.column_by_name("point").unwrap_or_else(|| panic!("{name} is not in the point layout")).as_struct();
        let index = point.column_by_name("spectrum_index").unwrap().as_primitive::<UInt64Type>();
        let (mz, intensity) = (point.column_by_name("mz").unwrap(), point.column_by_name("intensity").unwrap());
        assert_eq!(mz.null_count() + intensity.null_count(), 0, "{name}: null signal values");
        types = Some((mz.data_type().clone(), intensity.data_type().clone()));
        let (mz, intensity) = (column(mz), column(intensity));
        for (row, i) in index.values().iter().enumerate() {
            let (m, t) = out.entry(*i).or_insert_with(|| match (&mz, &intensity) {
                (Arr::F32(_), Arr::F32(_)) => (Arr::F32(vec![]), Arr::F32(vec![])),
                (Arr::F32(_), Arr::F64(_)) => (Arr::F32(vec![]), Arr::F64(vec![])),
                (Arr::F64(_), Arr::F32(_)) => (Arr::F64(vec![]), Arr::F32(vec![])),
                (Arr::F64(_), Arr::F64(_)) => (Arr::F64(vec![]), Arr::F64(vec![])),
            });
            for (to, from) in [(m, &mz), (t, &intensity)] {
                match (to, from) {
                    (Arr::F32(to), Arr::F32(from)) => to.push(from[row]),
                    (Arr::F64(to), Arr::F64(from)) => to.push(from[row]),
                    _ => unreachable!(),
                }
            }
        }
    }
    let (mz, intensity) = types.unwrap_or_else(|| panic!("{name} holds no rows"));
    (mz, intensity, out)
}

/// Bit patterns, so `-0.0`, NaN payloads and the last bit all count.
fn bits(a: &Arr) -> Vec<u64> {
    match a {
        Arr::F32(v) => v.iter().map(|x| u64::from(x.to_bits())).collect(),
        Arr::F64(v) => v.iter().map(|x| x.to_bits()).collect(),
    }
}

/// `--lossless` on mzML-family input: the stored arrays ARE the source arrays, at the declared
/// binary types, for profile data with zero runs, sparse low-mass 64-bit m/z, 32-bit m/z and
/// centroid lists with 64-bit intensities — and the same files through the default lanes are not.
#[test]
fn lossless_is_bit_exact_against_the_source_arrays() {
    let dir = scratch("lossless");
    let mut rng = Lcg(7);
    let mut case = |tag: &str, profile: bool, mz32: bool, int32: bool| {
        let spectra: Vec<(Arr, Arr)> = (0..6)
            .map(|_| {
                let (mz, it) = (sparse_low_mass(&mut rng, 48), intensities(&mut rng, 48, profile));
                (if mz32 { Arr::F32(f32s(&mz)) } else { Arr::F64(mz) }, if int32 { Arr::F32(f32s(&it)) } else { Arr::F64(it) })
            })
            .collect();
        let input = write_imzml(&dir, tag, &spectra, profile, false);
        let out = dir.join(format!("{tag}.mzpeak"));
        convert(&input, &out, &["--lossless"]);

        let facet = if profile { "spectra_data" } else { "spectra_peaks" };
        let (mz_type, int_type, stored) = stored_points(&out, &format!("{facet}.parquet"));
        assert_eq!(mz_type, if mz32 { DataType::Float32 } else { DataType::Float64 }, "{tag}: m/z column type");
        assert_eq!(int_type, if int32 { DataType::Float32 } else { DataType::Float64 }, "{tag}: intensity column type");
        assert_eq!(stored.len(), spectra.len(), "{tag}: spectra with points");
        for (i, (mz, it)) in spectra.iter().enumerate() {
            let (smz, sit) = &stored[&(i as u64)];
            assert_eq!(bits(smz), bits(mz), "{tag}: m/z of spectrum {i}");
            assert_eq!(bits(sit), bits(it), "{tag}: intensity of spectrum {i}");
        }

        // What the archive says about itself.
        assert!(transformations(&out).iter().all(|t| t == "mzml:dangling-reference-dropped"), "{tag}: {:?}", transformations(&out));
        let f = metadata(&out)["fidelity"].clone();
        let n = spectra.iter().map(|(mz, _)| mz.len() as u64).sum::<u64>();
        assert_eq!(f[facet]["layout"], "point", "{tag}: {f}");
        assert_eq!((f[facet]["source_points"].as_u64(), f[facet]["stored_points"].as_u64()), (Some(n), Some(n)), "{tag}: {f}");
        assert_eq!(f[facet]["source_types"], serde_json::json!({"mz": [spectra[0].0.type_name()], "intensity": [spectra[0].1.type_name()]}), "{tag}");
        assert_eq!(f[facet]["stored_types"], serde_json::json!({"mz": spectra[0].0.type_name(), "intensity": spectra[0].1.type_name()}), "{tag}");
        assert_eq!(f["mz_error"], serde_json::json!([]), "{tag}");
        (input, spectra)
    };
    let (profile_input, profile) = case("profile_f64_f32", true, false, true);
    case("profile_f32_f32", true, true, true);
    let (centroid_input, centroid) = case("centroid_f64_f64", false, false, false);
    case("centroid_f32_f64", false, true, false);

    // The default point layout of the same inputs is what `--lossless` is for: the profile file
    // loses its zero runs, the centroid file its intensities' low bits (mzdata's peak set holds
    // them as f32).
    let out = dir.join("default.mzpeak");
    convert(&profile_input, &out, &["--layout", "point"]);
    let (_, _, stored) = stored_points(&out, "spectra_data.parquet");
    assert!(stored[&0].0.len() < profile[0].0.len(), "the default profile archive kept every point");
    convert(&centroid_input, &out, &["--layout", "point", "--force"]);
    let (_, _, stored) = stored_points(&out, "spectra_peaks.parquet");
    assert_ne!(stored[&0].1.as_f64(), centroid[0].1.as_f64(), "the default centroid archive kept 64-bit intensities");
    let _ = std::fs::remove_dir_all(&dir);
}

/// `--lossless` refuses what contradicts it, and every lane where it cannot be checked, before
/// anything is written; and when the written archive is not bit-exact (m/z out of order, which the
/// peak facet cannot store unsorted) the conversion fails and leaves nothing behind.
#[test]
fn lossless_refuses_rather_than_pretends() {
    let dir = scratch("refuse");
    let mut rng = Lcg(11);
    let mz = sparse_low_mass(&mut rng, 24);
    let spectra = vec![(Arr::F64(mz.clone()), Arr::F32(f32s(&intensities(&mut rng, 24, false)))); 3];
    let input = write_imzml(&dir, "sorted", &spectra, false, false);
    let out = dir.join("out.mzpeak");

    refused(&input, &out, &["--lossless", "--layout", "chunked"], "--lossless conflicts with --layout chunked");
    refused(&input, &out, &["--lossless", "--tof-grid", "auto"], "--lossless conflicts with --tof-grid auto|on");
    refused(&input, &dir.join("out.mzML"), &["--lossless"], "--lossless conflicts with an mzML output");
    let config = dir.join("profile.yaml");
    std::fs::write(&config, "layout: chunked\n").unwrap();
    refused(&input, &out, &["--lossless", "--config", config.to_str().unwrap()], "--lossless conflicts with --layout chunked");
    // Accepted combinations: flags that say the same thing.
    convert(&input, &out, &["--lossless", "--layout", "point", "--no-numpress", "--keep-zero-runs", "--tof-grid", "off"]);

    // A lane that is not the mzML/imzML one: an existing archive (the filter lane), and a timsTOF
    // directory (refused by its name alone, before any reader opens it).
    refused(&out, &dir.join("again.mzpeak"), &["--lossless"], "--lossless is not available on the .mzpeak filter lane");
    let tdf = dir.join("run.d");
    std::fs::create_dir_all(&tdf).unwrap();
    for f in ["analysis.tdf", "analysis.tdf_bin"] {
        std::fs::write(tdf.join(f), b"not a real one").unwrap();
    }
    refused(&tdf, &dir.join("tdf.mzpeak"), &["--lossless"], "--lossless is not available on the timsTOF ims-compact lane");
    std::fs::write(&config, "lossless: true\n").unwrap();
    refused(&tdf, &dir.join("tdf.mzpeak"), &["--config", config.to_str().unwrap()], "--lossless is not available");

    // Out-of-order m/z: stored sorted, declared `sort-by-mz`, so not the source's arrays.
    let mut unsorted = mz;
    unsorted.swap(3, 9);
    let spectra = vec![(Arr::F64(unsorted), spectra[0].1.clone()); 3];
    let input = write_imzml(&dir, "unsorted", &spectra, false, false);
    let out = dir.join("unsorted.mzpeak");
    refused(&input, &out, &["--lossless"], "the conversion declares sort-by-mz");
    convert(&input, &out, &[]);
    assert!(transformations(&out).contains(&"sort-by-mz".to_string()));
    let _ = std::fs::remove_dir_all(&dir);
}

/// Continuous-mode imaging data: nine pixels on one 32-bit m/z axis, each with its own zero runs.
/// By default the mask leaves every pixel a different subset of the axis and the pixels decode to
/// different axes; `--keep-zero-runs` (flag or environment) stores all points, declares no
/// `zero-run-mask`, and every pixel decodes to the same axis. Intensities are untouched either way.
#[test]
fn keep_zero_runs_stores_every_point_and_one_shared_axis() {
    let dir = scratch("keep");
    let mut rng = Lcg(3);
    const N: usize = 1200;
    // Not a decimal lattice (that would select delta): a TOF-like axis, quadratic in the bin.
    let axis: Vec<f32> = (0..N).map(|i| (10.0 + 0.0137 * i as f64).powi(2) as f32).collect();
    let spectra: Vec<(Arr, Arr)> = (0..9)
        .map(|p| {
            // Signal in a few windows that move from pixel to pixel; zero elsewhere.
            let it: Vec<f32> = (0..N).map(|i| if (i + 37 * p) % 300 < 60 { (1.0 + 1e4 * rng.next()) as f32 } else { 0.0 }).collect();
            (Arr::F32(axis.clone()), Arr::F32(it))
        })
        .collect();
    let input = write_imzml(&dir, "continuous", &spectra, true, true);
    let source_points = (9 * N) as u64;

    let decoded = |archive: &Path| -> Vec<(Vec<f64>, Vec<f32>)> {
        let mut reader = MzPeakReader::new(archive).unwrap();
        (0..9).map(|i| {
            let a = reader.get_spectrum_arrays(i).unwrap().expect("signal arrays");
            (a.mzs().unwrap().to_vec(), a.intensities().unwrap().to_vec())
        })
        .collect()
    };
    let distinct_axes = |spectra: &[(Vec<f64>, Vec<f32>)]| {
        let mut axes: Vec<Vec<u64>> = spectra.iter().map(|(mz, _)| mz.iter().map(|x| x.to_bits()).collect()).collect();
        axes.sort();
        axes.dedup();
        axes.len()
    };

    let masked = dir.join("masked.mzpeak");
    convert(&input, &masked, &[]);
    let f = metadata(&masked)["fidelity"]["spectra_data"].clone();
    let m = decoded(&masked);
    let stored: u64 = m.iter().map(|(mz, _)| mz.len() as u64).sum();
    assert!(transformations(&masked).contains(&"zero-run-mask".to_string()));
    assert_eq!((f["source_points"].as_u64(), f["stored_points"].as_u64()), (Some(source_points), Some(stored)), "{f}");
    assert!(stored < source_points && distinct_axes(&m) == 9, "masked: {stored} points, {} axes", distinct_axes(&m));
    // No non-zero point is among the dropped ones.
    for ((_, kept), (_, source)) in m.iter().zip(&spectra) {
        let Arr::F32(source) = source else { unreachable!() };
        let nonzero = |v: &[f32]| v.iter().filter(|x| **x != 0.0).map(|x| x.to_bits()).collect::<Vec<_>>();
        assert_eq!(nonzero(kept), nonzero(source));
    }

    for (tag, args, env) in [("flag", &["--keep-zero-runs"][..], false), ("env", &[][..], true)] {
        let out = dir.join(format!("keep-{tag}.mzpeak"));
        let r = run(&input, &out, args, env);
        assert!(r.status.success(), "{tag}: {}", String::from_utf8_lossy(&r.stderr));
        let applied = transformations(&out);
        assert!(!applied.contains(&"zero-run-mask".to_string()) && applied.contains(&"numpress-linear".to_string()), "{tag}: {applied:?}");
        let f = metadata(&out)["fidelity"]["spectra_data"].clone();
        assert_eq!((f["source_points"].as_u64(), f["stored_points"].as_u64()), (Some(source_points), Some(source_points)), "{tag}: {f}");
        let k = decoded(&out);
        assert_eq!(distinct_axes(&k), 1, "{tag}: the pixels decode to different axes");
        for (i, ((mz, it), (_, source))) in k.iter().zip(&spectra).enumerate() {
            assert_eq!(mz.len(), N, "{tag}: pixel {i}");
            // numpress moves a 32-bit m/z by less than half a float32 step: rounding restores it.
            assert_eq!(f32s(mz), axis, "{tag}: pixel {i} m/z, rounded to the source's 32 bits");
            assert_eq!(&Arr::F32(it.clone()), source, "{tag}: pixel {i} intensities");
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// The numpress entry of `fidelity.mz_error` is a bound that holds: every m/z of a default archive,
/// decoded by the reader, is within `max_abs_error` and `max_rel_error_ppm` of its source value,
/// while the bare half fixed-point step the bound is built on is smaller than what is recorded. The
/// block also states the types (64-bit m/z, 32-bit intensity as declared) and the points the mask
/// dropped.
#[test]
fn the_recorded_numpress_bound_holds_for_every_decoded_value() {
    let dir = scratch("bound");
    let mut rng = Lcg(5);
    const N: usize = 4000;
    let spectra: Vec<(Arr, Arr)> = (0..6)
        .map(|_| {
            let mut mz: Vec<f64> = (0..N).map(|_| 80.0 + 1400.0 * rng.next()).collect();
            mz.sort_by(f64::total_cmp);
            (Arr::F64(mz), Arr::F32(f32s(&intensities(&mut rng, N, true))))
        })
        .collect();
    let input = write_imzml(&dir, "profile", &spectra, true, false);
    let out = dir.join("default.mzpeak");
    convert(&input, &out, &[]);

    let meta = metadata(&out);
    let facet = &meta["fidelity"]["spectra_data"];
    assert_eq!(facet["layout"], "chunked");
    assert_eq!(facet["source_types"], serde_json::json!({"mz": ["float64"], "intensity": ["float32"]}));
    assert_eq!(facet["stored_types"], serde_json::json!({"mz": "float64", "intensity": "float32"}));
    let errors = meta["fidelity"]["mz_error"].as_array().unwrap();
    assert_eq!(errors.len(), 1, "{errors:?}");
    let e = &errors[0];
    assert_eq!((&e["encoding"], &e["facet"], &e["basis"]), (&"numpress-linear".into(), &"spectra_data".into(), &"bound".into()));
    let (abs, rel_ppm, fixed_point) = (e["max_abs_error"].as_f64().unwrap(), e["max_rel_error_ppm"].as_f64().unwrap(), e["min_fixed_point"].as_f64().unwrap());
    assert!(abs > 0.5 / fixed_point && abs < 0.5 / fixed_point * 1.001, "bound {abs:e} against the bare half step {:e}", 0.5 / fixed_point);

    let mut reader = MzPeakReader::new(&out).unwrap();
    let (mut stored, mut worst_abs, mut worst_rel) = (0u64, 0.0f64, 0.0f64);
    for (i, (mz, _)) in spectra.iter().enumerate() {
        let source = mz.as_f64();
        let arrays = reader.get_spectrum_arrays(i as u64).unwrap().expect("signal arrays");
        let decoded = arrays.mzs().unwrap();
        stored += decoded.len() as u64;
        for d in decoded.iter() {
            // The stored points are a subset of the source's: the nearest source value is the one.
            let at = source.partition_point(|s| s < d);
            let nearest = [at.checked_sub(1), Some(at)].into_iter().flatten().filter_map(|j| source.get(j)).map(|s| (s - d).abs()).fold(f64::INFINITY, f64::min);
            assert!(nearest <= abs, "spectrum {i}: {d} is {nearest:e} from its source, above the recorded {abs:e}");
            assert!(nearest / d * 1e6 <= rel_ppm, "spectrum {i}: {d} is {:e} ppm off, above the recorded {rel_ppm:e}", nearest / d * 1e6);
            worst_abs = worst_abs.max(nearest);
            worst_rel = worst_rel.max(nearest / d * 1e6);
        }
    }
    assert!(worst_abs > 0.5 * abs, "the bound is loose: worst decoded error {worst_abs:e} against {abs:e}");
    assert!(worst_rel > 0.0);
    let source_points = (6 * N) as u64;
    assert_eq!((facet["source_points"].as_u64(), facet["stored_points"].as_u64()), (Some(source_points), Some(stored)));
    assert!(stored < source_points, "the mask dropped nothing");
    let _ = std::fs::remove_dir_all(&dir);
}

/// `--no-numpress` is delta, and delta is not bit-exact where a 64-bit m/z is more than twice its
/// predecessor: the archive decodes at least one m/z a unit in the last place off, declares no
/// transformation for it, and `fidelity.mz_error` names the chunks it can happen in. The same
/// values as 32-bit floats are exact and get no entry.
#[test]
fn delta_on_sparse_64_bit_mz_is_off_by_an_ulp_and_the_block_says_so() {
    let dir = scratch("delta");
    // Pairs (b, a) with a > 2b for which `b + (a - b) != a` in f64: the first two m/z of each spectrum.
    let mut rng = Lcg(9);
    let rounding: Vec<(f64, f64)> = (0..100_000)
        .map(|_| {
            let b = 1.0 + 9.0 * rng.next();
            (b, b * (2.2 + 2.0 * rng.next()))
        })
        .filter(|(b, a)| b + (a - b) != *a)
        .take(6)
        .collect();
    assert_eq!(rounding.len(), 6, "no pair rounds: the premise of this test is gone");
    let build = |as32: bool, rng: &mut Lcg| -> Vec<(Arr, Arr)> {
        rounding
            .iter()
            .map(|(b, a)| {
                let mut mz = vec![*b, *a];
                mz.extend((0..30).map(|i| 60.0 + 25.0 * f64::from(i) + rng.next()));
                let it = f32s(&intensities(rng, mz.len(), false));
                (if as32 { Arr::F32(f32s(&mz)) } else { Arr::F64(mz) }, Arr::F32(it))
            })
            .collect()
    };
    let decoded = |archive: &Path, n: usize| -> Vec<Vec<f64>> {
        let mut reader = MzPeakReader::new(archive).unwrap();
        (0..n as u64).map(|i| reader.get_spectrum_arrays(i).unwrap().expect("signal arrays").mzs().unwrap().to_vec()).collect()
    };

    let spectra = build(false, &mut rng);
    let input = write_imzml(&dir, "f64", &spectra, true, false);
    let out = dir.join("f64.mzpeak");
    convert(&input, &out, &["--no-numpress"]);
    let off: usize = decoded(&out, spectra.len()).iter().zip(&spectra).map(|(d, (mz, _))| d.iter().zip(mz.as_f64()).filter(|(x, y)| x.to_bits() != y.to_bits()).count()).sum();
    assert!(off >= 6, "delta decoded every 64-bit m/z exactly ({off} off)");
    assert!(transformations(&out).iter().all(|t| t == "mzml:dangling-reference-dropped"), "{:?}", transformations(&out));
    let errors = metadata(&out)["fidelity"]["mz_error"].clone();
    let e = &errors[0];
    assert_eq!((errors.as_array().unwrap().len(), &e["encoding"], &e["basis"]), (1, &"delta".into(), &"not measured".into()), "{errors}");
    assert!(e["chunks_not_exact_by_construction"].as_u64().unwrap() >= 6 && e["max_abs_error"].is_null(), "{e}");
    let at_risk = e["largest_mz_at_risk"].as_f64().unwrap();
    assert!(at_risk > 2.0 && at_risk < 60.0 && e["ulp_there"].as_f64().unwrap() < 1e-14, "{e}");

    // `--lossless` on the same file: every one of them back.
    let exact = dir.join("f64-lossless.mzpeak");
    convert(&input, &exact, &["--lossless"]);
    let (_, _, stored) = stored_points(&exact, "spectra_data.parquet");
    for (i, (mz, _)) in spectra.iter().enumerate() {
        assert_eq!(bits(&stored[&(i as u64)].0), bits(mz), "spectrum {i}");
    }

    // 32-bit m/z: delta is exact, and the block has nothing to report.
    let spectra = build(true, &mut rng);
    let input = write_imzml(&dir, "f32", &spectra, true, false);
    let out = dir.join("f32.mzpeak");
    convert(&input, &out, &["--no-numpress"]);
    for (d, (mz, _)) in decoded(&out, spectra.len()).iter().zip(&spectra) {
        assert_eq!(d, &mz.as_f64());
    }
    assert_eq!(metadata(&out)["fidelity"]["mz_error"], serde_json::json!([]));
    let _ = std::fs::remove_dir_all(&dir);
}

/// The `.mzpeak` → `.mzpeak` filter: a run that keeps every spectrum carries the block as it is;
/// one that removes spectra leaves it out (its counts describe the source archive) and says so.
#[test]
fn the_filter_lane_carries_the_block_or_drops_it() {
    let dir = scratch("filter");
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tiny.pwiz.1.1.mzML");
    let src = dir.join("src.mzpeak");
    convert(&fixture, &src, &[]);
    let block = metadata(&src)["fidelity"].clone();
    assert!(block["spectra_data"]["stored_points"].as_u64().unwrap() > 0 && block["spectra_peaks"]["stored_points"].as_u64().unwrap() > 0, "{block}");

    let all = dir.join("all.mzpeak");
    convert(&src, &all, &["--rt", "0-100000"]);
    let meta = metadata(&all);
    assert_eq!(meta["fidelity"], block, "a filter that kept every spectrum changed the block");
    assert!(meta["filter"].get("dropped_index_blocks").is_none(), "{}", meta["filter"]);

    let ms1 = dir.join("ms1.mzpeak");
    convert(&src, &ms1, &["--ms-level", "1"]);
    let meta = metadata(&ms1);
    assert!(meta.get("fidelity").is_none(), "the block survived a filter that removed spectra: {}", meta["fidelity"]);
    assert_eq!(meta["filter"]["dropped_index_blocks"], serde_json::json!(["fidelity"]));
    let _ = std::fs::remove_dir_all(&dir);
}

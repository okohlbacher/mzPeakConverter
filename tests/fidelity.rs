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
//! * `--no-numpress` (delta) is NOT bit-exact on sparse 64-bit m/z, at any mass, the block bounds
//!   the error at one unit in the last place, and `transformations` declares it (`delta-ulp`);
//! * a centroid intensity no float32 holds is stored as the nearest float32 on the default lanes,
//!   declared (`intensity-f32-rounding`) and counted, also where the column is a float64; and so
//!   is every intensity cast into a column of another type than its own, by what the column holds;
//! * a facet whose m/z are grid indices says so in `stored_types`, and the grid fit's tolerance
//!   holds for every decoded value;
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

/// `mzpeak-convert <input> -o <output> -q <args…>` under `envs`, with neither
/// `MZPC_KEEP_ZERO_RUNS` nor `MZPC_MAX_SPECTRA` inherited.
fn run_with(input: &Path, output: &Path, args: &[&str], envs: &[(&str, &str)]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_mzpeak-convert"));
    cmd.arg(input).arg("-o").arg(output).arg("-q").args(args).env_remove("MZPC_KEEP_ZERO_RUNS").env_remove("MZPC_MAX_SPECTRA");
    cmd.envs(envs.iter().copied()).output().expect("failed to run mzpeak-convert")
}

/// [`run_with`], with `MZPC_KEEP_ZERO_RUNS` set when `env`.
fn run(input: &Path, output: &Path, args: &[&str], env: bool) -> Output {
    run_with(input, output, args, if env { &[("MZPC_KEEP_ZERO_RUNS", "1")] } else { &[] })
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

/// The `transformation` userParams of the conversion's own processing method: the list
/// `transformations` holds, as a reader of `data_processing_method_list` alone sees it.
fn mirrored_transformations(archive: &Path) -> Vec<String> {
    metadata(archive)["data_processing_method_list"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|dp| dp["id"] == "mzpeak_convert_conversion")
        .flat_map(|dp| dp["methods"].as_array().unwrap().iter())
        .flat_map(|m| m["parameters"].as_array().unwrap().iter())
        .filter(|p| p["name"] == "transformation")
        .map(|p| p["value"].as_str().unwrap().to_string())
        .collect()
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

    // A cap on the spectra written (`MZPC_MAX_SPECTRA`): the archive would hold a part of the
    // input and pass the check on that part. Refused whether the cap bites (2 of 3) or not; the
    // same capped run without `--lossless` is written, and marked partial.
    let capped = dir.join("capped.mzpeak");
    for cap in ["2", "300"] {
        let r = run_with(&input, &capped, &["--lossless"], &[("MZPC_MAX_SPECTRA", cap)]);
        let err = String::from_utf8_lossy(&r.stderr);
        assert!(!r.status.success() && err.contains(&format!("--lossless conflicts with MZPC_MAX_SPECTRA={cap}")), "cap {cap}: {err}");
        assert!(!capped.exists() && !capped.with_extension("mzpeak.tmp").exists(), "cap {cap} left a file behind");
    }
    let r = run_with(&input, &capped, &["--layout", "point"], &[("MZPC_MAX_SPECTRA", "2")]);
    assert!(r.status.success(), "{}", String::from_utf8_lossy(&r.stderr));
    assert_eq!(metadata(&capped)["partial"]["spectra_written"], 2);

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
/// Since 0.17.0 the header's `IMS:1000030` is enough (owner decision D2): the default stores all
/// points, declares no `zero-run-mask`, states `storage_mode: continuous` and a verified
/// `shared_mz_axis`, and every pixel decodes to the same axis — what `--keep-zero-runs` (flag or
/// environment) did before and still does. The same spectra written in processed mode are masked by
/// default (every pixel a different subset of the axis, different decoded axes), `--keep-zero-runs`
/// the override. Intensities are untouched either way.
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
    let continuous = write_imzml(&dir, "continuous", &spectra, true, true);
    let processed = write_imzml(&dir, "processed", &spectra, true, false);
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
    convert(&processed, &masked, &[]);
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
    let marker = metadata(&masked)["imaging"].clone();
    assert_eq!(marker["storage_mode"], "processed", "{marker}");
    assert!(marker.get("shared_mz_axis").is_none(), "no shared-axis claim for processed mode: {marker}");

    for (tag, input, args, env) in [
        ("continuous-default", &continuous, &[][..], false),
        ("continuous-flag", &continuous, &["--keep-zero-runs"][..], false),
        ("continuous-env", &continuous, &[][..], true),
        ("processed-flag", &processed, &["--keep-zero-runs"][..], false),
    ] {
        let out = dir.join(format!("keep-{tag}.mzpeak"));
        let r = run(input, &out, args, env);
        assert!(r.status.success(), "{tag}: {}", String::from_utf8_lossy(&r.stderr));
        let applied = transformations(&out);
        assert!(!applied.contains(&"zero-run-mask".to_string()) && applied.contains(&"numpress-linear".to_string()), "{tag}: {applied:?}");
        let f = metadata(&out)["fidelity"]["spectra_data"].clone();
        assert_eq!((f["source_points"].as_u64(), f["stored_points"].as_u64()), (Some(source_points), Some(source_points)), "{tag}: {f}");
        let marker = metadata(&out)["imaging"].clone();
        if std::ptr::eq(input, &continuous) {
            assert_eq!((marker["storage_mode"].as_str(), marker["shared_mz_axis"].as_bool()), (Some("continuous"), Some(true)), "{tag}: {marker}");
        } else {
            assert_eq!(marker["storage_mode"], "processed", "{tag}: {marker}");
        }
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
/// predecessor in the same chunk, at low mass and at high: the archive decodes such m/z a unit in
/// the last place off (and carries the error into the values after it), declares no transformation
/// for it, and `fidelity.mz_error` counts the chunks it can happen in and bounds the error — every
/// decoded m/z is within `max_abs_error` and `max_rel_error_ppm` of its source. The same values as
/// 32-bit floats are exact and get no entry.
#[test]
fn delta_on_sparse_64_bit_mz_is_off_by_an_ulp_and_the_block_bounds_it() {
    let dir = scratch("delta");
    // Pairs (b, a) with a > 2b for which `b + (a - b) != a` in f64, the first two m/z of each
    // spectrum: six below m/z 50 (ToF-SIMS, GC-EI) and six between m/z 600 and 6,300 (a sparse
    // centroid list), where the chunk goes on past `a` and the values after it inherit its error.
    let mut rng = Lcg(9);
    let mut pairs = |lo: f64, hi: f64| -> Vec<(f64, f64)> {
        let found: Vec<(f64, f64)> = (0..100_000)
            .map(|_| {
                let b = lo + (hi - lo) * rng.next();
                (b, b * (2.2 + 2.0 * rng.next()))
            })
            .filter(|(b, a)| b + (a - b) != *a)
            .take(6)
            .collect();
        assert_eq!(found.len(), 6, "no pair rounds: the premise of this test is gone");
        found
    };
    let rounding: Vec<(f64, f64)> = pairs(1.0, 10.0).into_iter().chain(pairs(300.0, 1500.0)).collect();
    let build = |as32: bool, rng: &mut Lcg| -> Vec<(Arr, Arr)> {
        rounding
            .iter()
            .map(|(b, a)| {
                let mut mz = vec![*b, *a];
                mz.extend((1..=30).map(|i| a.max(35.0) + 25.0 * f64::from(i) + rng.next()));
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
    // The value change is declared, in `transformations` and in the processing method that mirrors
    // it: through 0.17.0-rc.1 the list was empty while `mz_error` reported the ulp.
    let declared = transformations(&out);
    assert!(declared.contains(&"delta-ulp".to_string()) && declared.iter().all(|t| t == "delta-ulp" || t == "mzml:dangling-reference-dropped"), "{declared:?}");
    assert_eq!(mirrored_transformations(&out), declared);
    let errors = metadata(&out)["fidelity"]["mz_error"].clone();
    let e = &errors[0];
    assert_eq!((errors.as_array().unwrap().len(), &e["encoding"], &e["facet"], &e["basis"]), (1, &"delta".into(), &"spectra_data".into(), &"bound".into()), "{errors}");
    assert!(e["chunks_not_exact_by_construction"].as_u64().unwrap() >= 12, "{e}");
    let (abs, rel_ppm, at_risk) = (e["max_abs_error"].as_f64().unwrap(), e["max_rel_error_ppm"].as_f64().unwrap(), e["largest_mz_at_risk"].as_f64().unwrap());
    // One unit in the last place: of the largest m/z at risk, and at most 2^-52 of any value.
    assert!(at_risk > 600.0 && at_risk < 8000.0 && abs >= at_risk * f64::EPSILON / 2.0 && abs <= at_risk * f64::EPSILON * 1.00001, "{e}");
    assert!(rel_ppm >= f64::EPSILON * 1e6 && rel_ppm < f64::EPSILON * 1e6 * 1.00001, "{e}");

    let (mut off_low, mut off_high, mut worst_abs, mut worst_rel) = (0usize, 0usize, 0.0f64, 0.0f64);
    for (i, (d, (mz, _))) in decoded(&out, spectra.len()).iter().zip(&spectra).enumerate() {
        let source = mz.as_f64();
        assert_eq!(d.len(), source.len(), "spectrum {i}");
        for (x, y) in d.iter().zip(&source) {
            let err = (x - y).abs();
            assert!(err <= abs, "spectrum {i}: {y} decoded as {x}, {err:e} off, above the recorded {abs:e}");
            assert!(err / y * 1e6 <= rel_ppm, "spectrum {i}: {y} decoded as {x}, {:e} ppm off, above the recorded {rel_ppm:e}", err / y * 1e6);
            off_low += usize::from(err > 0.0 && *y < 50.0);
            off_high += usize::from(err > 0.0 && *y > 600.0);
            worst_abs = worst_abs.max(err);
            worst_rel = worst_rel.max(err / y * 1e6);
        }
    }
    assert!(off_low >= 6, "delta decoded the low-mass 64-bit m/z exactly ({off_low} off)");
    assert!(off_high > 6, "delta decoded the m/z above 600 exactly, or carried no error on ({off_high} off)");
    assert!(worst_abs * 2.0 >= abs && worst_rel * 2.0 >= rel_ppm, "the bound is loose: worst {worst_abs:e} of {abs:e}, {worst_rel:e} of {rel_ppm:e} ppm");

    // `--lossless` on the same file: every one of them back.
    let exact = dir.join("f64-lossless.mzpeak");
    convert(&input, &exact, &["--lossless"]);
    let (_, _, stored) = stored_points(&exact, "spectra_data.parquet");
    for (i, (mz, _)) in spectra.iter().enumerate() {
        assert_eq!(bits(&stored[&(i as u64)].0), bits(mz), "spectrum {i}");
    }

    // 32-bit m/z: delta is exact, and neither the block nor `transformations` has anything to report.
    let spectra = build(true, &mut rng);
    let input = write_imzml(&dir, "f32", &spectra, true, false);
    let out = dir.join("f32.mzpeak");
    convert(&input, &out, &["--no-numpress"]);
    for (d, (mz, _)) in decoded(&out, spectra.len()).iter().zip(&spectra) {
        assert_eq!(d, &mz.as_f64());
    }
    assert_eq!(metadata(&out)["fidelity"]["mz_error"], serde_json::json!([]));
    assert!(!transformations(&out).contains(&"delta-ulp".to_string()), "{:?}", transformations(&out));

    // Dense 64-bit m/z (every chunk within a factor of two): delta chunks, none at risk, no entry.
    let dense: Vec<(Arr, Arr)> = (0..4)
        .map(|_| {
            let mz: Vec<f64> = (0..200).map(|i| 400.0 + 0.37 * f64::from(i) + 0.1 * rng.next()).collect();
            let it = f32s(&intensities(&mut rng, mz.len(), false));
            (Arr::F64(mz), Arr::F32(it))
        })
        .collect();
    let input = write_imzml(&dir, "dense", &dense, true, false);
    let out = dir.join("dense.mzpeak");
    convert(&input, &out, &["--no-numpress"]);
    assert_eq!(metadata(&out)["fidelity"]["mz_error"], serde_json::json!([]));
    assert!(!transformations(&out).contains(&"delta-ulp".to_string()), "{:?}", transformations(&out));
    let _ = std::fs::remove_dir_all(&dir);
}

/// The intensity column of one facet in row order (spectrum by spectrum, each in m/z order), as
/// f64, with its value type: the list items of a chunked facet, the column of a point facet.
fn stored_intensities(archive: &Path, name: &str) -> (DataType, Vec<f64>) {
    use arrow::datatypes::{Int32Type, Int64Type};
    let reader = ParquetRecordBatchReaderBuilder::try_new(bytes::Bytes::from(member(archive, name))).unwrap().build().unwrap();
    let (mut dtype, mut out) = (None, Vec::new());
    for batch in reader {
        let batch = batch.unwrap();
        let values = match (batch.column_by_name("chunk"), batch.column_by_name("point")) {
            (Some(chunk), _) => chunk.as_struct().column_by_name("intensity").unwrap().as_list::<i64>().values().clone(),
            (_, Some(point)) => point.as_struct().column_by_name("intensity").unwrap().clone(),
            _ => panic!("{name} is neither chunked nor in the point layout"),
        };
        assert_eq!(values.null_count(), 0, "{name}: null intensities");
        dtype = Some(values.data_type().clone());
        match values.data_type() {
            DataType::Float32 => out.extend(values.as_primitive::<Float32Type>().values().iter().map(|x| f64::from(*x))),
            DataType::Float64 => out.extend(values.as_primitive::<Float64Type>().values().iter().copied()),
            DataType::Int32 => out.extend(values.as_primitive::<Int32Type>().values().iter().map(|x| f64::from(*x))),
            DataType::Int64 => out.extend(values.as_primitive::<Int64Type>().values().iter().map(|x| *x as f64)),
            other => panic!("{name}: a {other} intensity column"),
        }
    }
    (dtype.unwrap_or_else(|| panic!("{name} holds no rows")), out)
}

/// A centroid spectrum's intensities reach the peak facet through mzdata's peak set, which holds
/// them as float32 whatever the file declares. Where a source value is no float32 the stored one
/// is another number: the conversion says so (`intensity-f32-rounding`, mirrored into the
/// processing method), the facet's `intensity_values_rounded` counts the values, and a warning
/// gives the count. Through 0.17.0-rc.1 `transformations` was empty for it, and where the peak
/// facet's column is a float64 (profile spectra first in the file: the schema is sampled from
/// them) the block showed nothing either — float64 in, float64 out, every value rounded.
#[test]
fn centroid_intensities_no_float32_holds_are_declared_and_counted() {
    let dir = scratch("f32-rounding");
    let mut rng = Lcg(77);
    let mz = |rng: &mut Lcg, n: usize| -> Vec<f64> {
        let mut mz: Vec<f64> = (0..n).map(|_| 100.0 + 900.0 * rng.next()).collect();
        mz.sort_by(f64::total_cmp);
        mz
    };
    // 64-bit intensities: four per spectrum that a float32 holds, the rest that none does.
    let spectra: Vec<(Arr, Arr)> = (0..6)
        .map(|_| {
            let m = mz(&mut rng, 40);
            let mut it = intensities(&mut rng, m.len(), false);
            it[..4].copy_from_slice(&[1.0, 1024.5, 16777216.0, 0.25]);
            (Arr::F64(m), Arr::F64(it))
        })
        .collect();
    let source: Vec<f64> = spectra.iter().flat_map(|(_, it)| it.as_f64()).collect();
    let not_f32 = source.iter().filter(|x| f64::from(**x as f32) != **x).count() as u64;
    assert_eq!(not_f32, 6 * 36, "the premise: 36 of each spectrum's 40 intensities are not float32 values");
    let input = write_imzml(&dir, "wide", &spectra, false, false);

    for (args, layout) in [(&[][..], "chunked"), (&["--no-numpress"][..], "chunked"), (&["--layout", "point"][..], "point")] {
        let out = dir.join(format!("wide-{}.mzpeak", args.join("")));
        let r = run(&input, &out, args, false);
        assert!(r.status.success(), "{args:?}: {}", String::from_utf8_lossy(&r.stderr));
        let declared = transformations(&out);
        assert!(declared.contains(&"intensity-f32-rounding".to_string()), "{args:?}: {declared:?}");
        assert_eq!(mirrored_transformations(&out), declared, "{args:?}");
        let facet = metadata(&out)["fidelity"]["spectra_peaks"].clone();
        assert_eq!((&facet["layout"], facet["intensity_values_rounded"].as_u64()), (&layout.into(), Some(not_f32)), "{args:?}: {facet}");
        assert_eq!((&facet["source_types"]["intensity"], &facet["stored_types"]["intensity"]), (&serde_json::json!(["float64"]), &"float32".into()), "{facet}");
        // What is stored: the float32 nearest each source value, which is what the entry says.
        if layout == "chunked" {
            let (dtype, stored) = stored_intensities(&out, "spectra_peaks.parquet");
            assert_eq!(dtype, DataType::Float32);
            assert_eq!(stored, source.iter().map(|x| f64::from(*x as f32)).collect::<Vec<_>>(), "{args:?}");
        }
    }

    // The count goes to the run's warning (without `-q`).
    let loud = Command::new(env!("CARGO_BIN_EXE_mzpeak-convert"))
        .arg(&input)
        .arg("-o")
        .arg(dir.join("wide-loud.mzpeak"))
        .env_remove("MZPC_KEEP_ZERO_RUNS")
        .env_remove("MZPC_MAX_SPECTRA")
        .output()
        .expect("failed to run mzpeak-convert");
    let err = String::from_utf8_lossy(&loud.stderr);
    assert!(loud.status.success() && err.contains(&format!("{not_f32} intensities")) && err.contains("intensity-f32-rounding"), "{err}");

    // `--lossless` stores the source's type and declares nothing.
    let exact = dir.join("wide-lossless.mzpeak");
    convert(&input, &exact, &["--lossless"]);
    assert!(!transformations(&exact).contains(&"intensity-f32-rounding".to_string()));
    let facet = metadata(&exact)["fidelity"]["spectra_peaks"].clone();
    assert!(facet.get("intensity_values_rounded").is_none() && facet["stored_types"]["intensity"] == "float64", "{facet}");

    // 64-bit intensities that are all float32 values (what the corpus's 14 such archives hold),
    // and 32-bit ones: nothing is changed and nothing is declared.
    for (stem, as64) in [("representable", true), ("narrow", false)] {
        let spectra: Vec<(Arr, Arr)> = (0..4)
            .map(|_| {
                let m = mz(&mut rng, 40);
                let it = f32s(&intensities(&mut rng, m.len(), false));
                (Arr::F64(m), if as64 { Arr::F64(it.iter().map(|x| f64::from(*x)).collect()) } else { Arr::F32(it) })
            })
            .collect();
        let input = write_imzml(&dir, stem, &spectra, false, false);
        let out = dir.join(format!("{stem}.mzpeak"));
        convert(&input, &out, &[]);
        assert!(!transformations(&out).contains(&"intensity-f32-rounding".to_string()), "{stem}: {:?}", transformations(&out));
        assert!(metadata(&out)["fidelity"]["spectra_peaks"].get("intensity_values_rounded").is_none(), "{stem}");
    }

    // Profile spectra first, centroid ones after, all with 64-bit intensities, as an mzML: the
    // peak facet's intensity column is a float64 (sampled from the profile arrays) and holds the
    // float32-rounded values of the centroid spectra. The block alone reads float64 → float64.
    let mixed = dir.join("mixed.mzML");
    let profile: Vec<(Vec<f64>, Vec<f64>)> = (0..3).map(|_| ((0..300).map(|i| 200.0 + 0.01 * f64::from(i)).collect(), intensities(&mut rng, 300, false))).collect();
    let centroid: Vec<(Vec<f64>, Vec<f64>)> = (0..3).map(|_| (mz(&mut rng, 50), intensities(&mut rng, 50, false))).collect();
    write_mzml_f64(&mixed, &profile, &centroid);
    let out = dir.join("mixed.mzpeak");
    convert(&mixed, &out, &[]);
    let facet = metadata(&out)["fidelity"]["spectra_peaks"].clone();
    let rounded = centroid.iter().flat_map(|(_, it)| it).filter(|x| f64::from(**x as f32) != **x).count() as u64;
    assert_eq!(rounded, 150);
    assert_eq!((&facet["source_types"]["intensity"], &facet["stored_types"]["intensity"]), (&serde_json::json!(["float64"]), &"float64".into()), "{facet}");
    assert_eq!(facet["intensity_values_rounded"].as_u64(), Some(rounded), "{facet}");
    assert!(transformations(&out).contains(&"intensity-f32-rounding".to_string()), "{:?}", transformations(&out));
    let (dtype, stored) = stored_intensities(&out, "spectra_peaks.parquet");
    assert_eq!(dtype, DataType::Float64);
    assert_eq!(stored, centroid.iter().flat_map(|(_, it)| it.iter().map(|x| f64::from(*x as f32))).collect::<Vec<_>>());
    // The profile arrays keep their 64-bit values, and are not counted.
    assert!(metadata(&out)["fidelity"]["spectra_data"].get("intensity_values_rounded").is_none());
    let _ = std::fs::remove_dir_all(&dir);
}

/// An mzML of `profile` spectra followed by `centroid` ones, m/z and intensity both declared and
/// written as 64-bit floats.
fn write_mzml_f64(path: &Path, profile: &[(Vec<f64>, Vec<f64>)], centroid: &[(Vec<f64>, Vec<f64>)]) {
    let spectrum = |profile: bool, (mz, intensity): &(Vec<f64>, Vec<f64>)| MzmlSpectrum { profile, mz: mz.clone(), intensity: Intensity::F64(intensity.clone()), charge: false };
    let spectra: Vec<MzmlSpectrum> = profile.iter().map(|s| spectrum(true, s)).chain(centroid.iter().map(|s| spectrum(false, s))).collect();
    write_mzml(path, &spectra);
}

/// An intensity array at its declared binary type.
#[derive(Clone)]
enum Intensity {
    F32(Vec<f32>),
    F64(Vec<f64>),
    I32(Vec<i32>),
}

impl Intensity {
    fn as_f64(&self) -> Vec<f64> {
        match self {
            Intensity::F32(v) => v.iter().map(|x| f64::from(*x)).collect(),
            Intensity::F64(v) => v.clone(),
            Intensity::I32(v) => v.iter().map(|x| f64::from(*x)).collect(),
        }
    }
}

/// One spectrum of [`write_mzml`]: 64-bit m/z, the intensities at their own type, and a charge
/// array beside them when `charge` (a third per-peak array: the writer then stores the arrays,
/// not mzdata's peak set).
struct MzmlSpectrum {
    profile: bool,
    mz: Vec<f64>,
    intensity: Intensity,
    charge: bool,
}

/// An mzML of `spectra` in order (mzdata's writer keeps each array's own type).
fn write_mzml(path: &Path, spectra: &[MzmlSpectrum]) {
    use mzdata::io::mzml::MzMLWriter;
    use mzdata::mzpeaks::{CentroidPeak, DeconvolutedPeak};
    use mzdata::prelude::*;
    use mzdata::spectrum::bindata::{ArrayType, BinaryDataArrayType, DataArray};
    use mzdata::spectrum::{BinaryArrayMap, MultiLayerSpectrum, SignalContinuity, SpectrumDescription};
    let mut w = MzMLWriter::new(std::fs::File::create(path).unwrap());
    w.set_spectrum_count(spectra.len() as u64);
    for (i, s) in spectra.iter().enumerate() {
        let mut arrays = BinaryArrayMap::new();
        let mut mz = DataArray::wrap(&ArrayType::MZArray, BinaryDataArrayType::Float64, Vec::new());
        mz.update_buffer(&s.mz).unwrap();
        arrays.add(mz);
        let intensity = match &s.intensity {
            Intensity::F32(v) => {
                let mut a = DataArray::wrap(&ArrayType::IntensityArray, BinaryDataArrayType::Float32, Vec::new());
                a.update_buffer(v).unwrap();
                a
            }
            Intensity::F64(v) => {
                let mut a = DataArray::wrap(&ArrayType::IntensityArray, BinaryDataArrayType::Float64, Vec::new());
                a.update_buffer(v).unwrap();
                a
            }
            Intensity::I32(v) => {
                let mut a = DataArray::wrap(&ArrayType::IntensityArray, BinaryDataArrayType::Int32, Vec::new());
                a.update_buffer(v).unwrap();
                a
            }
        };
        arrays.add(intensity);
        if s.charge {
            let mut a = DataArray::wrap(&ArrayType::ChargeArray, BinaryDataArrayType::Int32, Vec::new());
            a.update_buffer(&vec![2i32; s.mz.len()]).unwrap();
            arrays.add(a);
        }
        let continuity = if s.profile { SignalContinuity::Profile } else { SignalContinuity::Centroid };
        let description = SpectrumDescription { index: i, id: format!("scan={}", i + 1), ms_level: 1, signal_continuity: continuity, ..Default::default() };
        let spectrum: MultiLayerSpectrum<CentroidPeak, DeconvolutedPeak> = MultiLayerSpectrum::new(description, Some(arrays), None, None);
        w.write(&spectrum).unwrap();
    }
    w.close().unwrap();
}

/// A facet's intensity column has ONE type, fixed from the spectra the writer samples before the
/// first is written, and a spectrum whose array is of another type is cast into it. The first
/// version of the `intensity-f32-rounding` count took every array the writer stores as it is
/// (profile signal, a centroid spectrum with a third per-peak array) for exact, which holds only
/// in a column of the array's own type: a file whose first spectra carry 32-bit intensities and
/// later ones 64-bit had the later ones rounded with nothing declared, or too few counted.
///
/// Whatever the file mixes and whichever layout stores it, the block's counts are the number of
/// stored intensities that differ from the source's, read straight from the facet:
/// `intensity_values_rounded` where they went through or into a float32 (`intensity-f32-rounding`),
/// `intensity_values_narrowed` where an integer column cut them (`intensity-type-narrowing`).
#[test]
fn intensities_cast_into_a_column_of_another_type_are_declared_and_counted() {
    let dir = scratch("column-cast");
    let mut rng = Lcg(404);
    let centroid_mz = |rng: &mut Lcg| -> Vec<f64> {
        let mut mz: Vec<f64> = (0..40).map(|_| 100.0 + 900.0 * rng.next()).collect();
        mz.sort_by(f64::total_cmp);
        mz
    };
    let profile_mz = || -> Vec<f64> { (0..300).map(|i| 200.0 + 0.01 * f64::from(i)).collect() };
    // No float32 holds these, and none is an integer.
    let wide = |rng: &mut Lcg, n: usize| Intensity::F64(intensities(rng, n, false));
    let narrow = |rng: &mut Lcg, n: usize| Intensity::F32(f32s(&intensities(rng, n, false)));
    let counts = |rng: &mut Lcg, n: usize| Intensity::I32((0..n).map(|_| 1 + (1000.0 * rng.next()) as i32).collect());
    let not_f32 = |spectra: &[MzmlSpectrum]| spectra.iter().flat_map(|s| s.intensity.as_f64()).filter(|x| f64::from(*x as f32) != *x).count() as u64;
    let n = 3;

    // (name, the spectra, the facet they all go to, and under the default layout: the column's
    // type, the rounded and the narrowed count)
    let mut cases: Vec<(&str, Vec<MzmlSpectrum>, &str, DataType, u64, u64)> = Vec::new();
    // Plain 32-bit centroid spectra, then 64-bit ones with a charge array, whose arrays the
    // writer takes: a float32 column, the later spectra rounded. Declared nothing through rc.1
    // and in the first version of the count.
    let spectra: Vec<MzmlSpectrum> = (0..2 * n)
        .map(|i| {
            let mz = centroid_mz(&mut rng);
            let intensity = if i < n { narrow(&mut rng, mz.len()) } else { wide(&mut rng, mz.len()) };
            MzmlSpectrum { profile: false, mz, intensity, charge: i >= n }
        })
        .collect();
    let rounded = not_f32(&spectra[n..]);
    cases.push(("narrow-then-wide-with-charge", spectra, "spectra_peaks", DataType::Float32, rounded, 0));
    // Plain 64-bit centroid spectra (stored from the float32 peak set), then the same with a
    // charge array (cast into the float32 column): every value rounded. The first version counted
    // the first half only.
    let spectra: Vec<MzmlSpectrum> = (0..2 * n)
        .map(|i| {
            let mz = centroid_mz(&mut rng);
            let intensity = wide(&mut rng, mz.len());
            MzmlSpectrum { profile: false, mz, intensity, charge: i >= n }
        })
        .collect();
    let rounded = not_f32(&spectra);
    cases.push(("wide-then-wide-with-charge", spectra, "spectra_peaks", DataType::Float32, rounded, 0));
    // Profile spectra, 32-bit intensities first: the data facet's column is a float32 too.
    let spectra: Vec<MzmlSpectrum> = (0..2 * n)
        .map(|i| MzmlSpectrum { profile: true, mz: profile_mz(), intensity: if i < n { narrow(&mut rng, 300) } else { wide(&mut rng, 300) }, charge: false })
        .collect();
    let rounded = not_f32(&spectra[n..]);
    cases.push(("profile-narrow-then-wide", spectra, "spectra_data", DataType::Float32, rounded, 0));
    // Integer intensities first: an int32 column, and the floats that follow are cut to integers.
    let spectra: Vec<MzmlSpectrum> = (0..2 * n)
        .map(|i| MzmlSpectrum { profile: true, mz: profile_mz(), intensity: if i < n { counts(&mut rng, 300) } else { wide(&mut rng, 300) }, charge: false })
        .collect();
    let narrowed = (n * 300) as u64;
    cases.push(("profile-integer-then-float", spectra, "spectra_data", DataType::Int32, 0, narrowed));
    // 64-bit profile intensities first: a float64 column holds the 32-bit ones that follow.
    let spectra: Vec<MzmlSpectrum> = (0..2 * n)
        .map(|i| MzmlSpectrum { profile: true, mz: profile_mz(), intensity: if i < n { wide(&mut rng, 300) } else { narrow(&mut rng, 300) }, charge: false })
        .collect();
    cases.push(("profile-wide-then-narrow", spectra, "spectra_data", DataType::Float64, 0, 0));

    for (name, spectra, facet, column, rounded, narrowed) in &cases {
        assert!(*rounded + *narrowed > 0 || *column == DataType::Float64, "{name}: the premise");
        let input = dir.join(format!("{name}.mzML"));
        write_mzml(&input, spectra);
        let source: Vec<f64> = spectra.iter().flat_map(|s| s.intensity.as_f64()).collect();
        for layout in ["chunked", "point"] {
            let out = dir.join(format!("{name}-{layout}.mzpeak"));
            convert(&input, &out, &["--layout", layout]);
            let block = metadata(&out)["fidelity"].clone();
            let (dtype, stored) = stored_intensities(&out, &format!("{facet}.parquet"));
            assert_eq!(stored.len(), source.len(), "{name}, {layout}");
            // What the facet holds against what the file holds, value by value.
            let changed = stored.iter().zip(&source).filter(|(a, b)| a != b).count() as u64;
            let count = |key: &str| block[*facet][key].as_u64().unwrap_or(0);
            let (r, w) = (count("intensity_values_rounded"), count("intensity_values_narrowed"));
            assert_eq!(r + w, changed, "{name}, {layout}: {} stores {dtype}, {changed} of {} values changed; the block: {}", facet, source.len(), block[*facet]);
            let declared = transformations(&out);
            assert_eq!(declared.contains(&"intensity-f32-rounding".to_string()), r > 0, "{name}, {layout}: {declared:?}");
            assert_eq!(declared.contains(&"intensity-type-narrowing".to_string()), w > 0, "{name}, {layout}: {declared:?}");
            assert_eq!(mirrored_transformations(&out), declared, "{name}, {layout}");
            // A count is of what the stored type does: none under a type that holds every value.
            let other = if *facet == "spectra_data" { "spectra_peaks" } else { "spectra_data" };
            assert!(block[other].get("intensity_values_rounded").is_none() && block[other].get("intensity_values_narrowed").is_none(), "{name}: {block}");
            if layout == "chunked" {
                assert_eq!((&dtype, r, w), (column, *rounded, *narrowed), "{name}: {}", block[*facet]);
                if *column == DataType::Float32 {
                    assert_eq!(stored, source.iter().map(|x| f64::from(*x as f32)).collect::<Vec<_>>(), "{name}: the nearest float32 of each");
                }
            }
        }
    }

    // The warning names the count of each kind.
    let loud = |name: &str| {
        let r = Command::new(env!("CARGO_BIN_EXE_mzpeak-convert"))
            .arg(dir.join(format!("{name}.mzML")))
            .arg("-o")
            .arg(dir.join(format!("{name}-loud.mzpeak")))
            .env_remove("MZPC_KEEP_ZERO_RUNS")
            .env_remove("MZPC_MAX_SPECTRA")
            .output()
            .expect("failed to run mzpeak-convert");
        assert!(r.status.success());
        String::from_utf8_lossy(&r.stderr).to_string()
    };
    let err = loud("profile-narrow-then-wide");
    assert!(err.contains(&format!("{} intensities are stored as the nearest float32", cases[2].4)) && err.contains("intensity-f32-rounding"), "{err}");
    let err = loud("profile-integer-then-float");
    assert!(err.contains(&format!("{} intensities are stored in a column whose type does not hold", cases[3].5)) && err.contains("intensity-type-narrowing"), "{err}");

    // `--lossless` on the mixed-width file: the source's widest type, nothing changed or declared.
    let exact = dir.join("lossless.mzpeak");
    convert(&dir.join("narrow-then-wide-with-charge.mzML"), &exact, &["--lossless"]);
    let (dtype, stored) = stored_intensities(&exact, "spectra_peaks.parquet");
    let source: Vec<f64> = cases[0].1.iter().flat_map(|s| s.intensity.as_f64()).collect();
    assert_eq!((dtype, stored), (DataType::Float64, source));
    assert!(!transformations(&exact).iter().any(|t| t.starts_with("intensity-")), "{:?}", transformations(&exact));
    let _ = std::fs::remove_dir_all(&dir);
}

/// A facet whose m/z are grid indices says so: centroid m/z on a 1e-4 lattice take the fitted
/// linear grid by default, every row of the peaks facet is a grid row with an empty values column,
/// and `stored_types.mz` reads `grid:uint32`, not the type of the column that holds nothing. The
/// `grid-fit` entry carries its tolerance and the relative figure that follows from it, and both
/// hold for every decoded m/z. With `--no-mz-lattice` the same file is stored as 64-bit values.
#[test]
fn a_grid_facet_names_its_index_type_and_the_fit_tolerance_holds() {
    let dir = scratch("grid");
    let mut rng = Lcg(21);
    let spectra: Vec<(Arr, Arr)> = (0..8)
        .map(|_| {
            let mut mz: Vec<f64> = (0..300).map(|_| ((100.0 + 1800.0 * rng.next()) * 1e4).round() / 1e4).collect();
            mz.sort_by(f64::total_cmp);
            mz.dedup();
            let it = f32s(&intensities(&mut rng, mz.len(), false));
            (Arr::F64(mz), Arr::F32(it))
        })
        .collect();
    let input = write_imzml(&dir, "lattice", &spectra, false, false);
    let out = dir.join("grid.mzpeak");
    convert(&input, &out, &[]);
    assert!(transformations(&out).contains(&"grid-fit:1e-6Da".to_string()), "{:?}", transformations(&out));

    // Every row is a grid row, and the values column is empty.
    let reader = ParquetRecordBatchReaderBuilder::try_new(bytes::Bytes::from(member(&out, "spectra_peaks.parquet"))).unwrap().build().unwrap();
    let mut rows = 0;
    for batch in reader {
        let batch = batch.unwrap();
        let chunk = batch.column_by_name("chunk").expect("the peaks facet is chunked").as_struct();
        let (values, grid) = (chunk.column_by_name("mz_chunk_values").unwrap(), chunk.column_by_name("mz_grid").unwrap());
        assert_eq!((values.null_count(), grid.null_count()), (chunk.len(), 0), "a row holds m/z values, or no grid");
        let DataType::Struct(parts) = grid.data_type() else { panic!("mz_grid is not a struct") };
        let DataType::LargeList(item) = parts.iter().find(|p| p.name() == "indices").unwrap().data_type() else { panic!("no index list") };
        assert_eq!(item.data_type(), &DataType::UInt32);
        rows += chunk.len();
    }
    assert!(rows > 0);

    let meta = metadata(&out);
    let facet = &meta["fidelity"]["spectra_peaks"];
    assert_eq!(facet["source_types"]["mz"], serde_json::json!(["float64"]));
    assert_eq!(facet["stored_types"], serde_json::json!({"mz": "grid:uint32", "intensity": "float32"}), "{facet}");
    let errors = meta["fidelity"]["mz_error"].as_array().unwrap();
    assert_eq!(errors.len(), 1, "{errors:?}");
    let e = &errors[0];
    assert_eq!((&e["encoding"], &e["basis"], e["max_abs_error"].as_f64()), (&"grid-fit:1e-6Da".into(), &"tolerance".into(), Some(1e-6)), "{e}");
    let rel_ppm = e["max_rel_error_ppm"].as_f64().unwrap();
    let smallest = spectra.iter().map(|(mz, _)| mz.as_f64()[0]).fold(f64::INFINITY, f64::min);
    assert!(rel_ppm >= 1e-6 / smallest * 1e6 && rel_ppm < 1e-6 / smallest * 1e6 * 1.001, "{rel_ppm} against the tolerance over the smallest m/z {smallest}");

    let mut reader = MzPeakReader::new(&out).unwrap();
    let mut moved = 0usize;
    for (i, (mz, _)) in spectra.iter().enumerate() {
        let source = mz.as_f64();
        let arrays = reader.get_spectrum_peak_arrays_for(i as u64).unwrap().expect("peak arrays");
        let decoded = arrays.mzs().unwrap();
        assert_eq!(decoded.len(), source.len(), "spectrum {i}");
        for (x, y) in decoded.iter().zip(&source) {
            let err = (x - y).abs();
            assert!(err <= 1e-6 && err / y * 1e6 <= rel_ppm, "spectrum {i}: {y} decoded as {x}");
            moved += usize::from(err > 0.0);
        }
    }
    assert!(moved > 0, "the fitted grid gave every m/z back exactly: nothing to bound");

    // The lattice off: 64-bit values in the values column, and no grid entry.
    let plain = dir.join("plain.mzpeak");
    convert(&input, &plain, &["--no-mz-lattice"]);
    let meta = metadata(&plain);
    assert_eq!(meta["fidelity"]["spectra_peaks"]["stored_types"]["mz"], "float64");
    assert!(meta["fidelity"]["mz_error"].as_array().unwrap().iter().all(|e| !e["encoding"].as_str().unwrap().starts_with("grid-fit")));
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

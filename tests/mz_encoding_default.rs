//! The default m/z encoding of a chunked facet is decided from a sample of the run (owner decision
//! D1/D13 of 2026-10-01, principle P2: exact where it costs nothing), through the real binary on
//! imzML files this test writes itself, so the source arrays are known to the last bit:
//!
//! * every sampled m/z a 32-bit value: delta without a trial, exact, no `numpress-linear` and no
//!   `delta-ulp`, and the `encoding_prescan` block says why;
//! * a 64-bit sample: the sample is written under delta and under numpress-linear and the smaller
//!   arm is kept — numpress on a smooth profile axis (declared, bounded in `fidelity`), delta on
//!   values whose low mantissa bits are mostly zero, where its chunks at risk are counted in the
//!   block and `delta-ulp` is declared;
//! * `MZPC_ENCODING_PRESCAN=0` skips the rule (numpress, no block), and an explicit `--no-numpress`
//!   or `--layout point` is the user's choice, not a measured one (no block).
//!
//! Before 0.17.0 every chunked facet of these lanes was numpress-linear unless its m/z sat on a
//! decimal lattice: lossy, and 12–38 % larger than exact delta on 32-bit m/z (imzML: chilli,
//! LA-ESI, DESI, the Example files), 12–22 % larger on QC01, SZB8102938 and PXD009465 t04176.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use mzdata::prelude::*;
use mzpeak_prototyping::MzPeakReader;
use serde_json::Value;

fn scratch(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("mzpc-mz-encoding-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// `mzpeak-convert <input> -o <output> -q <args…>` under `envs`, with the rule's own lever and the
/// spectrum cap not inherited.
fn run(input: &Path, output: &Path, args: &[&str], envs: &[(&str, &str)]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_mzpeak-convert"));
    cmd.arg(input).arg("-o").arg(output).arg("-q").args(args);
    cmd.env_remove("MZPC_ENCODING_PRESCAN").env_remove("MZPC_MAX_SPECTRA").env_remove("MZPC_KEEP_ZERO_RUNS");
    cmd.envs(envs.iter().copied()).output().expect("failed to run mzpeak-convert")
}

fn convert(input: &Path, output: &Path, args: &[&str], envs: &[(&str, &str)]) {
    let r = run(input, output, args, envs);
    assert!(r.status.success(), "{args:?} {envs:?} failed: {}", String::from_utf8_lossy(&r.stderr));
}

fn metadata(archive: &Path) -> Value {
    let mut zip = zip::ZipArchive::new(std::fs::File::open(archive).unwrap()).unwrap();
    let mut e = zip.by_name("mzpeak_index.json").unwrap();
    let mut buf = Vec::new();
    std::io::Read::read_to_end(&mut e, &mut buf).unwrap();
    serde_json::from_slice::<Value>(&buf).unwrap()["metadata"].clone()
}

fn transformations(archive: &Path) -> Vec<String> {
    metadata(archive)["transformations"].as_array().map(|a| a.iter().map(|v| v.as_str().unwrap().to_string()).collect()).unwrap_or_default()
}

/// A spectrum's m/z as the reader hands them back: its raw arrays' when the facet decoded into
/// arrays, else its peak set's (a centroid archive's spectra come back as a peak set).
fn spectrum_mzs(s: &mzdata::spectrum::MultiLayerSpectrum) -> Vec<f64> {
    match s.raw_arrays().and_then(|a| a.mzs().ok()).filter(|v| !v.is_empty()) {
        Some(v) => v.to_vec(),
        None => s.peaks.as_ref().map(|p| p.iter().map(|p| p.mz).collect()).unwrap_or_default(),
    }
}

/// The m/z the archive decodes to, spectrum by spectrum.
fn decoded(archive: &Path, n: usize) -> Vec<Vec<f64>> {
    let mut reader = MzPeakReader::new(archive).unwrap();
    (0..n).map(|i| spectrum_mzs(&reader.get_spectrum(i).expect("spectrum"))).collect()
}

/// An m/z array at its declared binary type.
enum Mz {
    F32(Vec<f32>),
    F64(Vec<f64>),
}

impl Mz {
    fn len(&self) -> usize {
        match self {
            Mz::F32(v) => v.len(),
            Mz::F64(v) => v.len(),
        }
    }
    fn bytes(&self) -> Vec<u8> {
        match self {
            Mz::F32(v) => v.iter().flat_map(|x| x.to_le_bytes()).collect(),
            Mz::F64(v) => v.iter().flat_map(|x| x.to_le_bytes()).collect(),
        }
    }
    fn as_f64(&self) -> Vec<f64> {
        match self {
            Mz::F32(v) => v.iter().map(|x| f64::from(*x)).collect(),
            Mz::F64(v) => v.clone(),
        }
    }
    fn term(&self) -> (&'static str, &'static str) {
        match self {
            Mz::F32(_) => ("MS:1000521", "32-bit float"),
            Mz::F64(_) => ("MS:1000523", "64-bit float"),
        }
    }
}

/// Write `<dir>/<stem>.imzML` + `.ibd`: processed mode, one pixel per spectrum, 32-bit intensities,
/// `profile` or centroid as declared.
fn write_imzml(dir: &Path, stem: &str, spectra: &[(Mz, Vec<f32>)], profile: bool) -> PathBuf {
    const UUID: [u8; 16] = [0x7a, 0x2b, 0x3c, 0x4d, 0x5e, 0x6f, 0x70, 0x81, 0x92, 0x03, 0xb4, 0xc5, 0xd6, 0xe7, 0xf8, 0xa9];
    let mut ibd = UUID.to_vec();
    let mut list = String::new();
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
        let (mz_at, mz_bytes) = (ibd.len(), mz.bytes().len());
        ibd.extend(mz.bytes());
        let int_at = ibd.len();
        let int_bytes: Vec<u8> = intensity.iter().flat_map(|x| x.to_le_bytes()).collect();
        ibd.extend(&int_bytes);
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
            i % 4 + 1,
            i / 4 + 1,
            array("mzArray", mz.len(), mz_at, mz_bytes),
            array("intensityArray", intensity.len(), int_at, int_bytes.len()),
        ));
    }
    let kind = if profile { ("MS:1000128", "profile spectrum") } else { ("MS:1000127", "centroid spectrum") };
    let uuid: String = UUID.iter().map(|b| format!("{b:02x}")).collect();
    let (mz0, mz1) = spectra[0].0.term();
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
      <cvParam cvRef="IMS" accession="IMS:1000031" name="processed"/>
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
      <cvParam cvRef="MS" accession="MS:1000521" name="32-bit float"/>
    </referenceableParamGroup>
    <referenceableParamGroup id="spectrum1">
      <cvParam cvRef="MS" accession="MS:1000579" name="MS1 spectrum"/>
      <cvParam cvRef="MS" accession="MS:1000511" name="ms level" value="1"/>
      <cvParam cvRef="MS" accession="{k0}" name="{k1}"/>
    </referenceableParamGroup>
  </referenceableParamGroupList>
  <softwareList count="1"><software id="gen" version="1"><cvParam cvRef="MS" accession="MS:1000799" name="custom unreleased software tool" value="gen"/></software></softwareList>
  <scanSettingsList count="1"><scanSettings id="s1">
      <cvParam cvRef="IMS" accession="IMS:1000042" name="max count of pixels x" value="4"/>
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
        rows = spectra.len().div_ceil(4),
        n = spectra.len(),
    );
    std::fs::write(dir.join(format!("{stem}.ibd")), ibd).unwrap();
    let path = dir.join(format!("{stem}.imzML"));
    std::fs::write(&path, doc).unwrap();
    path
}

/// A tiny deterministic generator in [0, 1).
struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> f64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (self.0 >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// A flight-time-like profile axis, quadratic in the bin and scaled by an irrational factor: not a
/// decimal lattice (`(10 + 0.0137 i)²` alone has eight decimals and the lattice detector would
/// route it to delta before the rule runs), smooth enough for numpress-linear's prediction.
fn tof_axis(n: usize) -> Vec<f64> {
    (0..n).map(|i| (10.0 + 0.0137 * i as f64).powi(2) * std::f64::consts::PI / 3.0).collect()
}

/// Intensities with no zero run (the mask then drops nothing): every point is kept and compared.
fn signal(rng: &mut Lcg, n: usize) -> Vec<f32> {
    (0..n).map(|_| (1.0 + 1e4 * rng.next()) as f32).collect()
}

const THIRTY_TWO_BIT: &str = "every sampled m/z is a 32-bit value: delta returns them exactly and is smaller";

#[test]
fn thirty_two_bit_mz_take_delta_without_a_trial_and_decode_exactly() {
    let dir = scratch("f32");
    let mut rng = Lcg(11);
    const N: usize = 3000;
    let axis: Vec<f32> = tof_axis(N).into_iter().map(|x| x as f32).collect();
    let spectra: Vec<(Mz, Vec<f32>)> = (0..8).map(|_| (Mz::F32(axis.clone()), signal(&mut rng, N))).collect();
    let input = write_imzml(&dir, "f32", &spectra, true);
    let out = dir.join("f32.mzpeak");
    convert(&input, &out, &[], &[]);
    let meta = metadata(&out);
    let block = &meta["encoding_prescan"];
    assert_eq!((&block["chosen"]["mz"], &block["basis"]), (&Value::from("delta"), &Value::from(THIRTY_TWO_BIT)), "{block:#}");
    assert!(block.get("measured_bytes").is_none(), "no trial was needed: {block:#}");
    assert_eq!(block["sample"]["spectra"], 8, "{block:#}");
    let applied = transformations(&out);
    assert!(!applied.iter().any(|t| t == "numpress-linear" || t == "delta-ulp"), "{applied:?}");
    assert_eq!(meta["fidelity"]["mz_error"], serde_json::json!([]), "{}", meta["fidelity"]);
    assert_eq!(meta["fidelity"]["spectra_data"]["layout"], "chunked");
    for (i, (d, (mz, _))) in decoded(&out, spectra.len()).iter().zip(&spectra).enumerate() {
        assert_eq!(d, &mz.as_f64(), "spectrum {i}: the decoded m/z are the source's, bit for bit");
    }

    // The same values declared 64-bit are still 32-bit values: the rule reads the values, not the
    // declared type.
    let as64: Vec<(Mz, Vec<f32>)> = spectra.iter().map(|(mz, it)| (Mz::F64(mz.as_f64()), it.clone())).collect();
    let input = write_imzml(&dir, "f32-in-f64", &as64, true);
    let out = dir.join("f32-in-f64.mzpeak");
    convert(&input, &out, &[], &[]);
    let meta = metadata(&out);
    let block = &meta["encoding_prescan"];
    assert_eq!((&block["chosen"]["mz"], &block["basis"]), (&Value::from("delta"), &Value::from(THIRTY_TWO_BIT)), "{block:#}");
    for (d, (mz, _)) in decoded(&out, as64.len()).iter().zip(&as64) {
        assert_eq!(d, &mz.as_f64());
    }
    // Declared 64-bit, the values are still 32-bit ones and delta returns them exactly: the facet
    // records that (`mz_values_32bit`) and neither `delta-ulp` nor a `delta` bound is declared,
    // though the sparse ToF axis makes chunks that span more than a factor of two.
    let facet = &meta["fidelity"]["spectra_data"];
    assert_eq!((&facet["source_types"]["mz"], &facet["mz_values_32bit"]), (&serde_json::json!(["float64"]), &Value::from(true)), "{facet:#}");
    assert_eq!(meta["fidelity"]["mz_error"], serde_json::json!([]), "{}", meta["fidelity"]);
    assert!(!transformations(&out).iter().any(|t| t == "delta-ulp" || t == "numpress-linear"), "{:?}", transformations(&out));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_64_bit_sample_takes_the_smaller_arm_and_declares_it() {
    let dir = scratch("f64");
    let mut rng = Lcg(23);
    const N: usize = 3000;

    // A smooth 64-bit profile axis: numpress-linear's prediction leaves small residuals, delta's
    // 64-bit differences carry full mantissas. Numpress is the smaller arm, stays, and is declared
    // and bounded as before.
    let axis = tof_axis(N);
    let smooth: Vec<(Mz, Vec<f32>)> = (0..8).map(|_| (Mz::F64(axis.clone()), signal(&mut rng, N))).collect();
    let input = write_imzml(&dir, "smooth", &smooth, true);
    let out = dir.join("smooth.mzpeak");
    convert(&input, &out, &[], &[]);
    let meta = metadata(&out);
    let block = &meta["encoding_prescan"];
    assert_eq!((&block["chosen"]["mz"], &block["basis"]), (&Value::from("numpress-linear"), &Value::from("the smaller arm")), "{block:#}");
    let (delta, numpress) = (block["measured_bytes"]["mz"]["delta"].as_u64().unwrap(), block["measured_bytes"]["mz"]["numpress-linear"].as_u64().unwrap());
    assert!(numpress < delta, "{block:#}");
    assert_eq!(block["delta_chunks_at_risk"], 0, "{block:#}");
    assert!(transformations(&out).contains(&"numpress-linear".to_string()), "{:?}", transformations(&out));
    let errors = meta["fidelity"]["mz_error"].as_array().unwrap();
    assert!(errors.len() == 1 && errors[0]["encoding"] == "numpress-linear", "{errors:?}");

    // Sparse 64-bit centroid lists whose values are mostly 32-bit ones (one true 64-bit value per
    // spectrum keeps the 32-bit short cut out): delta's differences compress far better than
    // numpress's residuals, so delta is the smaller arm; neighbours more than a factor of two apart
    // make chunks at risk, counted in the block, declared as `delta-ulp` and bounded in `fidelity`.
    let sparse: Vec<(Mz, Vec<f32>)> = (0..8)
        .map(|k| {
            let mut mz: Vec<f64> = (0..40).map(|_| f64::from((30.0 + 1500.0 * rng.next()) as f32)).collect();
            mz.push(700.0 + 0.1 * f64::from(k as u32) + 1e-9); // not a 32-bit value
            mz.sort_by(f64::total_cmp);
            mz.dedup();
            let n = mz.len();
            (Mz::F64(mz), signal(&mut rng, n))
        })
        .collect();
    let input = write_imzml(&dir, "sparse", &sparse, false);
    let out = dir.join("sparse.mzpeak");
    convert(&input, &out, &[], &[]);
    let meta = metadata(&out);
    let block = &meta["encoding_prescan"];
    assert_eq!((&block["chosen"]["mz"], &block["basis"]), (&Value::from("delta"), &Value::from("the smaller arm")), "{block:#}");
    assert!(block["measured_bytes"]["mz"]["delta"].as_u64() < block["measured_bytes"]["mz"]["numpress-linear"].as_u64(), "{block:#}");
    assert!(block["delta_chunks_at_risk"].as_u64() > Some(0), "{block:#}");
    let applied = transformations(&out);
    assert!(applied.contains(&"delta-ulp".to_string()) && !applied.contains(&"numpress-linear".to_string()), "{applied:?}");
    let errors = meta["fidelity"]["mz_error"].as_array().unwrap();
    assert!(errors.len() == 1 && errors[0]["encoding"] == "delta" && errors[0]["chunks_not_exact_by_construction"].as_u64() > Some(0), "{errors:?}");
    let abs = errors[0]["max_abs_error"].as_f64().unwrap();
    for (i, (d, (mz, _))) in decoded(&out, sparse.len()).iter().zip(&sparse).enumerate() {
        let source = mz.as_f64();
        assert_eq!(d.len(), source.len(), "spectrum {i}");
        for (x, y) in d.iter().zip(&source) {
            assert!((x - y).abs() <= abs, "spectrum {i}: {y} decoded as {x}, beyond the recorded bound {abs:e}");
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_rule_is_skipped_on_request_and_an_explicit_choice_is_not_measured() {
    let dir = scratch("skip");
    let mut rng = Lcg(5);
    const N: usize = 2000;
    let axis: Vec<f32> = tof_axis(N).into_iter().map(|x| x as f32).collect();
    let spectra: Vec<(Mz, Vec<f32>)> = (0..6).map(|_| (Mz::F32(axis.clone()), signal(&mut rng, N))).collect();
    let input = write_imzml(&dir, "skip", &spectra, true);

    // The lever: numpress as requested, no block — what every archive before 0.17.0 got.
    let out = dir.join("env.mzpeak");
    convert(&input, &out, &[], &[("MZPC_ENCODING_PRESCAN", "0")]);
    let meta = metadata(&out);
    assert!(meta.get("encoding_prescan").is_none(), "{meta:#}");
    assert!(transformations(&out).contains(&"numpress-linear".to_string()), "{:?}", transformations(&out));

    // The user's own choice: written as asked, no block.
    for (tag, args) in [("delta", &["--no-numpress"][..]), ("point", &["--layout", "point"][..])] {
        let out = dir.join(format!("{tag}.mzpeak"));
        convert(&input, &out, args, &[]);
        let meta = metadata(&out);
        assert!(meta.get("encoding_prescan").is_none(), "{tag}: {meta:#}");
        assert!(!transformations(&out).contains(&"numpress-linear".to_string()), "{tag}: {:?}", transformations(&out));
        assert_eq!(meta["fidelity"]["spectra_data"]["layout"], if tag == "point" { "point" } else { "chunked" }, "{tag}");
        for (d, (mz, _)) in decoded(&out, spectra.len()).iter().zip(&spectra) {
            assert_eq!(d, &mz.as_f64(), "{tag}");
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

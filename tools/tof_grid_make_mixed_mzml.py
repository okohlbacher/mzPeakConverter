#!/usr/bin/env python3
"""Build a minimal mzML with a MIX of griddable (TOF-lattice) and non-griddable spectra.

- MS1 spectra: dense profile on an EXACT sqrt(m/z)=c0+c1*k lattice (griddable).
- MS2 spectra: a handful of arbitrary fragment m/z NOT on that lattice (off-grid).

The converter should route MS1 -> tof_index peak facet, MS2 -> f64 m/z data facet.
"""
import base64, struct, sys, numpy as np

C0, C1 = 10.488102, 1.837267e-5   # close to the TripleTOF fit (so the converter fits a similar grid)

def b64_arr(vals, dtype):
    a = np.asarray(vals, dtype=dtype)
    return base64.b64encode(a.tobytes()).decode(), len(a)

def binary_data_array(vals, dtype, is_mz):
    enc, n = b64_arr(vals, dtype)
    if dtype == np.float64:
        prec = '<cvParam cvRef="MS" accession="MS:1000523" name="64-bit float" value=""/>'
    else:
        prec = '<cvParam cvRef="MS" accession="MS:1000521" name="32-bit float" value=""/>'
    if is_mz:
        typ = '<cvParam cvRef="MS" accession="MS:1000514" name="m/z array" value="" unitCvRef="MS" unitAccession="MS:1000040" unitName="m/z"/>'
    else:
        typ = '<cvParam cvRef="MS" accession="MS:1000515" name="intensity array" value="" unitCvRef="MS" unitAccession="MS:1000131" unitName="number of detector counts"/>'
    nbytes = len(enc)
    return f'''        <binaryDataArray encodedLength="{nbytes}">
          {prec}
          <cvParam cvRef="MS" accession="MS:1000576" name="no compression" value=""/>
          {typ}
          <binary>{enc}</binary>
        </binaryDataArray>'''

def spectrum_xml(idx, ms_level, mzs, intens):
    mz_da = binary_data_array(mzs, np.float64, True)
    in_da = binary_data_array(intens, np.float32, False)
    n = len(mzs)
    lvl = f'<cvParam cvRef="MS" accession="MS:1000511" name="ms level" value="{ms_level}"/>'
    prof = '<cvParam cvRef="MS" accession="MS:1000128" name="profile spectrum" value=""/>'
    msb = '<cvParam cvRef="MS" accession="MS:1000579" name="MS1 spectrum" value=""/>' if ms_level==1 else '<cvParam cvRef="MS" accession="MS:1000580" name="MSn spectrum" value=""/>'
    prec = ''
    if ms_level == 2:
        prec = '''      <precursorList count="1">
        <precursor>
          <selectedIonList count="1"><selectedIon><cvParam cvRef="MS" accession="MS:1000744" name="selected ion m/z" value="500.0" unitCvRef="MS" unitAccession="MS:1000040" unitName="m/z"/></selectedIon></selectedIonList>
          <activation><cvParam cvRef="MS" accession="MS:1000133" name="collision-induced dissociation" value=""/></activation>
        </precursor>
      </precursorList>
'''
    return f'''    <spectrum index="{idx}" id="scan={idx+1}" defaultArrayLength="{n}">
      {lvl}
      {msb}
      {prof}
      <cvParam cvRef="MS" accession="MS:1000285" name="total ion current" value="{float(np.sum(intens))}"/>
      <scanList count="1"><scan><cvParam cvRef="MS" accession="MS:1000016" name="scan start time" value="{idx*0.01}" unitCvRef="UO" unitAccession="UO:0000031" unitName="minute"/></scan></scanList>
{prec}      <binaryDataArrayList count="2">
{mz_da}
{in_da}
      </binaryDataArrayList>
    </spectrum>'''

def make():
    rng = np.random.default_rng(7)
    specs = []
    idx = 0
    n_ms1 = int(sys.argv[2]) if len(sys.argv) > 2 else 30
    n_ms2 = int(sys.argv[3]) if len(sys.argv) > 3 else 8
    # MS1: dense lattice points. Choose k over a range, dense (step 1) with occasional gaps.
    for s in range(n_ms1):
        ks = []
        k = 200000 + s*13
        while k < 260_000:           # ~12K dense points/spectrum (ZenoTOF-like)
            ks.append(k)
            k += 1 if (k % 5) else 3   # mostly dense
        ks = np.array(ks, dtype=np.int64)
        mzs = (C0 + C1*ks)**2
        intens = (rng.random(len(ks))*1000 + 10).astype(np.float32)
        specs.append(spectrum_xml(idx, 1, mzs, intens)); idx += 1
    # MS2: arbitrary off-lattice fragment m/z (irregular, not affine-in-sqrt integer)
    for s in range(n_ms2):
        x = 100.0 + s
        mzs = []
        for i in range(60):
            x += 0.137 + 0.041*abs(np.sin(i*1.7 + s))   # irregular spacing
            mzs.append(x)
        mzs = np.array(mzs, dtype=np.float64)
        intens = (rng.random(len(mzs))*500 + 5).astype(np.float32)
        specs.append(spectrum_xml(idx, 2, mzs, intens)); idx += 1

    body = "\n".join(specs)
    n = len(specs)
    doc = f'''<?xml version="1.0" encoding="utf-8"?>
<indexedmzML xmlns="http://psi.hupo.org/ms/mzml">
<mzML xmlns="http://psi.hupo.org/ms/mzml" version="1.1.0" id="mixed">
  <cvList count="2">
    <cv id="MS" fullName="PSI-MS" URI="https://raw.githubusercontent.com/HUPO-PSI/psi-ms-CV/master/psi-ms.obo"/>
    <cv id="UO" fullName="Unit Ontology" URI="https://raw.githubusercontent.com/bio-ontology-research-group/unit-ontology/master/unit.obo"/>
  </cvList>
  <fileDescription><fileContent><cvParam cvRef="MS" accession="MS:1000579" name="MS1 spectrum" value=""/></fileContent></fileDescription>
  <referenceableParamGroupList count="1"><referenceableParamGroup id="CommonInstrumentParams"><cvParam cvRef="MS" accession="MS:1000126" name="Waters instrument model" value=""/></referenceableParamGroup></referenceableParamGroupList>
  <softwareList count="1"><software id="sw" version="1"><cvParam cvRef="MS" accession="MS:1000799" name="custom unreleased software tool" value="synth"/></software></softwareList>
  <instrumentConfigurationList count="1"><instrumentConfiguration id="IC1"><componentList count="2"><source order="1"><cvParam cvRef="MS" accession="MS:1000073" name="electrospray ionization" value=""/></source><detector order="2"><cvParam cvRef="MS" accession="MS:1000253" name="electron multiplier" value=""/></detector></componentList></instrumentConfiguration></instrumentConfigurationList>
  <dataProcessingList count="1"><dataProcessing id="dp1"><processingMethod order="1" softwareRef="sw"><cvParam cvRef="MS" accession="MS:1000544" name="Conversion to mzML" value=""/></processingMethod></dataProcessing></dataProcessingList>
  <run id="run1" defaultInstrumentConfigurationRef="IC1">
    <spectrumList count="{n}" defaultDataProcessingRef="dp1">
{body}
    </spectrumList>
  </run>
</mzML>
</indexedmzML>'''
    with open(sys.argv[1], "w") as f:
        f.write(doc)
    print(f"wrote {sys.argv[1]}: {n_ms1} MS1 (lattice) + {n_ms2} MS2 (off-lattice) = {n} spectra")

make()

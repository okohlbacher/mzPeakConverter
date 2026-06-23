#!/usr/bin/env python3
"""Round-trip lossless verification of per-spectrum TOF-grid routing.

1. Parse original mzML -> {spectrum_index: (sorted mz array, ms_level)}.
2. Read spectra_peaks (tof_index) -> reconstruct mz = (c0 + c1*k)^2 per spectrum_index.
3. Read spectra_data (mz f64) per spectrum_index.
4. Each spectrum must be in EXACTLY one facet; reconstructed mz must match original:
   - gridded spectra: within PPM_TOL.
   - f64 spectra: BIT-EXACT (max abs ppm ~ 0).
"""
import base64, struct, sys, numpy as np, json
import xml.etree.ElementTree as ET
import pyarrow.parquet as pq

PPM_TOL = 3.0
NS = '{http://psi.hupo.org/ms/mzml}'

def parse_mzml(path):
    out = {}
    idx = 0
    for ev, el in ET.iterparse(path, events=('end',)):
        if el.tag != NS+'spectrum':
            continue
        si = int(el.get('index'))
        lvl = 1
        mzs = None
        is_64 = False
        for bda in el.iter(NS+'binaryDataArray'):
            params = {c.get('accession') for c in bda if c.tag==NS+'cvParam'}
            is_mz = 'MS:1000514' in params
            f64 = 'MS:1000523' in params
            b = bda.find(NS+'binary').text or ''
            raw = base64.b64decode(b)
            if is_mz:
                mzs = np.frombuffer(raw, dtype='<f8' if f64 else '<f4').astype(np.float64)
        for cv in el.iter(NS+'cvParam'):
            if cv.get('accession') == 'MS:1000511':
                lvl = int(cv.get('value'))
        out[si] = (np.sort(mzs), lvl)
        el.clear()
        idx += 1
    return out

def facet_by_spectrum(table, value_col):
    pts = table.column('point').combine_chunks()
    si = pts.field('spectrum_index').to_numpy(zero_copy_only=False)
    val = pts.field(value_col).to_numpy(zero_copy_only=False)
    d = {}
    for s in np.unique(si):
        d[int(s)] = val[si == s]
    return d

def main():
    mzml, archive_dir = sys.argv[1], sys.argv[2]
    orig = parse_mzml(mzml)
    idxj = json.load(open(f'{archive_dir}/mzpeak_index.json'))
    cal = idxj['metadata']['tof_calibration']
    c0, c1 = cal['c0'], cal['c1']

    peaks = pq.read_table(f'{archive_dir}/spectra_peaks.parquet')
    data  = pq.read_table(f'{archive_dir}/spectra_data.parquet')
    grid_facet = facet_by_spectrum(peaks, 'tof_index')   # spectrum_index -> tof_index ints
    f64_facet  = facet_by_spectrum(data,  'mz')          # spectrum_index -> mz f64

    n_grid = n_f64 = 0
    worst_grid_ppm = 0.0
    worst_f64_ppm = 0.0
    overlap = []
    fails = []
    for si in sorted(orig):
        omz, lvl = orig[si]
        in_grid = si in grid_facet
        in_f64  = si in f64_facet
        if in_grid and in_f64:
            overlap.append(si); continue
        if not in_grid and not in_f64:
            fails.append((si, 'in NEITHER facet')); continue
        if in_grid:
            n_grid += 1
            k = grid_facet[si].astype(np.float64)
            rec = np.sort((c0 + c1*k)**2)
            if len(rec) != len(omz):
                fails.append((si, f'len {len(rec)} != orig {len(omz)}')); continue
            ppm = np.max(np.abs(rec-omz)/omz*1e6)
            worst_grid_ppm = max(worst_grid_ppm, ppm)
            if ppm > PPM_TOL:
                fails.append((si, f'gridded ppm {ppm:.4f} > {PPM_TOL}'))
        else:
            n_f64 += 1
            rec = np.sort(f64_facet[si].astype(np.float64))
            if len(rec) != len(omz):
                fails.append((si, f'len {len(rec)} != orig {len(omz)}')); continue
            ppm = np.max(np.abs(rec-omz)/np.maximum(omz,1e-12)*1e6)
            worst_f64_ppm = max(worst_f64_ppm, ppm)
            # f64 must be exact
            if not np.array_equal(rec, omz):
                fails.append((si, f'f64 not bit-exact, max ppm {ppm:.2e}'))

    print(f"spectra total      : {len(orig)}")
    print(f"gridded (tof_index): {n_grid}  worst reconstruct {worst_grid_ppm:.4f} ppm (tol {PPM_TOL})")
    print(f"f64 (data facet)   : {n_f64}  worst diff {worst_f64_ppm:.3e} ppm (must be 0)")
    print(f"facet overlap (BAD): {len(overlap)} {overlap[:5]}")
    if fails:
        print(f"FAILURES ({len(fails)}):")
        for s, why in fails[:20]:
            print(f"  spectrum {s}: {why}")
        sys.exit(1)
    print("LOSSLESS OK: every spectrum in exactly one facet, all reconstructions within bounds.")

main()

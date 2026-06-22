#!/usr/bin/env python3
"""Compare the ion-mobility (1/K0) axes of two or more mzPeak archives of the SAME timsTOF run.

Built to cross-check the `--bruker-sdk` path (vendor `tims_scannum_to_oneoverk0`) against the
default reader (mzdata / timsrust) and the native ims-compact path. The converters keep different
peaks/orderings, so per-peak alignment is unreliable; instead we compare the set of DISTINCT 1/K0
values — that set is the instrument's mobility axis (a fixed function of scan index + the
analysis.tdf calibration) and must agree between any two correct implementations.

Handles both storage layouts:
  * standard / SDK / mzdata: per-peak column spectra_peaks.point.mean_inverse_reduced_ion_mobility
  * native ims-compact:       per-spectrum auxiliary_arrays in spectra_metadata (uncompressed f64)

Usage:
  compare_ion_mobility.py A.mzpeak[=label] B.mzpeak[=label] ... [--plot out.png] [--tol 1e-6]

Exit 0 if every axis matches the first (reference) within tolerance, 1 otherwise.
"""
import io
import sys
import zipfile

import numpy as np
import pyarrow.parquet as pq

IM_FIELD = "mean_inverse_reduced_ion_mobility"
IM_AUX_NAME = "mean inverse reduced ion mobility array"


def _axis_from_peaks(z, sample_rows=3_000_000):
    if "spectra_peaks.parquet" not in z.namelist():
        return None
    pf = pq.ParquetFile(io.BytesIO(z.read("spectra_peaks.parquet")))
    if pf.metadata.num_rows == 0:
        return None
    for b in pf.iter_batches(batch_size=sample_rows, columns=["point"]):
        s = b.column("point")
        if IM_FIELD not in [f.name for f in s.type]:
            return None
        im = s.field(IM_FIELD).to_numpy(zero_copy_only=False)
        im = im[~np.isnan(im)]
        return np.unique(im)  # first batch already spans the full mobility axis
    return None


def _axis_from_meta_aux(z, sample_frames=400):
    if "spectra_metadata.parquet" not in z.namelist():
        return None
    pf = pq.ParquetFile(io.BytesIO(z.read("spectra_metadata.parquet")))
    vals = []
    for b in pf.iter_batches(batch_size=sample_frames, columns=["spectrum"]):
        spec = b.column("spectrum")
        aux = spec.field("auxiliary_arrays")
        for i in range(len(spec)):
            for x in aux[i].as_py() or []:
                nm = x["name"].get("name") if isinstance(x["name"], dict) else x["name"]
                if nm == IM_AUX_NAME and x.get("compression") in (None, "", "MS:1000576"):
                    vals.append(np.frombuffer(bytes(x["data"]), dtype="<f8"))
        break
    if not vals:
        return None
    return np.unique(np.concatenate(vals))


def load_axis(path):
    z = zipfile.ZipFile(path)
    ax = _axis_from_peaks(z)
    src = "spectra_peaks"
    if ax is None or ax.size == 0:
        ax = _axis_from_meta_aux(z)
        src = "metadata_aux"
    return (np.empty(0) if ax is None else ax), src


def nn_diff(ref, other):
    """Max nearest-neighbor |Δ| aligning the smaller axis onto the larger."""
    lo, hi = (ref, other) if ref.size <= other.size else (other, ref)
    idx = np.clip(np.searchsorted(hi, lo), 1, hi.size - 1)
    near = np.where(np.abs(lo - hi[idx - 1]) <= np.abs(lo - hi[idx]), hi[idx - 1], hi[idx])
    return np.abs(lo - near)


def main():
    args = [a for a in sys.argv[1:]]
    plot_path, tol = None, 1e-6
    paths = []
    i = 0
    while i < len(args):
        if args[i] == "--plot":
            plot_path = args[i + 1]; i += 2; continue
        if args[i] == "--tol":
            tol = float(args[i + 1]); i += 2; continue
        paths.append(args[i]); i += 1
    if len(paths) < 2:
        print(__doc__); return 2

    series = []
    for spec in paths:
        path, _, label = spec.partition("=")
        ax, src = load_axis(path)
        label = label or path.split("/")[-1]
        print(f"{label:14s} src={src:13s} n_distinct={ax.size:5d} "
              + (f"range=[{ax.min():.6f},{ax.max():.6f}]" if ax.size else "(no IM data)"))
        series.append((label, ax))

    ref_label, ref = series[0]
    ok = True
    print(f"\nagreement vs reference ({ref_label}):")
    for label, ax in series[1:]:
        if ref.size == 0 or ax.size == 0:
            print(f"  {label:14s}: cannot compare (missing IM data)"); ok = False; continue
        d = nn_diff(ref, ax)
        within = (d <= tol).mean() * 100
        print(f"  {label:14s}: max|Δ1/K0|={d.max():.3e}  mean={d.mean():.3e}  within {tol:.0e}: {within:.2f}%")
        ok = ok and d.max() <= tol

    if plot_path:
        try:
            make_plot(series, plot_path, ref)
            print(f"\nplot written: {plot_path}")
        except ImportError:
            print("\nplot skipped: matplotlib not installed (pip install matplotlib)")

    print(f"\nRESULT: mobility axes {'MATCH' if ok else 'DIFFER'} (tol {tol:.0e})")
    return 0 if ok else 1


def make_plot(series, out, ref):
    import matplotlib
    matplotlib.use("Agg")
    import matplotlib.pyplot as plt

    fig, (ax1, ax2) = plt.subplots(2, 1, figsize=(9, 7), gridspec_kw={"height_ratios": [2.2, 1]})
    styles = [("-", 2.4), ("--", 2.0), (":", 2.2), ("-.", 1.8)]
    for k, (label, ax) in enumerate(series):
        if ax.size == 0:
            continue
        x = np.linspace(0, 100, ax.size)
        ls, lw = styles[k % len(styles)]
        ax1.plot(x, ax, ls, lw=lw, label=f"{label}  (n={ax.size})")
    ax1.set_ylabel("1/K0  (Vs·s/cm²)")
    ax1.set_title("Inverse reduced ion mobility axis — converter path comparison")
    ax1.legend(loc="upper left", fontsize=9)
    ax1.grid(alpha=0.25)

    for k, (label, ax) in enumerate(series[1:], start=1):
        if ax.size == 0 or ref.size == 0:
            continue
        d = nn_diff(ref, ax)
        lo = ref if ref.size <= ax.size else ax
        ax2.plot(np.linspace(0, 100, d.size), d, styles[k % len(styles)][0], lw=1.6, label=label)
    ax2.set_xlabel("mobility scan position (%)")
    ax2.set_ylabel(f"|Δ 1/K0| vs {series[0][0]}")
    ax2.legend(loc="upper right", fontsize=9)
    ax2.grid(alpha=0.25)
    fig.tight_layout()
    fig.savefig(out, dpi=130)


if __name__ == "__main__":
    sys.exit(main())

//! Peak-by-peak comparison of two mzPeak archives produced from the SAME raw by
//! two reader paths (native vendor reader vs ProteoWizard `--via-msconvert`).
//!
//! Both archives are decoded with the reference `MzPeakReader`, so every encoding
//! (TOF-grid, numpress, ims-compact, chunked) is reconstructed to true m/z before
//! comparison — no format assumptions here.
//!
//! Spectra are matched first by id, then by (ms_level, retention-time, precursor m/z).
//! Unmatched spectra on either side explain how the spectrum counts can differ.
//! Matched pairs are diffed peak-by-peak within a ppm tolerance.
//!
//! Output: one JSON object (the per-file summary) on stdout.
//!
//!   cargo run --release -p mzpeak_prototyping --example compare_paths -- A.mzpeak B.mzpeak [--ppm 20]
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;

use clap::Parser;
use mzdata::prelude::*;
use mzdata::spectrum::SpectrumDescription;
use mzpeak_prototyping::MzPeakReader;

#[derive(Parser)]
struct App {
    /// "A" archive (native vendor-reader path)
    a: PathBuf,
    /// "B" archive (ProteoWizard --via-msconvert path)
    b: PathBuf,
    /// m/z match tolerance in ppm
    #[arg(long, default_value = "20.0")]
    ppm: f64,
    /// label for the report (e.g. dataset id)
    #[arg(long, default_value = "")]
    label: String,
}

#[derive(Clone)]
struct Meta {
    id: String,
    key: (u8, i64), // (ms_level, rt_milli); precursor windows disambiguated by FIFO order within bucket
}

fn meta_of<R: SpectrumLike>(s: &R) -> Meta {
    Meta {
        id: s.id().to_string(),
        key: (s.ms_level(), (s.start_time() * 60000.0).round() as i64), // start_time is minutes
    }
}

// SpectrumDescription (returned by metadata-only reads) doesn't implement SpectrumLike.
fn meta_from_descr(d: &SpectrumDescription) -> Meta {
    let rt = d.acquisition.first_scan().map(|s| s.start_time).unwrap_or(0.0);
    Meta {
        id: d.id.clone(),
        key: (d.ms_level, (rt * 60000.0).round() as i64),
    }
}

/// Two-pointer peak diff of two m/z-sorted spectra. Returns
/// (matched, a_only, b_only, max_abs_ppm, max_rel_intensity_diff).
fn diff_peaks(
    amz: &[f64],
    ai: &[f32],
    bmz: &[f64],
    bi: &[f32],
    ppm: f64,
) -> (u64, u64, u64, f64, f64) {
    let (mut i, mut j) = (0usize, 0usize);
    let (mut matched, mut a_only, mut b_only) = (0u64, 0u64, 0u64);
    let (mut max_ppm, mut max_int) = (0f64, 0f64);
    while i < amz.len() && j < bmz.len() {
        let tol = amz[i] * ppm / 1e6;
        let d = amz[i] - bmz[j];
        if d.abs() <= tol {
            matched += 1;
            let p = (d.abs() / amz[i]) * 1e6;
            if p > max_ppm {
                max_ppm = p;
            }
            let (x, y) = (ai[i] as f64, bi[j] as f64);
            let denom = x.max(y).max(1.0);
            let r = (x - y).abs() / denom;
            if r > max_int {
                max_int = r;
            }
            i += 1;
            j += 1;
        } else if d < 0.0 {
            a_only += 1;
            i += 1;
        } else {
            b_only += 1;
            j += 1;
        }
    }
    a_only += (amz.len() - i) as u64;
    b_only += (bmz.len() - j) as u64;
    (matched, a_only, b_only, max_ppm, max_int)
}

fn main() -> std::io::Result<()> {
    env_logger::init();
    let args = App::parse();

    // ---- B metadata index: id->idx and key->queue(idx) -----------------------------------------
    let mut breader = MzPeakReader::new(&args.b)?;
    let nb = breader.len();
    let mut b_by_id: HashMap<String, usize> = HashMap::with_capacity(nb);
    let mut b_by_key: HashMap<(u8, i64), VecDeque<usize>> = HashMap::new();
    for idx in 0..nb {
        if let Some(d) = breader.get_spectrum_metadata(idx as u64)? {
            let m = meta_from_descr(&d);
            b_by_id.insert(m.id.clone(), idx);
            b_by_key.entry(m.key).or_default().push_back(idx);
        }
    }

    // ---- walk A, match, diff -------------------------------------------------------------------
    let mut areader = MzPeakReader::new(&args.a)?;
    let na = areader.len();

    let (mut matched_spectra, mut a_only_spectra) = (0u64, 0u64);
    let (mut by_id, mut by_key) = (0u64, 0u64);
    let mut b_used = vec![false; nb];
    let (mut tot_match, mut tot_aonly, mut tot_bonly) = (0u64, 0u64, 0u64);
    let (mut gmax_ppm, mut gmax_int) = (0f64, 0f64);
    let mut peak_perfect = 0u64; // matched spectra with zero a_only+b_only peaks
    let mut worst: Vec<(String, u64, u64, f64)> = Vec::new(); // id, a_only, b_only, max_ppm
    // C: per-MS-level breakdown [spectra, peaks_matched, peaks_a_only, peaks_b_only].
    let mut per_level: std::collections::BTreeMap<u8, [u64; 4]> = std::collections::BTreeMap::new();
    // C: decode-gap guard — A spectra that decoded to ZERO points (neither profile m/z nor peaks).
    // A future decode regression (e.g. an unapplied grid transform) would spike this instead of being
    // silently mis-counted as "peaks lost".
    let mut a_empty_decode = 0u64;

    for aidx in 0..na {
        let aspec = match areader.get_spectrum(aidx) {
            Some(s) => s,
            None => continue,
        };
        let am = meta_of(&aspec);

        // C: flag a fully-empty decode (no profile m/z and no peaks) — a potential decode gap.
        let a_pts = aspec
            .raw_arrays()
            .and_then(|m| m.mzs().ok().map(|v| v.len()))
            .unwrap_or(0)
            + aspec.peaks().len();
        if a_pts == 0 {
            a_empty_decode += 1;
        }

        // find a B match: id first, else an unused entry under the same key
        let bidx = if let Some(&bi) = b_by_id.get(&am.id).filter(|&&bi| !b_used[bi]) {
            by_id += 1;
            Some(bi)
        } else if let Some(q) = b_by_key.get_mut(&am.key) {
            let mut got = None;
            while let Some(c) = q.pop_front() {
                if !b_used[c] {
                    got = Some(c);
                    break;
                }
            }
            if got.is_some() {
                by_key += 1;
            }
            got
        } else {
            None
        };

        let Some(bidx) = bidx else {
            a_only_spectra += 1;
            continue;
        };
        b_used[bidx] = true;
        matched_spectra += 1;

        let aar = aspec.raw_arrays();
        let bspec = breader.get_spectrum(bidx);
        let (amz, ai, bmz, bi) = match (aar, bspec.as_ref().and_then(|s| s.raw_arrays())) {
            (Some(a), Some(b)) => (a.mzs(), a.intensities(), b.mzs(), b.intensities()),
            _ => continue,
        };
        if let (Ok(amz), Ok(ai), Ok(bmz), Ok(bi)) = (amz, ai, bmz, bi) {
            let (m, ao, bo, mp, mi) = diff_peaks(&amz, &ai, &bmz, &bi, args.ppm);
            let lvl = per_level.entry(am.key.0).or_insert([0u64; 4]);
            lvl[0] += 1;
            lvl[1] += m;
            lvl[2] += ao;
            lvl[3] += bo;
            tot_match += m;
            tot_aonly += ao;
            tot_bonly += bo;
            if mp > gmax_ppm {
                gmax_ppm = mp;
            }
            if mi > gmax_int {
                gmax_int = mi;
            }
            if ao == 0 && bo == 0 {
                peak_perfect += 1;
            } else if worst.len() < 5000 {
                worst.push((am.id.clone(), ao, bo, mp));
            }
        }
    }
    let b_only_spectra = b_used.iter().filter(|&&u| !u).count() as u64;

    let per_level_json: Vec<String> = per_level
        .iter()
        .map(|(lvl, v)| {
            format!(
                "{{\"ms_level\":{},\"spectra\":{},\"peaks_matched\":{},\"peaks_a_only\":{},\"peaks_b_only\":{}}}",
                lvl, v[0], v[1], v[2], v[3]
            )
        })
        .collect();

    worst.sort_by(|x, y| (y.1 + y.2).cmp(&(x.1 + x.2)));
    let worst_json: Vec<String> = worst
        .iter()
        .take(5)
        .map(|(id, ao, bo, mp)| {
            format!(
                "{{\"id\":{:?},\"a_only_peaks\":{},\"b_only_peaks\":{},\"max_ppm\":{:.3}}}",
                id, ao, bo, mp
            )
        })
        .collect();

    println!(
        "{{\"label\":{:?},\"a\":{:?},\"b\":{:?},\"ppm_tol\":{},\
\"spectra_a\":{},\"spectra_b\":{},\
\"matched_spectra\":{},\"a_only_spectra\":{},\"b_only_spectra\":{},\
\"matched_by_id\":{},\"matched_by_key\":{},\
\"peak_perfect_spectra\":{},\
\"peaks_matched\":{},\"peaks_a_only\":{},\"peaks_b_only\":{},\
\"max_abs_ppm\":{:.4},\"max_rel_intensity_diff\":{:.4},\
\"a_empty_decode\":{},\"per_ms_level\":[{}],\
\"worst\":[{}]}}",
        args.label,
        args.a.display(),
        args.b.display(),
        args.ppm,
        na,
        nb,
        matched_spectra,
        a_only_spectra,
        b_only_spectra,
        by_id,
        by_key,
        peak_perfect,
        tot_match,
        tot_aonly,
        tot_bonly,
        gmax_ppm,
        gmax_int,
        a_empty_decode,
        per_level_json.join(","),
        worst_json.join(",")
    );
    Ok(())
}

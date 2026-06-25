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
    index: usize,
    id: String,
    key: (u8, i64, i64), // (ms_level, rt_milli, precursor_centi)
}

fn meta_of<R: SpectrumLike>(s: &R) -> Meta {
    let pmz = s.precursor().and_then(|p| p.ion()).map(|i| i.mz).unwrap_or(0.0);
    Meta {
        index: s.index(),
        id: s.id().to_string(),
        key: (
            s.ms_level(),
            (s.start_time() * 60000.0).round() as i64, // rt in ms (start_time is minutes)
            (pmz * 100.0).round() as i64,
        ),
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
    let mut b_by_key: HashMap<(u8, i64, i64), VecDeque<usize>> = HashMap::new();
    for idx in 0..nb {
        if let Some(d) = breader.get_spectrum_metadata(idx as u64)? {
            let m = meta_of(&d);
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

    for aidx in 0..na {
        let aspec = match areader.get_spectrum(aidx) {
            Some(s) => s,
            None => continue,
        };
        let am = meta_of(&aspec);

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
        worst_json.join(",")
    );
    Ok(())
}

//! What the native SciEX lane refuses to convert, decided from the glue's run-level counts.
//!
//! Host-independent ON PURPOSE (same reasoning as `pwiz_layout`): `sciex.rs` is `#[cfg(windows)]`,
//! so a decision inside it can be neither compiled nor tested here — and the one it held ran on the
//! mzPeak lane only, while `--to mzml` streamed the same file unchecked. The decision and its
//! messages live here with tests that run on every host; the glue calls stay gated in
//! `SciexReader::open_run`, the one opener both lanes use.
#![cfg_attr(not(windows), allow(dead_code))]

use std::path::Path;

/// Run-level counts the glue gathered while indexing the file.
#[derive(Debug, Clone, Copy, Default)]
pub struct SciexRunInfo {
    pub samples: i32,
    pub unreadable_samples: i32,
    pub dwell_experiments: i32,
    pub scan_experiments: i32,
    pub total_experiments: i32,
}

/// Why `path` must not be converted, or `None`. Decided BEFORE any spectrum is written:
///
/// * **MRM / SIM experiments.** Clearcore2 hands a dwell out as a one-point "spectrum" whose
///   m/z is the transition ORDINAL — the two published corpus archives built this way carried
///   154,520 and 2,215 such rows with no Q1/Q3, dwell or compound identity (BACKLOG). They are
///   transition chromatograms; msconvert writes them as SRM chromatograms with their identity,
///   so the harness's fallback is the right lane (the message is what
///   `tools/box_convert_remote.ps1` classifies on). A MIXED run is refused as a whole: dropping
///   the dwell experiments would lose channels msconvert keeps.
/// * **Unreadable samples.** A partial file must not publish as a complete one.
/// * **Several samples without `--sample`.** The archive is one run; concatenating N samples
///   under one run id is not a conversion of any of them (En_PPY: 116 of 117 samples in one
///   archive, the mzML lane keeps only the last).
/// * **`--sample` outside `1..=samples`.**
///
/// `types` and `sample_names` are the glue's run strings 0 and 5 (names U+001F-joined),
/// `last_error` its last error.
pub fn refusal(
    path: &Path,
    info: &SciexRunInfo,
    sample: Option<u32>,
    types: &str,
    sample_names: &str,
    last_error: &str,
) -> Option<String> {
    if info.dwell_experiments > 0 {
        return Some(format!(
            "{}: MRM/SIM dwell data only handled by the msconvert lane — {} of {} experiments are \
             MRM/SIM dwells and {} are scans (types: {}); the native reader would store each dwell \
             as a one-point spectrum without its transition. Use --via-msconvert, which writes SRM \
             chromatograms.",
            path.display(),
            info.dwell_experiments,
            info.total_experiments,
            info.scan_experiments,
            types
        ));
    }
    if info.unreadable_samples > 0 {
        return Some(format!(
            "{}: {} of {} samples (or their experiments) could not be read by Clearcore2; refusing \
             to write a partial archive. Last glue error: {}",
            path.display(),
            info.unreadable_samples,
            info.samples,
            last_error
        ));
    }
    if info.samples > 1 && sample.is_none() {
        return Some(format!(
            "{}: the WIFF holds {} samples and an archive is ONE run; pass --sample <1..{}> to \
             choose which to convert (sample names: {})",
            path.display(),
            info.samples,
            info.samples,
            sample_names.replace('\u{1F}', " | ")
        ));
    }
    match sample {
        // i64: `n as i32` wraps a huge --sample negative and let it past the bound.
        Some(n) if n == 0 || i64::from(n) > i64::from(info.samples.max(1)) => Some(format!(
            "{}: --sample {n} is out of range (the WIFF holds {} samples)",
            path.display(),
            info.samples
        )),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{refusal, SciexRunInfo};
    use std::path::Path;

    fn refuse(info: SciexRunInfo, sample: Option<u32>) -> Option<String> {
        refusal(Path::new("x.wiff"), &info, sample, "MRM", "a\u{1F}b", "boom")
    }

    fn run(samples: i32) -> SciexRunInfo {
        SciexRunInfo { samples, scan_experiments: 1, total_experiments: 1, ..Default::default() }
    }

    /// The box harness routes a refused dwell run to msconvert by matching `MRM/SIM dwell`, and a
    /// mixed run is refused whole — a valid `--sample` does not rescue it.
    #[test]
    fn dwell_experiments_are_refused() {
        let msg = refuse(SciexRunInfo { dwell_experiments: 2, total_experiments: 3, ..run(1) }, Some(1))
            .expect("a dwell run is refused");
        assert!(msg.starts_with("x.wiff: MRM/SIM dwell data only handled by the msconvert lane"), "{msg}");
        assert!(msg.contains("2 of 3 experiments are MRM/SIM dwells and 1 are scans (types: MRM)"), "{msg}");
    }

    #[test]
    fn unreadable_samples_are_refused() {
        let msg = refuse(SciexRunInfo { unreadable_samples: 1, ..run(3) }, Some(1))
            .expect("a partial file is refused");
        assert_eq!(
            msg,
            "x.wiff: 1 of 3 samples (or their experiments) could not be read by Clearcore2; refusing \
             to write a partial archive. Last glue error: boom"
        );
    }

    #[test]
    fn several_samples_need_a_choice() {
        assert_eq!(
            refuse(run(117), None).as_deref(),
            Some(
                "x.wiff: the WIFF holds 117 samples and an archive is ONE run; pass --sample <1..117> \
                 to choose which to convert (sample names: a | b)"
            )
        );
        assert_eq!(refuse(run(117), Some(117)), None, "a chosen sample converts");
        assert_eq!(refuse(run(1), None), None, "a single-sample file needs no choice");
    }

    #[test]
    fn sample_must_be_in_range() {
        for n in [0, 4, u32::MAX] {
            assert_eq!(
                refuse(run(3), Some(n)),
                Some(format!("x.wiff: --sample {n} is out of range (the WIFF holds 3 samples)"))
            );
        }
    }
}

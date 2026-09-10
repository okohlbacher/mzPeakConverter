//! Does a NATIVE-lane archive carry the same metadata as an mzML-lane (`--via-msconvert`) archive
//! built from the SAME source file?
//!
//! The two lanes obtain metadata from different places and that asymmetry is invisible in the
//! signal: the mzML lane inherits ProteoWizard's finished model (it walks the acquisition
//! directory, hashes every member, reads the device list and the acquisition software version,
//! emits non-MS device traces as chromatograms) and `copy_metadata_from` carries the lot across,
//! while the native lanes build an archive out of nothing but the per-spectrum descriptions their
//! reader returns plus whatever the caller put in `VendorHints` — so every field has to be plumbed
//! by hand and anything nobody plumbed is silently absent. That was measured on Agilent in
//! September 2026 (instrument serial, components, per-file checksums and the LC device
//! chromatograms all missing from the native build) and it is not an Agilent property: it follows
//! from the shape of the two lanes.
//!
//! This test makes the asymmetry a tracked quantity instead of a discovery. It reads pairs of
//! archives, extracts a normalised METADATA SURFACE from each — index blocks, run block, source
//! files and their checksums, instrument configurations, software, the column set and population of
//! every metadata facet, the chromatogram inventory, the spectrum-level histograms — diffs them,
//! and classifies every difference:
//!
//!   * a difference matching [`EXPECTED`] is reported with the reason it is accepted;
//!   * anything else FAILS the test.
//!
//! So a new metadata loss cannot land unnoticed, and closing one shows up as an `EXPECTED` rule
//! that no longer fires (also a failure — see [`unexpected_and_stale`]).
//!
//! ## Getting pairs
//! The native lanes are Windows-only, so the pairs cannot be built here. Point `MZPC_LANE_PAIRS` at
//! a directory holding `<stem>.native.mzpeak` + `<stem>.mzml.mzpeak` for one or more sources;
//! `tools/lane_pairs.ps1` builds exactly that on the Windows box from its raw cache. Without the
//! variable (or the directory) the test prints what it wanted and skips, like the other
//! corpus-gated tests.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::path::{Path, PathBuf};

use arrow::array::{Array, AsArray};
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

/// Why an accepted difference is accepted. The distinction is the point of this test: one kind is
/// the two lanes legitimately doing different things, the other is metadata the native lane does
/// not carry and should.
#[derive(PartialEq, Clone, Copy)]
enum Kind {
    /// A real, tracked LOSS — the native lane could carry this and does not.
    Defect,
    /// Not a loss: the lanes legitimately differ (encoding choice, or the native lane carrying MORE).
    ByDesign,
}

/// One accepted difference: the fact key it applies to, its kind, and WHY. Anything not covered
/// here fails the test. This list is the documentation of what the native lanes do not carry.
struct Expected {
    /// Matched against the fact key: exact, or a `*` suffix for a prefix match.
    key: &'static str,
    /// The native lanes the difference was measured on; `None` applies on every lane. A `*` rule must
    /// name them (`wildcard_rules_name_their_vendors`): it accepts every fact under its prefix —
    /// every column's population — so left open it waves through a loss on a lane nobody measured.
    vendors: Option<&'static [Vendor]>,
    kind: Kind,
    reason: &'static str,
}

/// The native lane a pair went through.
#[derive(PartialEq, Eq, PartialOrd, Ord, Clone, Copy, Debug)]
enum Vendor {
    Agilent,
    Bruker,
    Sciex,
    Shimadzu,
    Waters,
}

/// The vendor of a NATIVE archive, from the instrument family term its lane states beside the model
/// string (`instrument.param_accessions`). `None` when it states none (a Bruker file whose
/// `InstrumentName` does not say timsTOF): no vendor-scoped rule applies, its differences fail as NEW.
fn vendor_of(native: &Surface) -> Option<Vendor> {
    const FAMILY: &[(&str, Vendor)] = &[
        ("MS:1000490", Vendor::Agilent),
        ("MS:1003123", Vendor::Bruker),
        ("MS:1000121", Vendor::Sciex),
        ("MS:1002998", Vendor::Shimadzu),
        ("MS:1000126", Vendor::Waters),
    ];
    let accs = native.get("instrument.param_accessions")?;
    FAMILY.iter().find(|(acc, _)| accs.split(',').any(|a| a == *acc)).map(|(_, v)| *v)
}

const EXPECTED: &[Expected] = &[
    Expected {
        key: "software.ids",
        vendors: None,
        kind: Kind::Defect,
        reason: "the native Shimadzu lane still records no acquisition software (LabSolutions version needs a glue export); elsewhere the difference is ProteoWizard's own entries (pwiz, pwiz_Reader_*) and the vendor's FULL version string the native lane carries (`MassHunter GC/MS Acquisition 10.0.368 …`, `MassLynx 4.1 SCN916`) where pwiz prints `8.0` / `4.1`.",
    },
    Expected {
        key: "file_description.source_files.count",
        vendors: None,
        kind: Kind::ByDesign,
        reason: "ProteoWizard lists whatever sits in the acquisition directory, AppleDouble `._*` siblings of the box's copy included (blank1: 17 entries, 9 of them `._*` with one shared bogus digest); the native lane lists the vendor's members (Agilent AcqData files, Bruker analysis.tdf/tsf + _bin, Waters _FUNCnnn.DAT and side files, WIFF + WIFF.scan) with their real SHA-1s.",
    },
    Expected {
        key: "file_description.source_files.with_checksum",
        vendors: None,
        kind: Kind::ByDesign,
        reason: "follows source_files.count (every member the native lane lists is digested).",
    },
    Expected {
        key: "file_description.source_files.names",
        vendors: None,
        kind: Kind::ByDesign,
        reason: "follows source_files.count.",
    },
    Expected {
        key: "instrument.components",
        vendors: None,
        kind: Kind::ByDesign,
        reason: "the native lanes assert only what the vendor file states — the analyzers a device type implies (Agilent Devices.xml), ESI + quadrupole + TOF from the Shimadzu device id, the TOF of a timsTOF — never a guessed source or detector; ProteoWizard adds hand-tabled sources and detectors per vendor (EI + electron multiplier for a 5977, nanospray + MCP + PMT for a timsTOF).",
    },
    Expected {
        key: "instrument.param_accessions",
        vendors: None,
        kind: Kind::ByDesign,
        reason: "the native lanes carry the vendor's model string as the MS:1000031 value beside the family term (MS:1000490 Agilent, MS:1002998 Shimadzu, MS:1000126 Waters, MS:1003123 timsTOF, MS:1000121 SciEX) and the file's model number / instrument name as user params; ProteoWizard maps the string to a specific CV term through a hand-curated table and drops the string.",
    },
    Expected {
        key: "chromatograms.inventory",
        vendors: None,
        kind: Kind::Defect,
        reason: "ProteoWizard emits the LC device traces (pump pressure, flow, DAD) as chromatograms; \
                 the native lanes iterate MS scans only and write just the synthesised TIC/BPC.",
    },
    Expected {
        key: "facet.chromatograms_data.parquet.rows",
        vendors: None,
        kind: Kind::Defect,
        reason: "follows chromatograms.inventory: the device traces carry most of the points.",
    },
    Expected {
        key: "data_processing.count",
        vendors: None,
        kind: Kind::ByDesign,
        reason: "the mzML lane inherits ProteoWizard's own conversion entry beside ours; a native lane has no such step to inherit.",
    },
    Expected {
        key: "index.metadata.keys",
        vendors: None,
        kind: Kind::ByDesign,
        reason: "codec blocks differ by construction: a lane that grids or lattices its m/z declares \
                 `tof_calibration` / `mz_calibration`, one that chunks does not.",
    },
    Expected {
        key: "transformations",
        vendors: None,
        kind: Kind::ByDesign,
        reason: "the declared transformation list follows the encoding each lane chose, which is the \
                 point of declaring it.",
    },
    Expected {
        key: "facet.spectra_data.parquet.columns",
        vendors: None,
        kind: Kind::ByDesign,
        reason: "point versus chunk layout, and the integer-axis columns of a grid/lattice lane.",
    },
    Expected {
        key: "facet.spectra_peaks.parquet.columns",
        vendors: None,
        kind: Kind::ByDesign,
        reason: "point versus chunk layout, and the integer-axis columns of a grid/lattice lane.",
    },
    Expected {
        key: "facet.spectra_data.parquet.rows",
        vendors: None,
        kind: Kind::ByDesign,
        reason: "a chunk row holds a list of points; a point row holds one. Row counts are not \
                 comparable across layouts — the point totals are compared instead.",
    },
    Expected {
        key: "facet.spectra_peaks.parquet.rows",
        vendors: None,
        kind: Kind::ByDesign,
        reason: "same: chunk rows versus point rows.",
    },
    Expected {
        key: "cv.ids",
        vendors: None,
        kind: Kind::ByDesign,
        reason: "the native lanes declare the converter's own MZP vocabulary because they emit MZP \
                 terms (grid coefficients, mobility bands); the mzML lane has no MZP params to declare. \
                 The native lane carries MORE here, not less.",
    },
    Expected {
        key: "facet.spectra_metadata.parquet.columns",
        vendors: None,
        kind: Kind::ByDesign,
        reason: "the grid/lattice native lanes add their per-spectrum coefficient columns \
                 (opt_MZP_1000003_tof_c0 …), which the mzML lane has no equivalent for.",
    },
    Expected {
        key: "spectra_metadata.opt_MZP_*",
        vendors: Some(&[Vendor::Bruker, Vendor::Sciex, Vendor::Shimadzu]),
        kind: Kind::ByDesign,
        reason: "as above: per-spectrum grid coefficients exist only where a lane grids.",
    },
    Expected {
        key: "run.default_data_processing_id",
        vendors: None,
        kind: Kind::ByDesign,
        reason: "each lane names its own processing entry; both resolve within their archive.",
    },
    Expected {
        key: "run.default_source_file_id",
        vendors: None,
        kind: Kind::ByDesign,
        reason: "each lane names its own source-file entry; both resolve within their archive.",
    },
    Expected {
        key: "run.start_time",
        vendors: None,
        kind: Kind::ByDesign,
        reason: "the lanes read different things. A vendor-STATED offset is carried verbatim by the native lane (Agilent Contents.xml 13:11:27-04:00 = 17:11Z) while ProteoWizard shifts the same clock by the CONVERTING host's zone (18:11Z on blank1 — wrong by an hour); a vendor time WITHOUT a zone (Waters _HEADER.TXT, Shimadzu AnalysisDate, SciEX) stays null on the native lane and is preserved verbatim in the acquisition_time index block, while ProteoWizard labels the same wall clock Z (Capan2) — a claim the native lane refuses to make.",
    },
    Expected {
        key: "file_description.contents",
        vendors: None,
        kind: Kind::ByDesign,
        reason: "the native lane states what it wrote (MS1/MSn spectrum, centroid/profile, TIC chromatogram); ProteoWizard's list is per-vendor and inconsistent — its Waters reader says `MS1 spectrum` only while writing 136,400 MS2 spectra with precursors (Capan2).",
    },
    Expected {
        key: "facet.chromatograms_data.parquet.columns",
        vendors: None,
        kind: Kind::ByDesign,
        reason: "the mzML lane's chromatogram points carry an ms_level column its source declared.",
    },
    Expected {
        key: "sample.count",
        vendors: None,
        kind: Kind::ByDesign,
        reason: "the native lane carries the vendor's sample (Waters `Acquired Name`, Bruker SampleName, Agilent sample_info.xml, the selected WIFF sample) where ProteoWizard writes none (Waters, Bruker) or an unnamed one (Agilent).",
    },
    Expected {
        key: "run.id",
        vendors: None,
        kind: Kind::Defect,
        reason: "ProteoWizard names a WIFF run after its SAMPLE (En_PPY: the sample name), the native lane after the file stem; pwiz's XML-id escaping of a leading digit (`_x0032_0181203…`) is decoded before comparing, so only the SciEX naming rule remains.",
    },
    Expected {
        key: "facet.spectra_metadata.parquet.rows",
        vendors: None,
        kind: Kind::ByDesign,
        reason: "Waters ion mobility: the native lane writes one FRAME per MassLynx scan (Capan2: 1,989 rows, each carrying every drift bin's points with a per-point raw_ion_mobility array — verified bin for bin against pwiz), where ProteoWizard writes one spectrum per drift bin (397,800 = 1,989 × 200). Same data, 200× fewer rows.",
    },
    Expected {
        key: "facet.spectra_metadata_scans.parquet.rows",
        vendors: None,
        kind: Kind::ByDesign,
        reason: "follows facet.spectra_metadata.parquet.rows (frames vs drift bins).",
    },
    Expected {
        key: "spectra_metadata.*",
        vendors: Some(&[Vendor::Waters]),
        kind: Kind::ByDesign,
        reason: "follows facet.spectra_metadata.parquet.rows: every per-row count is 200× smaller on the frame side, and the VALUES agree — Capan2 ms levels 1,307 MS1 / 682 MS2 natively vs 261,400 / 136,400 per bin (× 200 exactly), RT, polarity and scan window now come from the SDK on every row. The SciEX MRM rows this rule was first written for are refused to the msconvert lane.",
    },
    Expected {
        key: "spectra_metadata_scans.*",
        vendors: Some(&[Vendor::Waters]),
        kind: Kind::ByDesign,
        reason: "follows spectra_metadata.* (frames vs drift bins; RT and scan window present on every native row).",
    },
    Expected {
        key: "instrument.model",
        vendors: None,
        kind: Kind::ByDesign,
        reason: "the native lane carries the vendor's model string as the MS:1000031 value (Devices.xml Name, Shimadzu SystemName, Waters _HEADER.TXT Instrument, Clearcore2 InstrumentName); ProteoWizard keeps it in a user param or maps it to a specific term.",
    },
    Expected {
        key: "sample.names",
        vendors: None,
        kind: Kind::ByDesign,
        reason: "follows sample.count: the native lane's sample carries the vendor's name; ProteoWizard's, where it exists, is unnamed.",
    },
    Expected {
        key: "acquisition_time.wall_clock",
        vendors: None,
        kind: Kind::ByDesign,
        reason: "the native lane's record of a vendor wall clock that has no zone; the mzML lane has no such block (it labelled the same clock Z or shifted it).",
    },
    Expected {
        key: "file_description.source_files.digests",
        vendors: None,
        kind: Kind::ByDesign,
        reason: "follows source_files.count: the AppleDouble entries in ProteoWizard's list carry one shared bogus digest; the members both lanes list agree byte for byte (measured on blank1's eight AcqData files).",
    },
    Expected {
        key: "facet.spectra_metadata_precursors.parquet.rows",
        vendors: None,
        kind: Kind::ByDesign,
        reason: "frames vs drift bins: the native Waters lane writes one precursor per MSn FRAME (Capan2: 682, one per elevated-energy MSe scan, from the SDK's scan items SET_MASS / COLLISION_ENERGY), where ProteoWizard writes its MSe placeholder precursor on each of the 136,400 drift-bin spectra it expands them into (682 × 200).",
    },
    Expected {
        key: "facet.spectra_metadata_selected_ions.parquet.rows",
        vendors: None,
        kind: Kind::ByDesign,
        reason: "an MSe elevated-energy scan selects nothing (SET_MASS = 0): both lanes state the acquisition range as the isolation window (target = midpoint), but the native lane writes no selected ion where ProteoWizard writes a placeholder ion at the midpoint on every drift-bin spectrum. A DDA function (SET_MASS > 0) gets a selected ion and a target-only window on both lanes.",
    },
    Expected {
        key: "spectra_metadata_precursors.*",
        vendors: Some(&[Vendor::Waters]),
        kind: Kind::ByDesign,
        reason: "follows the two row rules: ×200 rows on the pwiz side; the same acquisition-range MSe window on both, but the mzML twin carries only one offset (mzdata reads the lower one as 0) and the native lane adds the method's transfer-energy ramp (MS:1002013/1002014) and the window-source parameter.",
    },
    Expected {
        key: "spectra_metadata_selected_ions.*",
        vendors: Some(&[Vendor::Waters]),
        kind: Kind::ByDesign,
        reason: "follows facet.spectra_metadata_selected_ions.parquet.rows.",
    },
];

/// The first rule covering `key` on a pair that came through `vendor`'s native lane.
fn expected_rule(key: &str, vendor: Option<Vendor>) -> Option<&'static Expected> {
    EXPECTED.iter().find(|e| {
        e.key.strip_suffix('*').map_or(e.key == key, |p| key.starts_with(p))
            && e.vendors.is_none_or(|vs| vendor.is_some_and(|v| vs.contains(&v)))
    })
}

/// Long values (a 95-transition chromatogram inventory) make the report unreadable; keep the head.
fn brief(v: &str) -> String {
    const MAX: usize = 240;
    if v.chars().count() <= MAX {
        return v.to_string();
    }
    let head: String = v.chars().take(MAX).collect();
    format!("{head}… (+{} more chars)", v.chars().count() - MAX)
}

/// A metadata surface: flat `fact -> value`, so a diff is a map comparison and reads as a table.
type Surface = BTreeMap<String, String>;

fn members(archive: &Path) -> zip::ZipArchive<File> {
    zip::ZipArchive::new(File::open(archive).unwrap_or_else(|e| panic!("opening {}: {e}", archive.display())))
        .unwrap_or_else(|e| panic!("reading {} as a zip: {e}", archive.display()))
}

fn read_member(archive: &Path, name: &str) -> Option<Vec<u8>> {
    let mut z = members(archive);
    let mut f = z.by_name(name).ok()?;
    let mut buf = Vec::new();
    std::io::Read::read_to_end(&mut f, &mut buf).ok()?;
    Some(buf)
}

fn parquet_names(archive: &Path) -> Vec<String> {
    let z = members(archive);
    let mut out: Vec<String> = z.file_names().filter(|n| n.ends_with(".parquet")).map(str::to_string).collect();
    out.sort();
    out
}

/// Column names of a facet, flattening the single struct column the signal facets use so that
/// `point.mz` and `chunk.mz_chunk_values` are visible rather than just "point"/"chunk".
fn facet_columns(archive: &Path, member: &str, dir: &Path) -> (Vec<String>, usize) {
    let Some(bytes) = read_member(archive, member) else { return (Vec::new(), 0) };
    let p = dir.join(format!("{}-{member}", archive.file_stem().unwrap().to_string_lossy()));
    std::fs::write(&p, &bytes).unwrap();
    let b = ParquetRecordBatchReaderBuilder::try_new(File::open(&p).unwrap()).unwrap();
    let rows = b.metadata().file_metadata().num_rows() as usize;
    let schema = b.schema().clone();
    let mut cols = Vec::new();
    for f in schema.fields() {
        match f.data_type() {
            arrow::datatypes::DataType::Struct(children) if matches!(f.name().as_str(), "point" | "chunk") => {
                cols.extend(children.iter().map(|c| format!("{}.{}", f.name(), c.name())));
            }
            _ => cols.push(f.name().clone()),
        }
    }
    cols.sort();
    let _ = std::fs::remove_file(&p);
    (cols, rows)
}

/// Non-null count per column of a metadata facet, plus a value histogram for the small categorical
/// columns — "the column exists" is weaker than "the column is populated", and the losses this test
/// exists for are mostly the latter.
fn facet_population(archive: &Path, member: &str, dir: &Path, into: &mut Surface, prefix: &str) {
    let Some(bytes) = read_member(archive, member) else { return };
    let p = dir.join(format!("{}-pop-{member}", archive.file_stem().unwrap().to_string_lossy()));
    std::fs::write(&p, &bytes).unwrap();
    let builder = ParquetRecordBatchReaderBuilder::try_new(File::open(&p).unwrap()).unwrap();
    let schema = builder.schema().clone();
    let rdr = builder.build().unwrap();
    // Seed every declared column at 0 so an EMPTY facet reports "0 populated" rather than omitting
    // the facts: otherwise a lane with no spectra at all shows up as ~20 <absent> differences.
    let mut nonnull: BTreeMap<String, usize> =
        schema.fields().iter().map(|f| (f.name().clone(), 0usize)).collect();
    let mut hist: BTreeMap<String, BTreeMap<String, usize>> = BTreeMap::new();
    const CATEGORICAL: &[&str] = &["ms_level", "spectrum_representation", "spectrum_type", "scan_polarity", "chromatogram_type"];
    for batch in rdr {
        let batch = batch.unwrap();
        for (i, col) in batch.columns().iter().enumerate() {
            let name = batch.schema().field(i).name().clone();
            *nonnull.entry(name.clone()).or_default() += col.len() - col.null_count();
            if CATEGORICAL.contains(&name.as_str()) {
                let h = hist.entry(name).or_default();
                for r in 0..col.len() {
                    if col.is_null(r) {
                        continue;
                    }
                    let v = if let Some(a) = col.as_string_opt::<i32>() {
                        a.value(r).to_string()
                    } else if let Some(a) = col.as_string_opt::<i64>() {
                        a.value(r).to_string()
                    } else if let Some(a) = col.as_primitive_opt::<arrow::datatypes::UInt8Type>() {
                        a.value(r).to_string()
                    } else {
                        continue;
                    };
                    *h.entry(v).or_default() += 1;
                }
            }
        }
    }
    for (col, n) in nonnull {
        into.insert(format!("{prefix}.{col}.nonnull"), n.to_string());
    }
    for (col, h) in hist {
        let rendered = h.iter().map(|(v, n)| format!("{v}={n}")).collect::<Vec<_>>().join(",");
        into.insert(format!("{prefix}.{col}.values"), rendered);
    }
    let _ = std::fs::remove_file(&p);
}

/// ProteoWizard escapes characters an XML id may not start with as `_xHHHH_` (`_x0032_0181203…`
/// for a run whose name starts with a digit). The native lane uses the plain stem.
fn decode_pwiz_id(v: &str) -> String {
    let mut out = String::new();
    let mut rest = v;
    while let Some(i) = rest.find("_x") {
        let (head, tail) = rest.split_at(i);
        out.push_str(head);
        if tail.len() >= 8 && &tail[6..8] == "_" && tail[2..6].chars().all(|c| c.is_ascii_hexdigit()) {
            if let Some(ch) = u32::from_str_radix(&tail[2..6], 16).ok().and_then(char::from_u32) {
                out.push(ch);
                rest = &tail[8..];
                continue;
            }
        }
        out.push_str("_x");
        rest = &tail[2..];
    }
    out.push_str(rest);
    out
}

fn json_at<'a>(v: &'a serde_json::Value, path: &[&str]) -> Option<&'a serde_json::Value> {
    let mut cur = v;
    for p in path {
        cur = cur.get(p)?;
    }
    Some(cur)
}

fn params_of(v: &serde_json::Value) -> Vec<String> {
    v.get("parameters")
        .and_then(|p| p.as_array())
        .map(|a| {
            a.iter()
                .map(|p| {
                    let acc = p.get("accession").and_then(|x| x.as_str()).unwrap_or("");
                    let name = p.get("name").and_then(|x| x.as_str()).unwrap_or("");
                    if acc.is_empty() { format!("(no accession) {name}") } else { acc.to_string() }
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Everything this test compares, as flat facts.
fn surface(archive: &Path, dir: &Path) -> Surface {
    let mut s: Surface = BTreeMap::new();
    let idx: serde_json::Value =
        serde_json::from_slice(&read_member(archive, "mzpeak_index.json").expect("mzpeak_index.json")).unwrap();
    let meta = idx.get("metadata").cloned().unwrap_or_default();

    // --- index blocks -------------------------------------------------------------------------
    let keys: Vec<&str> = meta.as_object().map(|o| o.keys().map(String::as_str).collect()).unwrap_or_default();
    s.insert("index.metadata.keys".into(), keys.join(","));
    if let Some(t) = meta.get("transformations") {
        s.insert("transformations".into(), t.to_string());
    }
    if let Some(a) = meta.get("acquisition_time") {
        s.insert("acquisition_time.wall_clock".into(), a.get("wall_clock").map(|v| v.to_string()).unwrap_or_default());
    }

    // --- run ----------------------------------------------------------------------------------
    if let Some(run) = meta.get("run") {
        for f in ["default_instrument_id", "default_source_file_id", "default_data_processing_id"] {
            s.insert(format!("run.{f}"), run.get(f).map(|v| v.to_string()).unwrap_or_else(|| "absent".into()));
        }
        // The INSTANT, in UTC, so a vendor-stated offset and ProteoWizard's host-zone shift are
        // compared as values (presence-only let a wrong-by-hours time pass as "identical").
        s.insert(
            "run.start_time".into(),
            run.get("start_time")
                .and_then(|v| v.as_str())
                .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
                .map(|t| t.to_utc().to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
                .unwrap_or_else(|| "null".into()),
        );
        // The id itself is the run stem on both lanes; compare only its shape, not the string.
        s.insert(
            "run.id".into(),
            run.get("id")
                .and_then(|v| v.as_str())
                .map(|v| if v.is_empty() { "empty".into() } else { decode_pwiz_id(v) })
                .unwrap_or("absent".into()),
        );
    }

    // --- file description / source files ------------------------------------------------------
    let sfs = json_at(&meta, &["file_description", "source_files"]).and_then(|v| v.as_array()).cloned().unwrap_or_default();
    s.insert("file_description.source_files.count".into(), sfs.len().to_string());
    let with_sum = sfs.iter().filter(|sf| params_of(sf).iter().any(|p| p == "MS:1000569")).count();
    s.insert("file_description.source_files.with_checksum".into(), with_sum.to_string());
    let mut names: Vec<String> = sfs.iter().filter_map(|sf| sf.get("name").and_then(|v| v.as_str()).map(str::to_string)).collect();
    names.sort();
    s.insert("file_description.source_files.names".into(), names.join(","));
    let mut digests: Vec<String> = sfs
        .iter()
        .filter_map(|sf| {
            let name = sf.get("name").and_then(|v| v.as_str())?;
            let sha = sf.get("parameters")?.as_array()?.iter().find(|p| p.get("accession").and_then(|a| a.as_str()) == Some("MS:1000569"))?;
            let hex = sha.get("value").and_then(|v| v.get("string").or(Some(v))).and_then(|v| v.as_str()).unwrap_or("?");
            Some(format!("{}={hex}", name.to_ascii_lowercase()))
        })
        .collect();
    digests.sort();
    s.insert("file_description.source_files.digests".into(), digests.join(","));
    if let Some(c) = json_at(&meta, &["file_description", "contents"]) {
        let mut accs: Vec<String> = c.as_array().map(|a| a.iter().filter_map(|p| p.get("accession").and_then(|x| x.as_str()).map(str::to_string)).collect()).unwrap_or_default();
        accs.sort();
        s.insert("file_description.contents".into(), accs.join(","));
    }

    // --- instrument configurations ------------------------------------------------------------
    let configs = meta.get("instrument_configuration_list").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    s.insert("instrument.configs".into(), configs.len().to_string());
    let mut accs: BTreeSet<String> = BTreeSet::new();
    let mut components = 0usize;
    let mut serial = "absent".to_string();
    let mut model = "absent".to_string();
    let value_of = |p: &serde_json::Value| -> String {
        let v = p.get("value").cloned().unwrap_or_default();
        v.get("string").or(v.get("float")).or(v.get("integer")).cloned().unwrap_or(v).to_string().trim_matches('"').to_string()
    };
    for c in &configs {
        for p in params_of(c) {
            accs.insert(p);
        }
        for p in c.get("parameters").and_then(|v| v.as_array()).cloned().unwrap_or_default() {
            match p.get("accession").and_then(|a| a.as_str()) {
                Some("MS:1000529") => serial = value_of(&p),
                Some("MS:1000031") => model = value_of(&p),
                _ => {}
            }
        }
        components += c.get("components").and_then(|v| v.as_array()).map(|a| a.len()).unwrap_or(0);
    }
    s.insert("instrument.param_accessions".into(), accs.iter().cloned().collect::<Vec<_>>().join(","));
    s.insert("instrument.components".into(), components.to_string());
    s.insert("instrument.serial".into(), serial);
    s.insert("instrument.model".into(), model);

    // --- the other lists ----------------------------------------------------------------------
    for (key, field) in [
        ("software.ids", "software_list"),
        ("sample.count", "sample_list"),
        ("scan_settings.count", "scan_settings_list"),
        ("data_processing.count", "data_processing_method_list"),
        ("cv.ids", "cv_list"),
    ] {
        let arr = meta.get(field).and_then(|v| v.as_array()).cloned().unwrap_or_default();
        if key.ends_with(".ids") {
            let mut ids: Vec<String> = arr
                .iter()
                .filter_map(|e| {
                    let id = e.get("id").and_then(|v| v.as_str())?;
                    let version = e.get("version").and_then(|v| v.as_str()).unwrap_or("");
                    Some(if version.is_empty() { id.to_string() } else { format!("{id}@{version}") })
                })
                .collect();
            ids.sort();
            s.insert(key.into(), ids.join(","));
        } else {
            s.insert(key.into(), arr.len().to_string());
        }
        if key == "sample.count" {
            let mut names: Vec<String> = arr.iter().filter_map(|e| e.get("name").and_then(|v| v.as_str()).map(str::to_string)).collect();
            names.sort();
            s.insert("sample.names".into(), names.join(","));
        }
    }

    // --- facets: columns, rows, population ----------------------------------------------------
    for member in parquet_names(archive) {
        let (cols, rows) = facet_columns(archive, &member, dir);
        s.insert(format!("facet.{member}.columns"), cols.join(","));
        s.insert(format!("facet.{member}.rows"), rows.to_string());
    }
    for (member, prefix) in [
        ("spectra_metadata.parquet", "spectra_metadata"),
        ("spectra_metadata_scans.parquet", "spectra_metadata_scans"),
        ("spectra_metadata_precursors.parquet", "spectra_metadata_precursors"),
        ("spectra_metadata_selected_ions.parquet", "spectra_metadata_selected_ions"),
        ("chromatograms_metadata.parquet", "chromatograms_metadata"),
    ] {
        facet_population(archive, member, dir, &mut s, prefix);
    }

    // --- chromatogram inventory ---------------------------------------------------------------
    if let Some(bytes) = read_member(archive, "chromatograms_metadata.parquet") {
        let p = dir.join(format!("{}-chrom", archive.file_stem().unwrap().to_string_lossy()));
        std::fs::write(&p, &bytes).unwrap();
        let rdr = ParquetRecordBatchReaderBuilder::try_new(File::open(&p).unwrap()).unwrap().build().unwrap();
        let mut inv: Vec<String> = Vec::new();
        for batch in rdr {
            let batch = batch.unwrap();
            let id = batch.column_by_name("id").cloned();
            let ty = batch.column_by_name("chromatogram_type").cloned();
            let np = batch.column_by_name("number_of_data_points").cloned();
            for r in 0..batch.num_rows() {
                let sv = |c: &Option<arrow::array::ArrayRef>| -> String {
                    c.as_ref()
                        .filter(|a| !a.is_null(r))
                        .and_then(|a| {
                            a.as_string_opt::<i32>().map(|x| x.value(r).to_string()).or_else(|| {
                                a.as_string_opt::<i64>().map(|x| x.value(r).to_string())
                            })
                        })
                        .unwrap_or_else(|| "-".into())
                };
                let pts = np
                    .as_ref()
                    .filter(|a| !a.is_null(r))
                    .and_then(|a| a.as_primitive_opt::<arrow::datatypes::UInt64Type>().map(|x| x.value(r).to_string()))
                    .unwrap_or_else(|| "-".into());
                inv.push(format!("{}:{}:{pts}", sv(&id), sv(&ty)));
            }
        }
        inv.sort();
        s.insert("chromatograms.inventory".into(), inv.join(" | "));
        let _ = std::fs::remove_file(&p);
    }
    s
}

#[path = "common/corpus.rs"]
mod corpus;

/// `<stem>` for every pair present in `dir`.
fn pairs(dir: &Path) -> Vec<(String, PathBuf, PathBuf)> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else { return out };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if let Some(stem) = name.strip_suffix(".native.mzpeak") {
            let mzml = dir.join(format!("{stem}.mzml.mzpeak"));
            if mzml.is_file() {
                out.push((stem.to_string(), e.path(), mzml));
            }
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

#[track_caller]
fn pair_dir() -> Option<PathBuf> {
    let d = corpus::env_path("MZPC_LANE_PAIRS")?;
    assert!(d.is_dir(), "MZPC_LANE_PAIRS={} is not a directory", d.display());
    Some(d)
}

/// The report. Prints the full comparison for every pair, then fails if any difference is not in
/// [`EXPECTED`].
#[test]
#[ignore = "needs lane pairs built on the Windows box (tools/lane_pairs.ps1) via MZPC_LANE_PAIRS"]
fn native_and_mzml_lanes_carry_the_same_metadata() {
    let Some(dir) = pair_dir() else { return };
    let pairs = pairs(&dir);
    assert!(!pairs.is_empty(), "MZPC_LANE_PAIRS={} holds no <stem>.native/.mzml pair", dir.display());

    let scratch = std::env::temp_dir().join(format!("mzpc-lane-parity-{}", std::process::id()));
    std::fs::create_dir_all(&scratch).unwrap();

    let mut unexpected: Vec<String> = Vec::new();
    let mut fired: BTreeSet<&'static str> = BTreeSet::new();

    for (stem, native, mzml) in &pairs {
        let (a, b) = (surface(native, &scratch), surface(mzml, &scratch));
        let vendor = vendor_of(&a);
        let (mut by_design, mut defects) = (0usize, 0usize);
        println!("\n=== {stem} (native lane: {vendor:?}) ===");
        println!(
            "  native {:>12} B   mzml {:>12} B",
            std::fs::metadata(native).unwrap().len(),
            std::fs::metadata(mzml).unwrap().len()
        );
        let keys: BTreeSet<&String> = a.keys().chain(b.keys()).collect();
        let mut same = 0usize;
        for k in keys {
            let (va, vb) = (a.get(k), b.get(k));
            if va == vb {
                same += 1;
                continue;
            }
            let rule = expected_rule(k, vendor);
            if let Some(r) = rule {
                fired.insert(r.reason);
            }
            let mark = match rule.map(|r| r.kind) {
                Some(Kind::ByDesign) => { by_design += 1; "by design" }
                Some(Kind::Defect) => { defects += 1; "LOSS     " }
                None => "NEW      ",
            };
            println!("  [{mark}] {k}");
            println!("      native: {}", brief(va.map(String::as_str).unwrap_or("<absent>")));
            println!("      mzml  : {}", brief(vb.map(String::as_str).unwrap_or("<absent>")));
            if rule.is_none() {
                unexpected.push(format!(
                    "{stem}: {k}\n      native: {}\n      mzml  : {}",
                    brief(va.map(String::as_str).unwrap_or("<absent>")),
                    brief(vb.map(String::as_str).unwrap_or("<absent>"))
                ));
            }
        }
        println!("  {same} identical, {by_design} by design, {defects} tracked loss(es)");
    }
    let _ = std::fs::remove_dir_all(&scratch);

    assert!(
        unexpected.is_empty(),
        "the native lane differs from the mzML lane in {} way(s) that are not documented in \
         EXPECTED. Either plumb the field through the native lane, or add it to EXPECTED with the \
         reason it cannot be carried:\n\n  {}\n",
        unexpected.len(),
        unexpected.join("\n  ")
    );
}

/// An `EXPECTED` rule that never fires is either a loss that has been CLOSED — delete the rule and
/// keep the parity — or a rule that never matched anything, which means it is not protecting what
/// its author thought. Either way it must not sit there implying a difference exists.
#[test]
#[ignore = "needs lane pairs built on the Windows box (tools/lane_pairs.ps1) via MZPC_LANE_PAIRS"]
fn unexpected_and_stale() {
    let Some(dir) = pair_dir() else { return };
    let pairs = pairs(&dir);
    assert!(!pairs.is_empty(), "MZPC_LANE_PAIRS={} holds no <stem>.native/.mzml pair", dir.display());
    let scratch = std::env::temp_dir().join(format!("mzpc-lane-stale-{}", std::process::id()));
    std::fs::create_dir_all(&scratch).unwrap();
    let mut fired: BTreeSet<(&'static str, Option<Vendor>)> = BTreeSet::new();
    for (_, native, mzml) in &pairs {
        let (a, b) = (surface(native, &scratch), surface(mzml, &scratch));
        let vendor = vendor_of(&a);
        for k in a.keys().chain(b.keys()) {
            if a.get(k) != b.get(k) {
                if let Some(r) = expected_rule(k, vendor) {
                    fired.insert((r.key, vendor));
                }
            }
        }
    }
    let _ = std::fs::remove_dir_all(&scratch);
    // A vendor-scoped rule is stale PER VENDOR: every lane it names must still show the difference,
    // or its list claims a lane it no longer applies to.
    let stale: Vec<String> = EXPECTED
        .iter()
        .flat_map(|e| match e.vendors {
            None => (!fired.iter().any(|(k, _)| *k == e.key)).then(|| e.key.to_string()).into_iter().collect::<Vec<_>>(),
            Some(vs) => vs
                .iter()
                .filter(|v| !fired.contains(&(e.key, Some(**v))))
                .map(|v| format!("{} on {v:?}", e.key))
                .collect(),
        })
        .collect();
    // An assertion since 0.12: a rule that fires on no pair either records a loss that has been
    // CLOSED (delete it, the parity is the proof) or never matched anything. Which rules fire does
    // depend on which pairs are present, so the pair set must stay broad (one unit per native lane).
    assert!(
        stale.is_empty(),
        "{} EXPECTED rule(s) did not fire on this pair set — closed, or never applicable here; prune them:\n  - {}",
        stale.len(),
        stale.join("\n  - ")
    );
}

/// Fixture-free, so it runs on every host. A `*` rule accepts every fact under its prefix — every
/// column's population — so it must name the native lanes it was measured on, and must then match
/// nobody else's facts.
#[test]
fn wildcard_rules_name_their_vendors() {
    for e in EXPECTED.iter().filter(|e| e.key.ends_with('*')) {
        assert!(
            e.vendors.is_some_and(|v| !v.is_empty()),
            "`{}` accepts its whole prefix on every lane; name the lanes it was measured on",
            e.key
        );
    }
    let waters: Surface = [("instrument.param_accessions".to_string(), "MS:1000031,MS:1000126".to_string())].into();
    assert_eq!(vendor_of(&waters), Some(Vendor::Waters));
    // The frames-vs-drift-bins rule accepts a Waters population difference, and no other lane's.
    let key = "spectra_metadata.ms_level.nonnull";
    assert!(expected_rule(key, Some(Vendor::Waters)).is_some());
    assert!(expected_rule(key, Some(Vendor::Shimadzu)).is_none(), "a Shimadzu population loss must fail as NEW");
    assert!(expected_rule(key, None).is_none(), "an unidentified lane gets no vendor-scoped rule");
}

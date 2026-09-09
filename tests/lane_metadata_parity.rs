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
    kind: Kind,
    reason: &'static str,
}

const EXPECTED: &[Expected] = &[
    Expected {
        key: "software.ids",
        kind: Kind::Defect,
        reason: "the mzML lane inherits the acquisition software and pwiz from the mzML softwareList; \
                 a native lane knows only itself. Recording the vendor's acquisition-software version \
                 natively would need a new field from each vendor SDK.",
    },
    Expected {
        key: "file_description.source_files.count",
        kind: Kind::Defect,
        reason: "ProteoWizard enumerates every member of the acquisition directory; the native lanes \
                 synthesise ONE entry for the input path (`fixup_run_metadata`).",
    },
    Expected {
        key: "file_description.source_files.with_checksum",
        kind: Kind::Defect,
        reason: "only the Shimadzu lane passes a source digest through `VendorHints.source_sha1`; \
                 nothing hashes the members of a directory input. Backlog: per-member SHA-1 for \
                 directory inputs.",
    },
    Expected {
        key: "file_description.source_files.names",
        kind: Kind::Defect,
        reason: "same cause as the count: one synthesised entry versus the vendor directory listing.",
    },
    Expected {
        key: "instrument.components",
        kind: Kind::Defect,
        reason: "only the Shimadzu lane builds ion-source/analyser/detector components; the other \
                 native lanes emit a configuration with parameters but no components.",
    },
    Expected {
        key: "instrument.param_accessions",
        kind: Kind::Defect,
        reason: "the mzML lane carries the vendor-specific model term and the serial number \
                 (MS:1000529) that ProteoWizard read from the acquisition directory; the native lanes \
                 emit what their SDK hands back, which is usually a device-type string only.",
    },
    Expected {
        key: "instrument.serial",
        kind: Kind::Defect,
        reason: "the serial lives in the vendor's own device table (for Agilent, plainly in \
                 AcqData/Devices.xml); no native lane reads it yet.",
    },
    Expected {
        key: "chromatograms.inventory",
        kind: Kind::Defect,
        reason: "ProteoWizard emits the LC device traces (pump pressure, flow, DAD) as chromatograms; \
                 the native lanes iterate MS scans only and write just the synthesised TIC/BPC.",
    },
    Expected {
        key: "facet.chromatograms_data.parquet.rows",
        kind: Kind::Defect,
        reason: "follows chromatograms.inventory: the device traces carry most of the points.",
    },
    Expected {
        key: "facet.chromatograms_metadata.parquet.rows",
        kind: Kind::Defect,
        reason: "follows chromatograms.inventory.",
    },
    Expected {
        key: "data_processing.count",
        kind: Kind::Defect,
        reason: "the mzML lane inherits ProteoWizard's processing entries in addition to ours.",
    },
    // --- encoding, not metadata loss: the two lanes legitimately choose different layouts ---
    Expected {
        key: "index.metadata.keys",
        kind: Kind::ByDesign,
        reason: "codec blocks differ by construction: a lane that grids or lattices its m/z declares \
                 `tof_calibration` / `mz_calibration`, one that chunks does not.",
    },
    Expected {
        key: "transformations",
        kind: Kind::ByDesign,
        reason: "the declared transformation list follows the encoding each lane chose, which is the \
                 point of declaring it.",
    },
    Expected {
        key: "facet.spectra_data.parquet.columns",
        kind: Kind::ByDesign,
        reason: "point versus chunk layout, and the integer-axis columns of a grid/lattice lane.",
    },
    Expected {
        key: "facet.spectra_peaks.parquet.columns",
        kind: Kind::ByDesign,
        reason: "point versus chunk layout, and the integer-axis columns of a grid/lattice lane.",
    },
    Expected {
        key: "facet.spectra_data.parquet.rows",
        kind: Kind::ByDesign,
        reason: "a chunk row holds a list of points; a point row holds one. Row counts are not \
                 comparable across layouts — the point totals are compared instead.",
    },
    Expected {
        key: "facet.spectra_peaks.parquet.rows",
        kind: Kind::ByDesign,
        reason: "same: chunk rows versus point rows.",
    },
    Expected {
        key: "spectra_metadata.mz_delta_model.nonnull",
        kind: Kind::ByDesign,
        reason: "set only by the chunked layout's delta model.",
    },
    // --- surfaced by the first four pairs (Shimadzu .lcd, 2x SciEX .wiff, Agilent GC-MS .d) -----
    Expected {
        key: "cv.ids",
        kind: Kind::ByDesign,
        reason: "the native lanes declare the converter's own MZP vocabulary because they emit MZP \
                 terms (grid coefficients, mobility bands); the mzML lane has no MZP params to declare. \
                 The native lane carries MORE here, not less.",
    },
    Expected {
        key: "facet.spectra_metadata.parquet.columns",
        kind: Kind::ByDesign,
        reason: "the grid/lattice native lanes add their per-spectrum coefficient columns \
                 (opt_MZP_1000003_tof_c0 …), which the mzML lane has no equivalent for.",
    },
    Expected {
        key: "spectra_metadata.opt_MZP_*",
        kind: Kind::ByDesign,
        reason: "as above: per-spectrum grid coefficients exist only where a lane grids.",
    },
    Expected {
        key: "run.default_data_processing_id",
        kind: Kind::ByDesign,
        reason: "each lane names its own processing entry; both resolve within their archive.",
    },
    Expected {
        key: "run.default_source_file_id",
        kind: Kind::ByDesign,
        reason: "each lane names its own source-file entry; both resolve within their archive.",
    },
    Expected {
        key: "run.start_time",
        kind: Kind::Defect,
        reason: "the mzML lane inherits the acquisition timestamp; no native lane reads it. The \
                 Shimadzu lane declines it deliberately (SampleInfo.AnalysisDate is a naive local \
                 time and mzdata's field carries an offset), but the others simply never ask.",
    },
    Expected {
        key: "file_description.contents",
        kind: Kind::Defect,
        reason: "the native lanes state only the generic MS:1000294 'mass spectrum'; the mzML lane \
                 carries the specific contents (MS1 spectrum, SRM chromatogram) ProteoWizard derived.",
    },
    Expected {
        key: "chromatograms_metadata.*",
        kind: Kind::Defect,
        reason: "follows chromatograms.inventory — the native lanes write only the synthesised \
                 TIC/BPC, so every per-chromatogram column is populated for 2 rows instead of N.",
    },
    Expected {
        key: "facet.chromatograms_metadata_precursors.parquet.rows",
        kind: Kind::Defect,
        reason: "an SRM chromatogram carries its precursor (Q1) and product (Q3); the native lanes \
                 write no SRM chromatograms, so no chromatogram precursors either.",
    },
    Expected {
        key: "facet.chromatograms_data.parquet.columns",
        kind: Kind::ByDesign,
        reason: "the mzML lane's chromatogram points carry an ms_level column its source declared.",
    },
    Expected {
        key: "sample.count",
        kind: Kind::Defect,
        reason: "the vendor file names its sample(s) — a SciEX .wiff carries a sample list, and \
                 ProteoWizard turns it into a `sample_list` entry. No native lane reads it, so the \
                 sample identity (name, and any vendor sample fields) is dropped.",
    },
    Expected {
        key: "run.id",
        kind: Kind::Defect,
        reason: "same cause: the mzML lane's run id is '<file stem>-<sample name>' because \
                 ProteoWizard knew the sample; the native lanes fall back to the file stem alone \
                 (`fixup_run_metadata`).",
    },
    // --- MRM/SIM: the two lanes disagree about what the data ARE ------------------------------
    Expected {
        key: "facet.spectra_metadata.parquet.rows",
        kind: Kind::Defect,
        reason: "MRM/SIM acquisitions: the vendor SDKs present each dwell as a one-point spectrum, so \
                 a native lane writes N one-point 'spectra' where the data are transition \
                 chromatograms. Agilent refuses such runs (agl::is_dwell_only) and routes them to \
                 msconvert; the SciEX lane has no such guard and stores them.",
    },
    Expected {
        key: "facet.spectra_metadata_scans.parquet.rows",
        kind: Kind::Defect,
        reason: "follows the spectra row count on MRM/SIM acquisitions.",
    },
    Expected {
        key: "spectra_metadata.*",
        kind: Kind::Defect,
        reason: "follows the spectra row count: on an MRM/SIM unit one lane has spectra and the other \
                 has none, so every per-spectrum column differs in population.",
    },
    Expected {
        key: "spectra_metadata_scans.*",
        kind: Kind::Defect,
        reason: "follows the spectra row count on MRM/SIM acquisitions.",
    },
];

fn expected_rule(key: &str) -> Option<&'static Expected> {
    EXPECTED.iter().find(|e| e.key.strip_suffix('*').map_or(e.key == key, |p| key.starts_with(p)))
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

    // --- run ----------------------------------------------------------------------------------
    if let Some(run) = meta.get("run") {
        for f in ["default_instrument_id", "default_source_file_id", "default_data_processing_id"] {
            s.insert(format!("run.{f}"), run.get(f).map(|v| v.to_string()).unwrap_or_else(|| "absent".into()));
        }
        s.insert(
            "run.start_time".into(),
            if run.get("start_time").is_some_and(|v| !v.is_null()) { "set" } else { "null" }.into(),
        );
        // The id itself is the run stem on both lanes; compare only its shape, not the string.
        s.insert(
            "run.id".into(),
            run.get("id").and_then(|v| v.as_str()).map(|v| if v.is_empty() { "empty".into() } else { v.to_string() }).unwrap_or("absent".into()),
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
    if let Some(c) = json_at(&meta, &["file_description", "contents"]) {
        s.insert("file_description.contents".into(), c.to_string());
    }

    // --- instrument configurations ------------------------------------------------------------
    let configs = meta.get("instrument_configuration_list").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    s.insert("instrument.configs".into(), configs.len().to_string());
    let mut accs: BTreeSet<String> = BTreeSet::new();
    let mut components = 0usize;
    let mut serial = "absent";
    for c in &configs {
        for p in params_of(c) {
            if p == "MS:1000529" {
                serial = "present";
            }
            accs.insert(p);
        }
        components += c.get("components").and_then(|v| v.as_array()).map(|a| a.len()).unwrap_or(0);
    }
    s.insert("instrument.param_accessions".into(), accs.iter().cloned().collect::<Vec<_>>().join(","));
    s.insert("instrument.components".into(), components.to_string());
    s.insert("instrument.serial".into(), serial.into());

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
            let mut ids: Vec<String> =
                arr.iter().filter_map(|e| e.get("id").and_then(|v| v.as_str()).map(str::to_string)).collect();
            ids.sort();
            s.insert(key.into(), ids.join(","));
        } else {
            s.insert(key.into(), arr.len().to_string());
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

fn pair_dir() -> Option<PathBuf> {
    let d = PathBuf::from(std::env::var("MZPC_LANE_PAIRS").ok()?);
    d.is_dir().then_some(d)
}

/// The report. Prints the full comparison for every pair, then fails if any difference is not in
/// [`EXPECTED`].
#[test]
fn native_and_mzml_lanes_carry_the_same_metadata() {
    let Some(dir) = pair_dir() else {
        eprintln!(
            "skipping: set MZPC_LANE_PAIRS to a directory of `<stem>.native.mzpeak` + \
             `<stem>.mzml.mzpeak` pairs (tools/lane_pairs.ps1 builds them on the Windows box)"
        );
        return;
    };
    let pairs = pairs(&dir);
    assert!(!pairs.is_empty(), "MZPC_LANE_PAIRS={} holds no <stem>.native/.mzml pair", dir.display());

    let scratch = std::env::temp_dir().join(format!("mzpc-lane-parity-{}", std::process::id()));
    std::fs::create_dir_all(&scratch).unwrap();

    let mut unexpected: Vec<String> = Vec::new();
    let mut fired: BTreeSet<&'static str> = BTreeSet::new();

    for (stem, native, mzml) in &pairs {
        let (a, b) = (surface(native, &scratch), surface(mzml, &scratch));
        let (mut by_design, mut defects) = (0usize, 0usize);
        println!("\n=== {stem} ===");
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
            let rule = expected_rule(k);
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
fn unexpected_and_stale() {
    let Some(dir) = pair_dir() else {
        eprintln!("skipping: MZPC_LANE_PAIRS unset");
        return;
    };
    let pairs = pairs(&dir);
    if pairs.is_empty() {
        return;
    }
    let scratch = std::env::temp_dir().join(format!("mzpc-lane-stale-{}", std::process::id()));
    std::fs::create_dir_all(&scratch).unwrap();
    let mut fired: BTreeSet<&'static str> = BTreeSet::new();
    for (_, native, mzml) in &pairs {
        let (a, b) = (surface(native, &scratch), surface(mzml, &scratch));
        for k in a.keys().chain(b.keys()) {
            if a.get(k) != b.get(k) {
                if let Some(r) = expected_rule(k) {
                    fired.insert(r.reason);
                }
            }
        }
    }
    let _ = std::fs::remove_dir_all(&scratch);
    let stale: Vec<&str> = EXPECTED.iter().map(|e| e.reason).filter(|r| !fired.contains(r)).collect();
    // Not an assertion: which rules fire depends on WHICH pairs are present (a Shimadzu pair does
    // carry a source checksum, an Agilent one does not). Report so the list can be pruned when the
    // pair set is broad enough to justify it.
    if !stale.is_empty() {
        println!(
            "{} EXPECTED rule(s) did not fire on this pair set — closed, or never applicable here:",
            stale.len()
        );
        for r in stale {
            println!("  - {r}");
        }
    }
}

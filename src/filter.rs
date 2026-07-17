//! mzPeak → mzPeak filtering (Phase 1 + Phase 2).
//!
//! A `.mzpeak` is a ZIP of Parquet facets + `mzpeak_index.json`. This module reads such an archive
//! and writes a NEW `.mzpeak` with a subset of spectra and/or a changed aux-member set, keeping every
//! facet mutually consistent. Two orthogonal capabilities land here:
//!
//!   * **Phase 1 — aux remove/inject.** `--drop-aux <glob>` drops matching ZIP members; the existing
//!     `--image`/`--sdrf` inject new members (reused verbatim from `embed_aux`). Pure ZIP-member
//!     add/drop + index-manifest update.
//!   * **Phase 2 — spectrum-level filters.** `--rt MIN-MAX` (keep spectra whose `spectrum.time` is in
//!     the window) and `--ms-level N[,M…]` (keep spectra whose MS level is in the set). We compute the
//!     surviving `spectrum.index` set from `spectra_metadata`, then row-filter EVERY per-spectrum
//!     facet (metadata, peaks/data, vendor trailers) down to that set. Peak columns are row-filtered,
//!     never re-valued: keeping whole spectra leaves each spectrum's per-scan/per-chunk delta chain
//!     intact. Chromatograms are truncated to the RT window (else copied). Indices are NEVER
//!     renumbered — surviving spectra keep their original (now-sparse) indices, so every
//!     `source_index`/`precursor_index` cross-reference stays valid.
//!
//! **m/z-range filtering (Phase 3) is NOT implemented** — `--mz` errors loudly.
//!
//! Facets are classified by schema INTROSPECTION, not a hard-coded name list (the format is
//! extensible): a member is per-spectrum if it has `spectrum.index`, a `point`/`chunk.spectrum_index`,
//! or a top-level `ordinal`; a chromatogram facet if it has `chromatogram.index` /
//! `chunk.chromatogram_index`; otherwise run-global (copied verbatim). A member that LOOKS
//! per-spectrum-shaped (a top-level `point`/`chunk`/`peak` struct) but carries no key we can map to
//! survivors is a hard ERROR — we never silently ship a facet that references dropped spectra.

use std::collections::{BTreeSet, HashMap};
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use arrow::array::{
    Array, ArrayRef, BooleanArray, Float64Array, StructArray, UInt64Array,
};
use arrow::compute::filter_record_batch;
use arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use arrow::record_batch::RecordBatch;
use parquet::arrow::ArrowWriter;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::basic::{Compression, Encoding, ZstdLevel};
use parquet::file::metadata::KeyValue;
use parquet::file::properties::WriterProperties;
use parquet::schema::types::ColumnPath;

use mzpeak_prototyping::archive::{DataKind, EntityType, FileEntry, ZipArchiveWriter};

/// Effective filter settings for one `.mzpeak → .mzpeak` run. Built by `main.rs` from the CLI/config.
#[derive(Debug, Default, Clone)]
pub struct FilterOpts {
    /// Keep spectra whose `spectrum.time` ∈ [min, max]. Unit matches the stored `spectrum.time`.
    pub rt: Option<(f64, f64)>,
    /// Keep spectra whose ms_level is in this set. Empty = no MS-level filter.
    pub ms_levels: Vec<u8>,
    /// Drop archive members matching any of these globs (`*`/`?` wildcards, `*` spans `/`).
    pub drop_aux: Vec<String>,
    /// Inject optical images (verbatim), reusing the forward-path embed logic.
    pub images: Vec<PathBuf>,
    /// Inject an SDRF sample-metadata TSV (verbatim).
    pub sdrf: Option<PathBuf>,
    /// Present iff `--mz` was given — Phase 3, unimplemented; we error.
    pub mz_requested: bool,
}

/// Parse an `--rt MIN-MAX` argument. Either bound may be omitted for an open range (`10-`, `-30`).
pub fn parse_rt(s: &str) -> Result<(f64, f64)> {
    let (a, b) = s
        .split_once('-')
        .ok_or_else(|| anyhow!("--rt expects MIN-MAX (e.g. 10-30); got {s:?}"))?;
    let lo = if a.trim().is_empty() {
        f64::NEG_INFINITY
    } else {
        a.trim().parse().with_context(|| format!("parsing --rt lower bound {a:?}"))?
    };
    let hi = if b.trim().is_empty() {
        f64::INFINITY
    } else {
        b.trim().parse().with_context(|| format!("parsing --rt upper bound {b:?}"))?
    };
    if lo > hi {
        bail!("--rt lower bound {lo} exceeds upper bound {hi}");
    }
    Ok((lo, hi))
}

/// True when `input` should be treated as a mzPeak archive: a `.mzpeak` extension, or any ZIP whose
/// members include `mzpeak_index.json`.
pub fn is_mzpeak_input(input: &Path) -> bool {
    if !input.is_file() {
        return false;
    }
    if input
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("mzpeak"))
    {
        return true;
    }
    // Cheap ZIP probe: does it contain the index member?
    let Ok(f) = File::open(input) else { return false };
    let Ok(mut zip) = zip::ZipArchive::new(BufReader::new(f)) else {
        return false;
    };
    zip.by_name("mzpeak_index.json").is_ok()
}

/// Inspection report for a mzPeak input: member list + spectrum / chromatogram counts. Used on the
/// no-`--output` path (and `-v`) in place of the mzdata-reader report, which cannot open a `.mzpeak`.
pub fn report_inspect(input: &Path) -> Result<()> {
    let f = File::open(input).with_context(|| format!("opening {}", input.display()))?;
    let mut zip = zip::ZipArchive::new(BufReader::new(f))
        .with_context(|| format!("reading {} as a ZIP", input.display()))?;
    println!("input:         {}", input.display());
    println!("format:        mzPeak archive");
    let names: Vec<String> = zip.file_names().map(str::to_string).collect();
    println!("members:       {}", names.len());
    for n in &names {
        println!("  - {n}");
    }
    if names.iter().any(|n| n == "spectra_metadata.parquet") {
        let bytes = read_member(&mut zip, "spectra_metadata.parquet")?;
        println!("spectra:       {}", parquet_row_count(&bytes)?);
    }
    if names.iter().any(|n| n == "chromatograms_metadata.parquet") {
        let bytes = read_member(&mut zip, "chromatograms_metadata.parquet")?;
        println!("chromatograms: {}", parquet_row_count(&bytes)?);
    }
    Ok(())
}

/// Filter `input` (a `.mzpeak`) into `output` (a new `.mzpeak`) per `opts`.
pub fn run(input: &Path, output: &Path, opts: &FilterOpts) -> Result<()> {
    if opts.mz_requested {
        bail!("--mz filtering not yet implemented");
    }

    let f = File::open(input).with_context(|| format!("opening {}", input.display()))?;
    let mut zip = zip::ZipArchive::new(BufReader::new(f))
        .with_context(|| format!("reading {} as a mzPeak ZIP", input.display()))?;

    // Parse the original index so we can carry its metadata blocks + per-member FileEntry classes.
    let index_json = read_member(&mut zip, "mzpeak_index.json")
        .context("reading mzpeak_index.json")?;
    let index: serde_json::Value = serde_json::from_slice(&index_json)
        .context("parsing mzpeak_index.json")?;
    let orig_files = index_file_entries(&index);

    let member_names: Vec<String> = zip.file_names().map(str::to_string).collect();

    // ── survivors ───────────────────────────────────────────────────────────────────────────────
    // Read spectra_metadata once: compute the surviving spectrum-index set + the dangling-precursor
    // count. Metadata is one row per spectrum, so it is small relative to the peak facets.
    let filtering_spectra = opts.rt.is_some() || !opts.ms_levels.is_empty();
    let meta_bytes = read_member(&mut zip, "spectra_metadata.parquet")
        .context("reading spectra_metadata.parquet")?;
    let (survivors, total_spectra, dangling) =
        compute_survivors(&meta_bytes, opts).context("computing surviving spectra")?;
    if filtering_spectra {
        log::info!(
            "filter: keeping {}/{} spectra",
            survivors.len(),
            total_spectra
        );
        if dangling > 0 {
            log::warn!(
                "{dangling} fragment spectra now reference a filtered-out precursor"
            );
        }
    }

    // ── chromatogram truncation prepass ─────────────────────────────────────────────────────────
    // Under --rt, learn each chromatogram's surviving point count so we can also refresh the
    // per-chromatogram number_of_data_points field in chromatograms_metadata.
    let chrom_counts: Option<HashMap<u64, u64>> = if opts.rt.is_some()
        && member_names.iter().any(|n| n == "chromatograms_data.parquet")
    {
        let cd = read_member(&mut zip, "chromatograms_data.parquet")?;
        Some(chromatogram_point_counts(&cd, opts.rt.unwrap())?)
    } else {
        None
    };

    // ── write the output archive ────────────────────────────────────────────────────────────────
    if let Some(parent) = output.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).ok();
        }
    }
    let out = File::create(output).with_context(|| format!("creating {}", output.display()))?;
    let mut w = ZipArchiveWriter::new(out);

    let drop_globs = &opts.drop_aux;
    let mut dropped: Vec<String> = Vec::new();

    for name in &member_names {
        if name == "mzpeak_index.json" {
            continue; // regenerated at finish()
        }
        if drop_globs.iter().any(|g| glob_match(g, name)) {
            dropped.push(name.clone());
            continue;
        }
        let fe = orig_files
            .get(name)
            .cloned()
            .unwrap_or_else(|| synthesize_entry(name));

        if name.ends_with(".parquet") {
            let bytes = read_member(&mut zip, name)?;
            let class = classify_facet(&bytes)
                .with_context(|| format!("classifying facet {name}"))?;
            let out_bytes = process_parquet(
                &bytes,
                class,
                &survivors,
                filtering_spectra,
                opts,
                chrom_counts.as_ref(),
            )
            .with_context(|| format!("filtering facet {name}"))?;
            w.start_for_entry(fe)
                .map_err(|e| anyhow!("starting member {name}: {e}"))?;
            use std::io::Write as _;
            w.write_all(&out_bytes)
                .with_context(|| format!("writing member {name}"))?;
        } else {
            // Non-Parquet member (vendor side-file, image, sdrf, …): copy verbatim, streamed.
            let mut src = zip
                .by_name(name)
                .with_context(|| format!("opening member {name}"))?;
            w.add_file_from_read(&mut src, None::<&String>, Some(fe))
                .with_context(|| format!("copying member {name}"))?;
        }
    }

    // ── injection (Phase 1) ─────────────────────────────────────────────────────────────────────
    // Reuse the forward-path verbatim embed. `input` (the source mzpeak) drives sibling discovery /
    // imzML-grid reads; for a non-imzML mzpeak the grid is unknown, so a strict --image without a
    // grid will error there (documented). --sdrf needs no grid.
    let mut injected: Vec<String> = Vec::new();
    if !opts.images.is_empty() || opts.sdrf.is_some() {
        crate::embed_aux::embed_into_archive(&mut w, input, &opts.images, opts.sdrf.as_deref())
            .context("injecting aux members")?;
        for img in &opts.images {
            injected.push(img.file_name().and_then(|n| n.to_str()).unwrap_or("image").to_string());
        }
        if opts.sdrf.is_some() {
            injected.push("sample_metadata/sdrf.tsv".to_string());
        }
    }

    // ── index metadata: carry the originals, add filter provenance ──────────────────────────────
    carry_index_metadata(&mut w, &index, opts, input, &dropped, &injected)?;

    w.finish().map_err(|e| anyhow!("finalizing {}: {e}", output.display()))?;
    Ok(())
}

// ══════════════════════════════════════════════════════════════════════════════════════════════
// Survivors + dangling precursors
// ══════════════════════════════════════════════════════════════════════════════════════════════

/// Read `spectra_metadata`, apply the RT / MS-level predicates, and return
/// `(surviving spectrum.index set, total spectra, dangling-precursor count)`.
///
/// A "dangling precursor" is a SURVIVING fragment spectrum (ms_level ≥ 2) whose `precursor.precursor_id`
/// resolves (via `spectrum.id`) to a spectrum that did NOT survive. We count distinct such fragments.
/// `precursor_id` is the reliable cross-converter link — `precursor.source_index` is often null or,
/// on some native readers, points at the source scan numbering rather than the precursor's `index`.
fn compute_survivors(
    meta_bytes: &[u8],
    opts: &FilterOpts,
) -> Result<(BTreeSet<u64>, usize, usize)> {
    let builder = ParquetRecordBatchReaderBuilder::try_new(bytes_of(meta_bytes))
        .context("opening spectra_metadata")?;
    let reader = builder.build()?;

    let mut survivors: BTreeSet<u64> = BTreeSet::new();
    let mut total = 0usize;
    // id → index for every original spectrum (to resolve precursor_id references).
    let mut id_to_index: HashMap<String, u64> = HashMap::new();
    // precursor_id of each SURVIVING fragment (ms_level ≥ 2 with a non-empty precursor_id).
    let mut surviving_frag_pids: Vec<String> = Vec::new();

    let ms_set: BTreeSet<u8> = opts.ms_levels.iter().copied().collect();

    for batch in reader {
        let batch = batch?;
        let spectrum = struct_col(&batch, "spectrum")
            .ok_or_else(|| anyhow!("spectra_metadata has no `spectrum` struct column"))?;
        let index = u64_child(spectrum, "index")
            .ok_or_else(|| anyhow!("spectrum.index missing/!uint64"))?;
        let ids = lstr_child(spectrum, "id");
        let time = f64_child(spectrum, "time");
        let ms = u8_child(spectrum, "MS_1000511_ms_level");
        let precursor = struct_col(&batch, "precursor");
        let pids = precursor.and_then(|p| lstr_child(p, "precursor_id"));

        for row in 0..batch.num_rows() {
            total += 1;
            let idx = index.value(row);
            if let Some(ids) = ids {
                if ids.is_valid(row) {
                    id_to_index.insert(ids.value(row).to_string(), idx);
                }
            }
            let level = ms.map(|m| m.value(row)).unwrap_or(0);
            let mut keep = true;
            if let Some((lo, hi)) = opts.rt {
                let t = time.map(|t| t.value(row)).unwrap_or(f64::NAN);
                keep &= t >= lo && t <= hi;
            }
            if !ms_set.is_empty() {
                keep &= ms_set.contains(&level);
            }
            if keep {
                survivors.insert(idx);
                if level >= 2 {
                    if let Some(pids) = pids {
                        if pids.is_valid(row) {
                            let pid = pids.value(row);
                            if !pid.is_empty() {
                                surviving_frag_pids.push(pid.to_string());
                            }
                        }
                    }
                }
            }
        }
    }

    // A fragment is dangling if its precursor_id resolves to a known spectrum that is NOT a survivor.
    // (Unresolvable precursor_ids are ignored — we cannot tell whether they dangle.)
    let dangling = surviving_frag_pids
        .iter()
        .filter(|pid| {
            id_to_index
                .get(pid.as_str())
                .is_some_and(|src| !survivors.contains(src))
        })
        .count();

    Ok((survivors, total, dangling))
}

// ══════════════════════════════════════════════════════════════════════════════════════════════
// Facet classification (schema introspection)
// ══════════════════════════════════════════════════════════════════════════════════════════════

#[derive(Debug, Clone)]
enum Facet {
    /// `spectra_metadata`: top-level `spectrum` struct keyed by `spectrum.index`.
    SpectrumMeta,
    /// `spectra_peaks` / `spectra_data`: one top-level struct (`point`/`chunk`) with a
    /// `spectrum_index` child. Carries the struct field name.
    SpectrumData(String),
    /// `chromatograms_metadata`: top-level `chromatogram` struct keyed by `chromatogram.index`.
    ChromatogramMeta,
    /// `chromatograms_data`: a top-level struct with a `chromatogram_index` child + a `time` child.
    ChromatogramData(String),
    /// Per-spectrum vendor facet keyed by a top-level `ordinal` column (scan trailers / wide).
    VendorOrdinal,
    /// No spectrum linkage — copy verbatim (run-global vendor status log, etc.).
    RunGlobal,
}

/// Classify a Parquet facet by its arrow schema. Errors when a facet LOOKS per-spectrum-shaped (a
/// top-level `point`/`chunk`/`peak` struct) but has no key we can map to survivors.
fn classify_facet(bytes: &[u8]) -> Result<Facet> {
    let schema = parquet_schema(bytes)?;
    // Peaks/data: a struct field with a `spectrum_index` child.
    for f in schema.fields() {
        if let DataType::Struct(children) = f.data_type() {
            if children.iter().any(|c| c.name() == "spectrum_index") {
                return Ok(Facet::SpectrumData(f.name().clone()));
            }
        }
    }
    // Chromatogram data: a struct with `chromatogram_index`.
    for f in schema.fields() {
        if let DataType::Struct(children) = f.data_type() {
            if children.iter().any(|c| c.name() == "chromatogram_index") {
                return Ok(Facet::ChromatogramData(f.name().clone()));
            }
        }
    }
    // Spectrum metadata: `spectrum` struct with `index`.
    if let Some(f) = schema.column_with_name("spectrum") {
        if let DataType::Struct(children) = f.1.data_type() {
            if children.iter().any(|c| c.name() == "index") {
                return Ok(Facet::SpectrumMeta);
            }
        }
    }
    // Chromatogram metadata: `chromatogram` struct with `index`.
    if let Some(f) = schema.column_with_name("chromatogram") {
        if let DataType::Struct(children) = f.1.data_type() {
            if children.iter().any(|c| c.name() == "index") {
                return Ok(Facet::ChromatogramMeta);
            }
        }
    }
    // Vendor trailers keyed by a top-level `ordinal`.
    if schema.column_with_name("ordinal").is_some() {
        return Ok(Facet::VendorOrdinal);
    }
    // Refuse: a data-shaped top-level struct with no recognizable spectrum key.
    for f in schema.fields() {
        if matches!(f.name().as_str(), "point" | "chunk" | "peak") {
            bail!(
                "unrecognized per-spectrum facet (top-level `{}` struct has no spectrum_index / \
                 chromatogram_index key); refusing to produce an inconsistent file",
                f.name()
            );
        }
    }
    Ok(Facet::RunGlobal)
}

// ══════════════════════════════════════════════════════════════════════════════════════════════
// Per-facet processing
// ══════════════════════════════════════════════════════════════════════════════════════════════

/// Filter/copy one Parquet facet and return the re-encoded bytes.
fn process_parquet(
    bytes: &[u8],
    class: Facet,
    survivors: &BTreeSet<u64>,
    filtering_spectra: bool,
    opts: &FilterOpts,
    chrom_counts: Option<&HashMap<u64, u64>>,
) -> Result<Vec<u8>> {
    match class {
        Facet::RunGlobal => Ok(bytes.to_vec()), // verbatim
        Facet::SpectrumMeta => {
            if !filtering_spectra {
                reencode(bytes, |b| Ok(Some(b.clone())), CountMode::SpectrumMeta)
            } else {
                let surv = survivors.clone();
                reencode(
                    bytes,
                    move |b| filter_by_struct_key(b, "spectrum", "index", &surv, true),
                    CountMode::SpectrumMeta,
                )
            }
        }
        Facet::SpectrumData(field) => {
            if !filtering_spectra {
                reencode(bytes, |b| Ok(Some(b.clone())), CountMode::SpectrumData(field.clone()))
            } else {
                let surv = survivors.clone();
                let f2 = field.clone();
                reencode(
                    bytes,
                    move |b| filter_by_struct_key(b, &f2, "spectrum_index", &surv, false),
                    CountMode::SpectrumData(field),
                )
            }
        }
        Facet::VendorOrdinal => {
            if !filtering_spectra {
                reencode(bytes, |b| Ok(Some(b.clone())), CountMode::Vendor)
            } else {
                let surv = survivors.clone();
                reencode(
                    bytes,
                    move |b| filter_by_top_key(b, "ordinal", &surv),
                    CountMode::Vendor,
                )
            }
        }
        Facet::ChromatogramData(field) => {
            let rt = opts.rt;
            let f2 = field.clone();
            reencode(
                bytes,
                move |b| {
                    if let Some((lo, hi)) = rt {
                        filter_chromatogram_time(b, &f2, lo, hi)
                    } else {
                        Ok(Some(b.clone()))
                    }
                },
                CountMode::ChromatogramData(field),
            )
        }
        Facet::ChromatogramMeta => {
            // Only rewrite the per-chromatogram point counts under --rt; else copy through re-encode.
            let counts = chrom_counts.cloned();
            reencode(
                bytes,
                move |b| {
                    if let Some(cnts) = &counts {
                        Ok(Some(refresh_chrom_point_counts(b, cnts)?))
                    } else {
                        Ok(Some(b.clone()))
                    }
                },
                CountMode::ChromatogramMeta,
            )
        }
    }
}

/// How to recompute the per-facet count KVs after filtering.
enum CountMode {
    SpectrumMeta,
    SpectrumData(String),
    Vendor,
    ChromatogramMeta,
    ChromatogramData(String),
}

/// Stream a Parquet facet through `map` (batch → optional filtered batch), re-encoding to zstd with
/// the original key-value metadata preserved (minus ARROW:schema and the recomputed counts) and the
/// counts refreshed via `append_key_value_metadata`. Peak-column encodings are matched best-effort;
/// on any encoding incompatibility we retry the whole facet with plain zstd (correctness first).
fn reencode<F>(bytes: &[u8], map: F, mode: CountMode) -> Result<Vec<u8>>
where
    F: Fn(&RecordBatch) -> Result<Option<RecordBatch>>,
{
    match reencode_inner(bytes, &map, &mode, true) {
        Ok(v) => Ok(v),
        Err(_) => reencode_inner(bytes, &map, &mode, false),
    }
}

fn reencode_inner<F>(bytes: &[u8], map: &F, mode: &CountMode, fancy: bool) -> Result<Vec<u8>>
where
    F: Fn(&RecordBatch) -> Result<Option<RecordBatch>>,
{
    let builder = ParquetRecordBatchReaderBuilder::try_new(bytes_of(bytes))?;
    let schema: SchemaRef = builder.schema().clone();
    let orig_kv: Vec<KeyValue> = builder
        .metadata()
        .file_metadata()
        .key_value_metadata()
        .map(|v| v.clone())
        .unwrap_or_default();
    let reader = builder.build()?;

    // Preserve original KV except ARROW:schema (regenerated) and the count keys we recompute.
    let count_keys = ["spectrum_count", "spectrum_data_point_count", "chromatogram_count", "chromatogram_data_point_count"];
    let preserved: Vec<KeyValue> = orig_kv
        .into_iter()
        .filter(|kv| kv.key != "ARROW:schema" && !count_keys.contains(&kv.key.as_str()))
        .collect();

    let mut props = WriterProperties::builder()
        .set_compression(Compression::ZSTD(ZstdLevel::try_new(5).unwrap()))
        .set_key_value_metadata(Some(preserved));
    if fancy {
        props = apply_encodings(props, &schema);
    }
    let props = props.build();

    let mut buf: Vec<u8> = Vec::new();
    let mut rows: u64 = 0;
    let mut points: u64 = 0;
    let mut keys: BTreeSet<u64> = BTreeSet::new();
    {
        let mut writer = ArrowWriter::try_new(&mut buf, schema.clone(), Some(props))?;
        for batch in reader {
            let batch = batch?;
            let Some(out) = map(&batch)? else { continue };
            if out.num_rows() == 0 {
                continue;
            }
            accumulate_counts(&out, mode, &mut rows, &mut points, &mut keys);
            writer.write(&out)?;
        }
        // Append the recomputed counts as footer KV.
        for (k, v) in count_kvs(mode, rows, points, &keys) {
            writer.append_key_value_metadata(KeyValue::new(k, v));
        }
        writer.close()?;
    }
    Ok(buf)
}

/// Accumulate row / data-point / distinct-key counts from a filtered batch per `CountMode`.
fn accumulate_counts(
    batch: &RecordBatch,
    mode: &CountMode,
    rows: &mut u64,
    points: &mut u64,
    keys: &mut BTreeSet<u64>,
) {
    *rows += batch.num_rows() as u64;
    match mode {
        CountMode::SpectrumMeta => {
            if let Some(s) = struct_col(batch, "spectrum") {
                if let Some(idx) = u64_child(s, "index") {
                    for r in 0..idx.len() {
                        keys.insert(idx.value(r));
                    }
                }
            }
        }
        CountMode::SpectrumData(field) => {
            if let Some(s) = struct_col(batch, field) {
                if let Some(idx) = u64_child(s, "spectrum_index") {
                    for r in 0..idx.len() {
                        keys.insert(idx.value(r));
                    }
                }
                // point layout: one row per data point; chunk layout: sum the intensity-list items.
                match intensity_list_len(s) {
                    Some(n) => *points += n,
                    None => *points += batch.num_rows() as u64,
                }
            }
        }
        CountMode::Vendor => {
            if let Some(idx) = batch
                .column_by_name("ordinal")
                .and_then(|a| to_u64(a))
            {
                for r in 0..idx.len() {
                    keys.insert(idx.value(r));
                }
            }
        }
        CountMode::ChromatogramData(field) => {
            if let Some(s) = struct_col(batch, field) {
                if let Some(idx) = u64_child(s, "chromatogram_index") {
                    for r in 0..idx.len() {
                        keys.insert(idx.value(r));
                    }
                }
                // point layout → one point per row; chunk layout → sum of the intensity list lengths.
                match intensity_list_len(s) {
                    Some(n) => *points += n,
                    None => *points += batch.num_rows() as u64,
                }
            }
        }
        CountMode::ChromatogramMeta => {}
    }
}

/// Total item-count of the `intensity` list child in a `chunk`-style struct (the data-point count for
/// a chunk-layout facet). `intensity` is always a populated plain list, unlike the m/z/time value list
/// which may be empty when a numpress transform is active. Returns None for point-layout structs
/// (scalar `intensity`), where the data-point count is the row count.
fn intensity_list_len(s: &StructArray) -> Option<u64> {
    let col = s.column_by_name("intensity")?;
    match col.data_type() {
        DataType::LargeList(_) => {
            let l = col.as_any().downcast_ref::<arrow::array::LargeListArray>()?;
            Some((l.value_offsets().last().copied().unwrap_or(0)
                - l.value_offsets().first().copied().unwrap_or(0)) as u64)
        }
        DataType::List(_) => {
            let l = col.as_any().downcast_ref::<arrow::array::ListArray>()?;
            Some((l.value_offsets().last().copied().unwrap_or(0)
                - l.value_offsets().first().copied().unwrap_or(0)) as u64)
        }
        _ => None,
    }
}

/// The `intensity`-list length of a single chunk row (chunk-layout point count for that chunk).
fn intensity_row_len(s: &StructArray, row: usize) -> u64 {
    let Some(col) = s.column_by_name("intensity") else { return 1 };
    if let Some(l) = col.as_any().downcast_ref::<arrow::array::LargeListArray>() {
        (l.value_offsets()[row + 1] - l.value_offsets()[row]) as u64
    } else if let Some(l) = col.as_any().downcast_ref::<arrow::array::ListArray>() {
        (l.value_offsets()[row + 1] - l.value_offsets()[row]) as u64
    } else {
        1
    }
}

/// The count KV pairs to append to the footer for this facet.
fn count_kvs(mode: &CountMode, rows: u64, points: u64, keys: &BTreeSet<u64>) -> Vec<(String, String)> {
    match mode {
        CountMode::SpectrumMeta => vec![("spectrum_count".into(), rows.to_string())],
        CountMode::SpectrumData(_) => vec![
            ("spectrum_count".into(), keys.len().to_string()),
            ("spectrum_data_point_count".into(), points.to_string()),
        ],
        CountMode::Vendor => vec![("spectrum_count".into(), keys.len().to_string())],
        CountMode::ChromatogramMeta => vec![("chromatogram_count".into(), rows.to_string())],
        CountMode::ChromatogramData(_) => vec![
            ("chromatogram_count".into(), keys.len().to_string()),
            ("chromatogram_data_point_count".into(), points.to_string()),
        ],
    }
}

// ══════════════════════════════════════════════════════════════════════════════════════════════
// Row filters
// ══════════════════════════════════════════════════════════════════════════════════════════════

/// Keep rows whose `<field>.<key>` (a uint64 child) is in `survivors`. `keep_null_key` decides rows
/// whose key is null (metadata keeps them defensively; data facets drop them — they must have a key).
fn filter_by_struct_key(
    batch: &RecordBatch,
    field: &str,
    key: &str,
    survivors: &BTreeSet<u64>,
    keep_null_key: bool,
) -> Result<Option<RecordBatch>> {
    let s = struct_col(batch, field)
        .ok_or_else(|| anyhow!("expected `{field}` struct column"))?;
    let idx = u64_child(s, key)
        .ok_or_else(|| anyhow!("expected uint64 `{field}.{key}`"))?;
    let mask: BooleanArray = (0..idx.len())
        .map(|r| {
            if idx.is_null(r) {
                Some(keep_null_key)
            } else {
                Some(survivors.contains(&idx.value(r)))
            }
        })
        .collect();
    Ok(Some(filter_record_batch(batch, &mask)?))
}

/// Keep rows whose top-level `key` column (coerced to u64) is in `survivors`.
fn filter_by_top_key(
    batch: &RecordBatch,
    key: &str,
    survivors: &BTreeSet<u64>,
) -> Result<Option<RecordBatch>> {
    let col = batch
        .column_by_name(key)
        .ok_or_else(|| anyhow!("expected `{key}` column"))?;
    let idx = to_u64(col).ok_or_else(|| anyhow!("`{key}` is not an integer column"))?;
    let mask: BooleanArray = (0..idx.len())
        .map(|r| Some(!idx.is_null(r) && survivors.contains(&idx.value(r))))
        .collect();
    Ok(Some(filter_record_batch(batch, &mask)?))
}

/// Truncate chromatogram data to the RT window [lo, hi]. Two layouts:
///   * **point** (`<field>.time` double): keep rows whose time is in the window.
///   * **chunk** (`<field>.time_chunk_start`/`_end`): keep whole chunk rows that OVERLAP the window
///     (chunk-granularity truncation — we never edit chunk list contents, mirroring the whole-spectrum
///     peak policy). A layout with neither is copied unchanged (cannot locate a time axis).
fn filter_chromatogram_time(
    batch: &RecordBatch,
    field: &str,
    lo: f64,
    hi: f64,
) -> Result<Option<RecordBatch>> {
    let s = struct_col(batch, field)
        .ok_or_else(|| anyhow!("expected `{field}` struct column"))?;
    if let Some(time) = f64_child(s, "time") {
        let mask: BooleanArray = (0..time.len())
            .map(|r| Some(!time.is_null(r) && time.value(r) >= lo && time.value(r) <= hi))
            .collect();
        return Ok(Some(filter_record_batch(batch, &mask)?));
    }
    if let (Some(start), Some(end)) = (f64_child(s, "time_chunk_start"), f64_child(s, "time_chunk_end")) {
        let mask: BooleanArray = (0..start.len())
            .map(|r| Some(end.value(r) >= lo && start.value(r) <= hi))
            .collect();
        return Ok(Some(filter_record_batch(batch, &mask)?));
    }
    log::warn!("chromatograms: no recognizable time axis in `{field}`; copying unchanged");
    Ok(Some(batch.clone()))
}

// ══════════════════════════════════════════════════════════════════════════════════════════════
// Chromatogram per-chromatogram point-count refresh
// ══════════════════════════════════════════════════════════════════════════════════════════════

/// Count each chromatogram's surviving points after the RT window truncation (chromatogram_index →
/// count). Reads `chromatograms_data` once.
fn chromatogram_point_counts(bytes: &[u8], (lo, hi): (f64, f64)) -> Result<HashMap<u64, u64>> {
    let builder = ParquetRecordBatchReaderBuilder::try_new(bytes_of(bytes))?;
    let field = {
        let sch = builder.schema();
        sch.fields()
            .iter()
            .find(|f| matches!(f.data_type(), DataType::Struct(c) if c.iter().any(|x| x.name()=="chromatogram_index")))
            .map(|f| f.name().clone())
            .ok_or_else(|| anyhow!("chromatograms_data has no chromatogram_index struct"))?
    };
    let reader = builder.build()?;
    let mut counts: HashMap<u64, u64> = HashMap::new();
    for batch in reader {
        let batch = batch?;
        let s = struct_col(&batch, &field).unwrap();
        let idx = u64_child(s, "chromatogram_index").unwrap();
        if let Some(time) = f64_child(s, "time") {
            // Point layout: one surviving point per in-window row.
            for r in 0..idx.len() {
                if time.value(r) >= lo && time.value(r) <= hi {
                    *counts.entry(idx.value(r)).or_insert(0) += 1;
                }
            }
        } else if let (Some(start), Some(end)) =
            (f64_child(s, "time_chunk_start"), f64_child(s, "time_chunk_end"))
        {
            // Chunk layout: a kept (overlapping) chunk contributes its whole intensity-list length.
            for r in 0..idx.len() {
                if end.value(r) >= lo && start.value(r) <= hi {
                    *counts.entry(idx.value(r)).or_insert(0) += intensity_row_len(s, r);
                }
            }
        }
    }
    Ok(counts)
}

/// Rebuild `chromatograms_metadata` with the `chromatogram.MS_1003060_number_of_data_points` child
/// refreshed from `counts`.
fn refresh_chrom_point_counts(
    batch: &RecordBatch,
    counts: &HashMap<u64, u64>,
) -> Result<RecordBatch> {
    let field_name = "chromatogram";
    let Some(col_idx) = batch.schema().index_of(field_name).ok() else {
        return Ok(batch.clone());
    };
    let s = batch
        .column(col_idx)
        .as_any()
        .downcast_ref::<StructArray>()
        .ok_or_else(|| anyhow!("chromatogram column is not a struct"))?;
    let idx = u64_child(s, "index").ok_or_else(|| anyhow!("chromatogram.index missing"))?;
    let child = "MS_1003060_number_of_data_points";
    let Some((child_pos, _)) = s.fields().iter().enumerate().find(|(_, f)| f.name() == child) else {
        return Ok(batch.clone());
    };
    let new_counts: UInt64Array = (0..idx.len())
        .map(|r| counts.get(&idx.value(r)).copied().or(Some(0)))
        .collect();
    let new_struct = replace_struct_child(s, child_pos, Arc::new(new_counts) as ArrayRef)?;
    let mut cols: Vec<ArrayRef> = batch.columns().to_vec();
    cols[col_idx] = Arc::new(new_struct);
    Ok(RecordBatch::try_new(batch.schema(), cols)?)
}

/// Return a new StructArray with child `pos` replaced (same fields, nulls).
fn replace_struct_child(s: &StructArray, pos: usize, new_child: ArrayRef) -> Result<StructArray> {
    let fields = match s.data_type() {
        DataType::Struct(f) => f.clone(),
        _ => bail!("not a struct"),
    };
    let mut arrays: Vec<ArrayRef> = (0..s.num_columns()).map(|i| s.column(i).clone()).collect();
    arrays[pos] = new_child;
    let pairs: Vec<(Arc<Field>, ArrayRef)> = fields.iter().cloned().zip(arrays).collect();
    Ok(StructArray::try_new(
        fields,
        pairs.into_iter().map(|(_, a)| a).collect(),
        s.nulls().cloned(),
    )?)
}

// ══════════════════════════════════════════════════════════════════════════════════════════════
// Encodings, index, helpers
// ══════════════════════════════════════════════════════════════════════════════════════════════

/// Best-effort match of the mzPeak peak-column encodings: DELTA_BINARY_PACKED on integer `*_index`
/// direct struct children + top-level `ordinal`; BYTE_STREAM_SPLIT on `tof`/`intensity` primitives.
/// Applied only to direct (non-list) leaves of `point`/`chunk` structs and the vendor `ordinal`.
fn apply_encodings(
    mut props: parquet::file::properties::WriterPropertiesBuilder,
    schema: &Schema,
) -> parquet::file::properties::WriterPropertiesBuilder {
    for f in schema.fields() {
        if f.name() == "ordinal" && is_int(f.data_type()) {
            props = props.set_column_encoding(ColumnPath::from(vec![f.name().clone()]), Encoding::DELTA_BINARY_PACKED);
        }
        if let DataType::Struct(children) = f.data_type() {
            if !matches!(f.name().as_str(), "point" | "chunk" | "peak") {
                continue;
            }
            for c in children.iter() {
                let path = ColumnPath::from(vec![f.name().clone(), c.name().clone()]);
                let leaf = c.name().as_str();
                if leaf.ends_with("_index") && is_int(c.data_type()) {
                    props = props.set_column_encoding(path, Encoding::DELTA_BINARY_PACKED);
                } else if (leaf == "tof" || leaf == "intensity") && is_primitive_numeric(c.data_type()) {
                    props = props.set_column_encoding(path, Encoding::BYTE_STREAM_SPLIT);
                }
            }
        }
    }
    props
}

fn is_int(dt: &DataType) -> bool {
    matches!(
        dt,
        DataType::Int8 | DataType::Int16 | DataType::Int32 | DataType::Int64
            | DataType::UInt8 | DataType::UInt16 | DataType::UInt32 | DataType::UInt64
    )
}

fn is_primitive_numeric(dt: &DataType) -> bool {
    is_int(dt) || matches!(dt, DataType::Float32 | DataType::Float64)
}

/// Carry the original index `metadata` blocks into `w`, add a `data_processing` entry, and add the
/// `filter` provenance block. `ims_calibration` and every other block are preserved verbatim.
fn carry_index_metadata(
    w: &mut ZipArchiveWriter<File>,
    index: &serde_json::Value,
    opts: &FilterOpts,
    input: &Path,
    dropped: &[String],
    injected: &[String],
) -> Result<()> {
    if let Some(meta) = index.get("metadata").and_then(|m| m.as_object()) {
        for (k, v) in meta {
            // data_processing_method_list gets an appended entry; everything else verbatim.
            if k == "data_processing_method_list" {
                let mut list = v.clone();
                if let Some(arr) = list.as_array_mut() {
                    arr.push(filter_processing_entry(opts));
                }
                w.add_index_metadata(k, &list).map_err(|e| anyhow!("index metadata {k}: {e}"))?;
            } else {
                w.add_index_metadata(k, v).map_err(|e| anyhow!("index metadata {k}: {e}"))?;
            }
        }
    }
    let provenance = serde_json::json!({
        "source": input.file_name().and_then(|n| n.to_str()).unwrap_or(""),
        "rt": opts.rt.map(|(a, b)| serde_json::json!([a, b])),
        "ms_level": opts.ms_levels,
        "dropped_aux": dropped,
        "injected_aux": injected,
        "tool_version": env!("CARGO_PKG_VERSION"),
    });
    w.add_index_metadata("filter", &provenance)
        .map_err(|e| anyhow!("index metadata filter: {e}"))?;
    Ok(())
}

/// An mzML-style data_processing method entry describing this filter operation.
fn filter_processing_entry(opts: &FilterOpts) -> serde_json::Value {
    let mut desc = String::from("mzpeak-convert filter");
    if let Some((a, b)) = opts.rt {
        desc.push_str(&format!(" rt={a}-{b}"));
    }
    if !opts.ms_levels.is_empty() {
        desc.push_str(&format!(" ms_level={:?}", opts.ms_levels));
    }
    if !opts.drop_aux.is_empty() {
        desc.push_str(&format!(" drop_aux={:?}", opts.drop_aux));
    }
    serde_json::json!({
        "id": "mzpeak_convert_filter",
        "methods": [{
            "order": 1,
            "software_reference": "mzpeak-convert",
            "parameters": [
                {"accession": null, "name": "filter options", "unit": null, "value": desc},
                {"accession": "MS:1001486", "name": "data filtering", "unit": null, "value": null}
            ]
        }]
    })
}

/// Map member name → FileEntry from the original index `files` list.
fn index_file_entries(index: &serde_json::Value) -> HashMap<String, FileEntry> {
    let mut map = HashMap::new();
    if let Some(files) = index.get("files").and_then(|f| f.as_array()) {
        for f in files {
            if let Ok(entry) = serde_json::from_value::<FileEntry>(f.clone()) {
                map.insert(entry.name.clone(), entry);
            }
        }
    }
    map
}

/// Synthesize a FileEntry for a member absent from the original index files list.
fn synthesize_entry(name: &str) -> FileEntry {
    if name.ends_with(".parquet") {
        FileEntry::new(name.to_string(), EntityType::Other("other".into()), DataKind::Other("other".into()))
    } else {
        FileEntry::new(name.to_string(), EntityType::Other("other".into()), DataKind::Proprietary)
    }
}

// ── glob ────────────────────────────────────────────────────────────────────────────────────────

/// Minimal wildcard match: `*` matches any run (including `/`), `?` matches one char. Anchored.
fn glob_match(pat: &str, text: &str) -> bool {
    let p: Vec<char> = pat.chars().collect();
    let t: Vec<char> = text.chars().collect();
    fn m(p: &[char], t: &[char]) -> bool {
        match p.first() {
            None => t.is_empty(),
            Some('*') => m(&p[1..], t) || (!t.is_empty() && m(p, &t[1..])),
            Some('?') => !t.is_empty() && m(&p[1..], &t[1..]),
            Some(&c) => !t.is_empty() && t[0] == c && m(&p[1..], &t[1..]),
        }
    }
    m(&p, &t)
}

// ── parquet / arrow small helpers ────────────────────────────────────────────────────────────────

fn bytes_of(b: &[u8]) -> bytes::Bytes {
    bytes::Bytes::copy_from_slice(b)
}

fn read_member<R: Read + std::io::Seek>(zip: &mut zip::ZipArchive<R>, name: &str) -> Result<Vec<u8>> {
    let mut f = zip
        .by_name(name)
        .with_context(|| format!("member {name} not found"))?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)?;
    Ok(buf)
}

fn parquet_schema(bytes: &[u8]) -> Result<SchemaRef> {
    let builder = ParquetRecordBatchReaderBuilder::try_new(bytes_of(bytes))?;
    Ok(builder.schema().clone())
}

fn parquet_row_count(bytes: &[u8]) -> Result<i64> {
    let builder = ParquetRecordBatchReaderBuilder::try_new(bytes_of(bytes))?;
    Ok(builder.metadata().file_metadata().num_rows())
}

fn struct_col<'a>(batch: &'a RecordBatch, name: &str) -> Option<&'a StructArray> {
    batch.column_by_name(name)?.as_any().downcast_ref::<StructArray>()
}

fn u64_child<'a>(s: &'a StructArray, name: &str) -> Option<&'a UInt64Array> {
    s.column_by_name(name)?.as_any().downcast_ref::<UInt64Array>()
}

fn f64_child<'a>(s: &'a StructArray, name: &str) -> Option<&'a Float64Array> {
    s.column_by_name(name)?.as_any().downcast_ref::<Float64Array>()
}

fn u8_child<'a>(s: &'a StructArray, name: &str) -> Option<&'a arrow::array::UInt8Array> {
    s.column_by_name(name)?.as_any().downcast_ref::<arrow::array::UInt8Array>()
}

fn lstr_child<'a>(s: &'a StructArray, name: &str) -> Option<&'a arrow::array::LargeStringArray> {
    s.column_by_name(name)?.as_any().downcast_ref::<arrow::array::LargeStringArray>()
}

/// Coerce any integer array to a UInt64Array (owned) for uniform key handling.
fn to_u64(a: &ArrayRef) -> Option<UInt64Array> {
    use arrow::compute::cast;
    let out = cast(a, &DataType::UInt64).ok()?;
    out.as_any().downcast_ref::<UInt64Array>().cloned()
}

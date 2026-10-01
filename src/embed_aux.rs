//! FORWARD-PATH optical-image + SDRF embedding for the mzML/imzML convert path.
//!
//! Ported from the `mzML2mzPeak` prototype (`src/write/image.rs`, `src/write/convert.rs`,
//! `src/schema/optical.rs`, `src/sdrf/embed.rs`, `src/schema/study.rs`) so the SAME readers /
//! validator recognize this converter's output. The archive member paths and index-block JSON
//! keys/field names are matched EXACTLY against the prototype:
//!
//!   * Optical images → ZIP members `images/image_{ordinal:04}.<ext>` (0-based ordinal is the only
//!     attacker-uncontrolled part of the name; the source basename never reaches the archive path).
//!     Per-image descriptive metadata → the `metadata.imaging` index block's `images[]` array, with
//!     fields `archive_path`, `source_name`, `media_type`, `width`, `height`, `sha256`,
//!     `size_bytes`, `affine` (`{type:"affine", matrix:[6], maps:"image_px -> ms_px",
//!     registration_quality:"assumed_full_extent"}`), and `role:"optical"`. The affine is the
//!     imaging profile's: 0-based image pixel centres to 1-based MS pixel centres, here with the
//!     image's extent laid on the grid's ([`full_extent_affine`]). A Bruker MALDI archive acquired
//!     from a FlexImaging sequence embeds the sequence's own image with the registration its
//!     teach points fix ([`embed_sequence_image`], `registration_quality:"teach_points"`, the
//!     registration record beside the affine), places a later `--image` in that image's pixel
//!     frame the same way, and gives any other image NO affine ([`unregistered_reason`]): that
//!     grid is the bounding box of the acquired regions and the photo is of the whole target.
//!   * SDRF → ZIP member `sample_metadata/sdrf.tsv` (fixed name), entity_type `"sample-metadata"`,
//!     data_kind `"sdrf"`. Back-refs: `metadata.study` (`{dataset_accession, title,
//!     sample_metadata_ref}`) + `metadata.sample_metadata` (`{member, sha256, size_bytes,
//!     precedence:"repo_wins", embed_scope:"full", dataset_accession}`).
//!
//! Only the FORWARD verbatim-embed + back-ref paths are ported: no SDRF parse / match /
//! factor-values, and image auto-discovery is best-effort (explicit `--image` plus a sibling
//! `<stem>-opticalimage.{tif,tiff,png,jpg}` lookup), not the prototype's full imzML
//! `IMS:1006008`-reference parse.
//!
//! `anyhow` is used at this binary boundary (the project uses anyhow in main.rs).

use std::fs::File;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

use mzpeak_prototyping::archive::{DataKind, EntityType, FileEntry, ZipArchiveWriter};

/// Chunk size for the streamed SHA-256 + size pass (64 KiB — bounded memory regardless of size).
const CHUNK: usize = 64 * 1024;

/// Fixed archive member for the embedded SDRF (never derived from the source basename — no
/// path-injection surface). Matches the prototype's `MEMBER_NAME` constant.
const SDRF_MEMBER_NAME: &str = "sample_metadata/sdrf.tsv";

/// Entity-type / data-kind open-enum tokens for the SDRF member (prototype `src/schema/cv.rs`).
const SAMPLE_METADATA_ENTITY_TYPE: &str = "sample-metadata";
const SDRF_DATA_KIND: &str = "sdrf";

// ─────────────────────────────────────────────────────────────────────────────────────────────
// Public entry point: embed optical images + SDRF into an OPEN ZipArchiveWriter.
// Called by convert_file right before zip.finish(), alongside vendor::embed_into_archive.
// ─────────────────────────────────────────────────────────────────────────────────────────────

/// Embed any explicit `--image` paths plus a best-effort sibling optical image, and (if given) the
/// `--sdrf` file, into the open `zip`. Adds the images to the lane's `metadata.imaging` marker and
/// writes the `metadata.study` / `metadata.sample_metadata` index blocks. Does NOT call
/// `zip.finish()` — the caller owns that.
///
/// `input` is the source mzML/imzML path (used for sibling discovery + run-id/accession hints), or
/// on the filter lane the source `.mzpeak`, whose carried marker the images join (no sibling lookup).
///
/// STRICTNESS: a missing/unreadable explicit `--image`, an explicit `--image` on a run the lane did
/// not mark imaging, or the `--sdrf` file ERRORS the conversion; a soft auto-discovered sibling image
/// that is unreadable, or found beside a run that is not imaging, warns + is skipped.
pub fn embed_into_archive(
    zip: &mut ZipArchiveWriter<File>,
    input: &Path,
    images: &[PathBuf],
    sdrf: Option<&Path>,
) -> Result<()> {
    embed_optical_images(zip, input, images)?;
    if let Some(sdrf_path) = sdrf {
        embed_sdrf(zip, input, sdrf_path)?;
    }
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────────────────────
// Optical images
// ─────────────────────────────────────────────────────────────────────────────────────────────

/// Fail mode for the per-image embed helper — the only asymmetry between an explicit `--image`
/// (Strict: a bad path hard-fails the conversion) and an auto-discovered sibling (Soft: warn+skip).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EmbedMode {
    Strict,
    Soft,
}

fn embed_optical_images(
    zip: &mut ZipArchiveWriter<File>,
    input: &Path,
    images: &[PathBuf],
) -> Result<()> {
    // Build the ordered embed list: explicit --image (Strict) first, then a best-effort sibling
    // optical image (Soft). The prototype additionally parses every imzML IMS:1006008 reference;
    // here we do explicit + sibling discovery only (see module doc).
    let mut embed_list: Vec<(PathBuf, EmbedMode)> = Vec::new();
    for path in images {
        embed_list.push((path.clone(), EmbedMode::Strict));
    }
    // Not beside an existing archive (the filter lane): the run's sibling was looked for when it
    // was converted, and an archive written next to its imzML would embed that image a second time.
    if let Some(sibling) = discover_sibling_optical_image(input).filter(|_| !crate::filter::is_mzpeak_input(input)) {
        embed_list.push((sibling, EmbedMode::Soft));
    }

    if embed_list.is_empty() {
        return Ok(());
    }

    // The full-extent affine maps image pixels onto the MS pixel grid Nx×Ny: the grid of the lane's
    // own `metadata.imaging` marker, written only for a run with pixel positions (on the filter
    // lane, the source archive's marker, carried before this embed). Without one the run is not
    // imaging, and an image must not make it one: the block used to be invented here, from an imzML
    // header's counts or from nothing (review 2026-09-30 B10). An explicit --image is an error, an
    // auto-discovered one is skipped.
    let marker = zip.index().metadata.get("imaging").cloned();
    let grid = marker
        .as_ref()
        .and_then(|m| Some((m["pixel_count"]["x"].as_i64()?, m["pixel_count"]["y"].as_i64()?)));
    let (Some(mut block), Some((nx, ny))) = (marker, grid) else {
        let why = format!(
            "{} carries no pixel positions (no imaging marker with a pixel grid was written), so \
             there is no grid to overlay the image on",
            input.display()
        );
        if let Some((path, _)) = embed_list.iter().find(|(_, m)| *m == EmbedMode::Strict) {
            bail!("--image {}: {why}", path.display());
        }
        log::warn!("skipping auto-discovered optical image: {why}");
        return Ok(());
    };

    // How these images are placed: on a Bruker MALDI run acquired from a FlexImaging sequence, by
    // the sequence's teach-point registration when the archive carries one (an image in the
    // sequence image's pixel frame: the same size, or the same name when that size is unknown),
    // and otherwise not at all, said once; on any other grid, the full-extent assumption.
    let placement = match (sequence_registration(&zip.index().metadata, &block), unregistered_reason(&zip.index().metadata)) {
        (Some(p), _) => p,
        (None, Some(why)) => {
            log::warn!("optical image embedded WITHOUT an affine (no registration is written): {why}");
            Placement::Unplaced
        }
        (None, None) => Placement::FullExtent(nx, ny),
    };

    let mut entries: Vec<serde_json::Value> = Vec::with_capacity(embed_list.len());
    // ordinal advances ONLY on a successful embed, so a skipped soft image leaves no gap. It starts
    // past the images the archive already holds (the filter lane copies them, a sequence's image
    // went in before): a reused name would be a second member under it.
    let mut ordinal = next_image_ordinal(zip);
    // Dedup canonicalized paths so --image X and a sibling that resolves to X embed once, and by
    // digest against the images the archive already lists: the sequence's own image given again
    // as `--image`, or an image injected into an archive that holds it.
    let mut seen: Vec<PathBuf> = Vec::with_capacity(embed_list.len());
    let mut digests: Vec<String> = block["images"].as_array().into_iter().flatten().filter_map(|e| e["sha256"].as_str().map(str::to_string)).collect();

    for (path, mode) in &embed_list {
        let key = canonical_key(path);
        if seen.contains(&key) {
            continue;
        }
        if let Ok((sha, _)) = sha256_and_size(path) {
            if digests.contains(&sha) {
                log::info!("{} is already in the archive (same SHA-256): not embedded again", path.display());
                seen.push(key);
                continue;
            }
            digests.push(sha);
        }
        if let Some(mut entry) = embed_one_image(zip, path, ordinal, *mode)? {
            if let Some(why) = place(&mut entry, &placement) {
                log::warn!("{}: embedded WITHOUT an affine: {why}", path.display());
            }
            entries.push(entry);
            seen.push(key);
            ordinal += 1;
        }
    }

    if entries.is_empty() {
        return Ok(());
    }

    // metadata.imaging.images[] — match the prototype's block shape — added to the lane's marker,
    // after any images it already lists.
    let mut all = block["images"].as_array().cloned().unwrap_or_default();
    all.extend(entries);
    block["images"] = all.into();
    zip.add_index_metadata("imaging", &block)
        .context("writing metadata.imaging index")?;
    Ok(())
}

/// The first free ordinal of `images/image_NNNN.<ext>`: past every image the archive holds.
fn next_image_ordinal(zip: &ZipArchiveWriter<File>) -> usize {
    zip.index()
        .files
        .iter()
        .filter_map(|f| f.name.strip_prefix("images/image_")?.split('.').next()?.parse::<usize>().ok())
        .max()
        .map_or(0, |k| k + 1)
}

/// **The FlexImaging sequence's own image** (owner decision D8): on a Bruker MALDI run whose
/// `bruker_maldi` block holds a teach-point registration and names an `<ImageFile>` found beside
/// the `.mis`, embed that image as the next `images/image_NNNN.<ext>` with the registration's
/// affine (`registration_quality: teach_points`) and the registration itself in its `images[]`
/// entry, so a reader can redo the transform. Called for every lane from the archive epilogue:
/// no `--image` is involved, and the lanes that refuse `--image` embed it too. A sequence whose
/// image is not beside it, or could not be registered, was warned about when the `.d` was read
/// and leaves its record in the block; nothing is embedded. Soft on the file itself (unreadable
/// → warn and skip): the image is auto-discovered.
pub fn embed_sequence_image(zip: &mut ZipArchiveWriter<File>, input: &Path) -> Result<()> {
    let Some(bruker) = zip.index().metadata.get("bruker_maldi").cloned() else { return Ok(()) };
    let Some(file) = bruker["sequence_image"]["file"].as_str().map(str::to_string) else { return Ok(()) };
    if bruker["sequence_image"]["found"] != serde_json::json!(true) || bruker["registration"].is_null() {
        return Ok(());
    }
    let Some(mut marker) = zip.index().metadata.get("imaging").cloned() else { return Ok(()) };
    let registration = bruker["registration"].clone();
    let path = input.parent().unwrap_or(Path::new("")).join(&file);
    let ordinal = next_image_ordinal(zip);
    let Some(mut entry) = embed_one_image(zip, &path, ordinal, EmbedMode::Soft)? else { return Ok(()) };
    entry["source"] = format!("the <ImageFile> of the FlexImaging sequence {}", bruker["mis"].as_str().unwrap_or("?")).into();
    entry["affine"] = affine_of(&registration);
    entry["registration"] = registration;
    let mut all = marker["images"].as_array().cloned().unwrap_or_default();
    log::info!(
        "FlexImaging sequence image {file} embedded as {} with the teach-point registration ({} bytes)",
        entry["archive_path"].as_str().unwrap_or("?"),
        entry["size_bytes"]
    );
    all.push(entry);
    marker["images"] = all.into();
    zip.add_index_metadata("imaging", &marker).context("writing metadata.imaging index")?;
    Ok(())
}

/// The `affine` object of an `images[]` entry from a recorded registration.
fn affine_of(registration: &serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "type": "affine",
        "matrix": registration["matrix"],
        "maps": "image_px -> ms_px",
        "registration_quality": crate::mis_registration::QUALITY,
    })
}

/// How an added image is placed on the pixel grid.
enum Placement {
    /// Its extent laid on the grid's Nx × Ny: `assumed_full_extent`.
    FullExtent(i64, i64),
    /// The sequence's teach-point registration, for an image in the sequence image's pixel frame:
    /// `size` is that image's, when the archive holds it, else the image must carry its `name`.
    TeachPoints { registration: serde_json::Value, name: String, size: Option<(i64, i64)> },
    /// No affine (a FlexImaging run without a registration), said once by the caller.
    Unplaced,
}

/// The teach-point placement an archive's `bruker_maldi` block offers, if any: its `registration`
/// (written even when the sequence's image was not beside it), the image file it names, and that
/// image's size from the `images[]` entry the sequence embed wrote.
fn sequence_registration(metadata: &std::collections::HashMap<String, serde_json::Value>, marker: &serde_json::Value) -> Option<Placement> {
    let block = metadata.get("bruker_maldi")?;
    let registration = block.get("registration").filter(|r| r.is_object())?.clone();
    let name = block["sequence_image"]["file"].as_str()?.to_string();
    let size = marker["images"].as_array().into_iter().flatten().find_map(|e| {
        (e["affine"]["registration_quality"] == crate::mis_registration::QUALITY && e["source_name"] == name.as_str())
            .then(|| Some((e["width"].as_i64()?, e["height"].as_i64()?)))
            .flatten()
    });
    Some(Placement::TeachPoints { registration, name, size })
}

/// Give `entry` its `affine` under `placement`; `Some(why)` when it gets none.
fn place(entry: &mut serde_json::Value, placement: &Placement) -> Option<String> {
    let (w, h) = (entry["width"].as_i64().unwrap_or(0), entry["height"].as_i64().unwrap_or(0));
    match placement {
        Placement::FullExtent(nx, ny) => {
            // Full-extent affine: the image's real (w,h); a dimensionless (0,0) embed counts as ONE
            // image pixel over the whole grid (0 would divide by zero), whose centre is the grid's.
            let (aw, ah) = if w <= 0 || h <= 0 { (1, 1) } else { (w as u32, h as u32) };
            entry["affine"] = serde_json::json!({
                "type": "affine",
                "matrix": full_extent_affine(*nx, *ny, aw, ah),
                "maps": "image_px -> ms_px",
                "registration_quality": "assumed_full_extent",
            });
            None
        }
        Placement::TeachPoints { registration, name, size } => {
            let fits = match size {
                Some(size) => *size == (w, h),
                None => entry["source_name"] == name.as_str(),
            };
            if fits {
                entry["affine"] = affine_of(registration);
                entry["registration"] = registration.clone();
                return None;
            }
            Some(match size {
                Some((sw, sh)) => format!(
                    "the archive's teach-point registration is for the FlexImaging sequence image {name} ({sw} × {sh} px); this image is {w} × {h} px, so it is not in that pixel frame"
                ),
                None => format!(
                    "the archive's teach-point registration is for the FlexImaging sequence image {name}, which was not embedded, so only an image of that name is taken to be in its pixel frame"
                ),
            })
        }
        Placement::Unplaced => Some("see above".into()),
    }
}

/// Why an image added to this archive must not get the full-extent affine, if it must not: the
/// archive's `bruker_maldi` block names a FlexImaging sequence (`.mis`, used or rejected) and holds
/// no teach-point registration. Such a run's pixel grid is the bounding box of the acquired
/// regions, shifted to start at 1, while the sequence's own image is a photo of the whole target,
/// registered through its teach points: laying the photo's extent on the grid's misplaces it by up
/// to 11 MS pixels on MassIVE MSV000088438 (the photo spans MS pixels −11…54 of a 28-pixel grid).
/// `assumed_full_extent` would state a registration nobody made, so the image goes in without an
/// affine, which the profile allows (`affine` is optional in `schema/mzpeak_index.json`).
fn unregistered_reason(metadata: &std::collections::HashMap<String, serde_json::Value>) -> Option<String> {
    let block = metadata.get("bruker_maldi")?;
    let mis = block["mis"].as_str().or_else(|| block["mis_rejected"]["file"].as_str())?;
    let not_registered = block["not_registered"].as_str().map_or(String::new(), |why| format!(" ({why})"));
    Some(format!(
        "this is a Bruker MALDI run acquired from the FlexImaging sequence {mis}, so its pixel grid \
         is the bounding box of the acquired regions and not the extent of a photo of the target; \
         laying the image's extent on the grid would misplace it, and the archive holds no \
         teach-point registration{not_registered}; readers get this image with no placement"
    ))
}

/// Embed ONE optical image (any format) as `images/image_{ordinal:04}.<ext>`, returning its
/// `metadata.imaging.images[]` entry as a JSON value, without an `affine` (the caller places it,
/// [`place`]). The ordinal is the ONLY part of the archive name that varies — the
/// attacker-influenced source basename never reaches the archive path.
fn embed_one_image(
    zip: &mut ZipArchiveWriter<File>,
    path: &Path,
    ordinal: usize,
    mode: EmbedMode,
) -> Result<Option<serde_json::Value>> {
    // On a defect: Strict → Err (abort the conversion); Soft → warn + Ok(None) (skip this image).
    macro_rules! fail {
        ($ctx:expr) => {{
            match mode {
                EmbedMode::Strict => {
                    return Err(anyhow::anyhow!("{}: {}", path.display(), $ctx));
                }
                EmbedMode::Soft => {
                    log::warn!(
                        "skipping auto-discovered optical image {}: {}",
                        path.display(),
                        $ctx
                    );
                    return Ok(None);
                }
            }
        }};
    }

    // The source_name is descriptive-only but attacker-influenced — reject any residual path
    // separator. The ARCHIVE name is the fixed ordinal below, never the source name.
    let source_name = match path.file_name().and_then(|n| n.to_str()) {
        Some(n) => n.to_string(),
        None => fail!("image path has no UTF-8 file name component"),
    };
    if source_name.contains('/') || source_name.contains('\\') {
        fail!("derived source_name contains a path separator");
    }

    // Branch on format-by-magic-bytes. detect_format opens + reads the leading bytes, so its Err
    // arm is the existence/readability proof for ALL formats.
    let (w, h, media_type) = match detect_format(path) {
        Ok(ImageFormat::Tiff) => match read_tiff_dimensions(path) {
            Ok((w, h)) => (w, h, "image/tiff".to_string()),
            Err(_) => fail!("TIFF dimensions could not be read (malformed TIFF)"),
        },
        Ok(ImageFormat::Png) => {
            let (w, h) = read_png_dimensions(path).unwrap_or((0, 0));
            (w, h, "image/png".to_string())
        }
        Ok(ImageFormat::Jpeg) => {
            let (w, h) = read_jpeg_dimensions(path).unwrap_or((0, 0));
            (w, h, "image/jpeg".to_string())
        }
        Ok(ImageFormat::Other) => {
            let ext = path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("bin")
                .to_ascii_lowercase();
            (0u32, 0u32, media_type_for_extension(&ext))
        }
        Err(_) => fail!("file is missing or unreadable"),
    };

    // Archive member preserves the SOURCE EXTENSION (image_{ordinal:04}.<ext>, NOT a forced .tiff).
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_else(|| "bin".to_string());
    let member = format!("images/image_{ordinal:04}.{ext}");

    // Stream the bytes into the ZIP (64 KiB chunks inside add_file_from_read — never a whole-file
    // load) as the imaging profile lists an image: `entity_type` `image`, `data_kind` `other`.
    let mut f = match File::open(path) {
        Ok(f) => f,
        Err(_) => fail!("file became unreadable before embed"),
    };
    let fe = FileEntry::new(
        member.clone(),
        EntityType::Other("image".to_string()),
        DataKind::Other("other".to_string()),
    );
    if zip.add_file_from_read(&mut f, None::<&String>, Some(fe)).is_err() {
        fail!("failed to stream image bytes into the archive");
    }

    // SHA-256 + exact byte size over a SECOND bounded streamed pass.
    let (sha256, size) = match sha256_and_size(path) {
        Ok(v) => v,
        Err(_) => fail!("failed to digest image bytes"),
    };

    Ok(Some(serde_json::json!({
        "archive_path": member,
        "source_name": source_name,
        "media_type": media_type,
        "width": w as i64,
        "height": h as i64,
        "sha256": sha256,
        "size_bytes": size as i64,
        "role": "optical",
    })))
}

/// Build the full-extent affine `[a,b,c,d,e,f]`: `(x_ms, y_ms) = (a·col + c, e·row + f)`, `b=d=0`.
/// The imaging profile defines the affine from 0-based image pixel CENTRES to 1-based MS pixel
/// CENTRES, and `assumed_full_extent` says the image's extent is the grid's: the image's left edge
/// (col −0.5) lies on the grid's (x_ms 0.5, the left edge of MS pixel 1), its right edge (col W−0.5)
/// on the grid's (x_ms Nx+0.5). So `a = Nx/W`, `c = 0.5 + 0.5·Nx/W`; the same on y with Ny/H.
///
/// Through 0.16.0 this mapped the corner pixel centres onto each other (`a = (Nx−1)/(W−1)`, `c = 1`:
/// image pixel 0 → MS 1, pixel W−1 → MS Nx), which stretches the image by half an image pixel past
/// each edge: off by up to half an MS pixel at the edges, nothing at the centre, and equal only when
/// W = Nx.
fn full_extent_affine(nx: i64, ny: i64, w: u32, h: u32) -> [f64; 6] {
    let (a, e) = (nx as f64 / w as f64, ny as f64 / h as f64);
    [a, 0.0, 0.5 + 0.5 * a, 0.0, e, 0.5 + 0.5 * e]
}

/// Best-effort sibling optical-image discovery: `<dir>/<stem>-opticalimage.{tif,tiff,png,jpg,jpeg}`.
/// Returns the first existing candidate (Soft embed). The prototype instead parses imzML
/// `IMS:1006008` references; this is the documented simplification.
fn discover_sibling_optical_image(input: &Path) -> Option<PathBuf> {
    let dir = input.parent()?;
    let stem = input.file_stem()?.to_str()?;
    for ext in ["tif", "tiff", "png", "jpg", "jpeg"] {
        let candidate = dir.join(format!("{stem}-opticalimage.{ext}"));
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Canonicalize a path for dedup; fall back to the lexical path when canonicalize fails (a
/// not-yet-existing soft candidate). Mirrors the prototype's `canonical_key`.
fn canonical_key(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

// ─────────────────────────────────────────────────────────────────────────────────────────────
// SDRF
// ─────────────────────────────────────────────────────────────────────────────────────────────

/// Stream the SDRF file BYTE-FOR-BYTE into `zip` as the fixed `sample_metadata/sdrf.tsv` member,
/// then write the `metadata.study` + `metadata.sample_metadata` back-ref index blocks. A
/// missing/unreadable SDRF ERRORS the conversion (strict).
fn embed_sdrf(zip: &mut ZipArchiveWriter<File>, input: &Path, sdrf_path: &Path) -> Result<()> {
    // Verbatim embed via the TYPED FileEntry (start_for_entry), 64 KiB byte-copy loop.
    let mut src = File::open(sdrf_path)
        .with_context(|| format!("opening SDRF {}", sdrf_path.display()))?;
    let fe = FileEntry::new(
        SDRF_MEMBER_NAME.to_string(),
        EntityType::Other(SAMPLE_METADATA_ENTITY_TYPE.to_string()),
        DataKind::Other(SDRF_DATA_KIND.to_string()),
    );
    zip.add_file_from_read(&mut src, None::<&String>, Some(fe))
        .with_context(|| format!("streaming SDRF into {SDRF_MEMBER_NAME}"))?;

    // SECOND bounded pass: SHA-256 + exact byte count for the provenance back-ref.
    let (sha256, size_bytes) = sha256_and_size(sdrf_path)
        .with_context(|| format!("digesting SDRF {}", sdrf_path.display()))?;

    // Derive a dataset_accession hint from the SDRF filename stem (PXD…/MTBLS…/MSV… prefixes, else
    // the bare stem). The full prototype also reads characteristics[proteomexchange accession
    // number]; the verbatim blob retains that, so the hint is informative-only.
    let accession = accession_hint(sdrf_path);
    let title = accession.clone();

    // metadata.study: {dataset_accession, title, sample_metadata_ref} — exact StudyMetadata shape.
    let study = serde_json::json!({
        "dataset_accession": accession,
        "title": title,
        "sample_metadata_ref": SDRF_MEMBER_NAME,
    });
    zip.add_index_metadata("study", &study)
        .context("writing metadata.study index")?;

    // metadata.sample_metadata: the provenance back-ref carrying member + sha256 + size_bytes.
    let provenance = serde_json::json!({
        "member": SDRF_MEMBER_NAME,
        "sha256": sha256,
        "size_bytes": size_bytes,
        "precedence": "repo_wins",
        "embed_scope": "full",
        "dataset_accession": accession,
    });
    zip.add_index_metadata("sample_metadata", &provenance)
        .context("writing metadata.sample_metadata index")?;

    let _ = input; // run-id projection is out of scope for the forward-only port.
    Ok(())
}

/// Derive the dataset_accession hint from the SDRF filename stem (matching the prototype's
/// filename-stem fallback): strip a trailing `.sdrf`, accept PXD…/MTBLS…/MSV… prefixes, else the
/// whole stem.
fn accession_hint(sdrf_path: &Path) -> String {
    let stem = sdrf_path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    let bare = match stem.rfind('.') {
        Some(pos) => &stem[..pos],
        None => stem,
    };
    if bare.starts_with("PXD") || bare.starts_with("MTBLS") || bare.starts_with("MSV") {
        bare.to_string()
    } else {
        stem.to_string()
    }
}

// ─────────────────────────────────────────────────────────────────────────────────────────────
// Format detection + dimension reads + digest (ported from the prototype's image.rs)
// ─────────────────────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ImageFormat {
    Tiff,
    Png,
    Jpeg,
    Other,
}

/// Detect the container format of `path` from its leading magic bytes (never by extension). A
/// missing/unreadable file is an Err (the readability proof for ALL formats).
fn detect_format(path: &Path) -> std::io::Result<ImageFormat> {
    let mut f = File::open(path)?;
    let mut magic = [0u8; 8];
    let n = read_prefix(&mut f, &mut magic)?;
    let m = &magic[..n];
    Ok(if m.starts_with(b"II\x2A\x00") || m.starts_with(b"MM\x00\x2A") {
        ImageFormat::Tiff
    } else if m.starts_with(b"\x89PNG\r\n\x1a\n") {
        ImageFormat::Png
    } else if m.starts_with(b"\xFF\xD8\xFF") {
        ImageFormat::Jpeg
    } else {
        ImageFormat::Other
    })
}

/// Fill `buf`, returning bytes actually read (< buf.len() only at EOF). Handles short reads.
fn read_prefix(r: &mut impl Read, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match r.read(&mut buf[filled..])? {
            0 => break,
            n => filled += n,
        }
    }
    Ok(filled)
}

/// Read a TIFF's `(width, height)` from its FIRST IFD without decoding pixels (tiff crate's
/// `Decoder::dimensions()` — IFD-only, never `read_image()`).
fn read_tiff_dimensions(path: &Path) -> Result<(u32, u32)> {
    let reader = BufReader::new(File::open(path)?);
    let mut decoder = tiff::decoder::Decoder::new(reader)?;
    Ok(decoder.dimensions()?)
}

/// Read a PNG's `(width, height)` from its IHDR chunk (offsets 16/20, big-endian) without decoding.
fn read_png_dimensions(path: &Path) -> Result<(u32, u32)> {
    let mut f = File::open(path)?;
    let mut head = [0u8; 24];
    f.read_exact(&mut head)?;
    if &head[0..8] != b"\x89PNG\r\n\x1a\n" || &head[12..16] != b"IHDR" {
        bail!("not a PNG or missing IHDR chunk");
    }
    let w = u32::from_be_bytes([head[16], head[17], head[18], head[19]]);
    let h = u32::from_be_bytes([head[20], head[21], head[22], head[23]]);
    Ok((w, h))
}

/// Read a JPEG's `(width, height)` from its first SOF marker without decoding. Walks marker
/// segments by declared length until a SOFn (0xC0–0xCF except 0xC4/0xC8/0xCC).
fn read_jpeg_dimensions(path: &Path) -> Result<(u32, u32)> {
    let mut r = BufReader::new(File::open(path)?);
    let mut byte = || -> Result<u8> {
        let mut b = [0u8; 1];
        r.read_exact(&mut b)?;
        Ok(b[0])
    };
    if byte()? != 0xFF || byte()? != 0xD8 {
        bail!("not a JPEG (missing SOI marker)");
    }
    loop {
        let mut marker = byte()?;
        if marker != 0xFF {
            bail!("expected a JPEG marker (0xFF) between segments");
        }
        while marker == 0xFF {
            marker = byte()?;
        }
        match marker {
            0xD9 => bail!("reached end of image (EOI) before any SOF"),
            0x01 | 0xD0..=0xD7 => continue, // parameter-less standalone markers
            _ => {}
        }
        let len = u16::from_be_bytes([byte()?, byte()?]) as i64;
        if len < 2 {
            bail!("invalid JPEG segment length {len}");
        }
        let is_sof =
            (0xC0..=0xCF).contains(&marker) && marker != 0xC4 && marker != 0xC8 && marker != 0xCC;
        if is_sof {
            if len < 7 {
                bail!("JPEG SOF segment too short: len {len}");
            }
            let _precision = byte()?;
            let h = u16::from_be_bytes([byte()?, byte()?]) as u32;
            let w = u16::from_be_bytes([byte()?, byte()?]) as u32;
            return Ok((w, h));
        }
        // Skip this segment's payload (length counts its own 2 length bytes).
        for _ in 0..(len - 2) {
            byte()?;
        }
    }
}

/// Map a file extension (no leading dot) to an IANA media type for the verbatim-embed path.
fn media_type_for_extension(ext: &str) -> String {
    match ext.to_ascii_lowercase().as_str() {
        "tif" | "tiff" | "svs" => "image/tiff",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        _ => "application/octet-stream",
    }
    .to_string()
}

/// Stream a SHA-256 digest AND exact byte count in one bounded pass. Returns `(hex, size)`. Never
/// loads the whole file.
fn sha256_and_size(path: &Path) -> Result<(String, u64)> {
    stream_digest::<Sha256>(path)
}

/// SHA-1 of a file, hex — the `MS:1000569` digest msconvert records on every source file.
pub(crate) fn sha1_hex(path: &Path) -> Result<String> {
    Ok(stream_digest::<sha1::Sha1>(path)?.0)
}

/// Size + modification time of a source file: enough to tell whether a vendor library rewrote the
/// input while it held it open.
///
/// This is not paranoia. Some vendor SDKs have no read-only open at all — the Shimadzu glue opens
/// the `.lcd` read-write because that is the only overload the library exposes — and one code path
/// was already found committing changes back to the OLE2 storage of the file it was only supposed
/// to read. The `MS:1000569` digest we archive is taken BEFORE the library opens the file (it has to
/// be: the open takes a byte-range lock), so if the library then rewrites the input, the archive
/// records the digest of a file that no longer exists on disk and nothing anywhere notices.
///
/// Size and mtime cost one `stat` and catch the rewrite that actually happens (an OLE2 commit
/// changes both). Re-digesting the input would be authoritative but means a second full read of a
/// multi-gigabyte file on every conversion, so it is reserved for the case where this already
/// says something changed.
// Taken and compared only on the `#[cfg(windows)]` Shimadzu lane — the one vendor API with no
// read-only open. The logic itself is host-independent so that it can be tested here.
#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SourceFingerprint {
    pub len: u64,
    /// `None` when the platform/filesystem does not report one — then only the length is compared.
    pub mtime: Option<std::time::SystemTime>,
}

#[cfg_attr(not(windows), allow(dead_code))]
impl SourceFingerprint {
    /// `stat` the path. `None` when it cannot be read; a fingerprint we could not take is simply
    /// not compared, never treated as "unchanged".
    pub fn of(path: &Path) -> Option<Self> {
        let md = std::fs::metadata(path).ok()?;
        Some(Self { len: md.len(), mtime: md.modified().ok() })
    }
}

/// Describe how a source file changed while a vendor library held it open, or `None` if it did not.
///
/// `None` for either fingerprint means "could not tell" — a missing observation is not evidence of
/// a rewrite, so it stays quiet rather than crying wolf on, say, a filesystem with no mtime.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn describe_source_rewrite(
    before: Option<&SourceFingerprint>,
    after: Option<&SourceFingerprint>,
) -> Option<String> {
    let (b, a) = (before?, after?);
    let mut what = Vec::new();
    if b.len != a.len {
        what.push(format!("size {} → {} bytes", b.len, a.len));
    }
    // Only compare times when BOTH sides reported one.
    if let (Some(bt), Some(at)) = (b.mtime, a.mtime) {
        if bt != at {
            what.push("modification time advanced".to_string());
        }
    }
    (!what.is_empty()).then(|| what.join(", "))
}

/// One bounded pass for any `Digest`: `(hex, byte count)`.
fn stream_digest<D: Digest>(path: &Path) -> Result<(String, u64)> {
    let mut f = File::open(path)?;
    let mut hasher = D::new();
    let mut buf = vec![0u8; CHUNK];
    let mut size: u64 = 0;
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        size += n as u64;
    }
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(digest.len() * 2);
    for b in &digest {
        hex.push_str(&format!("{b:02x}"));
    }
    Ok((hex, size))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_tmp(name: &str, bytes: &[u8]) -> PathBuf {
        let p = std::env::temp_dir().join(format!("mzpc_embed_aux_{}_{name}", std::process::id()));
        File::create(&p).unwrap().write_all(bytes).unwrap();
        p
    }

    /// The vendor-rewrote-my-input detector. Runs on every host (no FFI, no vendor DLL): it is
    /// plain `stat` arithmetic, which is exactly why it lives here and not inside the
    /// Windows-gated Shimadzu lane where it could never be executed.
    #[test]
    fn source_rewrite_is_detected_by_size_or_mtime() {
        let p = write_tmp("fingerprint", b"original contents");
        let before = SourceFingerprint::of(&p).expect("fingerprint the file we just wrote");

        // Untouched: no report.
        assert_eq!(describe_source_rewrite(Some(&before), SourceFingerprint::of(&p).as_ref()), None);

        // A rewrite that changes the length is named, and names both lengths.
        File::create(&p).unwrap().write_all(b"vendor rewrote this, longer now").unwrap();
        let after = SourceFingerprint::of(&p).expect("fingerprint after the rewrite");
        let msg = describe_source_rewrite(Some(&before), Some(&after))
            .expect("a length change must be reported");
        assert!(msg.contains("17"), "old length missing from {msg:?}");
        assert!(msg.contains("31"), "new length missing from {msg:?}");

        // A same-length rewrite is caught by mtime alone. The moved time is SYNTHESIZED, not taken
        // from the rewrite above: two writes this close together share an mtime on Windows, whose
        // clock updates on a ~15.6 ms tick, and the test then asserted on whether the filesystem
        // happened to tick rather than on the logic under test. It was red on the box and green on
        // macOS (APFS, nanosecond stamps) for exactly that reason.
        let moved = before
            .mtime
            .map(|t| t + std::time::Duration::from_secs(1))
            .expect("the temp filesystem reports an mtime");
        let same_len = SourceFingerprint { len: before.len, mtime: Some(moved) };
        let msg = describe_source_rewrite(Some(&before), Some(&same_len))
            .expect("a same-length rewrite must still be reported when mtime moved");
        assert!(msg.contains("modification time"), "{msg:?}");

        // A missing observation is not evidence of a rewrite — it must stay silent, not guess.
        assert_eq!(describe_source_rewrite(None, Some(&after)), None);
        assert_eq!(describe_source_rewrite(Some(&before), None), None);
        // Nor is a filesystem that reports no mtime on either side.
        let no_time_a = SourceFingerprint { len: 5, mtime: None };
        let no_time_b = SourceFingerprint { len: 5, mtime: None };
        assert_eq!(describe_source_rewrite(Some(&no_time_a), Some(&no_time_b)), None);

        let _ = std::fs::remove_file(&p);
    }

    /// The full-extent affine lays the image's EXTENT on the grid's, in the profile's coordinates
    /// (0-based image pixel centres → 1-based MS pixel centres). It mapped the corner pixel centres
    /// onto each other: image pixel 0 of a 5-pixel image over 10 MS pixels covers MS pixels 1 and 2
    /// (centre 1.5) and was placed at 1.0.
    #[test]
    fn affine_maps_the_image_extent_onto_the_grid_extent() {
        // Nx=10, Ny=20, W=5, H=8 → a=2, c=1.5; e=2.5, f=1.75.
        let m = full_extent_affine(10, 20, 5, 8);
        assert_eq!(m, [2.0, 0.0, 1.5, 0.0, 2.5, 1.75]);
        let apply = |col: f64, row: f64| (m[0] * col + m[1] * row + m[2], m[3] * col + m[4] * row + m[5]);
        let close = |(x, y): (f64, f64), (ex, ey): (f64, f64)| (x - ex).abs() < 1e-12 && (y - ey).abs() < 1e-12;
        // Edges on edges: the image's top-left corner is the top-left corner of MS pixel (1, 1), its
        // bottom-right corner the bottom-right corner of MS pixel (Nx, Ny).
        assert!(close(apply(-0.5, -0.5), (0.5, 0.5)), "{:?}", apply(-0.5, -0.5));
        assert!(close(apply(4.5, 7.5), (10.5, 20.5)), "{:?}", apply(4.5, 7.5));
        // Pixel centres: the first image pixel is centred between MS pixels 1 and 2 on x.
        assert!(close(apply(0.0, 0.0), (1.5, 1.75)));
        assert!(close(apply(4.0, 7.0), (9.5, 19.25)));
        // An image with the grid's own size: pixel k is MS pixel k + 1.
        assert_eq!(full_extent_affine(10, 20, 10, 20), [1.0, 0.0, 1.0, 0.0, 1.0, 1.0]);
        // A high-resolution image: its centre stays on the grid's centre.
        let m = full_extent_affine(28, 24, 8064, 6048);
        assert!((m[0] * 4031.5 + m[2] - 14.5).abs() < 1e-9 && (m[4] * 3023.5 + m[5] - 12.5).abs() < 1e-9);
        // An image of unknown size counts as one pixel over the grid: its centre is the grid's.
        assert_eq!(full_extent_affine(28, 24, 1, 1), [28.0, 0.0, 14.5, 0.0, 24.0, 12.5]);
    }

    /// `--image` on an imaging archive (the filter lane's call): the image is laid on the grid of
    /// the marker with the full-extent affine — unless the archive is a Bruker MALDI run acquired
    /// from a FlexImaging sequence, whose grid is a bounding box of regions: then no affine at all,
    /// where it wrote `assumed_full_extent` (off by up to 11 MS pixels on MSV000088438).
    #[test]
    fn an_image_on_a_fleximaging_run_gets_no_affine() {
        use mzpeak_prototyping::writer::MzPeakWriterType;
        use mzpeaks::{CentroidPeak, DeconvolutedPeak};
        use std::io::Read as _;

        let mut png = Vec::new();
        png.extend_from_slice(b"\x89PNG\r\n\x1a\n");
        png.extend_from_slice(&13u32.to_be_bytes());
        png.extend_from_slice(b"IHDR");
        png.extend_from_slice(&56u32.to_be_bytes());
        png.extend_from_slice(&48u32.to_be_bytes());
        png.extend_from_slice(&[8, 2, 0, 0, 0]);
        let img = write_tmp("fleximaging.png", &png);
        let marker = serde_json::json!({"is_imaging": true, "coordinate_base": 1, "pixel_count": {"x": 28, "y": 24}});
        let images = |name: &str, bruker: Option<serde_json::Value>| {
            let out = std::env::temp_dir().join(format!("mzpc_embed_aux_{name}_{}.mzpeak", std::process::id()));
            let writer = MzPeakWriterType::<File, CentroidPeak, DeconvolutedPeak>::builder().build(File::create(&out).unwrap(), true);
            let mut zip = writer.finish_parquet().unwrap();
            zip.add_index_metadata("imaging", &marker).unwrap();
            if let Some(b) = &bruker {
                zip.add_index_metadata("bruker_maldi", b).unwrap();
            }
            embed_optical_images(&mut zip, Path::new("/x/run.mzpeak"), std::slice::from_ref(&img)).unwrap();
            zip.finish().unwrap();
            let mut archive = zip::ZipArchive::new(BufReader::new(File::open(&out).unwrap())).unwrap();
            assert!(archive.by_name("images/image_0000.png").is_ok(), "{name}: the image is embedded either way");
            let mut idx = String::new();
            archive.by_name("mzpeak_index.json").unwrap().read_to_string(&mut idx).unwrap();
            std::fs::remove_file(&out).ok();
            serde_json::from_str::<serde_json::Value>(&idx).unwrap()["metadata"]["imaging"]["images"].clone()
        };
        // Any other imaging archive, and a Bruker MALDI run with no sequence beside it.
        for (name, bruker) in [("plain", None), ("nomis", Some(serde_json::json!({"mis": null, "mis_rejected": null})))] {
            let entry = images(name, bruker)[0].clone();
            assert_eq!(entry["affine"]["registration_quality"], "assumed_full_extent", "{name}");
            assert_eq!(entry["affine"]["matrix"], serde_json::json!([0.5, 0.0, 0.75, 0.0, 0.5, 0.75]), "{name}");
        }
        // A sequence was read (used, or rejected): the image, its size and digest, and no affine.
        for (name, bruker) in [
            ("mis", serde_json::json!({"mis": "run.mis", "mis_rejected": null})),
            ("rejected", serde_json::json!({"mis": null, "mis_rejected": {"file": "run.mis", "reason": "RegionNumber 1 has no <Area> (the file has 1)"}})),
        ] {
            let entry = images(name, Some(bruker))[0].clone();
            assert!(entry.get("affine").is_none(), "{name}: {entry:#}");
            assert_eq!((&entry["archive_path"], &entry["width"], &entry["height"], &entry["role"]), (&serde_json::json!("images/image_0000.png"), &serde_json::json!(56), &serde_json::json!(48), &serde_json::json!("optical")), "{name}");
            assert_eq!(entry["sha256"].as_str().map(str::len), Some(64), "{name}");
        }
        std::fs::remove_file(&img).ok();
    }

    /// The sequence's image and later images on an archive that carries the teach-point
    /// registration (owner decision D8). `embed_sequence_image` embeds the `<ImageFile>` found
    /// beside the `.d` with the `teach_points` affine and the registration record. `--image` then
    /// places an image of the same size in that frame, gives a differently sized one no affine (and
    /// says why), does not embed the sequence's image a second time, and, when the sequence's image
    /// was not embedded, places only an image of its name.
    #[test]
    fn the_sequence_image_and_later_images_are_placed_by_the_registration() {
        use mzpeak_prototyping::writer::MzPeakWriterType;
        use mzpeaks::{CentroidPeak, DeconvolutedPeak};
        use std::io::Read as _;

        let png = |w: u32, h: u32, tail: u8| {
            let mut png = Vec::new();
            png.extend_from_slice(b"\x89PNG\r\n\x1a\n");
            png.extend_from_slice(&13u32.to_be_bytes());
            png.extend_from_slice(b"IHDR");
            png.extend_from_slice(&w.to_be_bytes());
            png.extend_from_slice(&h.to_be_bytes());
            png.extend_from_slice(&[8, 2, 0, 0, 0, tail]);
            png
        };
        let dir = std::env::temp_dir().join(format!("mzpc_embed_aux_seq_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("run.d")).unwrap();
        let dot_d = dir.join("run.d");
        std::fs::write(dir.join("IMG.png"), png(56, 48, 0)).unwrap();
        std::fs::write(dir.join("copy.png"), png(56, 48, 1)).unwrap();
        std::fs::write(dir.join("other.png"), png(57, 48, 0)).unwrap();
        let registration = serde_json::json!({"matrix": [0.25, 0.0, -3.0, 0.0, 0.25, -2.0], "sequence": "run.mis",
            "teach_points": [{"image_px": [1.0, 2.0], "stage_um": [10.0, 20.0]}], "reference_point": {"image_px": [1.0, 2.0], "raster_index": [3, 4]}});
        let marker = serde_json::json!({"is_imaging": true, "coordinate_base": 1, "pixel_count": {"x": 28, "y": 24}});
        let bruker = |found: bool| {
            serde_json::json!({"mis": "run.mis", "mis_rejected": null, "sequence_image": {"file": "IMG.png", "found": found, "other_images": []}, "registration": registration})
        };
        let run = |name: &str, found: bool, later: &[&str]| -> (Vec<String>, Vec<serde_json::Value>) {
            let out = dir.join(format!("{name}.mzpeak"));
            let writer = MzPeakWriterType::<File, CentroidPeak, DeconvolutedPeak>::builder().build(File::create(&out).unwrap(), true);
            let mut zip = writer.finish_parquet().unwrap();
            zip.add_index_metadata("imaging", &marker).unwrap();
            zip.add_index_metadata("bruker_maldi", &bruker(found)).unwrap();
            embed_sequence_image(&mut zip, &dot_d).unwrap();
            let later: Vec<PathBuf> = later.iter().map(|n| dir.join(n)).collect();
            embed_optical_images(&mut zip, Path::new("/x/run.mzpeak"), &later).unwrap();
            zip.finish().unwrap();
            let mut archive = zip::ZipArchive::new(BufReader::new(File::open(&out).unwrap())).unwrap();
            let members: Vec<String> = archive.file_names().filter(|n| n.starts_with("images/")).map(str::to_string).collect();
            let mut idx = String::new();
            archive.by_name("mzpeak_index.json").unwrap().read_to_string(&mut idx).unwrap();
            let images = serde_json::from_str::<serde_json::Value>(&idx).unwrap()["metadata"]["imaging"]["images"].as_array().cloned().unwrap_or_default();
            (members, images)
        };
        let quality = |e: &serde_json::Value| e.get("affine").map(|a| a["registration_quality"].as_str().unwrap().to_string());

        // The sequence's image first, then a same-size image, a differently sized one, and the
        // sequence's image again.
        let (members, images) = run("found", true, &["copy.png", "other.png", "IMG.png"]);
        assert_eq!(members, ["images/image_0000.png", "images/image_0001.png", "images/image_0002.png"]);
        assert_eq!(images.len(), 3, "{images:#?}");
        assert_eq!((images[0]["source_name"].as_str(), quality(&images[0])), (Some("IMG.png"), Some(QUALITY_STR.into())));
        assert_eq!(images[0]["affine"]["matrix"], registration["matrix"]);
        assert_eq!(images[0]["registration"], registration);
        assert!(images[0]["source"].as_str().unwrap().contains("run.mis"));
        assert_eq!((images[1]["source_name"].as_str(), quality(&images[1])), (Some("copy.png"), Some(QUALITY_STR.into())), "the same pixel frame");
        assert_eq!(images[1]["affine"]["matrix"], registration["matrix"]);
        assert_eq!((images[2]["source_name"].as_str(), quality(&images[2])), (Some("other.png"), None), "57 × 48 is not 56 × 48");
        assert!(images[2].get("registration").is_none());

        // The sequence's image was not beside it: nothing embedded by the sequence; a later image
        // of its name is placed, one of another name (even the same size) is not.
        let (members, images) = run("missing", false, &["copy.png", "IMG.png"]);
        assert_eq!(members, ["images/image_0000.png", "images/image_0001.png"]);
        assert_eq!((images[0]["source_name"].as_str(), quality(&images[0])), (Some("copy.png"), None));
        assert_eq!((images[1]["source_name"].as_str(), quality(&images[1])), (Some("IMG.png"), Some(QUALITY_STR.into())));
        let _ = std::fs::remove_dir_all(&dir);
    }

    const QUALITY_STR: &str = crate::mis_registration::QUALITY;

    #[test]
    fn detect_format_by_magic() {
        let p = write_tmp("tiff", b"II\x2A\x00rest");
        assert_eq!(detect_format(&p).unwrap(), ImageFormat::Tiff);
        std::fs::remove_file(&p).ok();
        let p = write_tmp("png", b"\x89PNG\r\n\x1a\nrest");
        assert_eq!(detect_format(&p).unwrap(), ImageFormat::Png);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn png_dimensions_from_ihdr() {
        let mut png = Vec::new();
        png.extend_from_slice(b"\x89PNG\r\n\x1a\n");
        png.extend_from_slice(&13u32.to_be_bytes());
        png.extend_from_slice(b"IHDR");
        png.extend_from_slice(&640u32.to_be_bytes());
        png.extend_from_slice(&480u32.to_be_bytes());
        let p = write_tmp("png_dim", &png);
        assert_eq!(read_png_dimensions(&p).unwrap(), (640, 480));
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn sha256_known_digest() {
        let p = write_tmp("sha", b"hello mzml2mzpeak");
        let (hex, size) = sha256_and_size(&p).unwrap();
        std::fs::remove_file(&p).ok();
        assert_eq!(hex, "e62b8c0e21fdf74bc00ea8b1d6fa563768c75ea98589b8283184e5ef985d841b");
        assert_eq!(size, 17);
    }

    #[test]
    fn accession_hint_strips_sdrf_suffix() {
        assert_eq!(accession_hint(Path::new("/x/MTBLS1129.sdrf.tsv")), "MTBLS1129");
        assert_eq!(accession_hint(Path::new("/x/PXD000001.tsv")), "PXD000001");
    }

    /// End-to-end self-check: convert a tiny in-memory mzpeak archive, embed an SDRF + a PNG image,
    /// finish, and assert the members + index blocks appear (the runnable check the task requires).
    #[test]
    fn embed_members_and_index_blocks_appear() {
        use mzpeak_prototyping::writer::MzPeakWriterType;
        use mzpeaks::{CentroidPeak, DeconvolutedPeak};
        use std::io::Read as _;

        let out = std::env::temp_dir().join(format!("mzpc_embed_aux_e2e_{}.mzpeak", std::process::id()));
        let _ = std::fs::remove_file(&out);

        // A tiny PNG (8x4) and an SDRF TSV.
        let mut png = Vec::new();
        png.extend_from_slice(b"\x89PNG\r\n\x1a\n");
        png.extend_from_slice(&13u32.to_be_bytes());
        png.extend_from_slice(b"IHDR");
        png.extend_from_slice(&8u32.to_be_bytes());
        png.extend_from_slice(&4u32.to_be_bytes());
        png.extend_from_slice(&[8, 2, 0, 0, 0]);
        let img = write_tmp("e2e.png", &png);
        // SDRF in a clean-named file so the accession-hint reads "MTBLS9999" from the stem.
        let sdrf_dir = std::env::temp_dir().join(format!("mzpc_embed_aux_sdrf_{}", std::process::id()));
        std::fs::create_dir_all(&sdrf_dir).unwrap();
        let sdrf = sdrf_dir.join("MTBLS9999.sdrf.tsv");
        File::create(&sdrf).unwrap().write_all(b"source name\tassay name\nS1\tA1\n").unwrap();

        let handle = File::create(&out).unwrap();
        let writer = MzPeakWriterType::<File, CentroidPeak, DeconvolutedPeak>::builder()
            .build(handle, true);
        let mut zip = writer.finish_parquet().expect("finish_parquet");

        // Strict image embed needs a grid; pass it directly via embed_one_image to avoid an imzML.
        let mut entry = embed_one_image(&mut zip, &img, 0, EmbedMode::Strict)
            .expect("embed image")
            .expect("image entry");
        assert_eq!(place(&mut entry, &Placement::FullExtent(8, 4)), None);
        let block = serde_json::json!({"is_imaging": true, "coordinate_base": 1, "images": [entry]});
        zip.add_index_metadata("imaging", &block).unwrap();

        embed_sdrf(&mut zip, Path::new("/x/run.mzML"), &sdrf).expect("embed sdrf");
        zip.finish().expect("finish");

        // Re-open and assert.
        let mut archive = zip::ZipArchive::new(BufReader::new(File::open(&out).unwrap())).unwrap();
        assert!(archive.by_name("images/image_0000.png").is_ok(), "image member present");
        assert!(archive.by_name(SDRF_MEMBER_NAME).is_ok(), "sdrf member present");

        let mut idx = String::new();
        archive.by_name("mzpeak_index.json").unwrap().read_to_string(&mut idx).unwrap();
        let v: serde_json::Value = serde_json::from_str(&idx).unwrap();
        let meta = &v["metadata"];
        assert_eq!(meta["imaging"]["images"][0]["archive_path"], "images/image_0000.png");
        // The imaging profile lists an image member as entity_type `image`, data_kind `other`.
        let listed = v["files"].as_array().unwrap().iter().find(|f| f["name"] == "images/image_0000.png").unwrap();
        assert_eq!((&listed["entity_type"], &listed["data_kind"]), (&serde_json::json!("image"), &serde_json::json!("other")), "{listed:#}");
        assert_eq!(meta["imaging"]["images"][0]["role"], "optical");
        assert_eq!(meta["imaging"]["images"][0]["affine"]["maps"], "image_px -> ms_px");
        assert_eq!(meta["study"]["sample_metadata_ref"], SDRF_MEMBER_NAME);
        assert_eq!(meta["study"]["dataset_accession"], "MTBLS9999");
        assert_eq!(meta["sample_metadata"]["member"], SDRF_MEMBER_NAME);
        assert_eq!(meta["sample_metadata"]["size_bytes"], 29);

        std::fs::remove_file(&out).ok();
        std::fs::remove_file(&img).ok();
        std::fs::remove_dir_all(&sdrf_dir).ok();
    }
}

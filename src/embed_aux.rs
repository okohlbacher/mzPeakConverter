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
//!     registration_quality:"assumed_full_extent"}`), and `role:"optical"`.
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
/// `--sdrf` file, into the open `zip`. Adds the `metadata.imaging` / `metadata.study` /
/// `metadata.sample_metadata` index blocks. Does NOT call `zip.finish()` — the caller owns that.
///
/// `input` is the source mzML/imzML path (used for sibling discovery + run-id/accession hints).
///
/// STRICTNESS: a missing/unreadable explicit `--image` or the `--sdrf` file ERRORS the conversion;
/// a soft auto-discovered sibling image that is unreadable warns + is skipped.
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
    if let Some(sibling) = discover_sibling_optical_image(input) {
        embed_list.push((sibling, EmbedMode::Soft));
    }

    if embed_list.is_empty() {
        return Ok(());
    }

    // The full-extent affine maps image pixels onto the MS pixel grid Nx×Ny. Read the declared grid
    // (IMS:1000042 / IMS:1000043) from the imzML header. If unknown, a Strict --image hard-fails
    // (we have no grid to map onto); a Soft-only run warns + embeds nothing.
    let grid = read_imzml_pixel_grid(input);
    let (nx, ny) = match grid {
        Some(g) => g,
        None => {
            if embed_list.iter().any(|(_, m)| *m == EmbedMode::Strict) {
                bail!(
                    "cannot build optical-image overlay affine: MS pixel grid (IMS:1000042/43) is \
                     unknown for {} — an explicit --image needs a coordinate grid to map onto",
                    input.display()
                );
            }
            log::warn!(
                "MS pixel_count unknown for {} — skipping auto-discovered optical image",
                input.display()
            );
            return Ok(());
        }
    };

    let mut entries: Vec<serde_json::Value> = Vec::with_capacity(embed_list.len());
    // ordinal advances ONLY on a successful embed, so a skipped soft image leaves no gap.
    let mut ordinal: usize = 0;
    // Dedup canonicalized paths so --image X and a sibling that resolves to X embed once.
    let mut seen: Vec<PathBuf> = Vec::with_capacity(embed_list.len());

    for (path, mode) in &embed_list {
        let key = canonical_key(path);
        if seen.contains(&key) {
            continue;
        }
        if let Some(entry) = embed_one_image(zip, path, ordinal, nx, ny, *mode)? {
            entries.push(entry);
            seen.push(key);
            ordinal += 1;
        }
    }

    if entries.is_empty() {
        return Ok(());
    }

    // metadata.imaging.images[] — match the prototype's block shape. The forward port carries only
    // the discovery flag + images[] (the prototype's full geometry projection is out of scope here).
    let block = serde_json::json!({
        "is_imaging": true,
        "coordinate_base": 1,
        "images": entries,
    });
    zip.add_index_metadata("imaging", &block)
        .context("writing metadata.imaging index")?;
    Ok(())
}

/// Embed ONE optical image (any format) as `images/image_{ordinal:04}.<ext>`, returning its
/// `metadata.imaging.images[]` entry as a JSON value. The ordinal is the ONLY part of the archive
/// name that varies — the attacker-influenced source basename never reaches the archive path.
fn embed_one_image(
    zip: &mut ZipArchiveWriter<File>,
    path: &Path,
    ordinal: usize,
    nx: i64,
    ny: i64,
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

    // Stream the bytes into the ZIP as an Other/Proprietary member (64 KiB chunks inside
    // add_file_from_read — never a whole-file load).
    let mut f = match File::open(path) {
        Ok(f) => f,
        Err(_) => fail!("file became unreadable before embed"),
    };
    let fe = FileEntry::new(
        member.clone(),
        EntityType::Other("image".to_string()),
        DataKind::Proprietary,
    );
    if zip.add_file_from_read(&mut f, None::<&String>, Some(fe)).is_err() {
        fail!("failed to stream image bytes into the archive");
    }

    // SHA-256 + exact byte size over a SECOND bounded streamed pass.
    let (sha256, size) = match sha256_and_size(path) {
        Ok(v) => v,
        Err(_) => fail!("failed to digest image bytes"),
    };

    // Full-extent affine: TIFF uses real (w,h); a dimensionless (0,0) embed passes (1,1) so the
    // helper yields the constant-axis identity (0 would divide-by-zero; W==1/H==1 is guarded).
    let (aw, ah) = if w == 0 || h == 0 { (1, 1) } else { (w, h) };
    let matrix = full_extent_affine(nx, ny, aw, ah);

    Ok(Some(serde_json::json!({
        "archive_path": member,
        "source_name": source_name,
        "media_type": media_type,
        "width": w as i64,
        "height": h as i64,
        "sha256": sha256,
        "size_bytes": size as i64,
        "affine": {
            "type": "affine",
            "matrix": matrix,
            "maps": "image_px -> ms_px",
            "registration_quality": "assumed_full_extent",
        },
        "role": "optical",
    })))
}

/// Build the full-extent affine `[a,b,c,d,e,f]` mapping 0-based image pixels into the 1-based MS
/// pixel grid Nx×Ny: `(x_ms, y_ms) = (a·col + c, e·row + f)`. `a = (nx-1)/(w-1)` (0 when w==1);
/// `e = (ny-1)/(h-1)` (0 when h==1); `b=d=0`, `c=f=1`. Corner check: (0,0)→(1,1), (W-1,H-1)→(Nx,Ny).
fn full_extent_affine(nx: i64, ny: i64, w: u32, h: u32) -> [f64; 6] {
    let a = if w > 1 {
        (nx - 1) as f64 / (w - 1) as f64
    } else {
        0.0
    };
    let e = if h > 1 {
        (ny - 1) as f64 / (h - 1) as f64
    } else {
        0.0
    };
    [a, 0.0, 1.0, 0.0, e, 1.0]
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

/// Read the declared MS pixel grid `(Nx, Ny)` from an imzML header's `IMS:1000042` ("max count of
/// pixel x") + `IMS:1000043` ("max count of pixel y") scanSettings cvParams. Returns None when the
/// input is not a readable imzML or the grid is not declared. Bounded scan of the header bytes.
fn read_imzml_pixel_grid(input: &Path) -> Option<(i64, i64)> {
    // Only mzML/imzML inputs are files; a vendor .d directory has no grid here.
    if !input.is_file() {
        return None;
    }
    // Read a bounded prefix of the header (the scanSettingsList lives before <run>). 4 MiB is far
    // more than any imzML header; we stop at the first <run> if seen.
    let mut f = File::open(input).ok()?;
    let mut buf = vec![0u8; 4 * 1024 * 1024];
    let n = read_prefix(&mut f, &mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf[..n]);
    let head = match text.find("<run") {
        Some(idx) => &text[..idx],
        None => &text[..],
    };
    let nx = grid_value(head, "IMS:1000042")?;
    let ny = grid_value(head, "IMS:1000043")?;
    Some((nx, ny))
}

/// Extract the integer `value="N"` of the `<cvParam accession="<acc>" ... value="N"/>` in `head`.
fn grid_value(head: &str, accession: &str) -> Option<i64> {
    let needle = format!("accession=\"{accession}\"");
    let start = head.find(&needle)?;
    // Find the value="..." after the accession, bounded to the same tag (before the next '>').
    let rest = &head[start..];
    let tag_end = rest.find('>').unwrap_or(rest.len());
    let tag = &rest[..tag_end];
    let v_idx = tag.find("value=\"")? + "value=\"".len();
    let v_rest = &tag[v_idx..];
    let v_end = v_rest.find('"')?;
    v_rest[..v_end].trim().parse::<i64>().ok()
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
    let mut f = File::open(path)?;
    let mut hasher = Sha256::new();
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

    #[test]
    fn affine_corner_maps() {
        // Nx=10, Ny=20, W=5, H=8 → a=2.25, e=19/7; (0,0)→(1,1), (4,7)→(10,20).
        let m = full_extent_affine(10, 20, 5, 8);
        let apply = |col: f64, row: f64| (m[0] * col + m[2], m[4] * row + m[5]);
        let (x0, y0) = apply(0.0, 0.0);
        assert!((x0 - 1.0).abs() < 1e-9 && (y0 - 1.0).abs() < 1e-9);
        let (x1, y1) = apply(4.0, 7.0);
        assert!((x1 - 10.0).abs() < 1e-9 && (y1 - 20.0).abs() < 1e-9);
    }

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

    #[test]
    fn grid_value_parses_cvparam() {
        let head = r#"<cvParam accession="IMS:1000042" name="max count of pixel x" value="260"/>"#;
        assert_eq!(grid_value(head, "IMS:1000042"), Some(260));
        assert_eq!(grid_value(head, "IMS:9999999"), None);
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
        let entry = embed_one_image(&mut zip, &img, 0, 8, 4, EmbedMode::Strict)
            .expect("embed image")
            .expect("image entry");
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

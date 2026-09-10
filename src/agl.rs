//! The `AGL2` file the net48 Agilent host (`glue/agilent/Glue.cs`) writes and `agilent.rs` reads.
//!
//! Host-independent ON PURPOSE (same reasoning as `pwiz_layout`): the reader module is
//! `#[cfg(windows)]`, so a parser inside it could be neither compiled nor tested here, and the
//! Shimadzu lane already showed what an unpinned struct twin does. The byte protocol lives here
//! with tests that run on every host; only the process spawn stays gated.
//!
//! ```text
//!   "AGL2" | count u64 | scan_types: len u32 + UTF-8 | device: len u32 + UTF-8 | offset[count] u64
//!   per record at offset[i]:
//!     rt f64 | msLevel i32 | polarity i32 | isCentroid i32 | scanId i32 | nPoints u64 |
//!     mz[nPoints] f64 | intensity[nPoints] f64
//! ```
//! Little-endian throughout. `scan_types` is MHDAC's `MSScanType` flags as its `ToString()`
//! ("Scan", "MultipleReaction, SelectedIon", …); `device` is
//! `"<DeviceType>\u{1F}<device name>\u{1F}<serial>"`, any part empty when MHDAC did not say.
//! `AGL1` (0.9.x) had no strings: a reader given one asks for a rebuilt host rather than
//! guessing where the table starts.
#![cfg_attr(not(windows), allow(dead_code))]

use std::io::{Read, Seek, SeekFrom};

use anyhow::{anyhow, bail, Context, Result};

pub const MAGIC: &[u8; 4] = b"AGL2";
const MAGIC_V1: &[u8; 4] = b"AGL1";
/// Bytes of a record header before the two arrays: rt f64 + 4 × i32 + nPoints u64.
pub const RECORD_HEADER_BYTES: u64 = 32;

/// The run-level part of an `AGL2` file.
#[derive(Debug, Clone, PartialEq)]
pub struct Index {
    pub scan_types: String,
    pub device: String,
    /// Absolute file offset of each record.
    pub offsets: Vec<u64>,
}

/// One record's header, as written by the host.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RecordHeader {
    pub rt_minutes: f64,
    pub ms_level: i32,
    pub polarity: i32,
    pub is_centroid: i32,
    pub scan_id: i32,
    pub n_points: u64,
}

/// What a successful host reported rewriting, read back from the `[count key=value]` tags on its
/// stderr notes (`Glue.cs`). A host built before the tags existed reports nothing here; its notes
/// are still logged.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HostCounts {
    /// Intensities MHDAC returned as NaN or ±Inf, stored as 0.
    pub nonfinite_intensities: u64,
    /// Spectra whose m/z and intensity arrays differed in length and were cut to the shorter.
    pub truncated_spectra: u64,
}

impl HostCounts {
    /// The `transformations` entries, each only when its count is above zero.
    pub fn transformations(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.nonfinite_intensities > 0 {
            out.push("agilent:nonfinite-intensity-to-zero".to_string());
        }
        if self.truncated_spectra > 0 {
            out.push("agilent:truncate-unequal-arrays".to_string());
        }
        out
    }
}

/// The [`HostCounts`] in a host's stderr. Lines without a tag, and unknown keys, are ignored.
pub fn host_counts(stderr: &str) -> HostCounts {
    let mut counts = HostCounts::default();
    for line in stderr.lines() {
        let Some((_, tag)) = line.rsplit_once("[count ") else { continue };
        let Some((key, value)) = tag.trim_end().trim_end_matches(']').split_once('=') else { continue };
        let Ok(n) = value.trim().parse::<u64>() else { continue };
        match key.trim() {
            "nonfinite_intensities" => counts.nonfinite_intensities += n,
            "truncated_spectra" => counts.truncated_spectra += n,
            _ => {}
        }
    }
    counts
}

fn read_u32(f: &mut impl Read) -> Result<u32> {
    let mut b = [0u8; 4];
    f.read_exact(&mut b)?;
    Ok(u32::from_le_bytes(b))
}
fn read_u64(f: &mut impl Read) -> Result<u64> {
    let mut b = [0u8; 8];
    f.read_exact(&mut b)?;
    Ok(u64::from_le_bytes(b))
}
fn read_i32(f: &mut impl Read) -> Result<i32> {
    let mut b = [0u8; 4];
    f.read_exact(&mut b)?;
    Ok(i32::from_le_bytes(b))
}
fn read_f64(f: &mut impl Read) -> Result<f64> {
    let mut b = [0u8; 8];
    f.read_exact(&mut b)?;
    Ok(f64::from_le_bytes(b))
}
fn read_vec(f: &mut impl Read, n: usize) -> Result<Vec<u8>> {
    let mut buf = vec![0u8; n];
    f.read_exact(&mut buf).context("short read")?;
    Ok(buf)
}

/// Read a `len u32 + UTF-8` string whose length must fit in what is left of the file.
fn read_string(f: &mut impl Read, remaining: u64) -> Result<(String, u64)> {
    let len = read_u32(f)? as u64;
    if len > remaining.saturating_sub(4) {
        bail!("string length {len} exceeds the {remaining} bytes left in the file");
    }
    let bytes = read_vec(f, len as usize)?;
    Ok((String::from_utf8_lossy(&bytes).into_owned(), 4 + len))
}

/// Parse the run-level index at the start of an `AGL2` file of `file_len` bytes. Every bound is
/// checked against `file_len` so a truncated or corrupt file fails here instead of in an
/// allocation: the host publishes atomically (`.part` rename), but "should never happen" is not a
/// reason to index an unchecked on-disk length.
pub fn parse_index(f: &mut (impl Read + Seek), file_len: u64) -> Result<Index> {
    f.seek(SeekFrom::Start(0))?;
    let mut magic = [0u8; 4];
    f.read_exact(&mut magic).context("reading magic")?;
    if &magic == MAGIC_V1 {
        bail!(
            "Agilent host output is the AGL1 format of an older AgilentGlueHost.exe — rebuild \
             glue/agilent (`dotnet build -c Release`) so the host and the converter agree"
        );
    }
    if &magic != MAGIC {
        bail!("Agilent host output has bad magic {magic:?}");
    }
    let count = read_u64(f).context("reading count")?;
    let mut consumed: u64 = 12;
    let (scan_types, n) = read_string(f, file_len.saturating_sub(consumed)).context("reading scan types")?;
    consumed += n;
    let (device, n) = read_string(f, file_len.saturating_sub(consumed)).context("reading device")?;
    consumed += n;
    // The offset table (count × 8) must fit, and count × 8 must not overflow.
    let table_bytes = count
        .checked_mul(8)
        .filter(|b| *b <= file_len.saturating_sub(consumed))
        .ok_or_else(|| anyhow!("Agilent host output declares {count} records, too many for its {file_len} bytes"))?;
    let bytes = read_vec(f, table_bytes as usize).context("reading offset table")?;
    let offsets: Vec<u64> = bytes.chunks_exact(8).map(|c| u64::from_le_bytes(c.try_into().unwrap())).collect();
    let first_record = consumed + table_bytes;
    if let Some(bad) = offsets
        .iter()
        .find(|&&o| o < first_record || o.checked_add(RECORD_HEADER_BYTES).is_none_or(|end| end > file_len))
    {
        bail!("Agilent host output has a record offset {bad} outside the file ({file_len} bytes)");
    }
    Ok(Index { scan_types, device, offsets })
}

/// Read the record at `offset`: header, m/z (f64) and intensity (narrowed f64 → f32, matching the
/// other vendor readers' `IntensityArray` dtype).
pub fn read_record(f: &mut (impl Read + Seek), offset: u64, file_len: u64) -> Result<(Vec<f64>, Vec<f32>, RecordHeader)> {
    let arrays_at = offset
        .checked_add(RECORD_HEADER_BYTES)
        .filter(|end| *end <= file_len)
        .ok_or_else(|| anyhow!("record offset {offset} leaves no room for a header in {file_len} bytes"))?;
    f.seek(SeekFrom::Start(offset)).with_context(|| format!("seeking to record at {offset}"))?;
    let hdr = (|| -> Result<RecordHeader> {
        Ok(RecordHeader {
            rt_minutes: read_f64(f)?,
            ms_level: read_i32(f)?,
            polarity: read_i32(f)?,
            is_centroid: read_i32(f)?,
            scan_id: read_i32(f)?,
            n_points: read_u64(f)?,
        })
    })()
    .with_context(|| format!("reading the record header at offset {offset}"))?;
    // Bound the two n×8 arrays against the file so a corrupt n_points cannot drive a huge alloc.
    let n = usize::try_from(hdr.n_points).map_err(|_| anyhow!("n_points {} does not fit in usize", hdr.n_points))?;
    n.checked_mul(16)
        .filter(|b| *b as u64 <= file_len - arrays_at)
        .ok_or_else(|| anyhow!("record at {offset}: n_points {n} exceeds the host file bounds"))?;
    let mz_bytes = read_vec(f, n * 8)?;
    let int_bytes = read_vec(f, n * 8)?;
    let mz: Vec<f64> = mz_bytes.chunks_exact(8).map(|c| f64::from_le_bytes(c.try_into().unwrap())).collect();
    let intensity: Vec<f32> =
        int_bytes.chunks_exact(8).map(|c| f64::from_le_bytes(c.try_into().unwrap()) as f32).collect();
    Ok((mz, intensity, hdr))
}

/// MHDAC `MSScanType` flag names that mean the run holds SPECTRA (a scan over an m/z range).
const SPECTRUM_SCAN_TYPES: &[&str] =
    &["Scan", "HighResolutionScan", "ProductIon", "PrecursorIon", "NeutralLoss", "NeutralGain"];
/// … and the ones that mean per-transition / per-ion dwell data, which are CHROMATOGRAMS.
const DWELL_SCAN_TYPES: &[&str] = &["MultipleReaction", "SelectedIon"];

/// True when MHDAC's `ScanTypes` names any MRM / SIM dwell kind at all — beside scans, in a mixed
/// method. Such a run keeps its scans, but MHDAC also yields the dwells as one-point spectra and the
/// host writes every row it is handed, so the caller warns (per-record scan types are a protocol
/// item in the backlog).
pub fn has_dwell(scan_types: &str) -> bool {
    scan_types.split(',').map(str::trim).any(|n| DWELL_SCAN_TYPES.contains(&n))
}

/// True when MHDAC's `ScanTypes` names only MRM / SIM dwell data and no spectrum scan type: such
/// a `.d` (a 6490 dMRM run, say) has no spectra to store — MHDAC still yields one one-point "MS2
/// spectrum" per dwell, which is what this lane wrote before the guard existed: 27,674 one-point
/// spectra for MTBLS243 where the msconvert lane writes 113 SRM transition chromatograms. An empty
/// or unreadable `scan_types` is not judged (false), so the conversion proceeds.
pub fn is_dwell_only(scan_types: &str) -> bool {
    let names: Vec<&str> = scan_types.split(',').map(str::trim).filter(|s| !s.is_empty()).collect();
    if names.is_empty() {
        return false;
    }
    let spectra = names.iter().any(|n| SPECTRUM_SCAN_TYPES.contains(n));
    let dwell = names.iter().any(|n| DWELL_SCAN_TYPES.contains(n));
    dwell && !spectra
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Write};

    /// Write an `AGL2` file the way Glue.cs does (little-endian, table back-filled).
    fn write_agl2(scan_types: &str, device: &str, records: &[(RecordHeader, Vec<f64>, Vec<f64>)]) -> Vec<u8> {
        let mut w = Cursor::new(Vec::new());
        w.write_all(MAGIC).unwrap();
        w.write_all(&(records.len() as u64).to_le_bytes()).unwrap();
        for s in [scan_types, device] {
            w.write_all(&(s.len() as u32).to_le_bytes()).unwrap();
            w.write_all(s.as_bytes()).unwrap();
        }
        let table_pos = w.position();
        for _ in records {
            w.write_all(&0u64.to_le_bytes()).unwrap();
        }
        let mut offsets = Vec::new();
        for (h, mz, it) in records {
            offsets.push(w.position());
            w.write_all(&h.rt_minutes.to_le_bytes()).unwrap();
            for v in [h.ms_level, h.polarity, h.is_centroid, h.scan_id] {
                w.write_all(&v.to_le_bytes()).unwrap();
            }
            w.write_all(&(mz.len() as u64).to_le_bytes()).unwrap();
            for v in mz {
                w.write_all(&v.to_le_bytes()).unwrap();
            }
            for v in it {
                w.write_all(&v.to_le_bytes()).unwrap();
            }
        }
        w.set_position(table_pos);
        for o in offsets {
            w.write_all(&o.to_le_bytes()).unwrap();
        }
        w.into_inner()
    }

    fn rec(rt: f64, n: usize) -> (RecordHeader, Vec<f64>, Vec<f64>) {
        let h = RecordHeader { rt_minutes: rt, ms_level: 1, polarity: 1, is_centroid: 0, scan_id: 7, n_points: n as u64 };
        ((h), (0..n).map(|i| 100.0 + i as f64 * 0.5).collect(), (0..n).map(|i| (i * 3) as f64).collect())
    }

    #[test]
    fn round_trips_index_and_records() {
        let bytes = write_agl2("Scan, ProductIon", "QuadrupoleTimeOfFlight\u{1F}6545 Q-TOF\u{1F}SG12345", &[rec(1.5, 3), rec(2.5, 0), rec(3.5, 5)]);
        let mut c = Cursor::new(bytes.clone());
        let idx = parse_index(&mut c, bytes.len() as u64).unwrap();
        assert_eq!(idx.scan_types, "Scan, ProductIon");
        assert_eq!(idx.device.split('\u{1F}').collect::<Vec<_>>(), ["QuadrupoleTimeOfFlight", "6545 Q-TOF", "SG12345"]);
        assert_eq!(idx.offsets.len(), 3);
        let (mz, it, h) = read_record(&mut c, idx.offsets[0], bytes.len() as u64).unwrap();
        assert_eq!((h.rt_minutes, h.scan_id, h.n_points), (1.5, 7, 3));
        assert_eq!(mz, vec![100.0, 100.5, 101.0]);
        assert_eq!(it, vec![0.0, 3.0, 6.0]);
        let (mz, it, h) = read_record(&mut c, idx.offsets[1], bytes.len() as u64).unwrap();
        assert!(mz.is_empty() && it.is_empty() && h.n_points == 0, "an empty scan is legitimate");
        let (mz, _, _) = read_record(&mut c, idx.offsets[2], bytes.len() as u64).unwrap();
        assert_eq!(mz.len(), 5);
    }

    #[test]
    fn refuses_the_v1_format_and_corrupt_bounds() {
        let mut v1 = write_agl2("", "", &[rec(1.0, 2)]);
        v1[..4].copy_from_slice(b"AGL1");
        let err = parse_index(&mut Cursor::new(v1.clone()), v1.len() as u64).unwrap_err().to_string();
        assert!(err.contains("AGL1") && err.contains("rebuild"), "{err}");

        let ok = write_agl2("Scan", "", &[rec(1.0, 2)]);
        // a count that cannot fit
        let mut big = ok.clone();
        big[4..12].copy_from_slice(&u64::MAX.to_le_bytes());
        assert!(parse_index(&mut Cursor::new(big.clone()), big.len() as u64).is_err());
        // a string length past the end
        let mut s = ok.clone();
        s[12..16].copy_from_slice(&1_000_000u32.to_le_bytes());
        assert!(parse_index(&mut Cursor::new(s.clone()), s.len() as u64).is_err());
        // a truncated file: the record's arrays are cut off
        let cut = ok[..ok.len() - 8].to_vec();
        let idx = parse_index(&mut Cursor::new(cut.clone()), cut.len() as u64).unwrap();
        assert!(read_record(&mut Cursor::new(cut.clone()), idx.offsets[0], cut.len() as u64).is_err());
    }

    #[test]
    fn dwell_only_runs_are_recognised_by_scan_type_names() {
        assert!(is_dwell_only("MultipleReaction"));
        assert!(is_dwell_only("SelectedIon, MultipleReaction"));
        assert!(is_dwell_only("TotalIon, SelectedIon"));
        assert!(!is_dwell_only("Scan"));
        assert!(!is_dwell_only("Scan, MultipleReaction"), "a run with scans keeps them");
        assert!(has_dwell("Scan, MultipleReaction"), "... but the caller is told about the dwells");
        assert!(!has_dwell("Scan, ProductIon"));
        assert!(!is_dwell_only("HighResolutionScan"));
        assert!(!is_dwell_only(""), "unknown is not judged");
        assert!(!is_dwell_only("Unspecified"));
    }

    /// The host is the other half of this protocol and is not compiled here: pin the C# writer's
    /// magic, string order and record field order against this parser, CRLF-normalised (the box
    /// checks out with `core.autocrlf=true`). The Shimadzu lane earned its ABI pin after exactly
    /// this drift class shipped; the Agilent twin gets one before it can.
    #[test]
    fn glue_writes_what_this_parser_reads() {
        let cs = include_str!("../glue/agilent/Glue.cs").replace("\r\n", "\n");
        assert!(
            cs.contains("bw.Write((byte)'A'); bw.Write((byte)'G'); bw.Write((byte)'L'); bw.Write((byte)'2');"),
            "Glue.cs no longer writes the AGL2 magic"
        );
        assert_eq!(MAGIC, b"AGL2");
        let count = cs.find("bw.Write((ulong)count);").expect("count u64");
        let st = cs.find("WriteString(bw, reader.ScanTypes);").expect("scan types string");
        let dev = cs.find("WriteString(bw, reader.Device);").expect("device string");
        let table = cs.find("long tablePos = fs.Position;").expect("offset table");
        assert!(count < st && st < dev && dev < table, "Glue.cs header order is count, scan types, device, table");
        let rec = "bw.Write(s.RtMinutes);\n                        bw.Write(s.MsLevel);\n                        bw.Write(s.Polarity);\n                        bw.Write(s.IsCentroid);\n                        bw.Write(s.ScanId);";
        assert!(cs.contains(rec), "Glue.cs record header order drifted from rt, msLevel, polarity, isCentroid, scanId");
        assert!(cs.contains("bw.Write((ulong)n);"), "nPoints u64 after the header");
        assert_eq!(RECORD_HEADER_BYTES, 8 + 4 * 4 + 8);
        assert!(cs.contains("bw.Write((uint)b.Length);"), "strings are len u32 + UTF-8, not BinaryWriter's 7-bit prefix");
    }
}

#[cfg(test)]
mod host_counts_tests {
    use super::*;

    #[test]
    fn host_counts_read_the_bracketed_tags() {
        let stderr = [
            "AgilentGlueHost: 3 intensity value(s) were NaN/Inf and were stored as 0 [count nonfinite_intensities=3]",
            "AgilentGlueHost: 2 spectrum/spectra had m/z and intensity arrays of different lengths and were cut to the shorter (5 point(s) dropped) [count truncated_spectra=2]",
            "an unrelated line",
        ]
        .join("\r\n");
        let c = host_counts(&stderr);
        assert_eq!(c, HostCounts { nonfinite_intensities: 3, truncated_spectra: 2 });
        assert_eq!(c.transformations(), ["agilent:nonfinite-intensity-to-zero", "agilent:truncate-unequal-arrays"]);
        // A host from before the tags: its note is logged, but nothing is counted or declared.
        let old = host_counts("AgilentGlueHost: 3 intensity value(s) were NaN/Inf and were stored as 0");
        assert_eq!(old, HostCounts::default());
        assert!(old.transformations().is_empty());
    }
}

//! The `fidelity` index block, and the check behind `--lossless`.
//!
//! `transformations` names what a conversion changed; this block says by how much. Per signal
//! facet (`spectra_data`, `spectra_peaks`): the points the reader handed over against the points
//! stored (the difference is what the zero-run mask left out), the numeric types the source
//! declares for m/z and intensity where the lane knows them (mzML and imzML: the binary data
//! types) against the stored column types, and, in `mz_error`, one entry per m/z encoding that can
//! move a value, with its maximum absolute and relative error.
//!
//! Where the numbers come from:
//!
//! * **Source side**: [`SourceTally`], counted by the lane over the spectra it writes, with the
//!   routing rule of the writer (`write_spectrum_data`): profile and unknown-continuity arrays go
//!   to `spectra_data`, centroid arrays and peak sets to `spectra_peaks`.
//! * **Stored side**: read back from the two facets as they sit in the archive, after the Parquet
//!   members are closed and before the index is written ([`stored`]): the footer's point count,
//!   the schema's column types, and the chunk rows' bounds, encoding and numpress bytes. Nothing
//!   is taken from what the lane was configured to do.
//! * **numpress-linear** is a BOUND, not a measurement: per stored chunk `0.5 / fixed point` plus
//!   four units in the last place of the chunk's largest m/z, the fixed point read from the first
//!   eight bytes of the encoded chunk. `0.5 / fixed point` alone is what the codec's rounding
//!   allows in exact arithmetic; the f64 products and the division add up to four ulp (observed:
//!   1e-14 Da beyond the bare bound at m/z 771, so a check against the bare bound fails). The
//!   relative figure divides each chunk's bound by the chunk's smallest m/z. A measured maximum
//!   would need the source values next to the decoded ones, which no single point of the write
//!   path holds without vendored changes; the bound is tight to one part in 1e6 on real data.
//! * **delta** stores `fl(a − b)`, and `b + fl(a − b) == a` is guaranteed only for `a ≤ 2b`
//!   (Sterbenz) or for m/z that are 32-bit values. A chunk whose last m/z is at most twice its
//!   first is therefore exact by construction; the others are counted, with the largest m/z at
//!   risk and the unit in the last place there (the errors seen are one such unit).
//! * **grid fits** (`grid-fit:<tol>Da`, `tof-grid:<ppm>ppm`) state the tolerance every fitted value
//!   was accepted within, taken from the `transformations` entry.

use std::collections::BTreeSet;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

use anyhow::{Context, Result, bail};
use arrow::array::{Array, ArrayRef, AsArray, StructArray};
use arrow::datatypes::{DataType, Field, Float64Type, UInt8Type};
use mzdata::prelude::*;
use mzdata::spectrum::bindata::{ArrayType, BinaryArrayMap, BinaryDataArrayType};
use mzdata::spectrum::{MultiLayerSpectrum, RefPeakDataLevel, SignalContinuity};
use mzpeak_prototyping::archive::ArchiveFacetReader;
use mzpeaks::{CentroidLike, DeconvolutedCentroidLike};
use parquet::arrow::ProjectionMask;
use parquet::arrow::arrow_reader::{ArrowReaderMetadata, ParquetRecordBatchReaderBuilder};
use rayon::prelude::*;
use serde_json::{Value, json};

/// The index block's key.
pub const BLOCK: &str = "fidelity";

/// The two signal facets the block describes, by archive member and block key.
const FACETS: [(&str, &str); 2] = [("spectra_data.parquet", "spectra_data"), ("spectra_peaks.parquet", "spectra_peaks")];

/// `transformations` entries that change the stored spectrum signal: a point dropped, re-ordered
/// or summed, or an m/z or intensity value moved. `--lossless` fails when any is declared. Every
/// other entry describes metadata (a dropped reference, a pixel size, a position), a chromatogram
/// or device trace, or a precursor window, and does not count. A NEW entry that touches spectrum
/// signal belongs here, or in [`SIGNAL_TRANSFORMATION_PREFIXES`] when it carries a parameter.
pub const SIGNAL_TRANSFORMATIONS: [&str; 17] = [
    "zero-run-mask",
    "numpress-linear",
    "sort-by-mz",
    "sort-by-wavelength",
    "shimadzu:span-trim",
    "shimadzu:coarse-mz",
    "agilent:drop-zero-samples",
    "agilent:intensity-f32-rounding",
    "agilent:nonfinite-intensity-to-zero",
    "agilent:truncate-unequal-arrays",
    "sciex:nan-intensity-to-zero",
    "sciex:clamp-intensity-to-f32",
    "sciex:truncate-unequal-arrays",
    "waters:drop-functions",
    "waters:sonar-summed",
    "bruker:mz-calibration-chord",
    "bruker:mz-calibrant-omitted",
];

/// Signal entries that carry their bound: `grid-fit:1e-6Da`, `tof-grid:5ppm`.
pub const SIGNAL_TRANSFORMATION_PREFIXES: [&str; 2] = ["grid-fit:", "tof-grid:"];

/// Is this `transformations` entry one of the signal set?
pub fn is_signal_transformation(entry: &str) -> bool {
    SIGNAL_TRANSFORMATIONS.contains(&entry) || SIGNAL_TRANSFORMATION_PREFIXES.iter().any(|p| entry.starts_with(p))
}

fn type_name(dtype: BinaryDataArrayType) -> &'static str {
    match dtype {
        BinaryDataArrayType::Float32 => "float32",
        BinaryDataArrayType::Float64 => "float64",
        BinaryDataArrayType::Int32 => "int32",
        BinaryDataArrayType::Int64 => "int64",
        BinaryDataArrayType::ASCII => "ascii",
        BinaryDataArrayType::Unknown => "unknown",
    }
}

/// What the reader handed over for one facet.
#[derive(Default)]
struct FacetSource {
    points: u64,
    mz: BTreeSet<&'static str>,
    intensity: BTreeSet<&'static str>,
}

impl FacetSource {
    /// A raw array map: its points (the m/z array's length, else the widest array's, as the
    /// writer counts a grid spectrum that carries an index axis and no m/z) and its array types.
    fn add_arrays(&mut self, arrays: &BinaryArrayMap, types: bool) {
        let n = match arrays.get(&ArrayType::MZArray) {
            Some(mz) => mz.data_len().unwrap_or(0),
            None => arrays.iter().filter_map(|(_, a)| a.data_len().ok()).max().unwrap_or(0),
        };
        self.points += n as u64;
        if types && n > 0 {
            self.add_types(arrays);
        }
    }

    fn add_types(&mut self, arrays: &BinaryArrayMap) {
        if let Some(a) = arrays.get(&ArrayType::MZArray) {
            self.mz.insert(type_name(a.dtype()));
        }
        if let Some(a) = arrays.get(&ArrayType::IntensityArray) {
            self.intensity.insert(type_name(a.dtype()));
        }
    }

    fn json(&self, types: bool) -> Value {
        let mut out = json!({"source_points": self.points});
        if types && !(self.mz.is_empty() && self.intensity.is_empty()) {
            out["source_types"] = json!({"mz": self.mz, "intensity": self.intensity});
        }
        out
    }
}

/// The lane's half of the block: the points and array types of every spectrum it handed to the
/// writer, by the facet the writer files them under.
pub struct SourceTally {
    data: FacetSource,
    peaks: FacetSource,
    /// The reader hands each array over at the binary type its source declares (mzML, imzML), so
    /// the types seen are the source's. A vendor reader's array types are its own choice.
    declared_types: bool,
}

impl SourceTally {
    pub fn new(declared_types: bool) -> Self {
        Self { data: FacetSource::default(), peaks: FacetSource::default(), declared_types }
    }

    /// Count one spectrum, as the writer will route it (`AbstractMzPeakWriter::write_spectrum_data`).
    /// Call it on the spectrum as it is handed to `write_spectrum`.
    pub fn observe<C: CentroidLike, D: DeconvolutedCentroidLike>(&mut self, spec: &MultiLayerSpectrum<C, D>) {
        // A wavelength spectrum goes to its own facet, which this block does not describe.
        if spec.spectrum_type().is_some_and(|t| !t.is_mass_spectrum()) {
            return;
        }
        let types = self.declared_types;
        let continuity = spec.signal_continuity();
        match spec.peaks() {
            RefPeakDataLevel::Missing => {}
            RefPeakDataLevel::RawData(arrays) => {
                if continuity == SignalContinuity::Centroid {
                    self.peaks.add_arrays(arrays, types);
                } else {
                    self.data.add_arrays(arrays, types);
                }
            }
            peaks @ (RefPeakDataLevel::Centroid(_) | RefPeakDataLevel::Deconvoluted(_)) => {
                let n = peaks.len();
                self.peaks.points += n as u64;
                if let Some(arrays) = spec.raw_arrays() {
                    if continuity == SignalContinuity::Profile {
                        // Profile arrays beside a peak set: both facets are written.
                        self.data.add_arrays(arrays, types);
                    } else if types && n > 0 {
                        // The peak set was built from these arrays; their types are the source's.
                        self.peaks.add_types(arrays);
                    }
                }
            }
        }
    }

    /// The `(key, value)` pair a lane puts among its index blocks; [`complete`] merges the stored
    /// side into it.
    pub fn block(&self) -> (String, Value) {
        let t = self.declared_types;
        (BLOCK.to_string(), json!({"spectra_data": self.data.json(t), "spectra_peaks": self.peaks.json(t)}))
    }
}

/// One member of a ZIP that is still being written: name, where its bytes start, how many.
#[derive(Debug, PartialEq, Eq)]
struct Member {
    name: String,
    offset: u64,
    size: u64,
}

/// The members of a ZIP archive that has no central directory yet, from its local file headers.
///
/// The archive writer streams each Parquet facet into the `.tmp` file and writes the directory
/// only when it finishes, after the index — which is where this block has to go. A closed member's
/// local header holds its sizes (the writer seeks back and patches them, in the ZIP64 extra field
/// for the large-file entries it writes); the member still open has size 0 and ends the walk.
/// Stored (uncompressed) members only, which is all the writer produces.
fn unfinished_zip_members(file: &mut File) -> io::Result<Vec<Member>> {
    const LOCAL_HEADER: u32 = 0x0403_4b50;
    const ZIP64_EXTRA: u16 = 0x0001;
    let len = file.metadata()?.len();
    let mut members = Vec::new();
    let mut pos = 0u64;
    while pos + 30 <= len {
        let mut h = [0u8; 30];
        file.seek(SeekFrom::Start(pos))?;
        file.read_exact(&mut h)?;
        let u16_at = |i: usize| u16::from_le_bytes([h[i], h[i + 1]]);
        let u32_at = |i: usize| u32::from_le_bytes([h[i], h[i + 1], h[i + 2], h[i + 3]]);
        // Bit 3: sizes in a trailing data descriptor, which a seekable writer does not use.
        if u32_at(0) != LOCAL_HEADER || u16_at(6) & 0x0008 != 0 || u16_at(8) != 0 {
            break;
        }
        let (mut size, name_len, extra_len) = (u64::from(u32_at(18)), usize::from(u16_at(26)), usize::from(u16_at(28)));
        let mut tail = vec![0u8; name_len + extra_len];
        file.read_exact(&mut tail)?;
        let (name, extra) = tail.split_at(name_len);
        if size == u64::from(u32::MAX) {
            // ZIP64: uncompressed then compressed size, each present when its 32-bit field is
            // saturated. Stored members have the two equal; take the compressed one.
            let both = u32_at(22) == u32::MAX;
            let mut e = extra;
            size = 0;
            while e.len() >= 4 {
                let (id, n) = (u16::from_le_bytes([e[0], e[1]]), usize::from(u16::from_le_bytes([e[2], e[3]])));
                let body = &e[4..(4 + n).min(e.len())];
                let at = if both { 8 } else { 0 };
                if id == ZIP64_EXTRA && body.len() >= at + 8 {
                    size = u64::from_le_bytes(body[at..at + 8].try_into().expect("eight bytes"));
                    break;
                }
                e = &e[(4 + n).min(e.len())..];
            }
        }
        let offset = pos + 30 + (name_len + extra_len) as u64;
        members.push(Member { name: String::from_utf8_lossy(name).into_owned(), offset, size });
        if size == 0 {
            // The open member (or an empty one): what follows is its data, not a header.
            break;
        }
        pos = offset + size;
    }
    Ok(members)
}

/// One unit in the last place of a positive finite f64.
fn ulp(x: f64) -> f64 {
    let x = x.abs();
    f64::from_bits(x.to_bits() + 1) - x
}

/// The error bound of one numpress-linear chunk (see the module docs): absolute, in m/z.
fn numpress_bound(fixed_point: f64, largest_mz: f64) -> f64 {
    0.5 / fixed_point + 4.0 * ulp(largest_mz)
}

/// What the stored m/z chunks of one facet say about its encodings.
#[derive(Default, Debug, PartialEq)]
struct ChunkStats {
    numpress_chunks: u64,
    min_fixed_point: f64,
    numpress_abs: f64,
    /// `None` once a numpress chunk starts at m/z ≤ 0, where a relative error has no meaning.
    numpress_rel: Option<f64>,
    delta_chunks: u64,
    /// Delta chunks whose last m/z is more than twice their first.
    delta_at_risk: u64,
    delta_largest_at_risk: f64,
}

impl ChunkStats {
    fn new() -> Self {
        Self { min_fixed_point: f64::INFINITY, numpress_rel: Some(0.0), ..Self::default() }
    }

    /// One numpress-linear chunk: its bounds and its encoded bytes (the first eight are read).
    fn numpress(&mut self, start: f64, end: f64, encoded: &[u8]) {
        let Some(head) = encoded.first_chunk::<8>() else { return };
        // MS-Numpress writes the fixed point first, most significant byte first.
        let fixed_point = f64::from_be_bytes(*head);
        let bound = numpress_bound(fixed_point, start.abs().max(end.abs()));
        self.numpress_chunks += 1;
        self.min_fixed_point = self.min_fixed_point.min(fixed_point);
        self.numpress_abs = self.numpress_abs.max(bound);
        self.numpress_rel = match self.numpress_rel {
            Some(rel) if start > 0.0 => Some(rel.max(bound / start)),
            _ => None,
        };
    }

    fn delta(&mut self, start: f64, end: f64) {
        self.delta_chunks += 1;
        if !(start > 0.0 && end <= 2.0 * start) {
            self.delta_at_risk += 1;
            self.delta_largest_at_risk = self.delta_largest_at_risk.max(end);
        }
    }

    /// Fold another part of the same facet in.
    fn merge(&mut self, other: &Self) {
        self.numpress_chunks += other.numpress_chunks;
        self.min_fixed_point = self.min_fixed_point.min(other.min_fixed_point);
        self.numpress_abs = self.numpress_abs.max(other.numpress_abs);
        self.numpress_rel = match (self.numpress_rel, other.numpress_rel) {
            (Some(a), Some(b)) => Some(a.max(b)),
            _ => None,
        };
        self.delta_chunks += other.delta_chunks;
        self.delta_at_risk += other.delta_at_risk;
        self.delta_largest_at_risk = self.delta_largest_at_risk.max(other.delta_largest_at_risk);
    }
}

/// One signal facet as it is stored.
#[derive(Debug)]
struct StoredFacet {
    layout: String,
    points: Option<u64>,
    mz_type: Option<String>,
    intensity_type: Option<String>,
    chunks: ChunkStats,
}

/// The stored value type of a facet column: the list item's for a chunk column.
fn stored_type(dt: &DataType) -> String {
    match dt {
        DataType::Float32 => "float32".into(),
        DataType::Float64 => "float64".into(),
        DataType::Int32 => "int32".into(),
        DataType::Int64 => "int64".into(),
        DataType::List(f) | DataType::LargeList(f) => stored_type(f.data_type()),
        other => other.to_string().to_lowercase(),
    }
}

fn meta<'a>(field: &'a Field, key: &str) -> &'a str {
    field.metadata().get(key).map_or("", String::as_str)
}

const MZ_ARRAY: &str = "MS:1000514";
const INTENSITY_ARRAY: &str = "MS:1000515";
const NUMPRESS_LINEAR: &str = "MS:1002312";
const DELTA: &str = "MS:1003089";

/// The first eight bytes of row `i` of a list-of-bytes column: a numpress chunk's fixed point.
fn list_head(column: &ArrayRef, i: usize) -> Option<[u8; 8]> {
    if column.is_null(i) {
        return None;
    }
    let (values, start, end) = match column.data_type() {
        DataType::LargeList(_) => {
            let list = column.as_list::<i64>();
            (list.values(), list.value_offsets()[i] as usize, list.value_offsets()[i + 1] as usize)
        }
        DataType::List(_) => {
            let list = column.as_list::<i32>();
            (list.values(), list.value_offsets()[i] as usize, list.value_offsets()[i + 1] as usize)
        }
        _ => return None,
    };
    let bytes = values.as_primitive_opt::<UInt8Type>()?.values();
    bytes.get(start..end)?.first_chunk::<8>().copied()
}

/// The columns of a chunk facet's m/z main axis that the scan reads, by name.
struct ChunkColumns {
    top: String,
    start: String,
    end: String,
    encoding: String,
    numpress: Option<String>,
}

/// The chunk rows of one row group: bounds, encoding and, where there is one, the numpress column.
fn scan_row_group(tmp: &Path, member: &Member, metadata: &ArrowReaderMetadata, columns: &ChunkColumns, row_group: usize) -> Result<ChunkStats> {
    let reader = ArchiveFacetReader::new(File::open(tmp)?, member.offset, member.size, 0);
    let builder = ParquetRecordBatchReaderBuilder::new_with_metadata(reader, metadata.clone());
    let wanted = [Some(&columns.start), Some(&columns.end), Some(&columns.encoding), columns.numpress.as_ref()];
    let leaves: Vec<usize> = builder
        .parquet_schema()
        .columns()
        .iter()
        .enumerate()
        .filter(|(_, c)| matches!(c.path().parts(), [t, name, ..] if *t == columns.top && wanted.contains(&Some(name))))
        .map(|(i, _)| i)
        .collect();
    let mask = ProjectionMask::leaves(builder.parquet_schema(), leaves);
    let mut chunks = ChunkStats::new();
    for batch in builder.with_projection(mask).with_row_groups(vec![row_group]).build()? {
        let batch = batch?;
        let rows: &StructArray = batch.column(0).as_struct_opt().context("the facet's column is not a struct")?;
        let f64_column = |name: &str| -> Result<ArrayRef> {
            let c = rows.column_by_name(name).with_context(|| format!("no {name} column"))?;
            Ok(arrow::compute::cast(c, &DataType::Float64)?)
        };
        let (starts, ends) = (f64_column(&columns.start)?, f64_column(&columns.end)?);
        let (starts, ends) = (starts.as_primitive::<Float64Type>(), ends.as_primitive::<Float64Type>());
        let encodings = arrow::compute::cast(rows.column_by_name(&columns.encoding).context("no encoding column")?, &DataType::Utf8)?;
        let encodings = encodings.as_string::<i32>();
        let bytes = columns.numpress.as_ref().and_then(|n| rows.column_by_name(n));
        for i in 0..rows.len() {
            if starts.is_null(i) || ends.is_null(i) || encodings.is_null(i) {
                continue;
            }
            match encodings.value(i) {
                NUMPRESS_LINEAR => {
                    if let Some(head) = bytes.and_then(|c| list_head(c, i)) {
                        chunks.numpress(starts.value(i), ends.value(i), &head);
                    }
                }
                DELTA => chunks.delta(starts.value(i), ends.value(i)),
                _ => {}
            }
        }
    }
    Ok(chunks)
}

/// Read one signal facet back from the archive being written.
fn stored_facet(tmp: &Path, member: &Member) -> Result<StoredFacet> {
    let reader = ArchiveFacetReader::new(File::open(tmp)?, member.offset, member.size, 0);
    let metadata = ArrowReaderMetadata::load(&reader, Default::default())?;
    let points = metadata
        .metadata()
        .file_metadata()
        .key_value_metadata()
        .and_then(|kv| kv.iter().find(|kv| kv.key == mzpeak_prototyping::constants::SPECTRUM_DATA_POINT_COUNT))
        .and_then(|kv| kv.value.as_deref()?.parse().ok());
    let top = metadata.schema().fields().first().context("a facet with no column")?.clone();
    let DataType::Struct(children) = top.data_type() else {
        bail!("{}: the facet's column is not a struct", member.name);
    };
    let layout = match top.name().as_str() {
        "chunk" => "chunked".to_string(),
        other => other.to_string(),
    };
    let value_column = |accession: &str| {
        children
            .iter()
            .find(|f| meta(f, "array_accession") == accession && matches!(meta(f, "buffer_format"), "point" | "chunk_values" | "chunk_secondary"))
            .map(|f| stored_type(f.data_type()))
    };
    let (mz_type, intensity_type) = (value_column(MZ_ARRAY), value_column(INTENSITY_ARRAY));

    // The chunk rows of an m/z main axis, one row group per worker: the numpress column is most of
    // a default archive, and reading it back on one core cost 6 % of a 68-million-point imzML
    // conversion. A point facet, an empty one and an integer main axis (timsTOF) have nothing to scan.
    let mut chunks = ChunkStats::new();
    let named = |format: &str| children.iter().find(|f| meta(f, "array_accession") == MZ_ARRAY && meta(f, "buffer_format") == format).map(|f| f.name().clone());
    let numpress = children
        .iter()
        .find(|f| meta(f, "array_accession") == MZ_ARRAY && meta(f, "transform") == NUMPRESS_LINEAR)
        .map(|f| f.name().clone());
    if let (true, Some(start), Some(end), Some(encoding)) = (points.unwrap_or(1) > 0, named("chunk_start"), named("chunk_end"), named("chunk_encoding")) {
        let columns = ChunkColumns { top: top.name().clone(), start, end, encoding, numpress };
        let scanned: Vec<Result<ChunkStats>> = (0..metadata.metadata().num_row_groups())
            .into_par_iter()
            .map(|g| scan_row_group(tmp, member, &metadata, &columns, g))
            .collect();
        for group in scanned {
            chunks.merge(&group?);
        }
    }
    Ok(StoredFacet { layout, points, mz_type, intensity_type, chunks })
}

/// The two signal facets of the archive being written at `tmp`, read back (see the module docs).
fn stored(tmp: &Path) -> Result<Vec<(&'static str, StoredFacet)>> {
    let members = unfinished_zip_members(&mut File::open(tmp)?)?;
    let mut out = Vec::new();
    for (member, key) in FACETS {
        if let Some(m) = members.iter().find(|m| m.name == member && m.size > 0) {
            out.push((key, stored_facet(tmp, m).with_context(|| format!("reading {member} back"))?));
        }
    }
    Ok(out)
}

/// `x` rounded UP to six significant digits: a bound stays a bound, and a figure this short comes
/// back from JSON as the same f64 (the `.mzpeak` filter parses and re-writes the index, and a
/// 17-digit float can return one unit in the last place off).
fn round_up(x: f64) -> f64 {
    if !(x.is_finite() && x > 0.0) {
        return x;
    }
    let scale = 10f64.powi(5 - x.log10().floor() as i32);
    (x * scale * (1.0 + 1e-12)).ceil() / scale
}

/// The number between `prefix` and `suffix` of a parametrised entry (`grid-fit:1e-6Da` → 1e-6).
fn entry_bound(entry: &str, prefix: &str, suffix: &str) -> Option<f64> {
    entry.strip_prefix(prefix)?.strip_suffix(suffix)?.parse().ok()
}

/// The finished block: the lane's source side (`lane`, from [`SourceTally::block`], when the lane
/// counts) merged with what is stored at `tmp`, and the m/z error of every encoding present.
/// `transformations` is the lane's declared list, for the grid fits' tolerances.
pub fn complete(lane: Option<&Value>, tmp: &Path, transformations: &[&str]) -> Result<Value> {
    let mut block = serde_json::Map::new();
    let mut mz_error: Vec<Value> = Vec::new();
    for (key, facet) in stored(tmp)? {
        let source = lane.and_then(|l| l.get(key));
        let source_points = source.and_then(|s| s["source_points"].as_u64());
        if facet.points.unwrap_or(0) == 0 && source_points.unwrap_or(0) == 0 {
            continue;
        }
        let mut entry = json!({"layout": facet.layout});
        if let Some(n) = source_points {
            entry["source_points"] = n.into();
        }
        if let Some(n) = facet.points {
            entry["stored_points"] = n.into();
        }
        let source_types = source.and_then(|s| s.get("source_types"));
        if let Some(t) = source_types {
            entry["source_types"] = t.clone();
        }
        entry["stored_types"] = json!({"mz": facet.mz_type, "intensity": facet.intensity_type});
        block.insert(key.to_string(), entry);

        let c = &facet.chunks;
        if c.numpress_chunks > 0 {
            mz_error.push(json!({
                "encoding": "numpress-linear",
                "facet": key,
                "chunks": c.numpress_chunks,
                "min_fixed_point": c.min_fixed_point,
                "max_abs_error": round_up(c.numpress_abs),
                "max_rel_error_ppm": c.numpress_rel.map(|r| round_up(r * 1e6)),
                "basis": "bound",
            }));
        }
        // 32-bit m/z values survive delta exactly whatever their spacing.
        let f32_source = source_types.and_then(|t| t["mz"].as_array()).is_some_and(|t| !t.is_empty() && t.iter().all(|v| v == "float32"));
        if c.delta_chunks > 0 && c.delta_at_risk > 0 && !f32_source {
            mz_error.push(json!({
                "encoding": "delta",
                "facet": key,
                "chunks": c.delta_chunks,
                "chunks_not_exact_by_construction": c.delta_at_risk,
                "largest_mz_at_risk": (c.delta_largest_at_risk * 1e6).ceil() / 1e6,
                "ulp_there": round_up(ulp(c.delta_largest_at_risk)),
                "max_abs_error": Value::Null,
                "basis": "not measured",
            }));
        }
    }
    for t in transformations {
        if let Some(tol) = entry_bound(t, "grid-fit:", "Da") {
            mz_error.push(json!({"encoding": t, "max_abs_error": tol, "basis": "tolerance"}));
        } else if let Some(ppm) = entry_bound(t, "tof-grid:", "ppm") {
            mz_error.push(json!({"encoding": t, "max_rel_error_ppm": ppm, "basis": "tolerance"}));
        } else if matches!(*t, "bruker:mz-calibration-chord" | "bruker:mz-calibrant-omitted" | "shimadzu:coarse-mz") {
            mz_error.push(json!({"encoding": t, "max_abs_error": Value::Null, "basis": "not measured"}));
        }
    }
    block.insert("mz_error".to_string(), mz_error.into());
    Ok(block.into())
}

/// Does storing a `source`-typed value in a `stored`-typed column keep every value? Equal types,
/// and the widenings that are exact.
fn keeps_every_value(source: &str, stored: &str) -> bool {
    source == stored || matches!((source, stored), ("float32", "float64") | ("int32", "int64") | ("int32", "float64"))
}

/// `--lossless`: the archive at hand is bit-exact, or the conversion fails. `block` is the finished
/// [`complete`] block of a lane that counted its source; `transformations` its declared list.
///
/// Bit-exact means: every point the source holds is stored, in the source's order, each m/z and
/// intensity with the value the source has (a 32-bit value in a 64-bit column still counts; the
/// block shows the widening). Checked from what is in the archive: no signal transformation
/// declared ([`is_signal_transformation`]), every facet in the point layout with as many points
/// as the reader handed over, no m/z encoding with an error, and no column narrower than its source.
pub fn check_lossless(block: &Value, transformations: &[&str]) -> Result<()> {
    let mut reasons: Vec<String> = Vec::new();
    let signal: Vec<&str> = transformations.iter().copied().filter(|t| is_signal_transformation(t)).collect();
    if !signal.is_empty() {
        reasons.push(format!("the conversion declares {}", signal.join(", ")));
    }
    for (_, key) in FACETS {
        let Some(facet) = block.get(key) else { continue };
        if facet["layout"] != "point" {
            reasons.push(format!("{key} is in the {} layout", facet["layout"].as_str().unwrap_or("?")));
        }
        match (facet["source_points"].as_u64(), facet["stored_points"].as_u64()) {
            (Some(a), Some(b)) if a == b => {}
            (Some(a), Some(b)) => reasons.push(format!("{key} stores {b} of the source's {a} points")),
            _ => reasons.push(format!("{key}: the source or stored point count is unknown")),
        }
        for array in ["mz", "intensity"] {
            let stored = facet["stored_types"][array].as_str();
            let source: Vec<&str> = facet["source_types"][array].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
            if source.is_empty() {
                reasons.push(format!("{key}: the source's {array} type is unknown"));
            }
            for s in source {
                if !stored.is_some_and(|t| keeps_every_value(s, t)) {
                    reasons.push(format!("{key} stores {array} as {} where the source declares {s}", stored.unwrap_or("nothing")));
                }
            }
        }
    }
    for e in block["mz_error"].as_array().into_iter().flatten() {
        reasons.push(format!("m/z is stored with {}", e["encoding"].as_str().unwrap_or("a lossy encoding")));
    }
    if reasons.is_empty() {
        return Ok(());
    }
    bail!("--lossless: the archive would not be bit-exact ({}); nothing was written", reasons.join("; "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::array::Float64Array;
    use mzdata::spectrum::DataArray;
    use mzpeak_prototyping::archive::ZipArchiveWriter;
    use mzpeak_prototyping::chunk_series::ChunkingStrategy;
    use std::io::Write;

    fn scratch(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("mzpc-fidelity-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// A tiny deterministic generator (the suite has no `rand`).
    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self) -> f64 {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (self.0 >> 11) as f64 / (1u64 << 53) as f64
        }
    }

    /// The local-header walk finds every closed member of an archive that has no central directory
    /// yet, at the offsets and sizes the finished archive's directory then states.
    #[test]
    fn closed_members_of_an_unfinished_zip_are_found() {
        let dir = scratch("zip");
        let path = dir.join("a.zip");
        let payloads: [(&str, Vec<u8>); 3] = [("spectra_data.parquet", vec![7u8; 70_000]), ("spectra_peaks.parquet", b"PAR1 peaks".to_vec()), ("open.bin", vec![1, 2, 3])];
        let mut zip = ZipArchiveWriter::new(File::create(&path).unwrap());
        for (name, bytes) in &payloads {
            zip.start_other(name).unwrap();
            zip.write_all(bytes).unwrap();
        }
        zip.flush().unwrap();
        // The third member is still open: its header holds no size yet.
        let found = unfinished_zip_members(&mut File::open(&path).unwrap()).unwrap();
        assert_eq!(found.len(), 3, "{found:?}");
        assert_eq!((found[0].name.as_str(), found[0].size), ("spectra_data.parquet", 70_000));
        assert_eq!((found[1].name.as_str(), found[1].size), ("spectra_peaks.parquet", 10));
        assert_eq!((found[2].name.as_str(), found[2].size), ("open.bin", 0));
        zip.finish().unwrap();
        let mut done = zip::ZipArchive::new(File::open(&path).unwrap()).unwrap();
        for m in &found[..2] {
            let entry = done.by_name(&m.name).unwrap();
            assert_eq!((entry.data_start(), entry.size()), (m.offset, m.size), "{}", m.name);
        }
        let mut bytes = vec![0u8; 10];
        let mut f = File::open(&path).unwrap();
        f.seek(SeekFrom::Start(found[1].offset)).unwrap();
        f.read_exact(&mut bytes).unwrap();
        assert_eq!(bytes, b"PAR1 peaks");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The recorded numpress bound holds for every decoded value, and the bare `0.5 / fixed point`
    /// does not: chunks of m/z are encoded and decoded with the writer's own codec, 200,000 values
    /// over the mass ranges real data covers. Every other chunk holds 32-bit values, as most
    /// imzML does: their products with the fixed point land on exact half steps, which is where
    /// the f64 division then carries a value past the bare bound.
    #[test]
    fn the_numpress_bound_holds_where_the_bare_half_step_does_not() {
        let strategy = ChunkingStrategy::NumpressLinear { chunk_size: 50.0 };
        let mut rng = Lcg(0x5eed);
        let (mut beyond_bare, mut tightest) = (0usize, 0.0f64);
        for chunk in 0..400 {
            // A chunk: up to 50 Th wide, starting anywhere between 0.02 and 3000 Th.
            let lo = 0.02 + 3000.0 * rng.next().powi(3);
            let single = chunk % 2 == 1;
            let mut mz: Vec<f64> = (0..500).map(|_| lo + 50.0 * rng.next()).map(|x| if single { f64::from(x as f32) } else { x }).collect();
            mz.sort_by(f64::total_cmp);
            let (start, end, encoded) = strategy.encode_arrow(&Float64Array::from(mz.clone()));
            let bytes = encoded.as_primitive::<UInt8Type>().values().to_vec();
            let mut stats = ChunkStats::new();
            stats.numpress(start, end, &bytes);
            let fixed_point = stats.min_fixed_point;
            let mut decoded = DataArray::from_name_and_type(&ArrayType::MZArray, BinaryDataArrayType::Float64);
            strategy.decode_arrow(&encoded, start, end, &mut decoded, None);
            let decoded = decoded.to_f64().unwrap();
            assert_eq!(decoded.len(), mz.len(), "chunk {chunk}");
            for (a, b) in mz.iter().zip(decoded.iter()) {
                let d = (a - b).abs();
                assert!(d <= stats.numpress_abs, "chunk {chunk}: |{a} - {b}| = {d:e} above the bound {:e}", stats.numpress_abs);
                assert!(d / a <= stats.numpress_rel.unwrap(), "chunk {chunk}: relative {:e} above {:e}", d / a, stats.numpress_rel.unwrap());
                beyond_bare += usize::from(d > 0.5 / fixed_point);
                tightest = tightest.max(d / stats.numpress_abs);
            }
        }
        assert!(beyond_bare > 0, "no value beyond 0.5/fixed point: the ulp term would be unnecessary");
        assert!(beyond_bare < 100, "{beyond_bare} values beyond 0.5/fixed point: more than rounding");
        assert!(tightest > 0.99, "the bound is loose: the worst value reached {tightest} of it");
    }

    /// Delta is exact by construction in a chunk whose last m/z is at most twice its first, and
    /// not otherwise: the rule the block counts chunks by, on the pairs the codec rounds.
    #[test]
    fn delta_is_exact_within_a_factor_of_two_and_not_beyond() {
        let mut rng = Lcg(42);
        let roundtrips = |a: f64, b: f64| b + (a - b) == a;
        let (mut within, mut beyond) = (0usize, 0usize);
        for _ in 0..200_000 {
            let b = 1.0 + 49.0 * rng.next();
            let near = b * (1.0 + rng.next());
            assert!(roundtrips(near, b), "{b} -> {near} is within a factor of two and must be exact");
            within += 1;
            let far = b * (2.0 + 6.0 * rng.next());
            beyond += usize::from(!roundtrips(far, b));
        }
        assert!(within > 0 && beyond > 1000, "beyond a factor of two some pairs must round: {beyond}");
        let mut stats = ChunkStats::new();
        stats.delta(60.0, 110.0);
        stats.delta(1.0078, 38.96);
        assert_eq!((stats.delta_chunks, stats.delta_at_risk, stats.delta_largest_at_risk), (2, 1, 38.96));
    }

    /// A recorded bound is rounded up, to six digits, and survives a JSON round trip unchanged.
    #[test]
    fn recorded_figures_round_up_and_survive_json() {
        for x in [1.7960996212517e-7, 4.656754985961351e-10, 0.0002330768570048625, 7.105427357601002e-15, 123456.7, 1.0, 9.999999e-3] {
            let r = round_up(x);
            assert!(r >= x && r <= x * 1.00001, "{x} -> {r}");
            let back: f64 = serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
            assert_eq!(back.to_bits(), r.to_bits(), "{x} -> {r} -> {back}");
            assert!(serde_json::to_string(&r).unwrap().len() <= 12, "{r}");
        }
        assert_eq!(round_up(0.0), 0.0);
    }

    /// The signal set: what `--lossless` fails on, and what it lets through.
    #[test]
    fn signal_transformations_are_told_from_the_rest() {
        for t in ["zero-run-mask", "numpress-linear", "sort-by-mz", "grid-fit:1e-6Da", "tof-grid:5ppm", "agilent:drop-zero-samples", "waters:sonar-summed"] {
            assert!(is_signal_transformation(t), "{t}");
        }
        for t in [
            "mzml:dangling-reference-dropped",
            "chromatogram-time-to-minutes",
            "sort-by-time",
            "imzml:pixel-size-unit-assumed-um",
            "imaging:pixel-count-from-positions",
            "thermo:target-only-isolation-window",
            "bruker:trace-unit-rescale",
        ] {
            assert!(!is_signal_transformation(t), "{t}");
        }
    }

    /// Every vendor-namespaced `transformations` entry the sources name is classified: in the signal
    /// set, or in the list here of entries that leave spectrum signal alone. A new entry lands in
    /// neither, and this test asks for the decision `--lossless` depends on. (The `imzml:`,
    /// `imaging:` and `mzml:` namespaces describe metadata and are not signal by their kind.)
    #[test]
    fn every_vendor_entry_in_the_sources_is_classified() {
        const NOT_SIGNAL: [&str; 7] = [
            "thermo:target-only-isolation-window",
            "bruker:trace-unit-rescale",
            "bruker:trace-sort-dedup",
            "bruker:raster-index-shifted-to-base-1",
            "bruker:pixel-size-from-beam-scan-size",
            "waters:off-grid-position-dropped",
            "waters:laser-position-fitted-to-grid",
        ];
        const NAMESPACES: [&str; 6] = ["agilent:", "bruker:", "sciex:", "shimadzu:", "thermo:", "waters:"];
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut seen = BTreeSet::new();
        for entry in std::fs::read_dir(src).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_none_or(|e| e != "rs") {
                continue;
            }
            // Every other `"`-separated piece of a line is a string literal's text (near enough:
            // an escaped quote only splits a literal into pieces that match nothing).
            for literal in std::fs::read_to_string(&path).unwrap().lines().flat_map(|l| l.split('"').skip(1).step_by(2).map(str::to_string).collect::<Vec<_>>()) {
                let Some(ns) = NAMESPACES.iter().find(|ns| literal.starts_with(**ns)) else { continue };
                let name = &literal[ns.len()..];
                if !name.is_empty() && name.contains('-') && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-') {
                    seen.insert(literal);
                }
            }
        }
        assert!(seen.len() >= 20, "the scan found only {seen:?}");
        let unclassified: Vec<&String> = seen.iter().filter(|e| !is_signal_transformation(e) && !NOT_SIGNAL.contains(&e.as_str())).collect();
        assert!(
            unclassified.is_empty(),
            "transformations entries that are neither in fidelity::SIGNAL_TRANSFORMATIONS (they change \
             spectrum signal, and --lossless must fail on them) nor in this test's NOT_SIGNAL list: {unclassified:?}"
        );
        for e in NOT_SIGNAL {
            assert!(!is_signal_transformation(e) && seen.contains(e), "{e} is listed here but gone from the sources, or is signal");
        }
    }

    fn exact_block() -> Value {
        json!({
            "spectra_data": {
                "layout": "point", "source_points": 10, "stored_points": 10,
                "source_types": {"mz": ["float64"], "intensity": ["float32"]},
                "stored_types": {"mz": "float64", "intensity": "float32"},
            },
            "mz_error": [],
        })
    }

    /// `--lossless` passes an exact block with metadata-only transformations, and names each
    /// reason it fails for: a signal entry, a dropped point, a chunked facet, an m/z error, a
    /// narrowed column.
    #[test]
    fn the_lossless_check_fails_closed() {
        check_lossless(&exact_block(), &["mzml:dangling-reference-dropped", "chromatogram-time-to-minutes"]).unwrap();
        let fails = |block: &Value, t: &[&str], needle: &str| {
            let e = check_lossless(block, t).unwrap_err().to_string();
            assert!(e.contains(needle), "{e}");
        };
        fails(&exact_block(), &["sort-by-mz"], "declares sort-by-mz");
        let mut b = exact_block();
        b["spectra_data"]["stored_points"] = 9.into();
        fails(&b, &[], "stores 9 of the source's 10 points");
        let mut b = exact_block();
        b["spectra_data"]["layout"] = "chunked".into();
        b["mz_error"] = json!([{"encoding": "delta"}]);
        fails(&b, &[], "chunked layout");
        fails(&b, &[], "stored with delta");
        let mut b = exact_block();
        b["spectra_data"]["source_types"]["intensity"] = json!(["float32", "float64"]);
        fails(&b, &[], "stores intensity as float32 where the source declares float64");
        let mut b = exact_block();
        b["spectra_data"]["source_types"] = json!({"mz": ["float32"], "intensity": ["int32"]});
        b["spectra_data"]["stored_types"]["intensity"] = "int64".into();
        check_lossless(&b, &[]).unwrap();
        let mut b = exact_block();
        b["spectra_data"].as_object_mut().unwrap().remove("source_types");
        fails(&b, &[], "type is unknown");
    }
}

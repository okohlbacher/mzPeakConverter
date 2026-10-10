//! The signal facets' row groups hold whole entries (`RowGroupCutter`): the guarantee the
//! `row_groups` index block declares.
use std::sync::Arc;

use arrow::array::{Float64Array, StructArray, UInt64Array};
use arrow::datatypes::{Field, Fields};

use arrow::array::{AsArray, RecordBatch};
use arrow::datatypes::{DataType, UInt64Type};
use mzpeak_prototyping::writer::{RowGroupCut, RowGroupCutter, batch_bytes};

/// A point batch: one row per key, `point.spectrum_index` and `point.mz`.
fn batch(keys: &[u64]) -> RecordBatch {
    let fields = Fields::from(vec![
        Field::new("spectrum_index", DataType::UInt64, true),
        Field::new("mz", DataType::Float64, true),
    ]);
    let point = StructArray::new(
        fields.clone(),
        vec![
            Arc::new(UInt64Array::from(keys.to_vec())),
            Arc::new(Float64Array::from(vec![1.0; keys.len()])),
        ],
        None,
    );
    RecordBatch::try_from_iter([("point", Arc::new(point) as _)]).unwrap()
}

/// The groups the cuts make, as the keys of their rows.
fn groups(cutter: &mut RowGroupCutter, batches: &[&[u64]]) -> Vec<Vec<u64>> {
    let mut out = vec![Vec::new()];
    for b in batches {
        for cut in cutter.cut(batch(b)) {
            match cut {
                RowGroupCut::Rows(r) => {
                    let s = r.column(0).as_struct();
                    let k = s.column(0).as_primitive::<UInt64Type>();
                    out.last_mut().unwrap().extend(k.values().iter().copied());
                }
                RowGroupCut::Close => out.push(Vec::new()),
            }
        }
    }
    out.retain(|g| !g.is_empty());
    out
}

/// No key appears in two groups.
fn aligned(groups: &[Vec<u64>]) -> bool {
    groups.windows(2).all(|w| w[0].last() != w[1].first())
}

#[test]
fn row_cap_never_splits_an_entry() {
    let mut c = RowGroupCutter::new(Some(4), usize::MAX);
    let g = groups(&mut c, &[&[0, 0, 0, 1, 1, 1, 2, 2, 3]]);
    assert!(aligned(&g), "{g:?}");
    assert_eq!(g, vec![vec![0, 0, 0], vec![1, 1, 1], vec![2, 2, 3]]);
}

#[test]
fn an_entry_larger_than_a_group_gets_its_own() {
    let mut c = RowGroupCutter::new(Some(2), usize::MAX);
    let g = groups(&mut c, &[&[0, 1, 1, 1, 1, 1, 2]]);
    assert_eq!(g, vec![vec![0], vec![1, 1, 1, 1, 1], vec![2]]);
}

#[test]
fn an_entry_continued_in_the_next_batch_stays_in_its_group() {
    // The group fills at the end of the first batch while entry 1 goes on in the next.
    let mut c = RowGroupCutter::new(Some(3), usize::MAX);
    let g = groups(&mut c, &[&[0, 1, 1], &[1, 1, 2, 2, 3]]);
    assert!(aligned(&g), "{g:?}");
    assert_eq!(g, vec![vec![0, 1, 1, 1, 1], vec![2, 2, 3]]);
}

#[test]
fn byte_cap_cuts_between_entries() {
    // A cap of three rows' bytes.
    let row = batch_bytes(&batch(&[0]));
    let mut c = RowGroupCutter::new(None, 3 * row);
    let g = groups(&mut c, &[&[0, 0, 1, 1, 2, 2, 2, 3]]);
    assert!(aligned(&g), "{g:?}");
    assert!(g.iter().all(|grp| grp.len() <= 3 || grp.iter().all(|k| *k == grp[0])), "{g:?}");
}

#[test]
fn request_close_waits_for_a_boundary() {
    let mut c = RowGroupCutter::new(None, usize::MAX);
    let mut g = groups(&mut c, &[&[0, 1, 1]]);
    c.request_close();
    g.extend(groups(&mut c, &[&[1, 2]]));
    // The close lands before entry 2, not inside entry 1.
    assert_eq!(c.rows(), 1);
    assert_eq!(g.concat(), vec![0, 1, 1, 1, 2]);
}

// ---- End to end: the written facets, and the `row_groups` index block that declares them. ----

const SMALL_RAW: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/small.RAW");
const SWATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/swath.api-sample-centroid.mzML.gz");

fn run(args: &[&std::ffi::OsStr]) {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_mzpeak-convert"))
        .args(args)
        .args(["--force", "-q"])
        .env_remove("MZPC_ROW_GROUP_ROWS")
        .env_remove("MZPC_ROW_GROUP_MB")
        .output()
        .expect("failed to run mzpeak-convert");
    assert!(out.status.success(), "{args:?}: {}\n{}", out.status, String::from_utf8_lossy(&out.stderr));
}

fn member(archive: &std::path::Path, name: &str) -> Option<Vec<u8>> {
    use std::io::Read;
    let mut zip = zip::ZipArchive::new(std::fs::File::open(archive).unwrap()).unwrap();
    let mut entry = zip.by_name(name).ok()?;
    let mut bytes = Vec::new();
    entry.read_to_end(&mut bytes).unwrap();
    Some(bytes)
}

/// Each row group's (min, max) of the facet's entry column, from the Parquet statistics.
fn entry_bounds(facet: Vec<u8>) -> Vec<(i64, i64)> {
    use parquet::file::reader::{FileReader, SerializedFileReader};
    use parquet::file::statistics::Statistics;
    let reader = SerializedFileReader::new(bytes::Bytes::from(facet)).unwrap();
    let md = reader.metadata();
    let col = md
        .file_metadata()
        .schema_descr()
        .columns()
        .iter()
        .position(|c| c.path().parts().last().is_some_and(|p| p.ends_with("_index")) && c.path().parts().len() == 2)
        .expect("no entry column");
    md.row_groups()
        .iter()
        .map(|rg| match rg.column(col).statistics().expect("a row group without statistics") {
            Statistics::Int64(v) => (*v.min_opt().unwrap(), *v.max_opt().unwrap()),
            Statistics::Int32(v) => (*v.min_opt().unwrap() as i64, *v.max_opt().unwrap() as i64),
            other => panic!("entry column statistics of type {other:?}"),
        })
        .collect()
}

/// Every signal facet of `archive` is cut into more than `min_groups` row groups (so the caps did
/// cut it), no entry spans two of them, and the index declares exactly that.
fn assert_aligned(archive: &std::path::Path, min_groups: usize) {
    let index: serde_json::Value = serde_json::from_slice(&member(archive, "mzpeak_index.json").unwrap()).unwrap();
    let block = &index["metadata"]["row_groups"];
    for facet in ["spectra_data.parquet", "spectra_peaks.parquet"] {
        let Some(bytes) = member(archive, facet) else { continue };
        let bounds = entry_bounds(bytes);
        if bounds.is_empty() {
            continue;
        }
        assert!(bounds.len() > min_groups, "{}: {facet} has {} row groups", archive.display(), bounds.len());
        assert!(
            bounds.windows(2).all(|w| w[0].1 < w[1].0),
            "{}: {facet} splits a spectrum across row groups: {bounds:?}",
            archive.display()
        );
        assert_eq!(block[facet], "spectrum-aligned", "{}: {facet} not declared: {block}", archive.display());
    }
}

fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("mzpc-rga-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A 1000-row cap cuts small.RAW's 20,000-point profile spectra in the point layout: each becomes
/// a group of its own instead of being split. In the chunked layout and on the SWATH centroids the
/// caps fall between spectra.
#[test]
fn conversions_cut_row_groups_between_spectra() {
    let dir = scratch("convert");
    for (input, layout, cap) in [
        (SMALL_RAW, "point", ["--row-group-rows", "1000"]),
        (SMALL_RAW, "chunked", ["--row-group-rows", "50"]),
        (SWATH, "chunked", ["--row-group-mb", "0.05"]),
        (SWATH, "point", ["--row-group-rows", "3000"]),
    ] {
        let out = dir.join(format!("{}-{layout}-{}.mzpeak", if input == SWATH { "swath" } else { "small" }, cap[1]));
        run(&[input.as_ref(), "-o".as_ref(), out.as_os_str(), "--layout".as_ref(), layout.as_ref(), cap[0].as_ref(), cap[1].as_ref()]);
        assert_aligned(&out, 2);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// The `.mzpeak` filter re-groups the facets it rewrites under the byte cap, between spectra too.
#[test]
fn the_filter_lane_cuts_row_groups_between_spectra() {
    let dir = scratch("filter");
    let source = dir.join("small.mzpeak");
    run(&[SMALL_RAW.as_ref(), "-o".as_ref(), source.as_os_str(), "--layout".as_ref(), "point".as_ref()]);
    let filtered = dir.join("small-ms1.mzpeak");
    run(&[source.as_os_str(), "-o".as_ref(), filtered.as_os_str(), "--ms-level".as_ref(), "1".as_ref(), "--row-group-mb".as_ref(), "0.05".as_ref()]);
    assert_aligned(&filtered, 2);
    let _ = std::fs::remove_dir_all(&dir);
}

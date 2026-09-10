//! One MS2 spectrum with TWO precursors must round-trip with one selected ion on each.
//!
//! The archive's precursor/selected-ion join key `(source_index, secondary_index)` is NOT unique
//! for such a spectrum — both rows carry the same pair — so the reader pairs them positionally, in
//! the order the rows were read. `sort_unstable_by` on that tied key was free to reorder the
//! precursors against the selected ions, and did: round-tripping a DDA-PASEF archive emitted a
//! frame's precursors back to front, with every ion attached to the last one. The sort is stable
//! now, and the reader no longer walks the rows in reverse when it attaches them (which kept the
//! order back to front after the sort was fixed); this pins the observable consequence.

use std::path::PathBuf;
use std::process::Command;

fn run(args: &[&str]) {
    let st = Command::new(env!("CARGO_BIN_EXE_mzpeak-convert"))
        .args(args)
        .status()
        .expect("failed to run mzpeak-convert");
    assert!(st.success(), "mzpeak-convert {args:?} failed: {st}");
}

/// The `value` of the first `<cvParam … accession="{acc}" … value="…"/>` in `xml`.
fn cv_value(xml: &str, acc: &str) -> f64 {
    let at = xml.find(&format!("accession=\"{acc}\"")).unwrap_or_else(|| panic!("no {acc} in:\n{xml}"));
    let tag = &xml[at..at + xml[at..].find('>').unwrap()];
    let v = tag.split("value=\"").nth(1).and_then(|s| s.split('"').next());
    let v = v.unwrap_or_else(|| panic!("{acc} carries no value: {tag}"));
    v.parse().unwrap_or_else(|e| panic!("{acc} value {v:?}: {e}"))
}

#[test]
fn two_precursors_on_one_spectrum_keep_one_selected_ion_each() {
    let tmp = std::env::temp_dir().join(format!("mzpc-2prec-{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();
    let archive: PathBuf = tmp.join("two.mzpeak");
    let back: PathBuf = tmp.join("two.roundtrip.mzML");
    let src = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/two_precursors.mzML");

    run(&[src, "-o", archive.to_str().unwrap(), "--force"]);
    run(&[archive.to_str().unwrap(), "-o", back.to_str().unwrap(), "--force"]);

    let xml = std::fs::read_to_string(&back).unwrap();
    let precursors = xml.matches("<precursor ").count(); // trailing space: not <precursorList
    let ions = xml.matches("<selectedIon>").count();
    assert_eq!(precursors, 2, "both precursors must survive the round trip:\n{xml:.0}");
    assert_eq!(ions, 2, "each precursor keeps its own selected ion (not 2 on one and 0 on the other)");

    // Each precursor block must contain exactly one selected ion, and the two ions must be the two
    // distinct m/z values of the fixture — the pairing, not just the count, is what broke.
    let blocks: Vec<&str> = xml.split("<precursor ").skip(1).collect();
    assert_eq!(blocks.len(), 2);
    for b in &blocks {
        let upto = b.split("</precursor>").next().unwrap();
        assert_eq!(upto.matches("<selectedIon>").count(), 1, "one ion per precursor, got:\n{upto}");
    }
    for mz in ["445.34", "645.34"] {
        assert!(xml.contains(mz), "selected ion {mz} missing from the round trip");
    }
    // One ion per block with both values somewhere in the document is also what precursors reversed
    // against their ions look like. Each block's ion must be ITS precursor's: the window targeting
    // 445.3 holds 445.34, the one targeting 645.3 holds 645.34, in the fixture's order.
    let pairs: Vec<(f64, f64)> = blocks
        .iter()
        .map(|b| {
            let upto = b.split("</precursor>").next().unwrap();
            (cv_value(upto, "MS:1000827"), cv_value(upto, "MS:1000744"))
        })
        .collect();
    let want = [(445.3, 445.34), (645.3, 645.34)];
    assert!(
        pairs.len() == want.len()
            && pairs.iter().zip(&want).all(|(p, w)| (p.0 - w.0).abs() < 1e-6 && (p.1 - w.1).abs() < 1e-6),
        "(isolation window target, selected ion) per precursor: got {pairs:?}, expected {want:?}"
    );
    let _ = std::fs::remove_dir_all(&tmp);
}

//! Imaging metadata the imzML lane checks or restores instead of copying blindly
//! (HUPO-PSI/mzPeak-specification#23; handoff 2026-09-29):
//!
//! * **Pixel size.** A survey of 784 public imzML headers found pixel sizes without a unit, the
//!   centimetre accession labelled "micrometer", and `IMS:1000046` given as an AREA (its meaning
//!   until 2017). [`pixel_size_fixes`] applies the issue author's rule: x and y with a unit are kept;
//!   x and y without one are taken as micrometre; a single value is tested against pixel count and
//!   extent — an area when `√value × count = extent` (written as its square root), a length when
//!   `value × count = extent` — and anything else is dropped. Nothing changes silently: every
//!   action becomes a `transformations` entry and a row of the `imaging_pixel_size` index block.
//! * **"one way"** (`IMS:1000411`, obsolete) is written as its stated replacement, flyback
//!   (`IMS:1000413`), and declared.
//! * **File provenance.** mzdata consumes storage mode, UUID and `.ibd` checksum into its
//!   `ImzMLFileMetadata` and leaves them out of `file_description`; [`provenance_params`] puts them
//!   back.
//!
//! mzdata keeps a unit by its accession only, so the unit NAME the file states — needed to see an
//! accession/name disagreement — is read from the header here ([`read_scan_settings`]).

use std::collections::HashMap;
use std::io::BufRead;
use std::path::Path;

use anyhow::{Context, Result};
use mzdata::params::{Param, Unit};
use mzdata::meta::ScanSettings;
use quick_xml::events::Event;

pub const UNIT_ASSUMED: &str = "imzml:pixel-size-unit-assumed-um";
pub const AREA_TO_LENGTH: &str = "imzml:pixel-size-area-to-length";
pub const DROPPED: &str = "imzml:pixel-size-dropped";
pub const ONE_WAY_AS_FLYBACK: &str = "imzml:one-way-as-flyback";
/// The unit mzdata wrote differs from the unit ACCESSION the file states (mzdata takes whichever of
/// `unitAccession` / `unitName` comes last, so a disagreeing pair resolves by attribute order).
pub const UNIT_FROM_NAME: &str = "imzml:unit-accession-replaced-by-name";

/// One `cvParam` of a `<scanSettings>`, as the file states it.
#[derive(Debug, Clone, PartialEq)]
pub struct RawParam {
    pub accession: String,
    pub value: String,
    pub unit_accession: Option<String>,
    pub unit_name: Option<String>,
}

/// A `<scanSettings>` element: its id and cvParams, referenced param groups expanded.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct RawSettings {
    pub id: String,
    pub params: Vec<RawParam>,
}

fn attr(e: &quick_xml::events::BytesStart, key: &[u8]) -> Option<String> {
    e.attributes()
        .flatten()
        .find(|a| a.key.as_ref() == key)
        .map(|a| String::from_utf8_lossy(&a.value).into_owned())
        .filter(|v| !v.is_empty())
}

fn raw_param(e: &quick_xml::events::BytesStart) -> Option<RawParam> {
    Some(RawParam {
        accession: attr(e, b"accession")?,
        value: attr(e, b"value").unwrap_or_default(),
        unit_accession: attr(e, b"unitAccession"),
        unit_name: attr(e, b"unitName"),
    })
}

/// The `<scanSettings>` of an imzML header. Reading stops at `<run>`; spectra are never touched.
pub fn read_scan_settings(path: &Path) -> Result<Vec<RawSettings>> {
    let file = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    read_scan_settings_from(std::io::BufReader::new(file))
}

pub fn read_scan_settings_from(input: impl BufRead) -> Result<Vec<RawSettings>> {
    let mut reader = quick_xml::Reader::from_reader(input);
    let mut buf = Vec::new();
    let mut groups: HashMap<String, Vec<RawParam>> = HashMap::new();
    let mut group: Option<(String, Vec<RawParam>)> = None;
    let mut settings: Vec<RawSettings> = Vec::new();
    let mut current: Option<RawSettings> = None;
    loop {
        let ev = reader.read_event_into(&mut buf).context("parsing the imzML header")?;
        match &ev {
            Event::Start(e) | Event::Empty(e) => {
                let empty = matches!(ev, Event::Empty(_));
                match e.local_name().as_ref() {
                    b"run" => break,
                    b"referenceableParamGroup" if !empty => {
                        group = attr(e, b"id").map(|id| (id, Vec::new()));
                    }
                    b"scanSettings" => {
                        let s = RawSettings { id: attr(e, b"id").unwrap_or_default(), params: Vec::new() };
                        if empty { settings.push(s) } else { current = Some(s) }
                    }
                    b"cvParam" => {
                        if let Some(p) = raw_param(e) {
                            if let Some(s) = current.as_mut() {
                                s.params.push(p);
                            } else if let Some((_, g)) = group.as_mut() {
                                g.push(p);
                            }
                        }
                    }
                    b"referenceableParamGroupRef" => {
                        if let (Some(s), Some(r)) = (current.as_mut(), attr(e, b"ref")) {
                            s.params.extend(groups.get(&r).cloned().unwrap_or_default());
                        }
                    }
                    _ => {}
                }
            }
            Event::End(e) => match e.local_name().as_ref() {
                b"referenceableParamGroup" => {
                    if let Some((id, g)) = group.take() {
                        groups.insert(id, g);
                    }
                }
                b"scanSettings" => settings.extend(current.take()),
                b"scanSettingsList" => break,
                _ => {}
            },
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    Ok(settings)
}

/// The canonical name of a length unit accession, for the accession/name check.
fn length_unit_name(accession: &str) -> Option<&'static str> {
    Some(match accession {
        "UO:0000008" => "meter",
        "UO:0000015" => "centimeter",
        "UO:0000016" => "millimeter",
        "UO:0000017" => "micrometer",
        "UO:0000018" => "nanometer",
        _ => return None,
    })
}

fn normalized_unit_name(name: &str) -> String {
    name.trim().to_lowercase().replace("metre", "meter").replace("µm", "micrometer").replace("μm", "micrometer")
}

/// What the pixel-size rule did to one `<scanSettings>`.
#[derive(Debug, Clone, PartialEq)]
pub struct PixelSizeFix {
    pub settings_id: String,
    /// Which case of the rule applied.
    pub case: &'static str,
    /// The `transformations` entry it declares, when the values changed.
    pub transformation: Option<&'static str>,
    /// Pixel sizes to write: accession (`IMS:1000046`/`47`), value, and whether its unit is set to
    /// micrometre. An accession absent here is removed.
    pub write: Vec<(&'static str, f64, bool)>,
    /// Unit accession/name disagreements seen on the pixel-size and extent params.
    pub unit_mismatches: Vec<String>,
    /// `(param accession, stated unit accession)` of those params, to compare with what was written.
    pub mismatched: Vec<(String, String)>,
    /// What [`check_written_units`] found: the unit each mismatched param was written with.
    pub written_units: Vec<String>,
    pub detail: String,
}

const PIXEL_X: &str = "IMS:1000046";
const PIXEL_Y: &str = "IMS:1000047";

fn approx(a: f64, b: f64) -> bool {
    (a - b).abs() <= 1e-6 * a.abs().max(b.abs()).max(1.0)
}

/// The rule for one `<scanSettings>`; `None` when it states no pixel size and no mismatch.
pub fn pixel_size_fix(s: &RawSettings) -> Option<PixelSizeFix> {
    let get = |acc: &str| s.params.iter().find(|p| p.accession == acc);
    let num = |acc: &str| get(acc).and_then(|p| p.value.trim().parse::<f64>().ok()).filter(|v| v.is_finite());
    let unit_mismatches: Vec<String> = ["IMS:1000044", "IMS:1000045", PIXEL_X, PIXEL_Y]
        .iter()
        .filter_map(|acc| get(acc))
        .filter_map(|p| {
            let canonical = length_unit_name(p.unit_accession.as_deref()?)?;
            let stated = p.unit_name.as_deref()?;
            (normalized_unit_name(stated) != canonical).then(|| {
                format!("{}: unit {} is {canonical} but named {stated:?}", p.accession, p.unit_accession.as_deref().unwrap_or(""))
            })
        })
        .collect();
    let mismatched: Vec<(String, String)> = ["IMS:1000044", "IMS:1000045", PIXEL_X, PIXEL_Y]
        .iter()
        .filter_map(|acc| get(acc))
        .filter_map(|p| {
            let ua = p.unit_accession.as_deref()?;
            let canonical = length_unit_name(ua)?;
            (normalized_unit_name(p.unit_name.as_deref()?) != canonical).then(|| (p.accession.clone(), ua.to_string()))
        })
        .collect();
    let (x, y) = (get(PIXEL_X), get(PIXEL_Y));
    let fix = |case, transformation, write, detail: String| PixelSizeFix {
        settings_id: s.id.clone(),
        case,
        transformation,
        write,
        unit_mismatches: unit_mismatches.clone(),
        mismatched: mismatched.clone(),
        written_units: Vec::new(),
        detail,
    };
    match (x, y) {
        (None, None) => (!unit_mismatches.is_empty())
            .then(|| fix("no pixel size stated", None, vec![], String::new())),
        (Some(px), Some(py)) => {
            let (Some(vx), Some(vy)) = (num(PIXEL_X), num(PIXEL_Y)) else {
                return Some(fix("x and y not numeric", Some(DROPPED), vec![], format!("x={:?} y={:?}", px.value, py.value)));
            };
            if px.unit_accession.is_some() && py.unit_accession.is_some() {
                Some(fix("x and y with a unit", None, vec![(PIXEL_X, vx, false), (PIXEL_Y, vy, false)], String::new()))
                    .filter(|f| !f.unit_mismatches.is_empty())
            } else {
                Some(fix(
                    "x and y without a unit: micrometre assumed",
                    Some(UNIT_ASSUMED),
                    vec![(PIXEL_X, vx, px.unit_accession.is_none()), (PIXEL_Y, vy, py.unit_accession.is_none())],
                    format!("x={vx} y={vy}"),
                ))
            }
        }
        (Some(p), None) | (None, Some(p)) => {
            let acc: &'static str = if p.accession == PIXEL_X { PIXEL_X } else { PIXEL_Y };
            let Some(v) = num(acc).filter(|v| *v > 0.0) else {
                return Some(fix("one value, not numeric", Some(DROPPED), vec![], format!("{acc}={:?}", p.value)));
            };
            // Test against the same axis first, then the other: pixels are square in every
            // surveyed file that states both.
            let axes = [("IMS:1000042", "IMS:1000044"), ("IMS:1000043", "IMS:1000045")];
            let tested: Vec<(f64, f64)> =
                axes.iter().filter_map(|(c, e)| Some((num(c)?, num(e)?))).filter(|(c, e)| *c > 0.0 && *e > 0.0).collect();
            if let Some((count, extent)) = tested.iter().find(|(c, e)| approx(v.sqrt() * c, *e)) {
                Some(fix(
                    "one value: an area (√value × count = extent)",
                    Some(AREA_TO_LENGTH),
                    // The square root of an area is a length: micrometre, whatever unit the area had.
                    vec![(acc, v.sqrt(), true)],
                    format!("{acc}={v} as area; √{v} × {count} = {extent}"),
                ))
            } else if let Some((count, extent)) = tested.iter().find(|(c, e)| approx(v * c, *e)) {
                let unit_assumed = p.unit_accession.is_none();
                Some(fix(
                    "one value: a length (value × count = extent)",
                    unit_assumed.then_some(UNIT_ASSUMED),
                    vec![(acc, v, unit_assumed)],
                    format!("{acc}={v}; {v} × {count} = {extent}"),
                ))
            } else {
                Some(fix(
                    "one value that tests as neither area nor length",
                    Some(DROPPED),
                    vec![],
                    format!("{acc}={v}; count/extent {tested:?}"),
                ))
            }
        }
    }
}

/// All fixes for a header.
pub fn pixel_size_fixes(settings: &[RawSettings]) -> Vec<PixelSizeFix> {
    settings.iter().filter_map(pixel_size_fix).collect()
}

/// Apply a fix to the writer's copy of the same `<scanSettings>`: the pixel-size params become
/// exactly `fix.write` (micrometre where the unit was assumed).
pub fn apply(fix: &PixelSizeFix, settings: &mut ScanSettings) {
    if fix.transformation.is_none() {
        return;
    }
    for acc in [PIXEL_X, PIXEL_Y] {
        let wanted = fix.write.iter().find(|(a, _, _)| *a == acc);
        let pos = settings.params.iter().position(|p| p.curie().is_some_and(|c| c.to_string() == acc));
        match (pos, wanted) {
            (Some(i), None) => {
                settings.params.remove(i);
            }
            (Some(i), Some((_, v, assume_um))) => {
                let p = &mut settings.params[i];
                p.value = (*v).into();
                if *assume_um {
                    p.unit = Unit::Micrometer;
                }
            }
            _ => {}
        }
    }
}

/// After [`apply`]: the unit each mismatched param was actually written with. `true` when one of
/// them differs from the accession the file states — mzdata resolved the pair by the name.
pub fn check_written_units(fix: &mut PixelSizeFix, settings: &ScanSettings) -> bool {
    let mut replaced = false;
    for (acc, stated) in &fix.mismatched {
        let Some(p) = settings.params.iter().find(|p| p.curie().is_some_and(|c| c.to_string() == *acc)) else { continue };
        let written = p.unit.to_curie().map(|c| c.to_string()).unwrap_or_else(|| "none".into());
        replaced |= written != *stated;
        fix.written_units.push(format!("{acc}: stated {stated}, written {written}"));
    }
    replaced
}

/// Replace the obsolete scan term "one way" (`IMS:1000411`) by flyback (`IMS:1000413`), its stated
/// replacement with the same definition. `true` when one was replaced.
pub fn one_way_to_flyback(settings: &mut ScanSettings) -> bool {
    let mut replaced = false;
    for p in settings.params.iter_mut() {
        if p.curie().is_some_and(|c| c.to_string() == "IMS:1000411") {
            *p = Param::builder().name("flyback").curie(mzdata::curie!(IMS:1000413)).build();
            replaced = true;
        }
    }
    replaced
}

/// The `imaging_pixel_size` index block row for a fix.
pub fn fix_json(f: &PixelSizeFix) -> serde_json::Value {
    serde_json::json!({
        "scan_settings": f.settings_id,
        "case": f.case,
        "transformation": f.transformation,
        "written_um": f.write.iter().map(|(a, v, assumed)| serde_json::json!({"accession": a, "value": v, "unit_assumed": assumed})).collect::<Vec<_>>(),
        "unit_mismatches": f.unit_mismatches,
        "written_units": f.written_units,
        "detail": f.detail,
    })
}

/// `file_description.contents` params for the imzML provenance mzdata consumed.
pub fn provenance_params(meta: &mzdata::io::imzml::reader::ImzMLFileMetadata) -> Vec<Param> {
    use mzdata::io::imzml::reader::IbdDataMode;
    let mut out = Vec::new();
    match meta.data_mode {
        Some(IbdDataMode::Continuous) => out.push(Param::builder().name("continuous").curie(mzdata::curie!(IMS:1000030)).build()),
        Some(IbdDataMode::Processed) => out.push(Param::builder().name("processed").curie(mzdata::curie!(IMS:1000031)).build()),
        _ => {}
    }
    if let Some(uuid) = meta.uuid {
        out.push(
            Param::builder()
                .name("universally unique identifier")
                .curie(mzdata::curie!(IMS:1000080))
                .value(format!("{{{}}}", uuid.hyphenated().to_string().to_uppercase()))
                .build(),
        );
    }
    if let Some(sum) = meta.ibd_checksum.as_deref().filter(|s| !s.is_empty()) {
        let term = match meta.ibd_checksum_type.as_deref() {
            Some("MD5") => Some(("ibd MD5", mzdata::curie!(IMS:1000090))),
            Some("SHA1") => Some(("ibd SHA-1", mzdata::curie!(IMS:1000091))),
            Some("SHA256") => Some(("ibd SHA-256", mzdata::curie!(IMS:1000092))),
            _ => None,
        };
        if let Some((name, curie)) = term {
            out.push(Param::builder().name(name).curie(curie).value(sum.to_string()).build());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(params: &[(&str, &str, Option<(&str, &str)>)]) -> RawSettings {
        RawSettings {
            id: "s1".into(),
            params: params
                .iter()
                .map(|(a, v, u)| RawParam {
                    accession: a.to_string(),
                    value: v.to_string(),
                    unit_accession: u.map(|u| u.0.to_string()),
                    unit_name: u.map(|u| u.1.to_string()),
                })
                .collect(),
        }
    }
    const UM: Option<(&str, &str)> = Some(("UO:0000017", "micrometer"));

    #[test]
    fn x_and_y_with_a_unit_are_kept() {
        let s = settings(&[("IMS:1000046", "50", UM), ("IMS:1000047", "50", UM)]);
        assert_eq!(pixel_size_fix(&s), None);
    }

    #[test]
    fn x_and_y_without_a_unit_become_micrometre() {
        let f = pixel_size_fix(&settings(&[("IMS:1000046", "20", None), ("IMS:1000047", "25", None)])).unwrap();
        assert_eq!(f.transformation, Some(UNIT_ASSUMED));
        assert_eq!(f.write, vec![(PIXEL_X, 20.0, true), (PIXEL_Y, 25.0, true)]);
    }

    #[test]
    fn one_value_is_tested_as_area_then_length() {
        // 100 px × 10 µm = 1000 µm: 100 (µm²) is an area, 10 a length.
        let area = pixel_size_fix(&settings(&[("IMS:1000046", "100", None), ("IMS:1000042", "100", None), ("IMS:1000044", "1000", UM)])).unwrap();
        assert_eq!((area.transformation, area.write.clone()), (Some(AREA_TO_LENGTH), vec![(PIXEL_X, 10.0, true)]));
        let length = pixel_size_fix(&settings(&[("IMS:1000046", "10", UM), ("IMS:1000042", "100", None), ("IMS:1000044", "1000", UM)])).unwrap();
        assert_eq!((length.transformation, length.write.clone()), (None, vec![(PIXEL_X, 10.0, false)]));
        // Only the y axis states count and extent: it is used.
        let y_axis = pixel_size_fix(&settings(&[("IMS:1000046", "400", UM), ("IMS:1000043", "50", None), ("IMS:1000045", "1000", UM)])).unwrap();
        assert_eq!(y_axis.transformation, Some(AREA_TO_LENGTH));
        let neither = pixel_size_fix(&settings(&[("IMS:1000046", "7", UM), ("IMS:1000042", "100", None), ("IMS:1000044", "1000", UM)])).unwrap();
        assert_eq!((neither.transformation, neither.write.is_empty()), (Some(DROPPED), true));
        let untestable = pixel_size_fix(&settings(&[("IMS:1000046", "7", UM)])).unwrap();
        assert_eq!(untestable.transformation, Some(DROPPED), "no count/extent: nothing to test against");
    }

    #[test]
    fn a_unit_accession_that_disagrees_with_its_name_is_reported() {
        let cm = Some(("UO:0000015", "micrometer"));
        let f = pixel_size_fix(&settings(&[("IMS:1000046", "50", cm), ("IMS:1000047", "50", cm)])).unwrap();
        assert_eq!(f.transformation, None, "reported, not rewritten");
        assert_eq!(f.unit_mismatches.len(), 2, "{f:?}");
        let fine = Some(("UO:0000017", "micrometre"));
        assert_eq!(pixel_size_fix(&settings(&[("IMS:1000046", "50", fine), ("IMS:1000047", "50", fine)])), None);
    }

    #[test]
    fn header_params_come_with_units_and_expanded_groups() {
        let xml = r#"<?xml version="1.0"?><mzML><referenceableParamGroupList count="1">
            <referenceableParamGroup id="px"><cvParam cvRef="IMS" accession="IMS:1000046" name="pixel size" value="100" unitCvRef="UO" unitAccession="UO:0000015" unitName="micrometer"/></referenceableParamGroup>
          </referenceableParamGroupList>
          <scanSettingsList count="1"><scanSettings id="scansettings1">
            <referenceableParamGroupRef ref="px"/>
            <cvParam cvRef="IMS" accession="IMS:1000042" name="max count of pixel x" value="3"/>
            <cvParam cvRef="IMS" accession="IMS:1000411" name="one way" value=""/>
          </scanSettings></scanSettingsList><run id="r"><spectrumList count="0"/></run></mzML>"#;
        let s = read_scan_settings_from(xml.as_bytes()).unwrap();
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].id, "scansettings1");
        let accs: Vec<&str> = s[0].params.iter().map(|p| p.accession.as_str()).collect();
        assert_eq!(accs, ["IMS:1000046", "IMS:1000042", "IMS:1000411"]);
        assert_eq!(s[0].params[0].unit_name.as_deref(), Some("micrometer"));
        assert_eq!(s[0].params[0].unit_accession.as_deref(), Some("UO:0000015"));
    }

    #[test]
    fn apply_rewrites_only_what_the_fix_says() {
        let mut ss = ScanSettings { id: "s1".into(), ..Default::default() };
        ss.params.push(Param::builder().name("pixel size x").curie(mzdata::curie!(IMS:1000046)).value(100.0).build());
        ss.params.push(Param::builder().name("one way").curie(mzdata::curie!(IMS:1000411)).build());
        let fix = PixelSizeFix {
            settings_id: "s1".into(),
            case: "area",
            transformation: Some(AREA_TO_LENGTH),
            write: vec![(PIXEL_X, 10.0, true)],
            unit_mismatches: vec![],
            mismatched: vec![],
            written_units: vec![],
            detail: String::new(),
        };
        apply(&fix, &mut ss);
        assert_eq!(ss.params[0].value.to_f64().unwrap(), 10.0);
        assert_eq!(ss.params[0].unit, Unit::Micrometer);
        assert!(one_way_to_flyback(&mut ss));
        assert_eq!(ss.params[1].curie().unwrap().to_string(), "IMS:1000413");
        let drop = PixelSizeFix { write: vec![], transformation: Some(DROPPED), ..fix };
        apply(&drop, &mut ss);
        assert!(ss.params.iter().all(|p| p.curie().unwrap().to_string() != "IMS:1000046"));
    }
}

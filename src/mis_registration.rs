//! The teach-point registration of a FlexImaging sequence's image onto the MS pixel grid (owner
//! decision D8, 2026-10-01): the affine `image_px -> ms_px` the imaging profile asks for, computed
//! from the `.mis` and `MaldiFrameInfo` alone, with `registration_quality: teach_points`.
//!
//! Three frames, two of them stated:
//! * **Image pixels → stage µm.** The `<TeachPoint>`s of the `.mis` pair an image pixel with its
//!   stage position (`imgx,imgy;stagex,stagey`); three or more fix an affine `A` (exact with three,
//!   least squares with more).
//! * **Raster index → motor µm.** `MaldiFrameInfo` states each frame's `XIndexPos/YIndexPos` and
//!   `MotorPositionX/Y`; a least-squares affine `IM` fits them (±1000 µm per step on MSV000088438,
//!   the y axis flipped, residual ≤ 0.03 µm). A rotated or mirrored stage is just another `IM`.
//! * **Motor µm → stage µm.** flexImaging's stage frame and timsControl's motor frame differ by a
//!   translation `t` stated in neither file (+54000.8, −45642.5 µm on both MSV000088438 runs, by
//!   flexImaging's spot list). What fixes it: the raster lattice passes through the sequence's
//!   `<ReferencePoint>` (its first teach point) — on both runs the point lies within 1 µm (0.001
//!   step) of a lattice node, a coincidence with odds of 4 × 10⁻⁶ per run otherwise. So
//!   `t = ref_stage − IM_offset − L·k` for an integer index `k`, and `k` is the one placement that
//!   puts every acquired spot inside its own `<Area>` outline (a spot one step off leaves a whole
//!   row of spots outside: 245 of 276 inside against 276 of 276 on the TSF run). The search is
//!   seeded by the bounding-box interval [`crate::bruker_maldi::MaldiInfo::mis_mismatch`] tests,
//!   wide enough to hold every placement that interval allows; none or several passing → no
//!   registration, with the reason recorded.
//!
//! Then `matrix = T⁻¹ ∘ A − position_offset`, `T = [L | IM_offset + t]` (raster index → stage).
//! Checked by the auditors (2026-10-01) and here against flexImaging's own spot list: every spot of
//! both runs within 0.001 MS px; `assumed_full_extent` was off by up to 11.5 MS px on the same
//! points. The matrix maps the sequence's pixel coordinates, taken as 0-based pixel centres (the
//! profile's convention; the half-pixel question is 0.004 MS px here), to 1-based MS pixel centres.

use std::collections::BTreeMap;

use crate::bruker_maldi::{MaldiInfo, Mis};

/// `[a, b, c, d, e, f]`: `x' = a·x + b·y + c`, `y' = d·x + e·y + f` (the imaging profile's order).
pub type Affine = [f64; 6];

pub fn apply(m: &Affine, (x, y): (f64, f64)) -> (f64, f64) {
    (m[0] * x + m[1] * y + m[2], m[3] * x + m[4] * y + m[5])
}

/// `None` when the linear part is singular.
pub fn invert(m: &Affine) -> Option<Affine> {
    let det = m[0] * m[4] - m[1] * m[3];
    if det == 0.0 || !det.is_finite() {
        return None;
    }
    let (a, b, d, e) = (m[4] / det, -m[1] / det, -m[3] / det, m[0] / det);
    Some([a, b, -(a * m[2] + b * m[5]), d, e, -(d * m[2] + e * m[5])])
}

/// `outer ∘ inner`: apply `inner` first.
pub fn compose(outer: &Affine, inner: &Affine) -> Affine {
    [
        outer[0] * inner[0] + outer[1] * inner[3],
        outer[0] * inner[1] + outer[1] * inner[4],
        outer[0] * inner[2] + outer[1] * inner[5] + outer[2],
        outer[3] * inner[0] + outer[4] * inner[3],
        outer[3] * inner[1] + outer[4] * inner[4],
        outer[3] * inner[2] + outer[4] * inner[5] + outer[5],
    ]
}

/// A source point and the point it maps to: (image px, stage µm) for a teach point, (raster
/// index, motor µm) for a frame.
pub type Pair = ((f64, f64), (f64, f64));

/// The least-squares affine taking each pair's first point to its second, and the largest
/// residual (Euclidean). `None` with fewer than three pairs or collinear sources. Centred normal
/// equations: image pixels run to 8000 and stage µm to 60000, so the uncentred sums lose digits.
pub fn fit(pairs: &[Pair]) -> Option<(Affine, f64)> {
    let n = pairs.len();
    if n < 3 {
        return None;
    }
    let mean = |f: &dyn Fn(&Pair) -> f64| pairs.iter().map(f).sum::<f64>() / n as f64;
    let (mx, my) = (mean(&|p| p.0 .0), mean(&|p| p.0 .1));
    let (mu, mv) = (mean(&|p| p.1 .0), mean(&|p| p.1 .1));
    let (mut sxx, mut sxy, mut syy) = (0.0, 0.0, 0.0);
    let (mut sxu, mut syu, mut sxv, mut syv) = (0.0, 0.0, 0.0, 0.0);
    for &((x, y), (u, v)) in pairs {
        let (dx, dy, du, dv) = (x - mx, y - my, u - mu, v - mv);
        sxx += dx * dx;
        sxy += dx * dy;
        syy += dy * dy;
        sxu += dx * du;
        syu += dy * du;
        sxv += dx * dv;
        syv += dy * dv;
    }
    let det = sxx * syy - sxy * sxy;
    if !det.is_finite() || det <= 1e-9 * sxx * syy {
        return None; // collinear (or coincident) sources, or NaN: no plane to fit
    }
    let a = (syy * sxu - sxy * syu) / det;
    let b = (sxx * syu - sxy * sxu) / det;
    let d = (syy * sxv - sxy * syv) / det;
    let e = (sxx * syv - sxy * sxv) / det;
    let m = [a, b, mu - a * mx - b * my, d, e, mv - d * mx - e * my];
    let residual = pairs
        .iter()
        .map(|&(src, (u, v))| {
            let (pu, pv) = apply(&m, src);
            ((pu - u).powi(2) + (pv - v).powi(2)).sqrt()
        })
        .fold(0.0, f64::max);
    Some((m, residual))
}

/// Whether `p` lies inside `poly` (even-odd rule) or within `eps` of one of its edges: a spot on
/// an area's boundary is flexImaging's to include, not ours to lose.
fn inside_or_near(poly: &[(f64, f64)], p: (f64, f64), eps: f64) -> bool {
    let n = poly.len();
    if n < 3 {
        return false;
    }
    let (x, y) = p;
    let mut inside = false;
    for i in 0..n {
        let ((x1, y1), (x2, y2)) = (poly[i], poly[(i + 1) % n]);
        if (y1 > y) != (y2 > y) && x < (x2 - x1) * (y - y1) / (y2 - y1) + x1 {
            inside = !inside;
        }
    }
    if inside || eps <= 0.0 {
        return inside;
    }
    (0..n).any(|i| {
        let ((x1, y1), (x2, y2)) = (poly[i], poly[(i + 1) % n]);
        let (dx, dy) = (x2 - x1, y2 - y1);
        let len2 = dx * dx + dy * dy;
        let s = if len2 > 0.0 { ((x - x1) * dx + (y - y1) * dy) / len2 } else { 0.0 }.clamp(0.0, 1.0);
        let (px, py) = (x1 + s * dx, y1 + s * dy);
        ((x - px).powi(2) + (y - py).powi(2)).sqrt() <= eps
    })
}

/// The registration of the sequence's image, and the evidence to redo it.
#[derive(Debug, Clone, PartialEq)]
pub struct Registration {
    /// Image px (0-based centres) → MS px (1-based centres, `position_x/y`).
    pub matrix: Affine,
    /// `A`: image px → stage µm (the teach points' frame).
    pub image_to_stage: Affine,
    /// `T`: raster index (`XIndexPos/YIndexPos`) → stage µm.
    pub index_to_stage: Affine,
    /// `t`: stage µm = motor µm + `t`.
    pub stage_minus_motor: (f64, f64),
    /// The teach points, `(image px, stage µm)`, as read.
    pub teach: Vec<((f64, f64), (f64, f64))>,
    pub teach_residual_um: f64,
    /// The `<ReferencePoint>` (image px) and the raster index of the lattice node on it.
    pub reference_image: (f64, f64),
    pub reference_index: (i64, i64),
    /// The fitted raster step (the shorter axis), µm, and the lattice fit's largest residual.
    pub step_um: f64,
    pub lattice_residual_um: f64,
    /// Spots whose placement inside their area was checked, and the lattice placements that passed
    /// the check out of those tried (1 of n).
    pub spots_checked: usize,
    pub placements_tried: usize,
}

/// Register `mis` (used, [`MaldiInfo::check_mis`] passed) on `info`'s frames; `Err` says why not.
pub fn register(info: &MaldiInfo, mis: &Mis) -> Result<Registration, String> {
    if mis.teach.len() < 3 {
        return Err(format!("{} has {} teach point(s); three are needed", mis.file, mis.teach.len()));
    }
    let (image_to_stage, teach_residual_um) =
        fit(&mis.teach).ok_or_else(|| format!("the teach points of {} are collinear: no map from the image to the stage", mis.file))?;
    // Raster index → motor µm from every frame that states both.
    let lattice: Vec<Pair> = info.spots.values().filter_map(|s| Some(((s.x as f64, s.y as f64), s.motor?))).collect();
    let (index_to_motor, lattice_residual_um) = fit(&lattice).ok_or_else(|| {
        format!("{} frames state a motor position; three on more than one line are needed to fit the raster lattice", lattice.len())
    })?;
    let step_um = (index_to_motor[0].hypot(index_to_motor[3])).min(index_to_motor[1].hypot(index_to_motor[4]));
    if step_um.is_nan() || step_um <= 0.0 || lattice_residual_um > 0.05 * step_um {
        return Err(format!(
            "the frames' XIndexPos/YIndexPos and MotorPositionX/Y do not fit one lattice (largest residual {lattice_residual_um:.1} µm at a {step_um:.1} µm step)"
        ));
    }
    if teach_residual_um > 0.5 * step_um {
        return Err(format!(
            "the {} teach points of {} do not fit one affine map (largest residual {teach_residual_um:.0} µm, more than half a {step_um:.0} µm raster step)",
            mis.teach.len(),
            mis.file
        ));
    }
    let motor_to_index = invert(&index_to_motor).ok_or("the raster lattice fit is singular")?;
    // The reference point on the stage: the stated stage position of the teach point it is, else
    // its image position mapped.
    let reference_image = mis.reference.unwrap_or(mis.teach[0].0);
    let ref_stage = mis
        .teach
        .iter()
        .find(|(img, _)| *img == reference_image)
        .map_or_else(|| apply(&image_to_stage, reference_image), |(_, stage)| *stage);
    // Each region's outline on the stage, and its spots' motor positions.
    let outlines: Vec<Vec<(f64, f64)>> = mis.areas.iter().map(|a| a.points.iter().map(|&p| apply(&image_to_stage, p)).collect()).collect();
    let mut regions: BTreeMap<usize, Vec<(f64, f64)>> = BTreeMap::new();
    for s in info.spots.values() {
        if let (Some(r), Some(m)) = (s.region.and_then(|r| usize::try_from(r).ok()), s.motor)
            && outlines.get(r).is_some_and(|o| o.len() >= 3)
        {
            regions.entry(r).or_default().push(m);
        }
    }
    let spots_checked: usize = regions.values().map(Vec::len).sum();
    if spots_checked == 0 {
        return Err("no acquired spot lies in a region whose <Area> outline is known, so no lattice placement can be checked".into());
    }
    // Seed: the interval of translations under which every region's spots lie in its area's stage
    // bounding box (half a step of slack), as the .mis check computes it; its midpoint names the
    // nearest lattice node, and its width says how many neighbours to try.
    let bounds = |v: &mut dyn Iterator<Item = f64>| v.fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), x| (lo.min(x), hi.max(x)));
    let (mut lo, mut hi) = ([f64::NEG_INFINITY; 2], [f64::INFINITY; 2]);
    for (&r, spots) in &regions {
        for k in 0..2 {
            let (a_lo, a_hi) = bounds(&mut outlines[r].iter().map(|p| if k == 0 { p.0 } else { p.1 }));
            let (s_lo, s_hi) = bounds(&mut spots.iter().map(|p| if k == 0 { p.0 } else { p.1 }));
            lo[k] = lo[k].max(a_lo - s_lo - step_um / 2.0);
            hi[k] = hi[k].min(a_hi - s_hi + step_um / 2.0);
        }
    }
    let mid = ((lo[0] + hi[0]) / 2.0, (lo[1] + hi[1]) / 2.0);
    let width = (hi[0] - lo[0]).max(hi[1] - lo[1]);
    let reach = if width.is_finite() && width > 0.0 { 3 + (width / step_um).ceil() as i64 } else { 3 }.min(12);
    let (kx0, ky0) = {
        let (fx, fy) = apply(&motor_to_index, (ref_stage.0 - mid.0, ref_stage.1 - mid.1));
        (fx.round() as i64, fy.round() as i64)
    };
    let eps = 0.02 * step_um;
    let mut passing: Vec<((i64, i64), (f64, f64))> = Vec::new();
    let mut placements_tried = 0;
    for di in -reach..=reach {
        for dj in -reach..=reach {
            let k = (kx0 + di, ky0 + dj);
            // t = ref_stage − IM(k): the translation that puts lattice node k on the reference point.
            let node = apply(&index_to_motor, (k.0 as f64, k.1 as f64));
            let t = (ref_stage.0 - node.0, ref_stage.1 - node.1);
            placements_tried += 1;
            let all_inside = regions.iter().all(|(&r, spots)| spots.iter().all(|&(mx, my)| inside_or_near(&outlines[r], (mx + t.0, my + t.1), eps)));
            if all_inside {
                passing.push((k, t));
            }
        }
    }
    let (reference_index, stage_minus_motor) = match passing.as_slice() {
        [one] => *one,
        [] => {
            return Err(format!(
                "no placement of the raster lattice through the reference point ({} tried) puts every acquired spot inside its <Area> outline",
                placements_tried
            ))
        }
        many => return Err(format!("{} placements of the raster lattice through the reference point put every acquired spot inside its <Area> outline: ambiguous", many.len())),
    };
    let index_to_stage = [
        index_to_motor[0],
        index_to_motor[1],
        index_to_motor[2] + stage_minus_motor.0,
        index_to_motor[3],
        index_to_motor[4],
        index_to_motor[5] + stage_minus_motor.1,
    ];
    let stage_to_index = invert(&index_to_stage).ok_or("the raster lattice fit is singular")?;
    let mut matrix = compose(&stage_to_index, &image_to_stage);
    matrix[2] -= (info.min.0 - 1) as f64;
    matrix[5] -= (info.min.1 - 1) as f64;
    Ok(Registration {
        matrix,
        image_to_stage,
        index_to_stage,
        stage_minus_motor,
        teach: mis.teach.clone(),
        teach_residual_um,
        reference_image,
        reference_index,
        step_um,
        lattice_residual_um,
        spots_checked,
        placements_tried,
    })
}

/// The value written as `registration_quality` for this registration.
pub const QUALITY: &str = "teach_points";

impl Registration {
    /// The record a reader needs to redo the transform (converter-defined keys, inside
    /// `metadata.imaging` and the `bruker_maldi` block).
    pub fn json(&self, mis_file: &str) -> serde_json::Value {
        let pt = |(x, y): (f64, f64)| serde_json::json!([x, y]);
        serde_json::json!({
            "sequence": mis_file,
            "method": "image px → stage µm from the teach points; raster index → motor µm fitted to the frames; the lattice placed through the reference point where every spot lies in its area",
            "pixel_convention": "0-based image pixel coordinates of the sequence, taken as pixel centres, to 1-based MS pixel centres (position_x/y = raster index − position_offset)",
            "matrix": self.matrix,
            "teach_points": self.teach.iter().map(|&(img, stage)| serde_json::json!({"image_px": pt(img), "stage_um": pt(stage)})).collect::<Vec<_>>(),
            "teach_point_fit_max_residual_um": self.teach_residual_um,
            "reference_point": {"image_px": pt(self.reference_image), "raster_index": [self.reference_index.0, self.reference_index.1]},
            "image_to_stage_um": self.image_to_stage,
            "raster_index_to_stage_um": self.index_to_stage,
            "stage_minus_motor_um": pt(self.stage_minus_motor),
            "raster_step_fitted_um": self.step_um,
            "lattice_fit_max_residual_um": self.lattice_residual_um,
            "spots_checked": self.spots_checked,
            "lattice_placements_tried": self.placements_tried,
        })
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::bruker_maldi::{read, read_mis_from};
    use rusqlite::Connection;

    /// MassIVE MSV000088438, as the two `.mis` files state it (teach points, reference point, the
    /// four areas), and per run the 13 spots at the extremes of each region (least and greatest x
    /// and y), with their `MotorPositionX/Y` from `MaldiFrameInfo` and their stage position from
    /// flexImaging's spot list (the truth the registration is checked against).
    pub(crate) struct Run {
        pub mis: &'static str,
        pub rows: &'static [(i64, i64, i64, f64, f64, &'static str)],
        /// `(spot name, stage x, stage y)` from `<stem>_spot_list.txt`.
        pub spots: &'static [(&'static str, f64, f64)],
        /// The auditors' matrix (reg2.py, every spot of the run).
        pub expect: Affine,
        pub image: (u32, u32),
    }

    pub(crate) const TSF: Run = Run {
        mis: "<ImagingSequence flexImagingVersion=\"5.1.52.0_1664_120\" last_modified=\"2021-09-20T16:47:01\">\r\n\
            <Comment>1000 um raster on previously imaged sample for Gordon</Comment>\r\n\
            <ImageFile>IMG_0000.jpg</ImageFile>\r\n<OriginalImage>C:\\Users\\Admin\\Downloads\\IMG_1357.jpg</OriginalImage>\r\n\
            <TeachPoint>1252,776;-22963,15832</TeachPoint>\r\n<TeachPoint>6842,696;21879,17145</TeachPoint>\r\n\
            <TeachPoint>2610,5064;-11828,-18267</TeachPoint>\r\n<ReferencePoint>1252,776</ReferencePoint>\r\n\
            <Area Type=\"3\" Name=\"vc_rugose_1\" ZoomArea=\"0\" Enabled=\"0\" ShowSpectra=\"0\" SpectrumColor=\"#993333\">\r\n<Raster>1000,1000</Raster>\r\n\
            <Point>1450,1592</Point>\r\n<Point>2750,1517</Point>\r\n<Point>2858,2817</Point>\r\n<Point>1517,2875</Point>\r\n</Area>\r\n\
            <Area Type=\"3\" Name=\"vc_rugose_2\" ZoomArea=\"0\" Enabled=\"0\" ShowSpectra=\"0\" SpectrumColor=\"#0000ff\">\r\n<Raster>1000,1000</Raster>\r\n\
            <Point>3688,1403</Point>\r\n<Point>4879,1444</Point>\r\n<Point>4871,2794</Point>\r\n<Point>3554,2744</Point>\r\n\
            <Point>3596,1394</Point>\r\n<Point>3693,1404</Point>\r\n<Point>3694,1404</Point>\r\n<Point>3697,1403</Point>\r\n</Area>\r\n\
            <Area Type=\"0\" Name=\"agar\" ZoomArea=\"0\" Enabled=\"0\" ShowSpectra=\"0\" SpectrumColor=\"#00ff00\">\r\n<Raster>1000,1000</Raster>\r\n\
            <Point>2322,3857</Point>\r\n<Point>2922,4340</Point>\r\n</Area>\r\n\
            <Area Type=\"0\" Name=\"agar2\" ZoomArea=\"0\" Enabled=\"0\" ShowSpectra=\"0\" SpectrumColor=\"#ff00ff\">\r\n<Raster>1000,1000</Raster>\r\n\
            <Point>3089,3807</Point>\r\n<Point>3681,4357</Point>\r\n</Area>\r\n</ImagingSequence>\r\n",
        rows: &[
            (0, 12, 13, 33038.81520112356, -36810.501636862755, "R00X012Y013"),
            (0, 22, 12, 43038.829524864756, -35810.48439621925, "R00X022Y012"),
            (0, 19, 12, 40038.831011255585, -35810.48439621925, "R00X019Y012"),
            (0, 13, 22, 34038.83244176706, -45810.49987666309, "R00X013Y022"),
            (1, 29, 11, 50038.844949429236, -34810.51959276199, "R01X029Y011"),
            (1, 39, 11, 60038.807607119285, -34810.51959276199, "R01X039Y011"),
            (1, 29, 21, 50038.844949429236, -44810.48282880336, "R01X029Y021"),
            (2, 19, 31, 40038.831011255585, -54810.49734532833, "R02X019Y031"),
            (2, 23, 31, 44038.84676550826, -54810.49734532833, "R02X023Y031"),
            (2, 19, 34, 40038.831011255585, -57810.496630072594, "R02X019Y034"),
            (3, 25, 30, 46038.82880960902, -53810.48010468483, "R03X025Y030"),
            (3, 29, 30, 50038.844949429236, -53810.48010468483, "R03X029Y030"),
            (3, 25, 34, 46038.82880960902, -57810.496630072594, "R03X025Y034"),
        ],
        spots: &[
            ("R00X012Y013", -20962.0, 8832.0),
            ("R00X022Y012", -10962.0, 9832.0),
            ("R00X019Y012", -13962.0, 9832.0),
            ("R00X013Y022", -19962.0, -168.0),
            ("R01X029Y011", -3962.0, 10832.0),
            ("R01X039Y011", 6038.0, 10832.0),
            ("R01X029Y021", -3962.0, 832.0),
            ("R02X019Y031", -13962.0, -9168.0),
            ("R02X023Y031", -9962.0, -9168.0),
            ("R02X019Y034", -13962.0, -12168.0),
            ("R03X025Y030", -7962.0, -8168.0),
            ("R03X029Y030", -3962.0, -8168.0),
            ("R03X025Y034", -7962.0, -12168.0),
        ],
        expect: [0.0080226287, 5.6032164e-05, -11.087812, -0.00012053145, 0.0079903675, -10.04962],
        image: (8064, 6048),
    };

    pub(crate) const TDF: Run = Run {
        mis: "<ImagingSequence flexImagingVersion=\"5.1.52.0_1664_120\" last_modified=\"2021-09-21T16:44:34\">\r\n\
            <Comment>1000 um data with tims for gordon</Comment>\r\n\
            <ImageFile>IMG_0000.jpg</ImageFile>\r\n<OriginalImage>C:\\Users\\Admin\\Downloads\\IMG_1390.jpg</OriginalImage>\r\n\
            <TeachPoint>1204,778;-22965,15855</TeachPoint>\r\n<TeachPoint>6706,648;21901,17125</TeachPoint>\r\n\
            <TeachPoint>2550,4990;-11848,-18292</TeachPoint>\r\n<ReferencePoint>1204,778</ReferencePoint>\r\n\
            <Area Type=\"3\" Name=\"vc_rugose_1\" ZoomArea=\"0\" Enabled=\"0\" ShowSpectra=\"0\" SpectrumColor=\"#993333\">\r\n<Raster>1000,1000</Raster>\r\n\
            <Point>1250,1817</Point>\r\n<Point>2658,1867</Point>\r\n<Point>2608,3150</Point>\r\n<Point>1200,3058</Point>\r\n\
            <Point>1255,1817</Point>\r\n<Point>1259,1817</Point>\r\n</Area>\r\n\
            <Area Type=\"3\" Name=\"vc_rugose_2\" ZoomArea=\"0\" Enabled=\"0\" ShowSpectra=\"0\" SpectrumColor=\"#0000ff\">\r\n<Raster>1000,1000</Raster>\r\n\
            <Point>3567,2154</Point>\r\n<Point>4727,2122</Point>\r\n<Point>4755,3210</Point>\r\n<Point>3615,3278</Point>\r\n<Point>3570,2154</Point>\r\n</Area>\r\n\
            <Area Type=\"0\" Name=\"agar_1\" ZoomArea=\"0\" Enabled=\"0\" ShowSpectra=\"0\" SpectrumColor=\"#00ff00\">\r\n<Raster>1000,1000</Raster>\r\n\
            <Point>1962,4015</Point>\r\n<Point>2454,4635</Point>\r\n</Area>\r\n\
            <Area Type=\"0\" Name=\"agar_2\" ZoomArea=\"0\" Enabled=\"0\" ShowSpectra=\"0\" SpectrumColor=\"#ff00ff\">\r\n<Raster>1000,1000</Raster>\r\n\
            <Point>2686,3987</Point>\r\n<Point>3246,4663</Point>\r\n</Area>\r\n</ImagingSequence>\r\n",
        rows: &[
            (0, 10, 15, 32036.80843194326, -38788.47702771425, "R00X010Y015"),
            (0, 20, 15, 42036.82352681955, -38788.47702771425, "R00X020Y015"),
            (0, 18, 25, 40036.84148271879, -48788.49192980677, "R00X018Y025"),
            (1, 29, 18, 51036.82138105234, -41788.476312458515, "R01X029Y018"),
            (1, 38, 25, 60036.819235285126, -48788.49192980677, "R01X038Y025"),
            (1, 31, 17, 53036.85528398802, -40788.51112343371, "R01X031Y017"),
            (1, 29, 26, 51036.82138105234, -49788.5089776665, "R01X029Y026"),
            (2, 16, 33, 38036.85943861803, -56788.52517336607, "R02X016Y033"),
            (2, 19, 33, 41036.85872336229, -56788.52517336607, "R02X019Y033"),
            (2, 16, 37, 38036.85943861803, -60788.48926156759, "R02X016Y037"),
            (3, 22, 32, 44036.85723697146, -55788.50793272257, "R03X022Y032"),
            (3, 25, 32, 47036.85652171572, -55788.50793272257, "R03X025Y032"),
            (3, 22, 37, 44036.85723697146, -60788.48926156759, "R03X022Y037"),
        ],
        spots: &[
            ("R00X010Y015", -21964.0, 6854.0),
            ("R00X020Y015", -11964.0, 6854.0),
            ("R00X018Y025", -13964.0, -3146.0),
            ("R01X029Y018", -2964.0, 3854.0),
            ("R01X038Y025", 6036.0, -3146.0),
            ("R01X031Y017", -964.0, 4854.0),
            ("R01X029Y026", -2964.0, -4146.0),
            ("R02X016Y033", -15964.0, -11146.0),
            ("R02X019Y033", -12964.0, -11146.0),
            ("R02X016Y037", -15964.0, -15146.0),
            ("R03X022Y032", -9964.0, -10146.0),
            ("R03X025Y032", -6964.0, -10146.0),
            ("R03X022Y037", -9964.0, -15146.0),
        ],
        expect: [0.0081552717, 3.3233691e-05, -9.8448029, -3.8978648e-05, 0.0081195246, -14.27006],
        image: (8064, 6048),
    };

    /// A `MaldiFrameInfo` with `rows`, frames numbered from 1 in order.
    pub(crate) fn table(conn: &Connection, rows: &[(i64, i64, i64, f64, f64, &str)]) {
        conn.execute_batch(
            "CREATE TABLE MaldiFrameInfo (Frame INTEGER PRIMARY KEY, Chip INTEGER, SpotName TEXT, RegionNumber INTEGER,
                                          XIndexPos INTEGER, YIndexPos INTEGER, MotorPositionX REAL, MotorPositionY REAL);",
        )
        .unwrap();
        for (i, (r, x, y, mx, my, name)) in rows.iter().enumerate() {
            conn.execute(
                "INSERT INTO MaldiFrameInfo VALUES (?1, 0, ?2, ?3, ?4, ?5, ?6, ?7)",
                rusqlite::params![i as i64 + 1, name, r, x, y, mx, my],
            )
            .unwrap();
        }
    }

    fn info_of(run: &Run) -> (MaldiInfo, Mis) {
        let c = Connection::open_in_memory().unwrap();
        table(&c, run.rows);
        let mut info = read(&c).unwrap();
        let mis = read_mis_from("run.mis", run.mis.as_bytes()).unwrap();
        assert_eq!(info.mis_mismatch(&mis), None, "the sequence describes these regions");
        info.mis = Some(mis.clone());
        (info, mis)
    }

    /// The largest distance, in MS px, between where `m` puts each spot (its stage position from
    /// the spot list, back through the teach-point map onto the image, then `m`) and its pixel.
    pub(crate) fn spot_list_error(run: &Run, reg: &Registration, m: &Affine) -> f64 {
        let stage_to_image = invert(&reg.image_to_stage).unwrap();
        let (ox, oy) = (run.rows.iter().map(|r| r.1).min().unwrap() - 1, run.rows.iter().map(|r| r.2).min().unwrap() - 1);
        run.rows
            .iter()
            .map(|&(_, x, y, _, _, name)| {
                let &(_, sx, sy) = run.spots.iter().find(|s| s.0 == name).unwrap();
                let (px, py) = apply(m, apply(&stage_to_image, (sx, sy)));
                (px - (x - ox) as f64).abs().max((py - (y - oy) as f64).abs())
            })
            .fold(0.0, f64::max)
    }

    /// Both MSV000088438 runs from the `.mis` text and 13 spots each: the matrix reproduces
    /// flexImaging's spot list within 0.001 MS px and the auditors' all-spot matrices within
    /// 0.0001 MS px over the whole image; the reference point sits on raster node (10, 6) / (9, 6);
    /// `assumed_full_extent` would be off by more than 10 MS px.
    #[test]
    fn both_msv000088438_runs_register_on_their_spot_lists() {
        for (run, name, node) in [(&TSF, "TSF", (10, 6)), (&TDF, "TDF", (9, 6))] {
            let (info, mis) = info_of(run);
            let reg = register(&info, &mis).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(reg.reference_index, node, "{name}");
            assert!(reg.lattice_residual_um < 0.05 && reg.teach_residual_um < 1e-6, "{name}: {reg:?}");
            assert!((reg.step_um - 1000.0).abs() < 0.01, "{name}: {}", reg.step_um);
            assert!((reg.stage_minus_motor.0 + 54001.8).abs() < 1.0 && (reg.stage_minus_motor.1 - 45643.0).abs() < 1.0, "{name}: {:?}", reg.stage_minus_motor);
            let err = spot_list_error(run, &reg, &reg.matrix);
            assert!(err < 0.0011, "{name}: {err} MS px off the spot list");
            let (w, h) = run.image;
            for corner in [(0.0, 0.0), (w as f64 - 1.0, 0.0), (0.0, h as f64 - 1.0), (w as f64 - 1.0, h as f64 - 1.0)] {
                let (ax, ay) = apply(&reg.matrix, corner);
                let (ex, ey) = apply(&run.expect, corner);
                assert!((ax - ex).abs() < 1e-4 && (ay - ey).abs() < 1e-4, "{name} at {corner:?}: {:?} vs auditors {:?}", (ax, ay), (ex, ey));
            }
            // The photo spans far more than the acquired grid: its extent is not the grid's.
            let (x0, y0) = apply(&reg.matrix, (0.0, 0.0));
            assert!(x0 < -9.0 && y0 < -9.0, "{name}: the image's corner lies {x0:.1}, {y0:.1} MS px before pixel 1");
            assert_eq!((reg.spots_checked, reg.teach.len()), (13, 3), "{name}");
            let j = reg.json("run.mis");
            assert_eq!(j["teach_points"][0], serde_json::json!({"image_px": [mis.teach[0].0 .0, mis.teach[0].0 .1], "stage_um": [mis.teach[0].1 .0, mis.teach[0].1 .1]}));
            assert_eq!(j["reference_point"]["raster_index"], serde_json::json!([node.0, node.1]), "{name}");
        }
    }

    /// A stage turned by 90° and mirrored, three regions, a reference point that is not a lattice
    /// node of the acquired spots: the matrix comes back as the truth it was built from, so the
    /// derivation does not lean on MSV000088438's axis-aligned, unmirrored geometry.
    #[test]
    fn a_rotated_and_mirrored_stage_registers_to_the_truth() {
        // Truth: image px → stage µm turns by 30°, 12 µm/px; raster index → motor: 500 µm steps,
        // x along −stage-y, y along +stage-x (90° and a mirror), motor = stage − t.
        let a = 30f64.to_radians();
        let image_to_stage: Affine = [12.0 * a.cos(), -12.0 * a.sin(), -30_000.0, 12.0 * a.sin(), 12.0 * a.cos(), 10_000.0];
        let t = (54_000.8, -45_642.5);
        let index_to_stage: Affine = [0.0, 500.0, 7_000.0, -500.0, 0.0, 22_000.0];
        let index_to_motor: Affine = [0.0, 500.0, 7_000.0 - t.0, -500.0, 0.0, 22_000.0 - t.1];
        let stage_to_image = invert(&image_to_stage).unwrap();
        // Three regions: index rectangles, each area the rectangle's stage box grown by 0.4 step,
        // as image-pixel polygons; the reference point: lattice node (3, 2), not acquired.
        let regions = [((20i64, 40i64), (10i64, 18i64)), ((45, 50), (12, 30)), ((20, 30), (25, 29))];
        let mut areas = String::new();
        let mut rows = Vec::new();
        for (r, &((x0, x1), (y0, y1))) in regions.iter().enumerate() {
            let corners = [(x0 as f64 - 0.4, y0 as f64 - 0.4), (x1 as f64 + 0.4, y0 as f64 - 0.4), (x1 as f64 + 0.4, y1 as f64 + 0.4), (x0 as f64 - 0.4, y1 as f64 + 0.4)];
            areas.push_str(&format!("<Area Type=\"3\" Name=\"r{r}\"><Raster>500,500</Raster>"));
            for c in corners {
                let (px, py) = apply(&stage_to_image, apply(&index_to_stage, c));
                areas.push_str(&format!("<Point>{px:.3},{py:.3}</Point>"));
            }
            areas.push_str("</Area>");
            for x in x0..=x1 {
                for y in y0..=y1 {
                    let (mx, my) = apply(&index_to_motor, (x as f64, y as f64));
                    rows.push((r as i64, x, y, mx, my, String::new()));
                }
            }
        }
        let teach: Vec<String> = [(100.0, 200.0), (3000.0, 150.0), (900.0, 2500.0)]
            .iter()
            .map(|&p| {
                let (sx, sy) = apply(&image_to_stage, p);
                format!("<TeachPoint>{},{};{sx:.4},{sy:.4}</TeachPoint>", p.0, p.1)
            })
            .collect();
        let (rx, ry) = apply(&stage_to_image, apply(&index_to_stage, (3.0, 2.0)));
        let xml = format!("<ImagingSequence>{}<ReferencePoint>{rx:.6},{ry:.6}</ReferencePoint>{areas}</ImagingSequence>", teach.join(""));
        let c = Connection::open_in_memory().unwrap();
        let rows: Vec<(i64, i64, i64, f64, f64, &str)> = rows.iter().map(|r| (r.0, r.1, r.2, r.3, r.4, "")).collect();
        table(&c, &rows);
        let mut info = read(&c).unwrap();
        let mis = read_mis_from("turned.mis", xml.as_bytes()).unwrap();
        assert_eq!(info.mis_mismatch(&mis), None);
        info.mis = Some(mis.clone());
        let reg = register(&info, &mis).unwrap();
        assert_eq!(reg.reference_index, (3, 2));
        assert!(reg.lattice_residual_um < 1e-6 && reg.teach_residual_um < 1e-3, "{reg:?}");
        assert!((reg.stage_minus_motor.0 - t.0).abs() < 1e-3 && (reg.stage_minus_motor.1 - t.1).abs() < 1e-3, "{:?}", reg.stage_minus_motor);
        let mut truth = compose(&invert(&index_to_stage).unwrap(), &image_to_stage);
        truth[2] -= (info.min.0 - 1) as f64;
        truth[5] -= (info.min.1 - 1) as f64;
        for p in [(0.0, 0.0), (4000.0, 0.0), (0.0, 3000.0), (4000.0, 3000.0)] {
            let (ax, ay) = apply(&reg.matrix, p);
            let (tx, ty) = apply(&truth, p);
            assert!((ax - tx).abs() < 1e-6 && (ay - ty).abs() < 1e-6, "at {p:?}: {:?} vs {:?}", (ax, ay), (tx, ty));
        }
        // The acquired spot (20, 10) is MS pixel (1, 1).
        let (px, py) = apply(&stage_to_image, apply(&index_to_stage, (20.0, 10.0)));
        let (x, y) = apply(&reg.matrix, (px, py));
        assert!((x - 1.0).abs() < 1e-6 && (y - 1.0).abs() < 1e-6, "{x} {y}");
    }

    /// What stops a registration, each with its reason: fewer than three teach points, collinear
    /// ones, frames without motor positions, areas without an outline, and spots that no lattice
    /// placement through the reference point puts inside their areas.
    #[test]
    fn what_stops_a_registration_says_why() {
        let (info, mis) = info_of(&TDF);
        let with = |xml: &str| read_mis_from("run.mis", xml.as_bytes()).unwrap();
        let two = with(&TDF.mis.replacen("<TeachPoint>2550,4990;-11848,-18292</TeachPoint>\r\n", "", 1));
        assert_eq!(two.teach.len(), 2);
        assert!(register(&info, &two).unwrap_err().contains("2 teach point(s)"));
        let collinear = with(&TDF.mis.replacen("2550,4990;-11848,-18292", "12208,518;66767,18395", 1));
        assert!(register(&info, &collinear).unwrap_err().contains("collinear"));
        let mut no_motor = info.clone();
        no_motor.spots.values_mut().for_each(|s| s.motor = None);
        assert!(register(&no_motor, &mis).unwrap_err().contains("0 frames state a motor position"));
        let mut no_outline = mis.clone();
        no_outline.areas.iter_mut().for_each(|a| a.points.clear());
        assert!(register(&info, &no_outline).unwrap_err().contains("no acquired spot lies in a region whose <Area> outline is known"));
        // Every area one rectangle over the whole image: dozens of lattice placements keep every
        // spot inside it, so none is the registration.
        let mut giant = mis.clone();
        giant.areas.iter_mut().for_each(|a| a.points = vec![(0.0, 0.0), (8063.0, 0.0), (8063.0, 6047.0), (0.0, 6047.0)]);
        let err = register(&info, &giant).unwrap_err();
        assert!(err.contains("placements of the raster lattice") && err.contains("ambiguous"), "{err}");
        // Area 0 drawn 16 mm to the right of its spots: no placement puts region 0 and the others
        // inside their areas at once (the .mis check rejects such a sequence as well).
        let mut moved = mis.clone();
        moved.areas[0].points.iter_mut().for_each(|p| p.0 += 2000.0);
        assert!(info.mis_mismatch(&moved).is_some());
        let err = register(&info, &moved).unwrap_err();
        assert!(err.contains("no placement of the raster lattice"), "{err}");
        // A uniform shift of every motor position is no inconsistency: the lattice fit absorbs it
        // and the matrix is the same (to rounding: the fit's sums run in another order on x86-64
        // than on arm64, 1e-15 relative).
        let mut shifted = info.clone();
        shifted.spots.values_mut().for_each(|s| s.motor = s.motor.map(|(x, y)| (x + 500.0, y - 250.0)));
        let (a, b) = (register(&shifted, &mis).unwrap().matrix, register(&info, &mis).unwrap().matrix);
        assert!(a.iter().zip(b.iter()).all(|(x, y)| (x - y).abs() <= 1e-9 * (1.0 + x.abs())), "{a:?} vs {b:?}");
        // One region only, its spots a single row: the lattice needs two dimensions.
        let c = Connection::open_in_memory().unwrap();
        table(&c, &TDF.rows[..2]);
        let one_row = read(&c).unwrap();
        assert!(register(&one_row, &mis).unwrap_err().contains("three on more than one line"));
    }

    #[test]
    fn affine_helpers_round_trip() {
        let m: Affine = [2.0, 0.5, 3.0, -0.25, 1.5, -7.0];
        let inv = invert(&m).unwrap();
        let p = (3.7, -2.2);
        let (x, y) = apply(&inv, apply(&m, p));
        assert!((x - p.0).abs() < 1e-12 && (y - p.1).abs() < 1e-12);
        let id = compose(&inv, &m);
        for (k, v) in [1.0, 0.0, 0.0, 0.0, 1.0, 0.0].iter().enumerate() {
            assert!((id[k] - v).abs() < 1e-12, "{id:?}");
        }
        assert_eq!(invert(&[1.0, 2.0, 0.0, 2.0, 4.0, 0.0]), None, "singular");
        // fit: an exact affine from four points comes back with residual 0; collinear sources fail.
        let pts = [(0.0, 0.0), (10.0, 0.0), (0.0, 10.0), (10.0, 10.0)];
        let pairs: Vec<_> = pts.iter().map(|&p| (p, apply(&m, p))).collect();
        let (f, r) = fit(&pairs).unwrap();
        assert!(r < 1e-9 && f.iter().zip(m.iter()).all(|(a, b)| (a - b).abs() < 1e-9), "{f:?} {r}");
        assert!(fit(&pairs[..2]).is_none());
        let line: Vec<_> = [(0.0, 0.0), (1.0, 1.0), (2.0, 2.0)].iter().map(|&p| (p, apply(&m, p))).collect();
        assert!(fit(&line).is_none());
        assert!(inside_or_near(&[(0.0, 0.0), (10.0, 0.0), (10.0, 10.0), (0.0, 10.0)], (5.0, 5.0), 0.0));
        assert!(!inside_or_near(&[(0.0, 0.0), (10.0, 0.0), (10.0, 10.0), (0.0, 10.0)], (10.5, 5.0), 0.0));
        assert!(inside_or_near(&[(0.0, 0.0), (10.0, 0.0), (10.0, 10.0), (0.0, 10.0)], (10.5, 5.0), 1.0), "within eps of an edge");
    }

    /// The real tables and sequences of MSV000088438: every spot of both runs within 0.001 MS px of
    /// flexImaging's spot list, and the auditors' matrices to 1e-7 relative.
    #[test]
    #[ignore = "needs MSV000088438 under ~/Claude/mzPeak/data/imaging-examples; run with --include-ignored"]
    fn msv000088438_real_runs_match_the_spot_lists() {
        let root = std::path::PathBuf::from(std::env::var("HOME").unwrap()).join("Claude/mzPeak/data/imaging-examples/MSV000088438");
        for (dir, db, run) in [("20210920_vc_rugose_1mMTCA_gordon", "analysis.tsf", &TSF), ("20210921_vc_rugose_tims_gordon", "analysis.tdf", &TDF)] {
            let base = root.join(dir);
            let conn = crate::vendor_sqlite::open(&base.join(format!("{dir}.d")).join(db)).unwrap();
            let mut info = read(&conn).unwrap();
            let mis = crate::bruker_maldi::read_mis(&base.join(format!("{dir}.mis"))).unwrap();
            assert_eq!(info.mis_mismatch(&mis), None);
            info.mis = Some(mis.clone());
            let reg = register(&info, &mis).unwrap();
            // Every spot: its stage position from the spot list, through the inverse teach map and
            // the matrix, against its pixel.
            let names: std::collections::HashMap<String, (f64, f64)> = std::fs::read_to_string(base.join(format!("{dir}_spot_list.txt")))
                .unwrap()
                .lines()
                .filter(|l| !l.starts_with('#'))
                .map(|l| {
                    let f: Vec<&str> = l.split_whitespace().collect();
                    (f[2].to_string(), (f[0].parse().unwrap(), f[1].parse().unwrap()))
                })
                .collect();
            let mut stmt = conn.prepare("SELECT XIndexPos, YIndexPos, SpotName FROM MaldiFrameInfo").unwrap();
            let stage_to_image = invert(&reg.image_to_stage).unwrap();
            let mut worst = 0f64;
            let mut n = 0;
            for row in stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?))).unwrap() {
                let (x, y, name) = row.unwrap();
                let (px, py) = apply(&reg.matrix, apply(&stage_to_image, names[&name]));
                worst = worst.max((px - (x - info.min.0 + 1) as f64).abs()).max((py - (y - info.min.1 + 1) as f64).abs());
                n += 1;
            }
            assert!(n >= 240 && worst < 0.0011, "{dir}: {n} spots, worst {worst} MS px");
            for (k, (a, e)) in reg.matrix.iter().zip(run.expect.iter()).enumerate() {
                assert!((a - e).abs() <= 1e-7 * e.abs().max(1e-3), "{dir} [{k}]: {a} vs {e}");
            }
        }
    }
}

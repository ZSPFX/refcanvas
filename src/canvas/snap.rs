//! snapping while dragging: align to other nodes' edges/centers, with guides.

use bevy::prelude::*;

/// snap distance while dragging, logical px.
pub const THRESHOLD_PX: f32 = 6.0;

/// one axis alignment: the guide line plus every box edge/center on it
/// (dragged box first, then each match).
#[derive(Clone, Debug)]
pub struct SnapGuide {
    /// world coordinate of the line: x for a vertical guide, y for horizontal.
    pub line: f32,
    /// perpendicular extents `(min, max)` of the boxes involved: dragged box
    /// first (widens the line only), then matches (× at both ends).
    pub spans: Vec<(f32, f32)>,
}

impl SnapGuide {
    /// combined min/max of every span: the drawn length of the guide.
    pub fn extent(&self) -> (f32, f32) {
        self.spans
            .iter()
            .fold((f32::INFINITY, f32::NEG_INFINITY), |(lo, hi), (a, b)| {
                (lo.min(*a), hi.max(*b))
            })
    }
}

pub struct Snap {
    /// adjusted delta.
    pub delta: Vec2,
    /// vertical guide (x-axis alignment), if snapped.
    pub guide_x: Option<SnapGuide>,
    /// horizontal guide (y-axis alignment), if snapped.
    pub guide_y: Option<SnapGuide>,
}

impl Snap {
    pub fn none(delta: Vec2) -> Self {
        Self {
            delta,
            guide_x: None,
            guide_y: None,
        }
    }
}

/// snap the dragged box (moved by `raw_delta`) to candidate edges/centers;
/// threshold is in world units.
pub fn snap_delta(dragged: Rect, candidates: &[Rect], raw_delta: Vec2, threshold: f32) -> Snap {
    let moved = Rect::from_corners(dragged.min + raw_delta, dragged.max + raw_delta);

    let x = best_adjust(candidates, x_features(moved), true, threshold);
    let y = best_adjust(candidates, y_features(moved), false, threshold);

    let mut delta = raw_delta;
    if let Some((adjust, _)) = x {
        delta.x += adjust;
    }
    if let Some((adjust, _)) = y {
        delta.y += adjust;
    }

    // guides come from the final resting place, after both adjustments.
    let final_rect = Rect::from_corners(dragged.min + delta, dragged.max + delta);
    let eps = line_eps(threshold);
    let guide_x =
        x.map(|(_, line)| guide_on_line(candidates, line, true, y_spans(final_rect), eps));
    let guide_y =
        y.map(|(_, line)| guide_on_line(candidates, line, false, x_spans(final_rect), eps));

    Snap {
        delta,
        guide_x,
        guide_y,
    }
}

/// one axis of a scale snap: the moving box line against candidate features.
pub struct ScaleAxisSnap {
    /// closest target distance (world units), arbitrates when a uniform scale
    /// can only honour one axis.
    pub dist: f32,
    /// delta to add to the scale factor.
    pub adjust: f32,
    /// world coordinate of the line the moving edge lands on.
    pub line: f32,
}

/// snap one axis of a scale drag. `movers` are `(current, unscaled)` coords of
/// the moving lines; features pinned to `anchor` don't move under the scale.
pub fn snap_scale_axis(
    candidates: &[Rect],
    movers: impl Iterator<Item = (f32, f32)>,
    anchor: f32,
    vertical: bool,
    threshold: f32,
) -> Option<ScaleAxisSnap> {
    let features = if vertical { x_features } else { y_features };
    let mut best: Option<ScaleAxisSnap> = None;
    for (current, unscaled) in movers {
        let den = unscaled - anchor;
        if den.abs() < 1e-6 {
            continue; // pinned to the anchor: scaling can't move it.
        }
        for rect in candidates {
            for target in features(*rect) {
                let a = target - current;
                if a.abs() <= threshold && best.as_ref().map_or(true, |b| a.abs() < b.dist) {
                    best = Some(ScaleAxisSnap {
                        dist: a.abs(),
                        adjust: a / den,
                        line: target,
                    });
                }
            }
        }
    }
    best
}

/// guide for a line an edge/center landed on: the dragged box's perpendicular
/// span plus every candidate on it.
pub fn guide_on_line(
    candidates: &[Rect],
    line: f32,
    vertical: bool,
    dragged_span: (f32, f32),
    eps: f32,
) -> SnapGuide {
    let (features, spans): (fn(Rect) -> [f32; 3], fn(Rect) -> (f32, f32)) = if vertical {
        (x_features, y_spans)
    } else {
        (y_features, x_spans)
    };
    let mut out = vec![dragged_span];
    for rect in candidates {
        if features(*rect).iter().any(|f| (f - line).abs() <= eps) {
            out.push(spans(*rect));
        }
    }
    SnapGuide { line, spans: out }
}

/// only boxes numerically on the line get a tick; eps is a fraction of the
/// threshold, so a tick can't sit visibly off.
pub fn line_eps(threshold: f32) -> f32 {
    (threshold * 0.1).max(1e-4)
}

/// closest dragged↔candidate edge/center pair within `threshold`:
/// `(adjustment, line coordinate)`.
fn best_adjust(
    candidates: &[Rect],
    dragged: [f32; 3],
    vertical: bool,
    threshold: f32,
) -> Option<(f32, f32)> {
    let features = if vertical { x_features } else { y_features };
    let mut best: Option<(f32, f32, f32)> = None;
    for rect in candidates {
        for other in features(*rect) {
            for line in dragged {
                let d = other - line;
                if d.abs() <= threshold && best.map_or(true, |(bd, _, _)| d.abs() < bd) {
                    best = Some((d.abs(), d, other));
                }
            }
        }
    }
    best.map(|(_, adjust, line)| (adjust, line))
}

fn x_features(r: Rect) -> [f32; 3] {
    [r.min.x, r.center().x, r.max.x]
}

fn y_features(r: Rect) -> [f32; 3] {
    [r.min.y, r.center().y, r.max.y]
}

fn x_spans(r: Rect) -> (f32, f32) {
    (r.min.x, r.max.x)
}

fn y_spans(r: Rect) -> (f32, f32) {
    (r.min.y, r.max.y)
}

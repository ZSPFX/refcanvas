//! Snapping while dragging: align to other nodes' edges/centers, with guides.

use bevy::prelude::*;

/// Snap distance while dragging, logical pixels.
pub const THRESHOLD_PX: f32 = 6.0;

/// One axis alignment: the guide line plus every box whose edge/center lies
/// on it (the dragged box first, then each matched candidate).
#[derive(Clone, Debug)]
pub struct SnapGuide {
    /// World coordinate of the line: x for a vertical guide, y for horizontal.
    pub line: f32,
    /// Perpendicular extents `(min, max)` of the boxes involved: the dragged
    /// box first (it only widens the line, no marker is drawn on it), then
    /// every matched node (marked with an × at both ends).
    pub spans: Vec<(f32, f32)>,
}

impl SnapGuide {
    /// Combined min/max of every span, the drawn length of the guide.
    pub fn extent(&self) -> (f32, f32) {
        self.spans
            .iter()
            .fold((f32::INFINITY, f32::NEG_INFINITY), |(lo, hi), (a, b)| {
                (lo.min(*a), hi.max(*b))
            })
    }
}

pub struct Snap {
    /// Adjusted delta.
    pub delta: Vec2,
    /// Vertical guide (x-axis alignment), if snapped.
    pub guide_x: Option<SnapGuide>,
    /// Horizontal guide (y-axis alignment), if snapped.
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

/// Snap the dragged bounding box (moved by `raw_delta`) to the edges/centers
/// of the candidate rects. Threshold is in world units.
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

    // Guides are built from the final resting place, so the line frames the
    // boxes after both adjustments.
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

/// One axis of a scale snap: the moving box line against candidate features.
pub struct ScaleAxisSnap {
    /// Closest target distance (world units), arbitrates between the axes
    /// when a uniform scale can only honour one adjustment.
    pub dist: f32,
    /// Delta to add to the scale factor.
    pub adjust: f32,
    /// World coordinate of the line the moving edge lands on.
    pub line: f32,
}

/// Snap one axis of a scale drag. `movers` are the moving lines of the box
/// as `(current, unscaled)`: world coordinate now and the same feature's
/// coordinate before scaling. Features pinned to the `anchor` don't move
/// under the scale factor and are ignored.
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
            continue; // Pinned to the anchor: scaling cannot move it.
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

/// Build the guide for a line that an edge/center landed on: the dragged
/// box's perpendicular span plus every candidate whose feature lies on it.
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

/// Only boxes whose edge is numerically on the line get a tick; the epsilon
/// is a fraction of the snap threshold, so a tick can never sit visibly off.
pub fn line_eps(threshold: f32) -> f32 {
    (threshold * 0.1).max(1e-4)
}

/// The closest dragged↔candidate edge/center pair within `threshold`:
/// `(adjustment to apply, coordinate of the line it lands on)`.
fn best_adjust(
    candidates: &[Rect],
    dragged: [f32; 3],
    vertical: bool,
    threshold: f32,
) -> Option<(f32, f32)> {
    let features = if vertical { x_features } else { y_features };
    let mut best: Option<(f32, f32, f32)> = None; // (|dist|, adjust, line)
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

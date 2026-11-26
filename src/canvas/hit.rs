//! hit-testing for nodes and selection handles (pure math, screen-space aware).

use bevy::prelude::*;

use super::node::Pose;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HitKind {
    /// scale handle at corner `i` (0=TL, 1=TR, 2=BR, 3=BL in local space).
    Corner(usize),
    /// scale handle at edge `i` (0=top, 1=right, 2=bottom, 3=left).
    Edge(usize),
    /// rotation zone just outside a corner.
    Rotate,
}

/// handle hit radius, logical px.
pub const HANDLE_HIT_PX: f32 = 9.0;
/// rotation zone radius beyond a corner, logical px.
pub const ROTATE_ZONE_PX: f32 = 26.0;
/// touch targets need a much larger radius than a mouse cursor.
pub const TOUCH_HANDLE_HIT_PX: f32 = 22.0;
/// rotation zone for touch, logical px.
pub const TOUCH_ROTATE_ZONE_PX: f32 = 36.0;
/// finger slop around a node when picking, logical px.
pub const TOUCH_NODE_SLOP_PX: f32 = 8.0;

/// corner directions, local space (y up): TL, TR, BR, BL.
pub const CORNER_DIRS: [Vec2; 4] = [
    Vec2::new(-1.0, 1.0),
    Vec2::new(1.0, 1.0),
    Vec2::new(1.0, -1.0),
    Vec2::new(-1.0, -1.0),
];

/// edge outward directions: top, right, bottom, left.
pub const EDGE_DIRS: [Vec2; 4] = [
    Vec2::new(0.0, 1.0),
    Vec2::new(1.0, 0.0),
    Vec2::new(0.0, -1.0),
    Vec2::new(-1.0, 0.0),
];

pub fn rot2(radians: f32) -> Rot2 {
    Rot2::radians(radians)
}

pub fn corner_world(pose: &Pose, i: usize) -> Vec2 {
    let d = CORNER_DIRS[i];
    pose.center + rot2(pose.rot) * (d * pose.size * 0.5)
}

pub fn edge_world(pose: &Pose, i: usize) -> Vec2 {
    let d = EDGE_DIRS[i];
    pose.center + rot2(pose.rot) * (d * pose.size * 0.5)
}

/// is `world` inside the (possibly rotated) node?
pub fn point_inside(world: Vec2, pose: &Pose) -> bool {
    point_inside_with_slop(world, pose, 0.0)
}

/// [`point_inside`] with a world-space slop around the node bounds.
pub fn point_inside_with_slop(world: Vec2, pose: &Pose, slop: f32) -> bool {
    let local = rot2(-pose.rot) * (world - pose.center);
    local.x.abs() <= pose.size.x * 0.5 + slop && local.y.abs() <= pose.size.y * 0.5 + slop
}

/// hit test the handles / rotation zones of a selection frame; `wpp` is world
/// units per logical px.
pub fn hit_frame(world: Vec2, frame: &Pose, wpp: f32) -> Option<HitKind> {
    hit_frame_with_radii(world, frame, wpp, HANDLE_HIT_PX, ROTATE_ZONE_PX)
}

/// touch radii for `frame`, logical px; the enlarged target shrinks on small
/// frames so it can't swallow the node body.
pub fn touch_radii(frame: &Pose, wpp: f32) -> (f32, f32) {
    let min_dim = (frame.size.min_element() / wpp.max(1e-6)).max(1.0);
    let handle = TOUCH_HANDLE_HIT_PX.min(min_dim * 0.28).max(6.0);
    (handle, TOUCH_ROTATE_ZONE_PX.min(handle + 14.0))
}

/// [`hit_frame`] with explicit handle and rotation radii, logical px.
pub fn hit_frame_with_radii(
    world: Vec2,
    frame: &Pose,
    wpp: f32,
    handle_px: f32,
    rotate_px: f32,
) -> Option<HitKind> {
    let handle_r = handle_px * wpp;
    let rotate_r = rotate_px * wpp;

    let mut best: Option<(f32, HitKind)> = None;
    let consider = |d: f32, kind: HitKind, best: &mut Option<(f32, HitKind)>| {
        if d <= handle_r && best.map_or(true, |(bd, _)| d < bd) {
            *best = Some((d, kind));
        }
    };
    for i in 0..4 {
        consider(world.distance(corner_world(frame, i)), HitKind::Corner(i), &mut best);
    }
    for i in 0..4 {
        consider(world.distance(edge_world(frame, i)), HitKind::Edge(i), &mut best);
    }
    if best.is_some() {
        return best.map(|(_, k)| k);
    }

    // rotation: just outside a corner.
    if !point_inside(world, frame) {
        for i in 0..4 {
            if world.distance(corner_world(frame, i)) <= rotate_r {
                return Some(HitKind::Rotate);
            }
        }
    }
    None
}

/// rects (min, max) in world space for a rotated node.
pub fn aabb_of(pose: &Pose) -> Rect {
    let mut min = Vec2::splat(f32::INFINITY);
    let mut max = Vec2::splat(f32::NEG_INFINITY);
    for i in 0..4 {
        let c = corner_world(pose, i);
        min = min.min(c);
        max = max.max(c);
    }
    Rect::from_corners(min, max)
}

pub fn rects_overlap(a: Rect, b: Rect) -> bool {
    a.min.x <= b.max.x && b.min.x <= a.max.x && a.min.y <= b.max.y && b.min.y <= a.max.y
}

/// sat test against a rotated node; unlike aabb overlap, corner gaps don't select.
pub fn rect_overlaps_pose(rect: Rect, pose: &Pose) -> bool {
    let node = [
        corner_world(pose, 0),
        corner_world(pose, 1),
        corner_world(pose, 2),
        corner_world(pose, 3),
    ];
    let boxx = [
        rect.min,
        Vec2::new(rect.max.x, rect.min.y),
        rect.max,
        Vec2::new(rect.min.x, rect.max.y),
    ];
    let axes = [
        Vec2::X,
        Vec2::Y,
        rot2(pose.rot) * Vec2::X,
        rot2(pose.rot) * Vec2::Y,
    ];
    for axis in axes {
        let (n_min, n_max) = project(&node, axis);
        let (r_min, r_max) = project(&boxx, axis);
        if n_max < r_min || r_max < n_min {
            return false;
        }
    }
    true
}

fn project(points: &[Vec2; 4], axis: Vec2) -> (f32, f32) {
    let mut min = f32::INFINITY;
    let mut max = f32::NEG_INFINITY;
    for p in points {
        let d = p.dot(axis);
        min = min.min(d);
        max = max.max(d);
    }
    (min, max)
}

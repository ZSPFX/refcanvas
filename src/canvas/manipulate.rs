//! Node manipulation: move/scale/rotate math, spawning, clipboard, focus.

use bevy::math::DVec2;
use bevy::prelude::*;

use super::camera::View;
use super::hit::{CORNER_DIRS, EDGE_DIRS, HitKind, aabb_of, corner_world, edge_world, rot2};
use super::node::{CanvasNode, ImageData, NodeKind, NodeSnapshot, Pose};
use super::selection::{NodeQ, Selection};
use super::snap::{self, SnapGuide};

/// Smallest visible crop extent, in world px. Keeps a Ctrl-crop drag from
/// collapsing a node to nothing.
const MIN_CROP_WORLD: f32 = 8.0;

/// Monotonic stacking counter. Nodes get `z` from this when raised/spawned.
#[derive(Resource)]
pub struct ZCounter(pub f32);

impl Default for ZCounter {
    fn default() -> Self {
        Self(1.0)
    }
}

impl ZCounter {
    pub fn next(&mut self) -> f32 {
        self.0 += 1.0;
        self.0
    }
}

pub fn current_pose(tf: &Transform, node: &CanvasNode) -> Pose {
    Pose {
        center: tf.translation.truncate(),
        size: node.size,
        rot: node.rotation,
    }
}

pub fn set_pose(
    tf: &mut Transform,
    sprite: Option<&mut Sprite>,
    node: &mut CanvasNode,
    pose: &Pose,
) {
    tf.translation.x = pose.center.x;
    tf.translation.y = pose.center.y;
    tf.rotation = Quat::from_rotation_z(pose.rot);
    node.size = pose.size;
    node.rotation = pose.rot;
    if let Some(s) = sprite {
        s.custom_size = Some(pose.size);
        // Image nodes: keep the sprite's texture rect in sync with the crop
        // (in the pixel space of the texture tier the sprite shows).
        if let Some(img) = node.image() {
            s.rect = Some(img.active_rect());
        }
    }
}

/// A frame that fits a set of poses: the node's own pose for a single node,
/// or the axis-aligned bounding box for several.
pub fn selection_frame(poses: &[Pose]) -> Option<Pose> {
    match poses {
        [] => None,
        [single] => Some(*single),
        many => {
            let mut r = aabb_of(&many[0]);
            for p in &many[1..] {
                r = r.union(aabb_of(p));
            }
            Some(Pose {
                center: r.center(),
                size: r.size(),
                rot: 0.0,
            })
        }
    }
}

pub fn union_aabb(poses: &[Pose]) -> Option<Rect> {
    let mut iter = poses.iter();
    let first = iter.next()?;
    let mut r = aabb_of(first);
    for p in iter {
        r = r.union(aabb_of(p));
    }
    Some(r)
}

pub fn spawn_image_node(
    commands: &mut Commands,
    zc: &mut ZCounter,
    tab_id: u64,
    visible: bool,
    data: ImageData,
    pose: Pose,
) -> (Entity, f32) {
    let z = zc.next();
    (spawn_image_node_with_z(commands, data, pose, tab_id, visible, z), z)
}

pub fn spawn_image_node_with_z(
    commands: &mut Commands,
    data: ImageData,
    pose: Pose,
    tab_id: u64,
    visible: bool,
    z: f32,
) -> Entity {
    let preview = data.preview.clone();
    // Preview tier: scale the crop into the preview texture's pixel space
    // (see `ImageData::tier_rect_of`).
    let rect = Some(data.tier_rect(false));
    commands
        .spawn((
            CanvasNode {
                db_id: None,
                kind: NodeKind::Image(super::node::ImageNode {
                    data,
                    full: None,
                    full_incoming: None,
                    sampler_nearest: false,
                    pending_cpu_free: None,
                    resident_bytes: 0,
                    last_needed: 0.0,
                    sprite_material: None,
                }),
                size: pose.size,
                rotation: pose.rot,
            },
            Sprite {
                image: preview,
                custom_size: Some(pose.size),
                rect,
                ..default()
            },
            Transform {
                translation: Vec3::new(pose.center.x, pose.center.y, z),
                rotation: Quat::from_rotation_z(pose.rot),
                ..default()
            },
            super::node::TabOwner(tab_id),
            if visible {
                Visibility::Inherited
            } else {
                Visibility::Hidden
            },
        ))
        .id()
}

// ---------------------------------------------------------------------------
// Move
// ---------------------------------------------------------------------------

pub fn apply_move(
    snapshots: &[(Entity, NodeSnapshot)],
    delta: Vec2,
    axis_lock: bool,
    q: &mut NodeQ,
) {
    let d = if axis_lock {
        if delta.x.abs() >= delta.y.abs() {
            Vec2::new(delta.x, 0.0)
        } else {
            Vec2::new(0.0, delta.y)
        }
    } else {
        delta
    };
    for (e, snap) in snapshots {
        let Ok((_, mut tf, mut sprite, mut node)) = q.get_mut(*e) else {
            continue;
        };
        let pose = Pose {
            center: snap.pose.center + d,
            ..snap.pose
        };
        set_pose(&mut tf, Some(&mut sprite), &mut node, &pose);
    }
}

// ---------------------------------------------------------------------------
// Scale
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct ScaleDrag {
    /// Selection frame at drag start.
    pub frame: Pose,
    pub handle: HitKind,
    /// Anchor (opposite corner/edge, or the center with Alt held).
    pub anchor: Vec2,
    /// This drag may crop with Ctrl (a single image node). The mode is read
    /// live from the keyboard, so Ctrl can be pressed or released mid-drag.
    pub crop_eligible: bool,
    pub snapshots: Vec<(Entity, NodeSnapshot)>,
    /// Live alignment guides while resizing (see [`super::snap`]).
    pub guide_x: Option<SnapGuide>,
    pub guide_y: Option<SnapGuide>,
}

pub fn begin_scale(
    frame: &Pose,
    handle: HitKind,
    from_center: bool,
    crop_eligible: bool,
    snapshots: Vec<(Entity, NodeSnapshot)>,
) -> Option<ScaleDrag> {
    // Alt (resize from the center) only applies to plain resizes; a crop
    // always anchors the opposite edge. Callers fold that into `from_center`.
    let anchor = if from_center {
        frame.center
    } else {
        match handle {
            HitKind::Corner(i) => corner_world(frame, (i + 2) % 4),
            HitKind::Edge(i) => edge_world(frame, (i + 2) % 4),
            _ => return None,
        }
    };
    Some(ScaleDrag {
        frame: *frame,
        handle,
        anchor,
        crop_eligible,
        snapshots,
        guide_x: None,
        guide_y: None,
    })
}

/// Free (or aspect-locked) scale from a handle drag. Multi-selections are
/// forced uniform so rotated children never skew. Moving edges/centers snap
/// to nearby nodes (`candidates`) like a move does, with the same guides.
#[allow(clippy::too_many_arguments)]
pub fn apply_scale(
    drag: &mut ScaleDrag,
    cursor_world: Vec2,
    uniform: bool,
    from_center: bool,
    min_world: f32,
    wpp: f32,
    candidates: &[Rect],
    q: &mut NodeQ,
) {
    drag.guide_x = None;
    drag.guide_y = None;

    let frame = drag.frame;
    // Alt resizes around the box centre; otherwise around the anchor picked
    // at press (the opposite corner/edge). The pivot decides BOTH the factor
    // math and where the transform is applied, so they can't disagree.
    let pivot = if from_center {
        frame.center
    } else {
        drag.anchor
    };
    let local = rot2(-frame.rot) * (cursor_world - pivot);
    let half = if from_center { 0.5 } else { 1.0 };

    let (mut sx, mut sy) = (1.0f32, 1.0f32);
    let (moves_x, moves_y);
    match drag.handle {
        HitKind::Corner(i) => {
            let d = CORNER_DIRS[i];
            sx = (local.x * d.x) / (frame.size.x * half).max(1e-3);
            sy = (local.y * d.y) / (frame.size.y * half).max(1e-3);
            moves_x = true;
            moves_y = true;
        }
        HitKind::Edge(i) => {
            let d = EDGE_DIRS[i];
            if d.x != 0.0 {
                sx = (local.x * d.x) / (frame.size.x * half).max(1e-3);
                moves_x = true;
                moves_y = false;
            } else {
                sy = (local.y * d.y) / (frame.size.y * half).max(1e-3);
                moves_x = false;
                moves_y = true;
            }
        }
        _ => return,
    }

    // Multi-selections scale uniformly; single nodes may resize freely
    // (images still never stretch; see the cover math below).
    let force_uniform = uniform || drag.snapshots.len() > 1;
    if force_uniform {
        let s = sx.max(sy);
        sx = s;
        sy = s;
    }
    // Stay above the minimum size, and don't invert (for now).
    let min_sx = min_world / frame.size.x.max(1e-3);
    let min_sy = min_world / frame.size.y.max(1e-3);
    sx = sx.max(min_sx).max(0.001);
    sy = sy.max(min_sy).max(0.001);

    // Snap the moving edges/centers to nearby nodes. Axis-aligned frames
    // only: on a rotated frame the scaled features are not the box's world
    // edges, and the world-axis guides/ticks wouldn't line up.
    if frame.rot == 0.0 && !candidates.is_empty() {
        let box_min = frame.center - frame.size * 0.5;
        let box_max = frame.center + frame.size * 0.5;
        // Feature position after scaling `f0` (unscaled) around the pivot.
        let line = |f0: f32, s: f32, a: f32| a + (f0 - a) * s;
        let movers = |f0_min: f32, f0_mid: f32, f0_max: f32, s: f32, a: f32| {
            [
                (line(f0_min, s, a), f0_min),
                (line(f0_mid, s, a), f0_mid),
                (line(f0_max, s, a), f0_max),
            ]
        };
        let threshold = snap::THRESHOLD_PX * wpp;
        let snap_x = if moves_x {
            snap::snap_scale_axis(
                candidates,
                movers(box_min.x, frame.center.x, box_max.x, sx, pivot.x).into_iter(),
                pivot.x,
                true,
                threshold,
            )
        } else {
            None
        };
        let snap_y = if moves_y {
            snap::snap_scale_axis(
                candidates,
                movers(box_min.y, frame.center.y, box_max.y, sy, pivot.y).into_iter(),
                pivot.y,
                false,
                threshold,
            )
        } else {
            None
        };

        let mut line_x = None;
        let mut line_y = None;
        if force_uniform {
            // A uniform scale can honour only one axis adjustment: take the
            // closer of the two (ties go to x).
            let chosen = match (snap_x, snap_y) {
                (Some(x), Some(y)) if y.dist < x.dist => Some((false, y)),
                (Some(x), _) => Some((true, x)),
                (None, Some(y)) => Some((false, y)),
                (None, None) => None,
            };
            if let Some((vertical, s)) = chosen {
                sx += s.adjust;
                sy += s.adjust;
                if vertical {
                    line_x = Some(s.line);
                } else {
                    line_y = Some(s.line);
                }
            }
        } else {
            if let Some(x) = snap_x {
                sx += x.adjust;
                line_x = Some(x.line);
            }
            if let Some(y) = snap_y {
                sy += y.adjust;
                line_y = Some(y.line);
            }
        }

        // Guides span the boxes as they end up after both adjustments.
        let fmin = Vec2::new(line(box_min.x, sx, pivot.x), line(box_min.y, sy, pivot.y));
        let fmax = Vec2::new(line(box_max.x, sx, pivot.x), line(box_max.y, sy, pivot.y));
        let eps = snap::line_eps(threshold);
        if let Some(l) = line_x {
            drag.guide_x = Some(snap::guide_on_line(
                candidates,
                l,
                true,
                (fmin.y, fmax.y),
                eps,
            ));
        }
        if let Some(l) = line_y {
            drag.guide_y = Some(snap::guide_on_line(
                candidates,
                l,
                false,
                (fmin.x, fmax.x),
                eps,
            ));
        }
    }

    apply_scale_factors(pivot, frame.rot, sx, sy, &drag.snapshots, q);
}

fn apply_scale_factors(
    anchor: Vec2,
    frame_rot: f32,
    sx: f32,
    sy: f32,
    snapshots: &[(Entity, NodeSnapshot)],
    q: &mut NodeQ,
) {
    for (e, snap) in snapshots {
        let Ok((_, mut tf, mut sprite, mut node)) = q.get_mut(*e) else {
            continue;
        };
        let p0 = snap.pose;
        let rel_local = rot2(-frame_rot) * (p0.center - anchor);
        let scaled = Vec2::new(rel_local.x * sx, rel_local.y * sy);
        let new_center = anchor + rot2(frame_rot) * scaled;
        let new_size = Vec2::new((p0.size.x * sx).max(1.0), (p0.size.y * sy).max(1.0));

        // Images behave like Figma's image "Fill" (CSS `object-fit: cover`):
        // the handle resizes the container freely, and the picture rescales
        // uniformly to cover it. Growing the container grows the whole image
        // (or crops it, if the aspect ratio demands it), never stretched.
        let size = if let Some(img) = node.image_mut() {
            let natural = img.data.natural.as_vec2().max(Vec2::ONE);
            let px = (new_size.x / natural.x)
                .max(new_size.y / natural.y)
                .max(1e-4);
            let crop_w = (new_size.x / px).min(natural.x);
            let crop_h = (new_size.y / px).min(natural.y);
            // Keep the currently visible region centered where it is.
            let prev = snap.crop.center();
            let cx = prev.x.clamp(crop_w * 0.5, natural.x - crop_w * 0.5);
            let cy = prev.y.clamp(crop_h * 0.5, natural.y - crop_h * 0.5);
            img.data.crop =
                Rect::from_center_size(Vec2::new(cx, cy), Vec2::new(crop_w, crop_h));
            img.data.px_scale = px;
            img.data.crop.size() * px
        } else {
            new_size
        };

        let pose = Pose {
            center: new_center,
            size,
            rot: p0.rot,
        };
        set_pose(&mut tf, Some(&mut sprite), &mut node, &pose);
    }
}

/// Ctrl + handle drag: precise crop. The dragged edge(s) move through the
/// texture, clamped to the image; the opposite edge stays exactly anchored and
/// the picture never rescales. Requires a single image node.
pub fn apply_crop(
    drag: &mut ScaleDrag,
    cursor_world: Vec2,
    wpp: f32,
    q: &mut NodeQ,
) {
    // Cropping is edge-precise by nature; no snapping guides here.
    drag.guide_x = None;
    drag.guide_y = None;

    let frame = &drag.frame;
    let local = rot2(-frame.rot) * (cursor_world - frame.center);
    let half = frame.size * 0.5;

    // Which edges are being dragged (0 = not).
    let dir = match drag.handle {
        HitKind::Corner(i) => CORNER_DIRS[i],
        HitKind::Edge(i) => EDGE_DIRS[i],
        _ => return,
    };
    // Displacement of the dragged edge(s) in frame-local axes.
    let e = local - Vec2::new(dir.x * half.x, dir.y * half.y);

    for (entity, snap) in &drag.snapshots {
        let Ok((_, mut tf, mut sprite, mut node)) = q.get_mut(*entity) else {
            continue;
        };
        let Some(img) = node.image_mut() else {
            continue; // only image nodes crop
        };
        let px = snap.px_scale;
        let natural = img.data.natural.as_vec2().max(Vec2::ONE);
        // IMPORTANT: always compute from the crop at drag start (snapshot),
        // never the live crop, otherwise the drag compounds every frame.
        let start = snap.crop;
        let min_texels = (MIN_CROP_WORLD * wpp / px.max(1e-6)).max(1.0).min(64.0);

        let mut new_min = start.min;
        let mut new_max = start.max;

        // Left / right (texture x). Bounds are pre-clamped so an unusually
        // small crop (or huge zoom-out) can never produce min > max.
        let max_left = (start.max.x - min_texels).max(0.0);
        let min_right = (start.min.x + min_texels).min(natural.x);
        if dir.x < 0.0 {
            new_min.x = (start.min.x + e.x / px).clamp(0.0, max_left);
        } else if dir.x > 0.0 {
            new_max.x = (start.max.x + e.x / px).clamp(min_right, natural.x);
        }
        // Top / bottom (texture y is flipped vs world y).
        let max_top = (start.max.y - min_texels).max(0.0);
        let min_bottom = (start.min.y + min_texels).min(natural.y);
        if dir.y > 0.0 {
            // world top edge: reveals content with smaller texture y
            new_min.y = (start.min.y - e.y / px).clamp(0.0, max_top);
        } else if dir.y < 0.0 {
            new_max.y = (start.max.y - e.y / px).clamp(min_bottom, natural.y);
        }

        img.data.crop = Rect::from_corners(new_min, new_max);

        // Recompute size/center from the actual (clamped) crop so the
        // anchored edge stays exactly put.
        let new_size = Vec2::new(
            (new_max.x - new_min.x) * px,
            (new_max.y - new_min.y) * px,
        );
        let d_left = (new_min.x - start.min.x) * px;
        let d_right = (new_max.x - start.max.x) * px;
        let d_top = (start.min.y - new_min.y) * px;
        let d_bottom = -(new_max.y - start.max.y) * px;

        let u = rot2(frame.rot) * Vec2::X;
        let v = rot2(frame.rot) * Vec2::Y;
        let center = frame.center
            + u * ((d_left + d_right) * 0.5)
            + v * ((d_bottom + d_top) * 0.5);

        let pose = Pose {
            center,
            size: new_size,
            rot: snap.pose.rot,
        };
        set_pose(&mut tf, Some(&mut sprite), &mut node, &pose);
    }
}

// ---------------------------------------------------------------------------
// Rotate
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct RotateDrag {
    pub pivot: Vec2,
    pub start_angle: f32,
    pub snapshots: Vec<(Entity, NodeSnapshot)>,
}

pub fn start_rotate_around(pivot: Vec2, cursor_world: Vec2, snapshots: Vec<(Entity, NodeSnapshot)>) -> RotateDrag {
    RotateDrag {
        pivot,
        start_angle: (cursor_world - pivot).to_angle(),
        snapshots,
    }
}

pub fn apply_rotate(
    drag: &RotateDrag,
    cursor_world: Vec2,
    snap: bool,
    q: &mut NodeQ,
) {
    let angle = (cursor_world - drag.pivot).to_angle();
    let mut delta = angle - drag.start_angle;
    if snap {
        if drag.snapshots.len() == 1 {
            let base = drag.snapshots[0].1.pose.rot;
            delta = snap_angle(base + delta) - base;
        } else {
            delta = snap_angle(delta);
        }
    }
    for (e, snap) in &drag.snapshots {
        let Ok((_, mut tf, mut sprite, mut node)) = q.get_mut(*e) else {
            continue;
        };
        let p0 = snap.pose;
        let pose = Pose {
            center: drag.pivot + rot2(delta) * (p0.center - drag.pivot),
            size: p0.size,
            rot: p0.rot + delta,
        };
        set_pose(&mut tf, Some(&mut sprite), &mut node, &pose);
    }
}

fn snap_angle(a: f32) -> f32 {
    let step = std::f32::consts::PI / 12.0; // 15 degrees
    (a / step).round() * step
}

// ---------------------------------------------------------------------------
// Selection helpers
// ---------------------------------------------------------------------------

pub fn raise(entity: Entity, q: &mut NodeQ, zc: &mut ZCounter) {
    if let Ok((_, mut tf, _, _)) = q.get_mut(entity) {
        tf.translation.z = zc.next();
    }
}

pub fn delete_selection_undoable(
    selection: &mut Selection,
    undo: &mut super::undo::UndoStack,
    q: &NodeQ,
    tab_id: u64,
    commands: &mut Commands,
) {
    let records = super::undo::records_of(&selection.nodes, tab_id, q);
    for e in selection.nodes.drain(..) {
        commands.entity(e).despawn();
    }
    if !records.is_empty() {
        undo.push(super::undo::UndoEntry::Removed { items: records });
    }
}

pub fn select_all(
    q: &NodeQ,
    keep: impl Fn(Entity) -> bool,
    selection: &mut Selection,
) {
    selection.nodes = q.iter().map(|(e, ..)| e).filter(|e| keep(*e)).collect();
}

pub fn duplicate_selection(
    selection: &Selection,
    offset: Vec2,
    tab_id: u64,
    q: &NodeQ,
    commands: &mut Commands,
    zc: &mut ZCounter,
) -> Vec<super::undo::NodeRecord> {
    let mut out = Vec::new();
    for e in &selection.nodes {
        let Ok((_, tf, _, node)) = q.get(*e) else {
            continue;
        };
        let Some(img) = node.image() else {
            continue;
        };
        let mut pose = current_pose(tf, node);
        pose.center += offset;
        let z = zc.next();
        let entity = spawn_image_node_with_z(commands, img.data.clone(), pose, tab_id, true, z);
        out.push(super::undo::NodeRecord {
            entity,
            tab_id,
            data: super::undo::NodeData::Image(img.data.clone()),
            snap: NodeSnapshot {
                pose,
                z,
                flip_x: false,
                flip_y: false,
                crop: img.data.crop,
                px_scale: img.data.px_scale,
            },
        });
    }
    out
}

// ---------------------------------------------------------------------------
// Focus (F / A)
// ---------------------------------------------------------------------------

/// Extra space left around the focused content, as a multiple of its size.
/// The same rule applies whether one image or the whole project is framed, so
/// the gap to the viewport edge is always identical (≈8% on the fitting axis).
/// Deliberately a constant: the fit must not depend on the current zoom.
const FOCUS_MARGIN: f32 = 1.2;

/// Arrow-key hops frame the target with extra breathing room (≈2.5× its
/// size), so the neighbours you're likely to hop to next stay on screen.
/// `F` keeps the tight [`FOCUS_MARGIN`] framing.
pub const ARROW_FOCUS_MARGIN: f32 = 2.5;

/// Frame the given poses. May zoom in well past 100% (Maya-style `F`).
///
/// The zoom comes from the content size and the viewport only, a constant
/// margin, so it never depends on the current zoom level.
pub fn focus_poses(poses: &[Pose], viewport_logical: Vec2, view: &mut View) {
    focus_poses_with_margin(poses, viewport_logical, view, FOCUS_MARGIN);
}

/// [`focus_poses`] with a custom content-size multiplier (`margin`), used by
/// arrow-key navigation to leave more context around the target.
pub fn focus_poses_with_margin(
    poses: &[Pose],
    viewport_logical: Vec2,
    view: &mut View,
    margin: f32,
) {
    let Some(frame) = selection_frame(poses) else {
        return;
    };
    // Fit the rotated bounds, so a single rotated image can't overflow.
    let fit = aabb_of(&frame);
    let needed = fit.size().max(Vec2::splat(1.0)) * margin;
    // World units per logical pixel the view should end up with.
    let want = (needed.x / viewport_logical.x.max(1.0))
        .max(needed.y / viewport_logical.y.max(1.0))
        .max(1e-6) as f64;
    // `log_scale` is the natural log of world-units-per-logical-pixel (Bevy
    // sizes the orthographic projection from the logical viewport), so the fit
    // is exact, not a log-base guess.
    let log_new = want.ln();
    view.fly_to(
        DVec2::new(fit.center().x as f64, fit.center().y as f64),
        log_new,
    );
}

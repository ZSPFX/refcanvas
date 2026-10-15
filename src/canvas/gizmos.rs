//! Selection overlay: outline, handles, marquee (drawn with gizmos).

use bevy::prelude::*;

use super::camera::View;
use super::hit::{CORNER_DIRS, corner_world, edge_world, rot2};
use super::manipulate::{self, current_pose};
use super::node::{InactiveTab, CanvasNode, Pose, TabOwner};
use super::selection::{Interaction, Selection};
use super::snap::SnapGuide;
use crate::project::tabs::Tabs;
use crate::theme::Accent;

const HANDLE_FILL: Color = Color::srgb(0.96, 0.96, 0.96);
const GUIDE: Color = Color::srgba(1.0, 0.35, 0.65, 0.9);
/// Dim boundary drawn around the gray fill of texture-pending nodes.
const PENDING_LOAD: Color = Color::srgb(0.30, 0.30, 0.30);

/// Rotation-zone hints: a tiny rounded elbow (two very short edge lines
/// joined by a quarter arc, a CSS `border-top`/`border-right` with
/// `border-radius`) floating just outside each selection corner, where the
/// rotate grab zone lives. Logical px; scaled by `wpp` at draw time.
const ROTATE_HINT_GAP: f32 = 12.0;
const ROTATE_HINT_RADIUS: f32 = 9.0;
const ROTATE_HINT_TAIL: f32 = 5.0;

/// Marquee outline alpha (the hue follows the desktop accent; see
/// [`crate::theme::Accent`]).
const ACCENT_SOFT_ALPHA: f32 = 0.4;
/// Translucent fill for the drag-selection marquee. Gizmos can only stroke, so
/// the fill is a plain sprite kept above the canvas content (below the camera
/// at z = 1000). Tune alpha here.
const MARQUEE_FILL_ALPHA: f32 = 0.10;
/// Above the nodes (ZCounter grows from 1) and below the camera at z = 1000.
const MARQUEE_FILL_Z: f32 = 900.0;

#[derive(Component)]
pub struct MarqueeFill;

pub fn spawn_marquee_fill(mut commands: Commands, accent: Res<Accent>) {
    commands.spawn((
        Sprite::from_color(accent.alpha(MARQUEE_FILL_ALPHA), Vec2::ONE),
        Transform::from_xyz(0.0, 0.0, MARQUEE_FILL_Z),
        Visibility::Hidden,
        MarqueeFill,
    ));
}

pub fn update_marquee_fill(
    interaction: Res<Interaction>,
    accent: Res<Accent>,
    mut fill: Query<(&mut Transform, &mut Sprite, &mut Visibility), With<MarqueeFill>>,
) {
    let Ok((mut tf, mut sprite, mut vis)) = fill.single_mut() else {
        return;
    };
    // The desktop accent can change at runtime; keep the fill in sync.
    let want = accent.alpha(MARQUEE_FILL_ALPHA);
    if sprite.color != want {
        sprite.color = want;
    }
    match interaction.marquee_rect() {
        Some(rect) => {
            tf.translation.x = rect.center().x;
            tf.translation.y = rect.center().y;
            sprite.custom_size = Some(rect.size().max(Vec2::ONE));
            *vis = Visibility::Inherited;
        }
        None => {
            *vis = Visibility::Hidden;
        }
    }
}

pub fn configure_gizmos(mut store: ResMut<GizmoConfigStore>) {
    let (config, _) = store.config_mut::<DefaultGizmoConfigGroup>();
    config.line.width = 1.5;
}

pub fn draw_selection(
    mut gizmos: Gizmos,
    selection: Res<Selection>,
    interaction: Res<Interaction>,
    view: Res<View>,
    accent: Res<Accent>,
    q: Query<(&Transform, &CanvasNode), Without<InactiveTab>>,
) {
    let wpp = view.scale() as f32;
    let accent_color = accent.base();
    let poses: Vec<Pose> = selection
        .nodes
        .iter()
        .filter_map(|e| q.get(*e).ok().map(|(tf, n)| current_pose(tf, n)))
        .collect();

    if let Some(frame) = manipulate::selection_frame(&poses) {
        let iso = Isometry2d::new(frame.center, Rot2::radians(frame.rot));
        gizmos.rect_2d(iso, frame.size, accent_color);

        let hs = 6.0 * wpp;
        for i in 0..4 {
            handle_square(&mut gizmos, corner_world(&frame, i), hs, accent_color);
        }
        for i in 0..4 {
            handle_square(&mut gizmos, edge_world(&frame, i), hs, accent_color);
        }

        // Rotation hints: at each corner, two very short lines along the
        // offset edges of a phantom rounded rectangle, joined by a quarter
        // arc, the rotate zone sits right there (just outside the corner).
        let rot = rot2(frame.rot);
        let gap = ROTATE_HINT_GAP * wpp;
        // Shrink the elbow on tiny selections so neighbouring hints and
        // stubs can't run into each other.
        let side = frame.size.x.min(frame.size.y);
        let radius = (ROTATE_HINT_RADIUS * wpp).min((side * 0.5 - gap).max(1.0));
        let tail = (ROTATE_HINT_TAIL * wpp).min(side * 0.25);
        for i in 0..4 {
            let d = CORNER_DIRS[i];
            let outer = corner_world(&frame, i) + rot * (d * gap);
            // Rounded-rectangle corner: arc center pulled in by the radius.
            let center = outer - rot * Vec2::new(d.x * radius, d.y * radius);
            // Tangency points on the two offset edges meeting at the corner.
            let from = center + rot * Vec2::new(0.0, d.y * radius);
            let to = center + rot * Vec2::new(d.x * radius, 0.0);
            gizmos
                .short_arc_2d_between(center, from, to, accent_color)
                .resolution(12);
            // The short straight stubs continuing the arc along each edge.
            gizmos.line_2d(from, from + rot * Vec2::new(-d.x * tail, 0.0), accent_color);
            gizmos.line_2d(to, to + rot * Vec2::new(0.0, -d.y * tail), accent_color);
        }
    }

    if let Some(rect) = interaction.marquee_rect() {
        gizmos.rect_2d(
            Isometry2d::from_translation(rect.center()),
            rect.size(),
            accent.alpha(ACCENT_SOFT_ALPHA),
        );
    }

    // Snap guides while moving or resizing: each line spans only the boxes
    // it aligns (the dragged one plus every matched node), with a small tick
    // at each box edge it passes through, so it's obvious what snapped to
    // what. Boxes outside the view can't snap (see `snap_candidates`).
    match &*interaction {
        Interaction::Move {
            guide_x, guide_y, ..
        } => draw_guides(&mut gizmos, guide_x, guide_y, wpp),
        Interaction::Scale(drag) => draw_guides(&mut gizmos, &drag.guide_x, &drag.guide_y, wpp),
        _ => {}
    }
}

/// Draw the alignment guides for the given axes: line + an × on every
/// *other* box edge it touches (the dragged selection itself stays clean,
/// its span only contributes to the line's extent).
fn draw_guides(
    gizmos: &mut Gizmos,
    guide_x: &Option<SnapGuide>,
    guide_y: &Option<SnapGuide>,
    wpp: f32,
) {
    let pad = 6.0 * wpp;
    // Half-extent of the × marker: noticeably larger than the old tick.
    let cross = 5.0 * wpp;
    if let Some(g) = guide_x {
        let (lo, hi) = g.extent();
        gizmos.line_2d(
            Vec2::new(g.line, lo - pad),
            Vec2::new(g.line, hi + pad),
            GUIDE,
        );
        // `spans[0]` is the dragged box; mark only the matched nodes.
        for (a, b) in g.spans.iter().skip(1) {
            draw_cross(gizmos, Vec2::new(g.line, *a), cross);
            draw_cross(gizmos, Vec2::new(g.line, *b), cross);
        }
    }
    if let Some(g) = guide_y {
        let (lo, hi) = g.extent();
        gizmos.line_2d(
            Vec2::new(lo - pad, g.line),
            Vec2::new(hi + pad, g.line),
            GUIDE,
        );
        for (a, b) in g.spans.iter().skip(1) {
            draw_cross(gizmos, Vec2::new(*a, g.line), cross);
            draw_cross(gizmos, Vec2::new(*b, g.line), cross);
        }
    }
}

/// A small × marking where a guide crosses a box edge.
fn draw_cross(gizmos: &mut Gizmos, at: Vec2, half: f32) {
    let d = Vec2::splat(half);
    gizmos.line_2d(at - d, at + d, GUIDE);
    gizmos.line_2d(at - Vec2::new(d.x, -d.y), at + Vec2::new(d.x, -d.y), GUIDE);
}

fn handle_square(gizmos: &mut Gizmos, pos: Vec2, size: f32, accent: Color) {
    let iso = Isometry2d::from_translation(pos);
    gizmos.rect_2d(iso, Vec2::splat(size), accent);
    gizmos.rect_2d(iso, Vec2::splat(size * 0.58), HANDLE_FILL);
}

/// Dim boundary around nodes of the active tab that are placed but still
/// waiting for their texture (project restore: only the transparent
/// placeholder is bound until the preview loads). Without it a restoring
/// canvas looks empty, with nodes popping in one by one.
pub fn draw_pending_loads(
    mut gizmos: Gizmos,
    tabs: Res<Tabs>,
    q: Query<(&Transform, &CanvasNode, &TabOwner), Without<InactiveTab>>,
) {
    let Some(active) = tabs.active_id() else {
        return;
    };
    for (tf, node, owner) in &q {
        if owner.0 != active {
            continue;
        }
        let Some(img) = node.image() else {
            continue;
        };
        // A pending preview whose full-res texture already bound is visible:
        // no placeholder needed.
        if !img.data.preview_pending || img.full.is_some() {
            continue;
        }
        let iso = Isometry2d::new(tf.translation.truncate(), Rot2::radians(node.rotation));
        gizmos.rect_2d(iso, node.size, PENDING_LOAD);
    }
}

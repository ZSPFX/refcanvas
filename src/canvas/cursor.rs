//! Hover feedback: transform handles change the mouse cursor shape.

use bevy::prelude::*;
use bevy::window::{CursorIcon, PrimaryWindow, SystemCursorIcon};

use super::camera::{View, world_per_logical_px};
use super::hit::{self, HitKind};
use super::node::Pose;
use super::selection::{self, Interaction, NodeQ, Selection};

/// Cursor matching a handle whose outward direction is `dir` (world space, y up).
/// Rotated nodes therefore get correctly rotated diagonal cursors.
fn resize_cursor(dir: Vec2) -> SystemCursorIcon {
    let oct = (dir.to_angle() / (std::f32::consts::PI / 4.0)).round() as i32;
    match oct.rem_euclid(8) {
        0 => SystemCursorIcon::EResize,
        1 => SystemCursorIcon::NeResize,
        2 => SystemCursorIcon::NResize,
        3 => SystemCursorIcon::NwResize,
        4 => SystemCursorIcon::WResize,
        5 => SystemCursorIcon::SwResize,
        6 => SystemCursorIcon::SResize,
        _ => SystemCursorIcon::SeResize,
    }
}

fn handle_dir(frame: &Pose, kind: HitKind) -> Vec2 {
    let d = match kind {
        HitKind::Corner(i) => hit::CORNER_DIRS[i],
        HitKind::Edge(i) => hit::EDGE_DIRS[i],
        HitKind::Rotate => Vec2::Y,
    };
    hit::rot2(frame.rot) * d
}

/// Cursor when merely hovering (no button held).
fn hover_cursor(
    window: &Window,
    camera: &Camera,
    cam_tf: &GlobalTransform,
    selection: &Selection,
    q: &NodeQ,
) -> Option<SystemCursorIcon> {
    let cursor = window.cursor_position()?;
    let world = camera.viewport_to_world_2d(cam_tf, cursor).ok()?;
    let wpp = world_per_logical_px(camera, cam_tf)? as f32;
    let frame = selection::selection_frame_of(q, selection)?;
    let kind = hit::hit_frame(world, &frame, wpp)?;
    Some(match kind {
        HitKind::Corner(_) | HitKind::Edge(_) => resize_cursor(handle_dir(&frame, kind)),
        HitKind::Rotate => SystemCursorIcon::Crosshair,
    })
}

/// Runs late (after egui's own cursor handling) so canvas cursors win while
/// the pointer is over the canvas, and egui's win while it's over the UI.
pub fn update_cursor(
    window: Single<(Entity, &Window), With<PrimaryWindow>>,
    camera: Single<(&Camera, &GlobalTransform)>,
    mut cursor_icons: Query<&mut CursorIcon>,
    mut commands: Commands,
    mut contexts: bevy_egui::EguiContexts,
    view: Res<View>,
    interaction: Res<Interaction>,
    selection: Res<Selection>,
    q: NodeQ,
) {
    let (window_entity, window) = *window;

    // The component may not exist yet (bevy_egui only adds it when egui
    // requests an icon).
    if cursor_icons.get(window_entity).is_err() {
        commands
            .entity(window_entity)
            .insert(CursorIcon::System(SystemCursorIcon::Default));
        return;
    }

    let (camera, cam_tf) = *camera;
    let icon = if crate::canvas::egui_pointer_busy(&mut contexts) {
        // Over the HUD / menus: egui owns the cursor.
        None
    } else if view.is_panning() {
        Some(SystemCursorIcon::Grabbing)
    } else if let Some(sign) = view.dolly_sign() {
        Some(if sign > 0.0 {
            SystemCursorIcon::ZoomIn
        } else {
            SystemCursorIcon::ZoomOut
        })
    } else {
        match &*interaction {
            Interaction::Scale(drag) => {
                Some(resize_cursor(handle_dir(&drag.frame, drag.handle)))
            }
            Interaction::Move { .. } | Interaction::PendingMove { .. } => {
                Some(SystemCursorIcon::Move)
            }
            Interaction::Rotate(_) => Some(SystemCursorIcon::Crosshair),
            // Nothing special under the pointer: reset to the default arrow,
            // otherwise the handle cursor would stay stuck.
            _ => Some(
                hover_cursor(window, camera, cam_tf, &selection, &q)
                    .unwrap_or(SystemCursorIcon::Default),
            ),
        }
    };

    if let (Some(icon), Ok(mut current)) = (icon, cursor_icons.get_mut(window_entity)) {
        let wanted = CursorIcon::System(icon);
        if *current != wanted {
            *current = wanted;
        }
    }
}

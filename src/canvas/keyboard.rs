//! Global keyboard shortcuts.

use bevy::prelude::*;
use bevy::window::PrimaryWindow;
use bevy_egui::EguiContexts;

use super::camera::View;
use super::export;
use super::frames;
use super::grid::GridFade;
use super::hit;
use super::manipulate::{self, ZCounter};
use super::node::{InactiveTab, NodeSnapshot, Pose, TabOwner};
use super::selection::{self, Interaction, NodeQ, Selection};
use super::undo::{NodeData, NodeRecord, UndoEntry, UndoStack};
use crate::clipboard::{self, SystemClipboard};
use crate::project::tabs::Tabs;

/// Grouped resources for the shortcut system (Bevy systems take ≤ 16 params).
#[derive(bevy::ecs::system::SystemParam)]
pub struct ShortcutCtx<'w> {
    pub selection: ResMut<'w, Selection>,
    pub interaction: ResMut<'w, Interaction>,
    pub view: ResMut<'w, View>,
    pub zc: ResMut<'w, ZCounter>,
    pub system_clipboard: ResMut<'w, SystemClipboard>,
    pub undo: ResMut<'w, UndoStack>,
    pub grid_fade: ResMut<'w, GridFade>,
    pub tabs: Res<'w, Tabs>,
}

#[allow(clippy::too_many_arguments)]
pub fn shortcuts(
    window: Single<&Window, With<PrimaryWindow>>,
    keys: Res<ButtonInput<KeyCode>>,
    camera: Single<(&Camera, &GlobalTransform)>,
    mut contexts: EguiContexts,
    chord_mem: Res<crate::canvas::ChordMemory>,
    chord_frame: Res<super::lod::FrameCounter>,
    mut ctx: ShortcutCtx,
    q_own: Query<(Entity, &TabOwner), Without<InactiveTab>>,
    mut commands: Commands,
    mut q: NodeQ,
) {
    if super::egui_keyboard_busy(&mut contexts) {
        return;
    }

    let ctrl = crate::canvas::ctrl_down(&keys, &chord_mem, chord_frame.0);

    // ---- Focus: F frames the selection (or everything), A frames all ----
    let focus_selection = keys.just_pressed(KeyCode::KeyF);
    let focus_all = keys.just_pressed(KeyCode::KeyA) && !ctrl;
    if focus_selection || focus_all {
        let mut poses: Vec<Pose> = Vec::new();
        if focus_selection && !ctx.selection.nodes.is_empty() {
            poses = selection::gather_snapshots(&q, &ctx.selection.nodes)
                .into_iter()
                .map(|(_, s)| s.pose)
                .collect();
        }
        if poses.is_empty() {
            let keep = selection::owner_filter(ctx.tabs.active_id(), &q_own);
            poses = q
                .iter()
                .filter(|(e, ..)| keep(*e))
                .map(|(_, tf, _, node)| manipulate::current_pose(tf, node))
                .collect();
        }
        if !poses.is_empty() {
            manipulate::focus_poses(&poses, window.size(), &mut ctx.view);
        }
    }

    // ---- Arrow keys: hop the selection to the nearest node in that
    // direction, refocusing with a wider fly-to framing than `F` ----
    let arrow = if keys.just_pressed(KeyCode::ArrowUp) {
        Some(Vec2::Y)
    } else if keys.just_pressed(KeyCode::ArrowDown) {
        Some(Vec2::NEG_Y)
    } else if keys.just_pressed(KeyCode::ArrowRight) {
        Some(Vec2::X)
    } else if keys.just_pressed(KeyCode::ArrowLeft) {
        Some(Vec2::NEG_X)
    } else {
        None
    };
    if let Some(dir) = arrow {
        if ctx.interaction.is_idle() && ctx.selection.nodes.len() == 1 {
            let current = ctx.selection.nodes[0];
            let found = q
                .get(current)
                .ok()
                .map(|(_, tf, _, node)| (manipulate::current_pose(tf, node), tf.translation.z));
            if let Some((pose, current_z)) = found {
                let keep = selection::owner_filter(ctx.tabs.active_id(), &q_own);
                // Visible images are first-class; off-screen ones are the
                // fallback (see `arrow_target`).
                let (cam, cam_tf) = *camera;
                let view = super::camera::visible_world_rect(cam, cam_tf, window.size());
                if let Some(target) = arrow_target(&q, &keep, current, &pose, current_z, dir, view)
                {
                    ctx.selection.nodes = vec![target];
                    let poses: Vec<Pose> = selection::gather_snapshots(&q, &ctx.selection.nodes)
                        .into_iter()
                        .map(|(_, s)| s.pose)
                        .collect();
                    if !poses.is_empty() {
                        // Wider than `F`, so the neighbours stay visible.
                        manipulate::focus_poses_with_margin(
                            &poses,
                            window.size(),
                            &mut ctx.view,
                            manipulate::ARROW_FOCUS_MARGIN,
                        );
                    }
                }
            }
        }
    }

    // ---- Reset zoom to 100% ----
    if ctrl && keys.just_pressed(KeyCode::Digit0) {
        let center = ctx.view.center;
        ctx.view.fly_to(center, 0.0);
    }

    // ---- Delete ----
    if keys.any_just_pressed([KeyCode::Delete, KeyCode::Backspace]) {
        let tab_id = ctx.tabs.active_id().unwrap_or(0);
        manipulate::delete_selection_undoable(
            &mut ctx.selection,
            &mut ctx.undo,
            &q,
            tab_id,
            &mut commands,
        );
        *ctx.interaction = Interaction::Idle;
    }

    // ---- Escape: cancel the current drag (restoring state) or deselect ----
    if keys.just_pressed(KeyCode::Escape) {
        if ctx.interaction.is_idle() {
            ctx.selection.clear();
        } else {
            selection::cancel_interaction(&mut ctx.interaction, &mut q);
        }
    }

    // ---- Selection & clipboard ----
    if ctrl && keys.just_pressed(KeyCode::KeyA) {
        let keep = selection::owner_filter(ctx.tabs.active_id(), &q_own);
        manipulate::select_all(&q, keep, &mut ctx.selection);
    }
    if ctrl && keys.just_pressed(KeyCode::KeyC) {
        let nodes = export::gather(&ctx.selection, &q);
        if nodes.is_empty() {
            info!("clipboard: nothing to copy, select an image node first");
        } else {
            clipboard::copy_selection(&mut ctx.system_clipboard, nodes);
        }
    }
    if ctrl && keys.just_pressed(KeyCode::KeyD) {
        let offset = Vec2::new(24.0, -24.0) * ctx.view.scale() as f32;
        let tab_id = ctx.tabs.active_id().unwrap_or(0);
        let new = manipulate::duplicate_selection(
            &ctx.selection,
            offset,
            tab_id,
            &q,
            &mut commands,
            &mut ctx.zc,
        );
        if !new.is_empty() {
            ctx.selection.nodes = new.iter().map(|r| r.entity).collect();
            ctx.undo.push(UndoEntry::Added { items: new });
        }
    }
    // Ctrl+V (and Ctrl+Shift+V, historically the file-paste shortcut, the
    // Wayland stand-in for drag-and-drop) reads the *system* clipboard: an
    // image becomes a new node, files copied in a file manager are imported.
    if ctrl && keys.just_pressed(KeyCode::KeyV) {
        let at = window
            .cursor_position()
            .and_then(|c| camera.0.viewport_to_world_2d(camera.1, c).ok())
            .unwrap_or_else(|| ctx.view.center.as_vec2());
        clipboard::request_paste(&mut ctx.system_clipboard, at);
    }

    // ---- Ctrl+G: wrap the selection in a frame ----
    if ctrl && keys.just_pressed(KeyCode::KeyG) && !ctx.selection.nodes.is_empty() {
        let poses: Vec<Pose> = selection::gather_snapshots(&q, &ctx.selection.nodes)
            .into_iter()
            .map(|(_, s)| s.pose)
            .collect();
        if let Some(bounds) = manipulate::union_aabb(&poses) {
            let pad = 8.0 * ctx.view.scale() as f32;
            // Union AABB, not `selection_frame`: for a single node that
            // returns its own (possibly rotated) rect, which would leave the
            // frame cutting across a rotated image instead of containing it.
            let pose = Pose {
                center: bounds.center(),
                size: bounds.size() + Vec2::splat(pad * 2.0),
                rot: 0.0,
            };
            let min_z = ctx
                .selection
                .nodes
                .iter()
                .filter_map(|e| q.get(*e).ok().map(|(_, tf, _, _)| tf.translation.z))
                .fold(f32::INFINITY, f32::min);
            // Below every selected node. `min` first so a missing z (inf)
            // can't put the frame on top of the stack.
            let z = min_z.min(ctx.zc.0) - 0.5;
            let tab_id = ctx.tabs.active_id().unwrap_or(0);
            let label = "Frame".to_string();
            let e = frames::spawn_frame(
                &mut commands,
                tab_id,
                true,
                None,
                label.clone(),
                pose,
                z,
            );
            // Keep the previous selection (the framed nodes) instead of
            // selecting the new frame: the selection outline + handles are
            // drawn right at the frame's border, which with the tight padding
            // reads as the frame sitting on top of a single image. Click the
            // frame to select/move the group.
            ctx.undo.push(UndoEntry::Added {
                items: vec![NodeRecord {
                    entity: e,
                    tab_id,
                    data: NodeData::Frame { label },
                    snap: NodeSnapshot {
                        pose,
                        z,
                        flip_x: false,
                        flip_y: false,
                        crop: Rect::from_corners(Vec2::ZERO, Vec2::ONE),
                        px_scale: 1.0,
                    },
                }],
            });
        }
    }

    // ---- Grid visibility ----
    if !ctrl && keys.just_pressed(KeyCode::KeyG) {
        ctx.grid_fade.0 = if ctx.grid_fade.0 > 0.5 { 0.0 } else { 1.0 };
    }
}

/// Sample rays cast across the selection's cross-section for arrow hops.
const ARROW_RAYS: usize = 17;

/// Arrow-key target. On-screen images are first-class: the ray vote runs over
/// the camera's visible world rect first, and only when nothing visible in
/// that direction catches the rays do off-screen candidates act as a
/// fallback (so the hop flies the camera back to something you can't see only
/// when there is no visible next step).
fn arrow_target(
    q: &NodeQ,
    keep: &impl Fn(Entity) -> bool,
    current: Entity,
    pose: &Pose,
    current_z: f32,
    dir: Vec2,
    view: Option<Rect>,
) -> Option<Entity> {
    if let Some(e) = ray_vote(q, keep, current, pose, current_z, dir, view) {
        return Some(e);
    }
    ray_vote(q, keep, current, pose, current_z, dir, None)
}

/// Cast [`ARROW_RAYS`] parallel rays across the selected node's cross-section
/// from its centre plane, marching in `dir`. Each ray votes for the *nearest*
/// node it enters, including a node lying on top of the selection, which the
/// ray starts inside, and the node with the best score wins:
///
/// ```text
/// score = coverage * selection_length - entry_distance
/// ```
///
/// Distance (from the selection's centre plane to the node's near edge, in
/// "selection lengths") is what ranks candidates: a much closer node wins
/// even from a diagonal, and a neighbour down the row beats the same row
/// further away. Coverage, how much of the cross-section the node catches,
/// acts as a bonus worth up to one selection length, so a barely clipping
/// sliver can't outrank an adjacent aligned neighbour. When `view` is set,
/// only nodes overlapping it are considered. Frames are containers, not hop
/// targets; they never catch rays either.
fn ray_vote(
    q: &NodeQ,
    keep: &impl Fn(Entity) -> bool,
    current: Entity,
    pose: &Pose,
    current_z: f32,
    dir: Vec2,
    view: Option<Rect>,
) -> Option<Entity> {
    let cur = hit::aabb_of(pose);
    let vertical = dir.x != 0.0;
    let (band_lo, band_hi) = if vertical {
        (cur.min.y, cur.max.y)
    } else {
        (cur.min.x, cur.max.x)
    };
    let plane = if vertical {
        cur.center().x
    } else {
        cur.center().y
    };
    // Distance unit for the vote decay: the selection's extent along the ray.
    let unit = if vertical { cur.size().x } else { cur.size().y }.max(1.0);

    // Nearest hit per sample ray: (entry distance, z, entity).
    let mut nearest: Vec<Option<(f32, f32, Entity)>> = vec![None; ARROW_RAYS];
    for (e, tf, _, node) in q.iter() {
        if e == current || !keep(e) || node.kind.is_frame() {
            continue;
        }
        let r = hit::aabb_of(&manipulate::current_pose(tf, node));
        if let Some(view) = view {
            if !hit::rects_overlap(r, view) {
                continue; // Off-screen in this pass.
            }
        }
        let (perp_lo, perp_hi, near, far) = if vertical {
            (r.min.y, r.max.y, r.min.x, r.max.x)
        } else {
            (r.min.x, r.max.x, r.min.y, r.max.y)
        };
        let d = if vertical { dir.x } else { dir.y };
        // Wholly behind the centre plane: the rays never reach it.
        if (far - plane) * d <= 0.0 {
            continue;
        }
        // Distance from the plane to the box's near edge (0 when the rays
        // start inside it, a box lying over the selection).
        let entry = ((near - plane) * d).max(0.0);
        // A box the rays start inside only counts when it's drawn above the
        // selection: that's the "image on top" hop. A box underneath the
        // same spot must not steal the hop (it would just toggle back).
        let z = tf.translation.z;
        if entry == 0.0 && z <= current_z {
            continue;
        }
        for (i, slot) in nearest.iter_mut().enumerate() {
            let t = band_lo + (band_hi - band_lo) * i as f32 / (ARROW_RAYS - 1) as f32;
            if t < perp_lo || t > perp_hi {
                continue; // This ray misses the box.
            }
            let better = match *slot {
                None => true,
                Some((be, bz, _)) => entry < be - 0.01 || ((entry - be).abs() <= 0.01 && z > bz),
            };
            if better {
                *slot = Some((entry, z, e));
            }
        }
    }

    // Rank by distance, with coverage worth up to one selection length: a
    // clearly closer node wins, and among similarly placed ones the one the
    // rays mostly run into wins.
    let mut votes: Vec<(Entity, f32, f32)> = Vec::new();
    for slot in nearest.iter().flatten() {
        let (entry, _, e) = *slot;
        match votes.iter_mut().find(|(v, _, _)| *v == e) {
            Some((_, _, hits)) => *hits += 1.0,
            None => votes.push((e, entry, 1.0)),
        }
    }
    votes
        .into_iter()
        .map(|(e, entry, hits)| (e, (hits / ARROW_RAYS as f32) * unit - entry))
        .max_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(e, _)| e)
}

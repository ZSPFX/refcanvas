//! global keyboard shortcuts.

use bevy::prelude::*;
use bevy::window::PrimaryWindow;
use bevy_egui::EguiContexts;

use super::camera::View;
use super::export;
use super::frames;
use super::grid::GridFade;
use super::hit;
use super::manipulate::{self, ZCounter};
use super::node::{InactiveTab, Pose, TabOwner};
use super::selection::{self, Interaction, NodeQ, Selection};
use super::undo::{UndoEntry, UndoStack};
use crate::clipboard::{self, SystemClipboard};
use crate::project::tabs::Tabs;

/// grouped resources so the shortcut system stays under the 16-param limit
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

    let ctrl = crate::canvas::command_down(&keys, &chord_mem, chord_frame.0);
    let read_only = ctx.tabs.active_read_only();

    let focus_selection = keys.just_pressed(KeyCode::KeyF);
    if focus_selection {
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

    // arrow keys hop to the nearest node, refocusing wider than `F`
    let arrow = if keys.just_pressed(KeyCode::ArrowUp) || keys.just_pressed(KeyCode::KeyW) {
        Some(Vec2::Y)
    } else if keys.just_pressed(KeyCode::ArrowDown) || keys.just_pressed(KeyCode::KeyS) {
        Some(Vec2::NEG_Y)
    } else if keys.just_pressed(KeyCode::ArrowRight) || keys.just_pressed(KeyCode::KeyD) {
        Some(Vec2::X)
    } else if keys.just_pressed(KeyCode::ArrowLeft) || keys.just_pressed(KeyCode::KeyA) {
        Some(Vec2::NEG_X)
    } else {
        None
    };
    if let Some(dir) = arrow
        && !ctrl
    {
        if ctx.interaction.is_idle() && ctx.selection.nodes.len() == 1 {
            let current = ctx.selection.nodes[0];
            let found = q
                .get(current)
                .ok()
                .map(|(_, tf, _, node)| (manipulate::current_pose(tf, node), tf.translation.z));
            if let Some((pose, current_z)) = found {
                let keep = selection::owner_filter(ctx.tabs.active_id(), &q_own);
                // visible images first; off-screen only as a fallback
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

    if ctrl && keys.just_pressed(KeyCode::Digit0) {
        let center = ctx.view.center;
        ctx.view.fly_to(center, 0.0);
    }

    if !read_only && keys.any_just_pressed([KeyCode::Delete, KeyCode::Backspace]) {
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

    // escape cancels the drag, or deselects when idle
    if keys.just_pressed(KeyCode::Escape) {
        if ctx.interaction.is_idle() {
            ctx.selection.clear();
        } else {
            selection::cancel_interaction(&mut ctx.interaction, &mut q);
        }
    }

    if ctrl && keys.just_pressed(KeyCode::KeyA) {
        let keep = selection::owner_filter(ctx.tabs.active_id(), &q_own);
        manipulate::select_all(&q, keep, &mut ctx.selection);

        // focus entire thing
        if let Some(frame) = selection::selection_frame_of(&q, &ctx.selection) {
            manipulate::focus_poses(&[frame], window.size(), &mut ctx.view);
        }
    }
    if ctrl && keys.just_pressed(KeyCode::KeyC) {
        let nodes = export::gather(&ctx.selection, &q);
        if nodes.is_empty() {
            info!("clipboard: nothing to copy, select an image node first");
        } else {
            clipboard::copy_selection(&mut ctx.system_clipboard, nodes);
        }
    }
    if !read_only && ctrl && keys.just_pressed(KeyCode::KeyD) {
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
    // ctrl+v (and ctrl+shift+v, the file-paste fallback for drag-and-drop) reads
    // the system clipboard: an image becomes a node, files get imported.
    if !read_only && ctrl && keys.just_pressed(KeyCode::KeyV) {
        let at = window
            .cursor_position()
            .and_then(|c| camera.0.viewport_to_world_2d(camera.1, c).ok())
            .unwrap_or_else(|| ctx.view.center.as_vec2());
        clipboard::request_paste(&mut ctx.system_clipboard, at);
    }

    if !read_only && ctrl && keys.just_pressed(KeyCode::KeyG) && !ctx.selection.nodes.is_empty() {
        let tab_id = ctx.tabs.active_id().unwrap_or(0);
        frames::group_selection(
            &ctx.selection,
            &q,
            tab_id,
            ctx.view.scale() as f32,
            &mut ctx.zc,
            &mut commands,
            &mut ctx.undo,
        );
    }

    if !ctrl
        && !crate::canvas::ctrl_down(&keys, &chord_mem, chord_frame.0)
        && keys.just_pressed(KeyCode::KeyG)
    {
        ctx.grid_fade.0 = if ctx.grid_fade.0 > 0.5 { 0.0 } else { 1.0 };
    }
}

/// rays sampled across the selection's cross-section for arrow hops
const ARROW_RAYS: usize = 17;

/// on-screen images first: vote over the visible rect, then fall back to
/// off-screen candidates only when nothing visible catches the rays.
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

/// cast [`ARROW_RAYS`] rays across the selection, each voting for the nearest
/// node it enters; score = coverage * selection_length - entry_distance.
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
    // vote decay unit: the selection's extent along the ray
    let unit = if vertical { cur.size().x } else { cur.size().y }.max(1.0);

    // per-ray nearest hit (entry, z, entity)
    let mut nearest: Vec<Option<(f32, f32, Entity)>> = vec![None; ARROW_RAYS];
    for (e, tf, _, node) in q.iter() {
        if e == current || !keep(e) || node.kind.is_frame() {
            continue;
        }
        let r = hit::aabb_of(&manipulate::current_pose(tf, node));
        if let Some(view) = view {
            if !hit::rects_overlap(r, view) {
                continue;
            }
        }
        let (perp_lo, perp_hi, near, far) = if vertical {
            (r.min.y, r.max.y, r.min.x, r.max.x)
        } else {
            (r.min.x, r.max.x, r.min.y, r.max.y)
        };
        let d = if vertical { dir.x } else { dir.y };
        if (far - plane) * d <= 0.0 {
            continue;
        }
        // distance to the near edge; 0 when the rays start inside, i.e. a box
        // lying over the selection.
        let entry = ((near - plane) * d).max(0.0);
        // a box the rays start inside counts only when drawn above the selection;
        // one underneath would just steal the hop and toggle back.
        let z = tf.translation.z;
        if entry == 0.0 && z <= current_z {
            continue;
        }
        for (i, slot) in nearest.iter_mut().enumerate() {
            let t = band_lo + (band_hi - band_lo) * i as f32 / (ARROW_RAYS - 1) as f32;
            if t < perp_lo || t > perp_hi {
                continue;
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

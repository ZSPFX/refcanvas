//! selection state and the pointer interaction state machine.

use bevy::prelude::*;
use bevy::window::PrimaryWindow;
use bevy_egui::EguiContexts;

use super::camera::world_per_logical_px;
use super::egui_pointer_busy;
use super::frames::{FrameLabelHits, FrameRename, LABEL_DOUBLE_CLICK};
use super::hit::{self, HitKind};
use super::manipulate::{self, RotateDrag, ScaleDrag, ZCounter, current_pose};
use super::node::{InactiveTab, CanvasNode, NodeKind, NodeSnapshot, Pose, TabOwner};
use super::touch::{Pointer, TOUCH_DRAG_THRESHOLD_PX};
use super::undo::{UndoEntry, UndoStack, snapshot_of};
use crate::project::tabs::Tabs;

/// grouped modifiers so `pointer_events` stays under the 16-param system limit
#[derive(bevy::ecs::system::SystemParam)]
pub struct Chords<'w> {
    pub mem: Res<'w, crate::canvas::ChordMemory>,
    pub frame: Res<'w, super::lod::FrameCounter>,
}

pub type NodeQ<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        &'static mut Transform,
        &'static mut Sprite,
        &'static mut CanvasNode,
    ),
    Without<InactiveTab>,
>;

/// min scale size, logical px
pub const MIN_SIZE_PX: f32 = 6.0;
/// click/drag threshold, logical px
const MOVE_THRESHOLD_PX: f32 = 3.0;
/// travel before a marquee affects selection; below it a click, which must not
/// select through the aabb gaps of rotated nodes.
const MARQUEE_THRESHOLD_PX: f32 = 4.0;
/// snap candidates must be within this many logical px of the viewport; aligning
/// to unseen nodes is just noise.
const SNAP_FALLOFF_PX: f32 = 120.0;

#[derive(Resource, Default)]
pub struct Selection {
    pub nodes: Vec<Entity>,
}

impl Selection {
    pub fn contains(&self, e: Entity) -> bool {
        self.nodes.contains(&e)
    }

    pub fn clear(&mut self) {
        self.nodes.clear();
    }
}

#[derive(Resource, Default)]
pub enum Interaction {
    #[default]
    Idle,
    /// pressed on a node; becomes Move past the threshold
    PendingMove {
        press_screen: Vec2,
        start_world: Vec2,
        snapshots: Vec<(Entity, NodeSnapshot)>,
        /// shift+press on a selected node: drag moves the selection, a click
        /// with no drag removes it again on release.
        toggle_off: Option<Entity>,
    },
    Move {
        start_world: Vec2,
        snapshots: Vec<(Entity, NodeSnapshot)>,
        /// live guides drawn by the overlay
        guide_x: Option<super::snap::SnapGuide>,
        guide_y: Option<super::snap::SnapGuide>,
    },
    Marquee {
        start_world: Vec2,
        current_world: Vec2,
        base: Vec<Entity>,
    },
    Scale(ScaleDrag),
    Rotate(RotateDrag),
}

impl Interaction {
    pub fn is_idle(&self) -> bool {
        matches!(self, Interaction::Idle)
    }

    pub fn snapshots(&self) -> Option<&[(Entity, NodeSnapshot)]> {
        match self {
            Interaction::PendingMove { snapshots, .. } | Interaction::Move { snapshots, .. } => {
                Some(snapshots)
            }
            Interaction::Scale(drag) => Some(&drag.snapshots),
            Interaction::Rotate(drag) => Some(&drag.snapshots),
            _ => None,
        }
    }

    pub fn marquee_rect(&self) -> Option<Rect> {
        match self {
            Interaction::Marquee {
                start_world,
                current_world,
                ..
            } => Some(Rect::from_corners(*start_world, *current_world)),
            _ => None,
        }
    }
}

/// selection plus images inside selected frames: dragging a frame drags
/// its contents.
pub fn drag_snapshots(q: &NodeQ, selection: &Selection) -> Vec<(Entity, NodeSnapshot)> {
    let mut snapshots = gather_snapshots(q, &selection.nodes);
    let frames: Vec<Pose> = snapshots
        .iter()
        .filter_map(|(e, snap)| {
            q.get(*e)
                .ok()
                .filter(|(_, _, _, node)| node.kind.is_frame())
                .map(|_| snap.pose)
        })
        .collect();
    if frames.is_empty() {
        return snapshots;
    }
    for e in super::frames::contained_images(&frames, q) {
        if snapshots.iter().any(|(s, _)| *s == e) {
            continue;
        }
        if let Ok((_, tf, sprite, node)) = q.get(e) {
            snapshots.push((e, snapshot_of(tf, node, sprite)));
        }
    }
    snapshots
}

pub fn gather_snapshots(q: &NodeQ, entities: &[Entity]) -> Vec<(Entity, NodeSnapshot)> {
    entities
        .iter()
        .filter_map(|e| {
            q.get(*e)
                .ok()
                .map(|(_, tf, sprite, node)| (*e, snapshot_of(tf, node, sprite)))
        })
        .collect()
}

pub fn selection_frame_of(q: &NodeQ, selection: &Selection) -> Option<Pose> {
    let poses: Vec<Pose> = gather_snapshots(q, &selection.nodes)
        .into_iter()
        .map(|(_, s)| s.pose)
        .collect();
    manipulate::selection_frame(&poses)
}

/// active-tab filter; `None` keeps everything
pub fn owner_filter<'a>(
    active: Option<u64>,
    q_own: &'a Query<(Entity, &TabOwner), Without<InactiveTab>>,
) -> impl Fn(Entity) -> bool + 'a {
    move |e| {
        active.map_or(true, |id| {
            q_own.get(e).map(|(_, o)| o.0 == id).unwrap_or(false)
        })
    }
}

pub fn pick_topmost(
    cursor_world: Vec2,
    q: &NodeQ,
    keep: impl Fn(Entity) -> bool,
    label_hits: &super::frames::FrameLabelHits,
    slop: f32,
) -> Option<(Entity, Pose)> {
    let mut items: Vec<(Entity, f32)> = q
        .iter()
        .filter(|(e, ..)| keep(*e))
        .map(|(e, tf, _, _)| (e, tf.translation.z))
        .collect();
    items.sort_unstable_by(|a, b| b.1.total_cmp(&a.1));
    for (e, _) in items {
        if let Ok((_, tf, _, node)) = q.get(e) {
            let pose = current_pose(tf, node);
            // label is chrome outside the body: a click on it hits the frame
            let on_label = label_hits
                .get(e)
                .is_some_and(|label| hit::point_inside(cursor_world, label));
            if hit::point_inside_with_slop(cursor_world, &pose, slop) || on_label {
                return Some((e, pose));
            }
        }
    }
    None
}

/// returns true if it started a rename
pub fn press_label(
    entity: Entity,
    cursor_world: Vec2,
    label_hits: &FrameLabelHits,
    rename: &mut FrameRename,
    selection: &mut Selection,
    q: &NodeQ,
) -> bool {
    if !label_hits
        .get(entity)
        .is_some_and(|label| hit::point_inside(cursor_world, label))
    {
        rename.last_click = None;
        return false;
    }
    let now = std::time::Instant::now();
    let double = rename
        .last_click
        .is_some_and(|(t, e)| e == entity && now.duration_since(t) <= LABEL_DOUBLE_CLICK);
    rename.last_click = if double { None } else { Some((now, entity)) };
    if double && rename.frame.is_none() {
        if let Ok((_, _, _, node)) = q.get(entity) {
            if let NodeKind::Frame(frame_node) = &node.kind {
                rename.start(entity, frame_node.label.clone());
                selection.nodes = vec![entity];
                return true;
            }
        }
    }
    false
}

/// camera view grown by the snap falloff band
fn snap_view_rect(window: &Window, camera: &Camera, cam_tf: &GlobalTransform, wpp: f32) -> Rect {
    match super::camera::visible_world_rect(camera, cam_tf, window.size()) {
        Some(view) => view.inflate(SNAP_FALLOFF_PX * wpp),
        // no projection: don't range-limit snapping.
        None => Rect::from_corners(Vec2::splat(f32::MIN), Vec2::splat(f32::MAX)),
    }
}

/// non-dragged active-tab nodes overlapping the falloff-grown view
fn snap_candidates(
    q: &NodeQ,
    q_own: &Query<(Entity, &TabOwner), Without<InactiveTab>>,
    active: Option<u64>,
    dragged: &[(Entity, NodeSnapshot)],
    view: Rect,
) -> Vec<Rect> {
    let keep = owner_filter(active, q_own);
    q.iter()
        .filter_map(|(e, tf, _, node)| {
            if dragged.iter().any(|(d, _)| *d == e) || !keep(e) {
                return None;
            }
            let rect = super::hit::aabb_of(&current_pose(tf, node));
            super::hit::rects_overlap(rect, view).then_some(rect)
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
pub fn pointer_events(
    pointer: Res<Pointer>,
    keys: Res<ButtonInput<KeyCode>>,
    camera: Single<(&Camera, &GlobalTransform)>,
    mut contexts: EguiContexts,
    mut selection: ResMut<Selection>,
    mut interaction: ResMut<Interaction>,
    mut zc: ResMut<ZCounter>,
    mut undo: ResMut<UndoStack>,
    chords: Chords,
    mut rename: ResMut<FrameRename>,
    tabs: Res<Tabs>,
    label_hits: Res<FrameLabelHits>,
    q_own: Query<(Entity, &TabOwner), Without<InactiveTab>>,
    mut q: NodeQ,
) {
    let egui_busy = egui_pointer_busy(&mut contexts);

    if pointer.just_released {
        // shift press that never dragged is a shift-click: toggle it back out
        let toggle = match &*interaction {
            Interaction::PendingMove {
                toggle_off: Some(e),
                ..
            } => Some(*e),
            _ => None,
        };
        if let Some(e) = toggle {
            selection.nodes.retain(|n| *n != e);
        }
        // commit the edit if anything moved
        if let Some(snaps) = interaction.snapshots() {
            let mut items = Vec::new();
            for (e, before) in snaps {
                if let Ok((_, tf, sprite, node)) = q.get(*e) {
                    let after = snapshot_of(tf, node, sprite);
                    if after != *before {
                        items.push((*e, *before, after));
                    }
                }
            }
            if !items.is_empty() {
                undo.push(UndoEntry::Changed { items });
            }
        }
        *interaction = Interaction::Idle;
    }

    if !pointer.just_pressed || egui_busy || (pointer.is_touch && pointer.touch_pan) {
        return;
    }
    let shift = crate::canvas::shift_down(&keys, &chords.mem, chords.frame.0);
    let alt = crate::canvas::alt_down(&keys, &chords.mem, chords.frame.0);
    if !pointer.is_touch && keys.pressed(KeyCode::Space) {
        return; // space+lmb pans
    }

    let (camera, cam_tf) = *camera;
    let Some(cursor) = pointer.screen else {
        return;
    };
    let Ok(cursor_world) = camera.viewport_to_world_2d(cam_tf, cursor) else {
        return;
    };
    let wpp = world_per_logical_px(camera, cam_tf).unwrap_or(1.0) as f32;

    // a label under the pointer wins over overlapping handles, so grabbing the
    // text moves the frame instead of resizing from the top-left corner.
    let on_selected_label = selection.nodes.iter().any(|e| {
        label_hits
            .get(*e)
            .is_some_and(|l| hit::point_inside(cursor_world, l))
    });
    if !on_selected_label {
        if let Some(frame) = selection_frame_of(&q, &selection) {
            let (handle_px, rotate_px) = if pointer.is_touch {
                hit::touch_radii(&frame, wpp)
            } else {
                (hit::HANDLE_HIT_PX, hit::ROTATE_ZONE_PX)
            };
            if let Some(kind) =
                hit::hit_frame_with_radii(cursor_world, &frame, wpp, handle_px, rotate_px)
            {
                let ctrl = crate::canvas::ctrl_down(&keys, &chords.mem, chords.frame.0);
                match kind {
                    HitKind::Corner(_) | HitKind::Edge(_) => {
                        // resize transforms the selection only; a frame resizes
                        // without its contents. marquee the frame *and* images to scale both.
                        let snapshots = gather_snapshots(&q, &selection.nodes);
                        // ctrl+drag on one image = precise crop; read live, so ctrl
                        // can also be pressed mid-drag.
                        let crop_eligible = snapshots.len() == 1
                            && snapshots
                                .first()
                                .and_then(|(e, _)| q.get(*e).ok())
                                .map(|(_, _, _, n)| n.image().is_some())
                                .unwrap_or(false);
                        // crop anchors the opposite edge; alt's resize-from-center
                        // only applies to plain resizes
                        let from_center = alt && !(ctrl && crop_eligible);
                        if let Some(drag) = manipulate::begin_scale(
                            &frame,
                            kind,
                            from_center,
                            crop_eligible,
                            snapshots,
                        ) {
                            *interaction = Interaction::Scale(drag);
                        }
                    }
                    HitKind::Rotate => {
                        *interaction = Interaction::Rotate(manipulate::start_rotate_around(
                            frame.center,
                            cursor_world,
                            drag_snapshots(&q, &selection),
                        ));
                    }
                }
                if !interaction.is_idle() {
                    return;
                }
            }
        }
    }

    // touch double-tap inside the frame drags the whole selection, gaps included
    let double_tap_selection = pointer.is_touch
        && pointer.double_tap
        && !on_selected_label
        && !selection.nodes.is_empty()
        && selection_frame_of(&q, &selection)
            .is_some_and(|frame| hit::point_inside(cursor_world, &frame));
    if double_tap_selection {
        *interaction = Interaction::PendingMove {
            press_screen: cursor,
            start_world: cursor_world,
            snapshots: drag_snapshots(&q, &selection),
            toggle_off: None,
        };
        return;
    }

    let keep = owner_filter(tabs.active_id(), &q_own);
    let slop = if pointer.is_touch {
        hit::TOUCH_NODE_SLOP_PX * wpp
    } else {
        0.0
    };
    let hit = pick_topmost(cursor_world, &q, &keep, &label_hits, slop);
    if let Some((entity, _pose)) = hit {
        // double-click on a label renames the frame; the first click behaves normally
        if press_label(
            entity,
            cursor_world,
            &label_hits,
            &mut rename,
            &mut selection,
            &q,
        ) {
            *interaction = Interaction::Idle;
            return;
        }
        // shift can't decide membership at press time: shift+press then drag must
        // move, so a click with no drag toggles it off on release instead.
        let mut toggle_off = None;
        if shift {
            if selection.contains(entity) {
                toggle_off = Some(entity);
            } else {
                selection.nodes.push(entity);
            }
        } else if !selection.contains(entity) {
            selection.nodes = vec![entity];
        }
        // frames stay below their contents: never raise them. a pending
        // shift-click raises nothing either, it may end as a toggle-off.
        if toggle_off.is_none()
            && !q
                .get(entity)
                .map(|(_, _, _, n)| n.kind.is_frame())
                .unwrap_or(true)
        {
            manipulate::raise(entity, &mut q, &mut zc);
        }

        let snapshots = drag_snapshots(&q, &selection);
        *interaction = Interaction::PendingMove {
            press_screen: cursor,
            start_world: cursor_world,
            snapshots,
            toggle_off,
        };
        return;
    }

    // one finger pans; only a double tap starts a marquee
    if pointer.is_touch && !pointer.double_tap {
        *interaction = Interaction::Idle;
        return;
    }
    rename.last_click = None;
    if !shift {
        selection.clear();
    }
    let base = selection.nodes.clone();
    *interaction = Interaction::Marquee {
        start_world: cursor_world,
        current_world: cursor_world,
        base,
    };
}

pub fn apply_interaction(
    window: Single<&Window, With<PrimaryWindow>>,
    pointer: Res<Pointer>,
    keys: Res<ButtonInput<KeyCode>>,
    camera: Single<(&Camera, &GlobalTransform)>,
    mut selection: ResMut<Selection>,
    mut interaction: ResMut<Interaction>,
    chord_mem: Res<crate::canvas::ChordMemory>,
    chord_frame: Res<super::lod::FrameCounter>,
    tabs: Res<Tabs>,
    q_own: Query<(Entity, &TabOwner), Without<InactiveTab>>,
    mut q: NodeQ,
) {
    let mut promote: Option<(Vec2, Vec<(Entity, NodeSnapshot)>)> = None;
    if let Interaction::PendingMove {
        press_screen,
        start_world,
        snapshots,
        ..
    } = &*interaction
    {
        if let Some(cursor) = pointer.screen {
            let threshold = if pointer.is_touch {
                TOUCH_DRAG_THRESHOLD_PX
            } else {
                MOVE_THRESHOLD_PX
            };
            if pointer.pressed && cursor.distance(*press_screen) > threshold {
                promote = Some((*start_world, snapshots.clone()));
            }
        }
    }
    if let Some((start_world, snapshots)) = promote {
        *interaction = Interaction::Move {
            start_world,
            snapshots,
            guide_x: None,
            guide_y: None,
        };
    }

    let shift = crate::canvas::shift_down(&keys, &chord_mem, chord_frame.0);
    let alt = crate::canvas::alt_down(&keys, &chord_mem, chord_frame.0);

    let (camera, cam_tf) = *camera;
    let cursor = pointer.screen;
    let cursor_world = cursor.and_then(|c| camera.viewport_to_world_2d(cam_tf, c).ok());
    let wpp = world_per_logical_px(camera, cam_tf).unwrap_or(1.0) as f32;
    // snap only against on-screen nodes (plus falloff): an unseen target is aimless
    let snap_view = snap_view_rect(&window, camera, cam_tf, wpp);

    match &mut *interaction {
        Interaction::Idle | Interaction::PendingMove { .. } => {}
        Interaction::Move {
            start_world,
            snapshots,
            guide_x,
            guide_y,
        } => {
            if let Some(cw) = cursor_world {
                let raw = cw - *start_world;
                let snap_enabled = !crate::canvas::ctrl_down(&keys, &chord_mem, chord_frame.0);
                let snapped = if snap_enabled {
                    let dragged = snapshots
                        .iter()
                        .filter_map(|(_, s)| Some(super::hit::aabb_of(&s.pose)))
                        .reduce(|a, b| a.union(b));
                    match dragged {
                        Some(rect) => {
                            let others =
                                snap_candidates(&q, &q_own, tabs.active_id(), snapshots, snap_view);
                            super::snap::snap_delta(
                                rect,
                                &others,
                                raw,
                                super::snap::THRESHOLD_PX * wpp,
                            )
                        }
                        None => super::snap::Snap::none(raw),
                    }
                } else {
                    super::snap::Snap::none(raw)
                };
                *guide_x = snapped.guide_x;
                *guide_y = snapped.guide_y;
                manipulate::apply_move(snapshots, snapped.delta, shift, &mut q);
            }
        }
        Interaction::Marquee {
            start_world,
            current_world,
            base,
        } => {
            if let Some(cw) = cursor_world {
                *current_world = cw;
                // below the threshold it's a click, not a marquee; a zero-size
                // rect would select every node whose aabb covers the press point.
                if (*current_world - *start_world).length() >= MARQUEE_THRESHOLD_PX * wpp {
                    let rect = Rect::from_corners(*start_world, *current_world);
                    let mut sel = base.clone();
                    let keep = owner_filter(tabs.active_id(), &q_own);
                    for (e, tf, _, node) in q.iter() {
                        if sel.contains(&e) || !keep(e) {
                            continue;
                        }
                        let pose = current_pose(tf, node);
                        if hit::rect_overlaps_pose(rect, &pose) {
                            sel.push(e);
                        }
                    }
                    selection.nodes = sel;
                }
            }
        }
        Interaction::Scale(drag) => {
            if let Some(cw) = cursor_world {
                let ctrl_now = crate::canvas::ctrl_down(&keys, &chord_mem, chord_frame.0);
                if ctrl_now && drag.crop_eligible {
                    // ctrl at press or mid-drag: trim the visible region. both
                    // modes read the press-time snapshot, so switching never compounds.
                    manipulate::apply_crop(drag, cw, wpp, &mut q);
                } else {
                    // shift keeps aspect, alt resizes from the center; the drag
                    // set is the selection only.
                    let candidates =
                        snap_candidates(&q, &q_own, tabs.active_id(), &drag.snapshots, snap_view);
                    manipulate::apply_scale(
                        drag,
                        cw,
                        shift,
                        alt,
                        MIN_SIZE_PX * wpp,
                        wpp,
                        &candidates,
                        &mut q,
                    );
                }
            }
        }
        Interaction::Rotate(drag) => {
            if let Some(cw) = cursor_world {
                manipulate::apply_rotate(drag, cw, shift, &mut q);
            }
        }
    }
}

pub fn prune_selection(mut selection: ResMut<Selection>, q: Query<Entity, With<CanvasNode>>) {
    if selection.nodes.iter().any(|e| !q.contains(*e)) {
        selection.nodes.retain(|e| q.contains(*e));
    }
}

/// restore the press-time state, then idle (escape)
pub fn cancel_interaction(interaction: &mut Interaction, q: &mut NodeQ) {
    if let Some(snaps) = interaction.snapshots() {
        for (e, snap) in snaps {
            if let Ok((_, mut tf, mut sprite, mut node)) = q.get_mut(*e) {
                super::undo::apply_snapshot(&mut tf, Some(&mut sprite), &mut node, snap);
            }
        }
    }
    *interaction = Interaction::Idle;
}

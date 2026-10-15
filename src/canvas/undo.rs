//! Undo / redo (Ctrl+Z / Ctrl+Shift+Z, Ctrl+Y).
//!
//! Entries are snapshots, not diffs: transforms restore exactly, and
//! add/remove entries carry the node data needed to respawn entities.

use bevy::prelude::*;
use bevy_egui::EguiContexts;

use super::manipulate::{current_pose, set_pose, ZCounter};
use super::node::{CanvasNode, ImageData, ImageNode, NodeKind, NodeSnapshot, TabOwner};
use super::selection::NodeQ;
use crate::project::tabs::Tabs;

/// What a record restores into.
pub enum NodeData {
    Image(ImageData),
    Frame { label: String },
}

/// A node as it exists (or existed) on the canvas.
pub struct NodeRecord {
    pub entity: Entity,
    pub tab_id: u64,
    pub data: NodeData,
    pub snap: NodeSnapshot,
}

pub enum UndoEntry {
    /// Transform/flip/z changes on live nodes.
    Changed {
        items: Vec<(Entity, NodeSnapshot, NodeSnapshot)>,
    },
    /// Nodes were added (present now): undo removes them, redo re-adds.
    Added { items: Vec<NodeRecord> },
    /// Nodes were removed (gone now): undo re-adds them, redo removes.
    Removed { items: Vec<NodeRecord> },
}

#[derive(Resource, Default)]
pub struct UndoStack {
    pub past: Vec<UndoEntry>,
    pub future: Vec<UndoEntry>,
}

const MAX_UNDO: usize = 200;

impl UndoStack {
    pub fn push(&mut self, entry: UndoEntry) {
        self.past.push(entry);
        self.future.clear();
        while self.past.len() > MAX_UNDO {
            self.past.remove(0);
        }
    }
}

pub fn snapshot_of(tf: &Transform, node: &CanvasNode, sprite: &Sprite) -> NodeSnapshot {
    let (crop, px_scale) = node
        .image()
        .map(|i| (i.data.crop, i.data.px_scale))
        .unwrap_or((Rect::from_corners(Vec2::ZERO, Vec2::ONE), 1.0));
    NodeSnapshot {
        pose: current_pose(tf, node),
        z: tf.translation.z,
        flip_x: sprite.flip_x,
        flip_y: sprite.flip_y,
        crop,
        px_scale,
    }
}

pub fn apply_snapshot(
    tf: &mut Transform,
    mut sprite: Option<&mut Sprite>,
    node: &mut CanvasNode,
    snap: &NodeSnapshot,
) {
    if let Some(img) = node.image_mut() {
        img.data.crop = snap.crop;
        img.data.px_scale = snap.px_scale;
    }
    set_pose(tf, sprite.as_deref_mut(), node, &snap.pose);
    tf.translation.z = snap.z;
    if let Some(sprite) = sprite {
        sprite.flip_x = snap.flip_x;
        sprite.flip_y = snap.flip_y;
    }
}

/// Respawn a node from a record (used by undo of deletes / redo of adds).
fn respawn(
    commands: &mut Commands,
    zc: &mut ZCounter,
    record: &NodeRecord,
    visible: bool,
) -> Entity {
    let entity = match &record.data {
        NodeData::Image(data) => {
            let preview = data.preview.clone();
            commands
                .spawn((
                    CanvasNode {
                        db_id: None, // fresh row in the project db
                        kind: NodeKind::Image(ImageNode {
                            data: data.clone(),
                            full: None,
                            full_incoming: None,
                            sampler_nearest: false,
                            pending_cpu_free: None,
                            resident_bytes: 0,
                            last_needed: 0.0,
                            sprite_material: None,
                        }),
                        size: record.snap.pose.size,
                        rotation: record.snap.pose.rot,
                    },
                    Sprite {
                        image: preview,
                        custom_size: Some(record.snap.pose.size),
                        // Preview tier (see `ImageData::tier_rect_of`).
                        rect: Some(ImageData::tier_rect_of(
                            record.snap.crop,
                            data.natural,
                            data.preview_size,
                            false,
                        )),
                        flip_x: record.snap.flip_x,
                        flip_y: record.snap.flip_y,
                        ..default()
                    },
                    Transform {
                        translation: Vec3::new(
                            record.snap.pose.center.x,
                            record.snap.pose.center.y,
                            record.snap.z,
                        ),
                        rotation: Quat::from_rotation_z(record.snap.pose.rot),
                        ..default()
                    },
                    TabOwner(record.tab_id),
                    if visible {
                        Visibility::Inherited
                    } else {
                        Visibility::Hidden
                    },
                ))
                .id()
        }
        NodeData::Frame { label } => super::frames::spawn_frame(
            commands,
            record.tab_id,
            visible,
            None,
            label.clone(),
            record.snap.pose,
            record.snap.z,
        ),
    };
    // Keep the z counter above everything we restore.
    zc.0 = zc.0.max(record.snap.z);
    entity
}

pub fn records_of(
    entities: &[Entity],
    active_tab: u64,
    q: &NodeQ,
) -> Vec<NodeRecord> {
    entities
        .iter()
        .filter_map(|e| {
            let (_, tf, sprite, node) = q.get(*e).ok()?;
            let data = match &node.kind {
                NodeKind::Image(img) => NodeData::Image(img.data.clone()),
                NodeKind::Frame(frame) => NodeData::Frame {
                    label: frame.label.clone(),
                },
            };
            Some(NodeRecord {
                entity: *e,
                tab_id: active_tab,
                data,
                snap: snapshot_of(tf, node, sprite),
            })
        })
        .collect()
}

/// Undo/redo: `Z` alone (or Ctrl+Z) undoes; Shift+Z, Ctrl+Shift+Z or Ctrl+Y
/// redo.
pub fn undo_redo(
    keys: Res<ButtonInput<KeyCode>>,
    chord_mem: Res<crate::canvas::ChordMemory>,
    chord_frame: Res<super::lod::FrameCounter>,
    mut contexts: EguiContexts,
    mut stack: ResMut<UndoStack>,
    mut commands: Commands,
    mut zc: ResMut<ZCounter>,
    tabs: Res<Tabs>,
    mut q: NodeQ,
) {
    if super::egui_keyboard_busy(&mut contexts) {
        return;
    }
    let ctrl = crate::canvas::ctrl_down(&keys, &chord_mem, chord_frame.0);
    let shift = crate::canvas::shift_down(&keys, &chord_mem, chord_frame.0);
    let undo = keys.just_pressed(KeyCode::KeyZ) && !shift;
    let redo =
        (keys.just_pressed(KeyCode::KeyZ) && shift) || (ctrl && keys.just_pressed(KeyCode::KeyY));
    if !undo && !redo {
        return;
    }

    let active = tabs.active_id().unwrap_or(0);
    if undo {
        if let Some(entry) = stack.past.pop() {
            let redo_entry = apply(entry, &mut commands, &mut zc, &active, &mut q, true);
            stack.future.push(redo_entry);
        }
    } else if let Some(entry) = stack.future.pop() {
        let undo_entry = apply(entry, &mut commands, &mut zc, &active, &mut q, false);
        stack.past.push(undo_entry);
    }
}

/// Apply an entry in reverse (`undoing`) or forward direction; returns the
/// entry to push onto the opposite stack.
fn apply(
    entry: UndoEntry,
    commands: &mut Commands,
    zc: &mut ZCounter,
    active_tab: &u64,
    q: &mut NodeQ,
    undoing: bool,
) -> UndoEntry {
    match entry {
        UndoEntry::Changed { items } => {
            info!("apply Changed: {} items, undoing={undoing}", items.len());
            for (e, before, after) in &items {
                let target = if undoing { before } else { after };
                if let Ok((_, mut tf, mut sprite, mut node)) = q.get_mut(*e) {
                    apply_snapshot(&mut tf, Some(&mut sprite), &mut node, target);
                }
            }
            UndoEntry::Changed { items }
        }
        UndoEntry::Added { mut items } => {
            if undoing {
                for record in &items {
                    commands.entity(record.entity).despawn();
                }
            } else {
                for record in &mut items {
                    record.entity = respawn(commands, zc, record, record.tab_id == *active_tab);
                }
            }
            UndoEntry::Added { items }
        }
        UndoEntry::Removed { mut items } => {
            if undoing {
                for record in &mut items {
                    record.entity = respawn(commands, zc, record, record.tab_id == *active_tab);
                }
            } else {
                for record in &items {
                    commands.entity(record.entity).despawn();
                }
            }
            UndoEntry::Removed { items }
        }
    }
}

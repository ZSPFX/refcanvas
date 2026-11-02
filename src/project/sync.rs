//! Background reconciliation with the shared project folder.
//!
//! Polling is deliberate: replicated folders and network shares do not provide
//! reliable local change notifications, and the projection is cheap because
//! each file is read incrementally. All merged state stays local; nothing is
//! written back to a file that another machine may have produced.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bevy::prelude::*;

use crate::canvas::manipulate::ZCounter;
use crate::canvas::node::{CanvasNode, FrameLabel, NodeKind, TabOwner};
use crate::canvas::selection::{Interaction, Selection};

use super::local;
use super::oplog::{self, Change};
use super::persist::{self, PreviewLoader};
use super::store::ProjectStore;
use super::tabs::{TabRequest, TabRequests, Tabs};

#[derive(Resource, Default)]
pub struct SyncTimer {
    poll: f32,
    cache: f32,
    retry: f32,
    checkpoint: f32,
    prune: f32,
}

/// Results of background folder-existence probes. A dead network share can
/// block a filesystem call for a long time, so the main thread never waits on
/// one.
#[derive(Resource)]
pub struct PathProbe {
    ready: Arc<Mutex<Vec<(PathBuf, bool)>>>,
    in_flight: HashSet<PathBuf>,
}

impl Default for PathProbe {
    fn default() -> Self {
        Self {
            ready: Arc::new(Mutex::new(Vec::new())),
            in_flight: HashSet::new(),
        }
    }
}

/// True while remote changes are waiting to be applied. Used by the idle
/// throttle so the app keeps stepping while reconciliation is in progress.
pub fn has_pending(tabs: &Tabs) -> bool {
    tabs.stored
        .iter()
        .any(|tab| tab.loaded && !tab.pending.is_empty())
}

pub fn poll_project_files(time: Res<Time>, mut timer: ResMut<SyncTimer>, mut tabs: ResMut<Tabs>) {
    timer.poll += time.delta_secs();
    if timer.poll < 1.0 {
        return;
    }
    timer.poll = 0.0;

    let mut newest: Option<oplog::Rev> = None;
    for tab in tabs.stored.iter_mut() {
        match oplog::read_project(&tab.store.root, &mut tab.merge, &mut tab.files) {
            Ok(changes) => {
                if !changes.is_empty() {
                    // An unloaded tab will replay the merged state when it is
                    // first activated; queueing live changes for it would keep
                    // the idle loop busy for no visible effect.
                    if tab.loaded {
                        tab.pending.extend(changes);
                    }
                    tab.state_dirty = true;
                    tab.checkpoint_dirty = true;
                }
            }
            Err(err) => debug!(
                "project read failed ({}): {err:#}",
                tab.store.root.display()
            ),
        }
        if let Some(rev) = tab.merge.max_seen() {
            let replace = match &newest {
                Some(current) => rev > current,
                None => true,
            };
            if replace {
                newest = Some(rev.clone());
            }
        }
    }
    if let Some(rev) = newest {
        tabs.clock.observe(&rev);
    }

    timer.cache += 1.0;
    if timer.cache >= 15.0 {
        timer.cache = 0.0;
        for tab in tabs.stored.iter_mut() {
            if tab.state_dirty {
                tab.state_dirty = false;
                local::save_state_async(
                    tab.store.root.clone(),
                    tab.merge.clone(),
                    tab.files.clone(),
                );
            }
        }
    }

    timer.prune += 1.0;
    if timer.prune >= 3600.0 {
        timer.prune = 0.0;
        std::thread::spawn(|| {
            local::sweep_missing(Duration::from_secs(30 * 24 * 60 * 60));
        });
    }

    timer.checkpoint += 1.0;
    if timer.checkpoint >= 900.0 {
        timer.checkpoint = 0.0;
        let device = tabs.clock.device_id().to_string();
        for tab in tabs.stored.iter_mut() {
            if !tab.checkpoint_dirty {
                continue;
            }
            // Snapshotting a large board is better done off the frame thread;
            // the journal is already durable, so a missed checkpoint only
            // costs startup replay time.
            tab.checkpoint_dirty = false;
            let dir = tab.store.checkpoint_dir();
            let state = tab.merge.clone();
            let device = device.clone();
            std::thread::spawn(move || {
                if let Err(err) = oplog::write_checkpoint(&dir, &device, &state) {
                    warn!("checkpoint write failed: {err:#}");
                }
            });
        }
    }
}

/// Flush local scene changes before resolving queued remote edits. A gesture
/// can finish before the next autosave; without this, a remote revision that
/// was queued earlier could overwrite work that just happened locally.
pub fn flush_local_before_remote(
    mut tabs: ResMut<Tabs>,
    interaction: Res<Interaction>,
    mut q: Query<(Entity, &Transform, &mut CanvasNode, &Sprite, &TabOwner)>,
) {
    if !interaction.is_idle() {
        return;
    }
    let tabs = &mut *tabs;
    for index in 0..tabs.stored.len() {
        if tabs.stored[index].pending.is_empty() || !tabs.stored[index].loaded {
            continue;
        }
        let clock = &mut tabs.clock;
        let tab = &mut tabs.stored[index];
        if persist::save_nodes(
            tab.id,
            &mut tab.journal,
            &mut tab.merge,
            &mut tab.state,
            clock,
            &mut q,
        ) {
            tab.state_dirty = true;
            tab.checkpoint_dirty = true;
        }
    }
}

/// Apply merged remote changes to the live scene.
#[allow(clippy::too_many_arguments)]
pub fn apply_remote_changes(
    mut commands: Commands,
    mut tabs: ResMut<Tabs>,
    mut images: ResMut<Assets<Image>>,
    mut zc: ResMut<ZCounter>,
    mut loader: ResMut<PreviewLoader>,
    interaction: Res<Interaction>,
    mut selection: ResMut<Selection>,
    mut q: Query<(
        Entity,
        &mut Transform,
        &mut CanvasNode,
        &mut Sprite,
        &TabOwner,
    )>,
    mut labels: Query<(&FrameLabel, &mut Text2d)>,
) {
    // Do not yank a node out from under a gesture in progress; changes stay
    // queued and are applied as soon as the pointer is idle again.
    if !interaction.is_idle() {
        return;
    }
    let active = tabs.active_id();
    let mut placeholder: Option<Handle<Image>> = None;

    for index in 0..tabs.stored.len() {
        if tabs.stored[index].pending.is_empty() || !tabs.stored[index].loaded {
            continue;
        }
        let changes = std::mem::take(&mut tabs.stored[index].pending);
        let tab_id = tabs.stored[index].id;
        let visible = active == Some(tab_id);
        let store = tabs.stored[index].store.clone();

        // Several queued ops may touch the same node; resolve each id to its
        // final merged value before touching the scene. This also avoids
        // double spawns when a node's kind changes, because entity commands are
        // deferred until the end of the system.
        let mut ids = Vec::new();
        let mut touched = HashSet::new();
        for change in changes {
            let id = match change {
                Change::Upsert(id) | Change::Remove(id) => id,
            };
            if touched.insert(id) {
                ids.push(id);
            }
        }
        let resolved: Vec<(oplog::NodeId, Option<oplog::SavedNode>, bool)> = ids
            .into_iter()
            .filter_map(|id| {
                tabs.stored[index]
                    .merge
                    .nodes
                    .get(&id)
                    .map(|value| (id, value.node.clone(), value.deleted))
            })
            .collect();

        for (id, node, deleted) in resolved {
            if deleted || node.is_none() {
                let existing = q
                    .iter()
                    .find(|(_, _, node, _, owner)| owner.0 == tab_id && node.node_id == Some(id))
                    .map(|(entity, ..)| entity);
                if let Some(entity) = existing {
                    commands.entity(entity).despawn();
                    selection.nodes.retain(|selected| *selected != entity);
                }
                tabs.stored[index].state.saved.remove(&id);
                tabs.stored[index].state_dirty = true;
                continue;
            }
            let saved = persist::normalize_saved(node.unwrap());
            zc.0 = zc.0.max(saved.z);
            let existing = q
                .iter()
                .find(|(_, _, node, _, owner)| owner.0 == tab_id && node.node_id == Some(id))
                .map(|(entity, ..)| entity);

            let mut spawned = false;
            if let Some(entity) = existing {
                let same_kind = {
                    let node = q.get(entity).map(|(_, _, node, _, _)| node).ok();
                    matches!(
                        node.map(|node| (&node.kind, saved.kind.as_str())),
                        Some((NodeKind::Image(_), "image")) | Some((NodeKind::Frame(_), "frame"))
                    )
                };
                if same_kind {
                    if let Ok((_, mut tf, mut node, mut sprite, _)) = q.get_mut(entity) {
                        patch_entity(
                            entity,
                            &saved,
                            &mut tf,
                            &mut node,
                            &mut sprite,
                            &store,
                            &mut loader,
                            tab_id,
                            &mut labels,
                        );
                    }
                } else {
                    commands.entity(entity).despawn();
                    selection.nodes.retain(|selected| *selected != entity);
                    spawned = true;
                }
            }

            if existing.is_none() || spawned {
                let handle = placeholder
                    .get_or_insert_with(|| images.add(persist::pending_placeholder()))
                    .clone();
                persist::spawn_saved_node(
                    &mut commands,
                    tab_id,
                    id,
                    &saved,
                    visible,
                    &store,
                    &handle,
                    &mut loader,
                    &mut zc,
                );
            }

            let baseline = if let Some(entity) = existing.filter(|_| !spawned) {
                if let Ok((_, tf, node, sprite, _)) = q.get(entity) {
                    persist::saved_from(tf, node, sprite)
                } else {
                    saved
                }
            } else {
                saved
            };
            tabs.stored[index].state.saved.insert(id, baseline);
            tabs.stored[index].state_dirty = true;
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn patch_entity(
    entity: Entity,
    saved: &oplog::SavedNode,
    tf: &mut Transform,
    node: &mut CanvasNode,
    sprite: &mut Sprite,
    store: &ProjectStore,
    loader: &mut PreviewLoader,
    tab_id: u64,
    labels: &mut Query<(&FrameLabel, &mut Text2d)>,
) {
    tf.translation = Vec3::new(saved.x, saved.y, saved.z);
    tf.rotation = Quat::from_rotation_z(saved.rot);
    node.size = Vec2::new(saved.w, saved.h);
    node.rotation = saved.rot;
    sprite.custom_size = Some(node.size);
    sprite.flip_x = saved.flip_x;
    sprite.flip_y = saved.flip_y;

    match &mut node.kind {
        NodeKind::Image(img) => {
            let natural = UVec2::new(
                saved.img_w.unwrap_or(1).max(1) as u32,
                saved.img_h.unwrap_or(1).max(1) as u32,
            );
            let has_crop = matches!(
                (saved.crop_w, saved.crop_h),
                (Some(w), Some(h)) if w > 0.0 && h > 0.0
            );
            let crop = if has_crop {
                Rect::from_corners(
                    Vec2::new(saved.crop_x.unwrap_or(0.0), saved.crop_y.unwrap_or(0.0)),
                    Vec2::new(
                        saved.crop_x.unwrap_or(0.0) + saved.crop_w.unwrap_or(0.0),
                        saved.crop_y.unwrap_or(0.0) + saved.crop_h.unwrap_or(0.0),
                    ),
                )
            } else {
                crate::canvas::node::ImageData::full_crop(natural)
            };
            let px_scale = saved.px_scale.filter(|p| *p > 0.0).unwrap_or_else(|| {
                let nat = natural.as_vec2().max(Vec2::ONE);
                (saved.w / nat.x).max(saved.h / nat.y).max(1e-4)
            });

            let rel_path = saved.src.clone().unwrap_or_default();
            let hash = saved.hash.clone().unwrap_or_default();
            let changed = img.data.hash != hash || img.data.rel_path != rel_path;
            if changed {
                img.data.rel_path = rel_path;
                img.data.abs_path = store.root.join(&img.data.rel_path);
                img.data.orig_name = saved.orig_name.clone().unwrap_or_default();
                img.data.hash = hash;
                img.data.natural = natural;
                img.data.preview_pending = true;
                img.full = None;
                img.full_incoming = None;
                img.pending_cpu_free = None;
                img.resident_bytes = 0;
                sprite.image = img.data.preview.clone();
                loader.enqueue(
                    tab_id,
                    entity,
                    img.data.abs_path.clone(),
                    store.cache_dir(),
                    img.data.hash.clone(),
                );
            }
            img.data.crop = crop;
            img.data.px_scale = px_scale;
            sprite.rect = Some(img.active_rect());
        }
        NodeKind::Frame(frame) => {
            let label = saved.label.clone().unwrap_or_else(|| "Frame".to_string());
            frame.label = label.clone();
            for (target, mut text) in labels.iter_mut() {
                if target.0 == entity {
                    text.0 = label.clone();
                }
            }
        }
    }
}

pub fn retry_unavailable(
    time: Res<Time>,
    mut timer: ResMut<SyncTimer>,
    mut tabs: ResMut<Tabs>,
    mut requests: ResMut<TabRequests>,
    mut probe: ResMut<PathProbe>,
) {
    timer.retry += time.delta_secs();
    if timer.retry < 5.0 {
        return;
    }
    timer.retry = 0.0;

    let results = std::mem::take(&mut *probe.ready.lock().unwrap());
    for (path, exists) in results {
        probe.in_flight.remove(&path);
        if exists && tabs.unavailable.iter().any(|pending| pending == &path) {
            tabs.unavailable.retain(|pending| pending != &path);
            requests.queue.push(TabRequest::OpenRestored(path));
        }
    }

    for path in tabs.unavailable.iter().cloned() {
        if !probe.in_flight.insert(path.clone()) {
            continue;
        }
        let ready = probe.ready.clone();
        std::thread::spawn(move || {
            let exists = path.is_dir();
            if let Ok(mut ready) = ready.lock() {
                ready.push((path, exists));
            }
        });
    }
}

/// Local state for a folder that is gone is dropped only after a long grace
/// period, so an unmounted drive or an offline share does not lose the camera.
pub fn prune_local_state() {
    std::thread::spawn(|| {
        local::sweep_missing(Duration::from_secs(30 * 24 * 60 * 60));
    });
}

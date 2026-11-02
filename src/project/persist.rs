//! Per-tab persistence: loading merged state into the ECS, streaming preview
//! textures, writing local edits to the operation journal, and autosaving.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use bevy::prelude::*;
use bevy::tasks::{AsyncComputeTaskPool, Task, futures::check_ready};

use crate::canvas::camera::View;
use crate::canvas::frames;
use crate::canvas::lod::{FrameCounter, GpuTextureState};
use crate::canvas::manipulate::ZCounter;
use crate::canvas::node::{
    CanvasNode, ImageData, ImageNode, NodeKind, Pose, TabOwner, to_bevy_image,
};
use uuid::Uuid;

use super::import::{read_preview_cache, write_preview_cache};
use super::local;
use super::oplog::{self, Clock, Journal, MergeState, NodeId, Op, SavedNode};
use super::store::ProjectStore;
use super::tabs::Tabs;

/// Per-project save bookkeeping (last-known state of every node).
#[derive(Default)]
pub struct PersistenceState {
    pub saved: HashMap<NodeId, SavedNode>,
    pub seconds_since_save: f32,
}

#[derive(Resource, Default)]
pub struct PreviewLoader {
    queues: HashMap<u64, VecDeque<PreviewJob>>,
    in_flight: Vec<(PreviewJob, Task<Regen>)>,
    /// Failed decodes whose source file has not arrived yet. Delayed so a
    /// missing file does not spin every frame.
    retry: Vec<(Instant, PreviewJob)>,
    active: Option<u64>,
}

impl PreviewLoader {
    /// True while previews are still queued or regenerating. The idle throttle
    /// keeps the loop at full speed until this drains.
    pub fn is_active(&self) -> bool {
        !self.in_flight.is_empty()
            || self
                .active
                .and_then(|id| self.queues.get(&id))
                .is_some_and(|q| !q.is_empty())
    }

    pub fn activate(&mut self, tab_id: u64) {
        if self.active == Some(tab_id) {
            return;
        }
        if let Some(previous) = self.active {
            let queue = self.queues.entry(previous).or_default();
            // Dropping tasks cancels queued work; a running decode may finish once.
            for (job, _task) in self.in_flight.drain(..) {
                queue.push_front(job);
            }
        }
        self.active = Some(tab_id);
    }

    pub fn forget(&mut self, tab_id: u64) {
        self.queues.remove(&tab_id);
        self.retry.retain(|(_, job)| job.tab_id != tab_id);
        if self.active == Some(tab_id) {
            self.in_flight.clear();
            self.active = None;
        }
    }

    pub fn enqueue(
        &mut self,
        tab_id: u64,
        entity: Entity,
        image_path: PathBuf,
        cache_dir: PathBuf,
        hash: String,
    ) {
        self.queues
            .entry(tab_id)
            .or_default()
            .push_back(PreviewJob {
                entity,
                tab_id,
                image_path,
                cache_dir,
                hash,
                attempts: 0,
            });
    }
}

#[derive(Clone)]
struct PreviewJob {
    entity: Entity,
    tab_id: u64,
    image_path: PathBuf,
    cache_dir: PathBuf,
    hash: String,
    attempts: u32,
}

struct Regen {
    entity: Entity,
    tab_id: u64,
    image_path: PathBuf,
    cache_dir: PathBuf,
    hash: String,
    attempts: u32,
    image: Option<Image>,
    size: UVec2,
}

const AUTOSAVE_INTERVAL: f32 = 1.5;
/// Cap on preview decodes in flight at once.
const MAX_PREVIEW_IN_FLIGHT: usize = 32;
/// Preview longest edge, pixels. The regenerator here and the pre-load size
/// guess in `node.rs` must both match `import::PREVIEW_MAX`.
pub const PREVIEW_MAX: u32 = 512;
/// Retry delay for a preview whose source file has not arrived yet.
const PREVIEW_RETRY_MIN: u64 = 2;
const PREVIEW_RETRY_MAX: u64 = 30;

/// Solid dark-gray texture bound to restored nodes whose preview hasn't loaded
/// yet: the node reads as a dim card instead of an invisible sprite.
pub(crate) fn pending_placeholder() -> Image {
    to_bevy_image(image::DynamicImage::ImageRgba8(
        image::RgbaImage::from_pixel(1, 1, image::Rgba([0x33, 0x33, 0x33, 0xFF])),
    ))
}

/// Load a project's merged nodes into the world (hidden when `visible` is
/// false). Returns the number of nodes spawned.
#[allow(clippy::too_many_arguments)]
pub fn load_into_world(
    store: &ProjectStore,
    merge: &MergeState,
    tab_id: u64,
    visible: bool,
    commands: &mut Commands,
    images: &mut Assets<Image>,
    zc: &mut ZCounter,
    loader: &mut PreviewLoader,
    state: &mut PersistenceState,
) -> anyhow::Result<usize> {
    let placeholder = images.add(pending_placeholder());
    let mut count = 0usize;
    let mut missing = 0usize;

    for (id, saved) in merge.visible_nodes() {
        let saved = normalize_saved(saved.clone());
        spawn_saved_node(
            commands,
            tab_id,
            id,
            &saved,
            visible,
            store,
            &placeholder,
            loader,
            zc,
        );
        if saved.kind == "image" {
            let abs_path = saved
                .src
                .as_ref()
                .map(|src| store.root.join(src))
                .unwrap_or_default();
            if !abs_path.exists() {
                missing += 1;
                if missing <= 5 {
                    warn!("image file missing: {}", abs_path.display());
                }
            }
        }
        state.saved.insert(id, saved);
        count += 1;
    }

    if missing > 0 {
        warn!(
            "{missing} image file(s) missing under {}",
            store.root.display()
        );
    }

    Ok(count)
}

/// Spawn one node from stored state and queue its preview if needed.
#[allow(clippy::too_many_arguments)]
pub fn spawn_saved_node(
    commands: &mut Commands,
    tab_id: u64,
    id: NodeId,
    saved: &SavedNode,
    visible: bool,
    store: &ProjectStore,
    placeholder: &Handle<Image>,
    loader: &mut PreviewLoader,
    zc: &mut ZCounter,
) -> Option<Entity> {
    zc.0 = zc.0.max(saved.z);
    if saved.kind == "frame" {
        return Some(frames::spawn_frame(
            commands,
            tab_id,
            visible,
            Some(id),
            saved.label.clone().unwrap_or_else(|| "Frame".to_string()),
            Pose {
                center: Vec2::new(saved.x, saved.y),
                size: Vec2::new(saved.w, saved.h),
                rot: saved.rot,
            },
            saved.z,
        ));
    }
    if saved.kind != "image" {
        return None;
    }
    let rel_path = saved.src.clone()?;
    let natural = UVec2::new(
        saved.img_w.unwrap_or(1).max(1) as u32,
        saved.img_h.unwrap_or(1).max(1) as u32,
    );
    let hash = saved.hash.clone().unwrap_or_default();
    let abs_path = store.root.join(&rel_path);
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
        ImageData::full_crop(natural)
    };
    let px_scale = saved.px_scale.filter(|p| *p > 0.0).unwrap_or_else(|| {
        let nat = natural.as_vec2().max(Vec2::ONE);
        (saved.w / nat.x).max(saved.h / nat.y).max(1e-4)
    });
    let size = crop.size() * px_scale;
    let pose = Pose {
        center: Vec2::new(saved.x, saved.y),
        size,
        rot: saved.rot,
    };
    let preview_size = ImageData::preview_size_for(natural);
    let data = ImageData {
        rel_path,
        abs_path: abs_path.clone(),
        orig_name: saved.orig_name.clone().unwrap_or_default(),
        hash: hash.clone(),
        natural,
        preview: placeholder.clone(),
        preview_size,
        preview_pending: true,
        crop,
        px_scale,
    };

    let entity = commands
        .spawn((
            CanvasNode {
                node_id: Some(id),
                kind: NodeKind::Image(ImageNode {
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
                image: placeholder.clone(),
                custom_size: Some(pose.size),
                rect: Some(ImageData::tier_rect_of(crop, natural, preview_size, false)),
                flip_x: saved.flip_x,
                flip_y: saved.flip_y,
                ..default()
            },
            Transform {
                translation: Vec3::new(pose.center.x, pose.center.y, saved.z),
                rotation: Quat::from_rotation_z(pose.rot),
                ..default()
            },
            TabOwner(tab_id),
            if visible {
                Visibility::Inherited
            } else {
                Visibility::Hidden
            },
        ))
        .id();

    if !visible {
        commands
            .entity(entity)
            .insert(crate::canvas::node::InactiveTab);
    }
    loader.enqueue(tab_id, entity, abs_path, store.cache_dir(), hash);
    Some(entity)
}

/// Save all nodes belonging to `tab_id` (insert / update / delete) as one
/// journal batch, then update the merge state and local bookkeeping.
pub fn save_nodes(
    tab_id: u64,
    journal: &mut Journal,
    merge: &mut MergeState,
    state: &mut PersistenceState,
    clock: &mut Clock,
    q: &mut Query<(Entity, &Transform, &mut CanvasNode, &Sprite, &TabOwner)>,
) -> bool {
    let t0 = Instant::now();

    let mut inserts: Vec<(Entity, NodeId, SavedNode)> = Vec::new();
    let mut updates: Vec<(NodeId, SavedNode)> = Vec::new();
    let mut current: HashSet<NodeId> = HashSet::new();

    for (e, tf, node, sprite, owner) in q.iter_mut() {
        if owner.0 != tab_id {
            continue;
        }
        let saved = saved_from(tf, &node, sprite);
        match node.node_id {
            None => {
                let id = Uuid::now_v7();
                current.insert(id);
                inserts.push((e, id, saved));
            }
            Some(id) => {
                current.insert(id);
                if state.saved.get(&id) != Some(&saved) {
                    updates.push((id, saved));
                }
            }
        }
    }

    let gone: Vec<NodeId> = state
        .saved
        .keys()
        .filter(|id| !current.contains(id))
        .copied()
        .collect();

    if inserts.is_empty() && updates.is_empty() && gone.is_empty() {
        return false;
    }

    let mut ops: Vec<Op> = Vec::with_capacity(inserts.len() + updates.len() + gone.len());
    for (_, id, node) in &inserts {
        ops.push(Op {
            id: *id,
            rev: clock.tick(),
            deleted: false,
            node: Some(node.clone()),
        });
    }
    for (id, node) in &updates {
        ops.push(Op {
            id: *id,
            rev: clock.tick(),
            deleted: false,
            node: Some(node.clone()),
        });
    }
    for id in &gone {
        ops.push(Op {
            id: *id,
            rev: clock.tick(),
            deleted: true,
            node: None,
        });
    }

    if let Err(err) = journal.append(ops.clone()) {
        warn!("journal append failed: {err:#}");
        return false;
    }
    local::save_clock(clock);
    for op in &ops {
        merge.merge(op.id, op.rev.clone(), op.deleted, op.node.clone());
    }

    for (e, id, saved) in inserts {
        if let Ok((_, _, mut node, _, _)) = q.get_mut(e) {
            node.node_id = Some(id);
        }
        state.saved.insert(id, saved);
    }
    for (id, saved) in updates {
        state.saved.insert(id, saved);
    }
    for id in gone {
        state.saved.remove(&id);
    }

    let ms = t0.elapsed().as_secs_f64() * 1000.0;
    if ms > 40.0 {
        debug!("save diag: {ms:.1} ms (slow save!)");
    }
    true
}

/// Fill in defaults that the scene derives on spawn, so the saved baseline
/// matches what the ECS will report on the next diff. Missing crop/scale fields
/// come from older rows and must not look like a local edit.
pub(crate) fn normalize_saved(mut saved: SavedNode) -> SavedNode {
    if saved.kind == "frame" {
        if saved.label.is_none() {
            saved.label = Some("Frame".to_string());
        }
        return saved;
    }
    if saved.kind != "image" {
        return saved;
    }
    let natural = Vec2::new(
        saved.img_w.unwrap_or(1).max(1) as f32,
        saved.img_h.unwrap_or(1).max(1) as f32,
    );
    if !matches!(
        (saved.crop_w, saved.crop_h),
        (Some(w), Some(h)) if w > 0.0 && h > 0.0
    ) {
        saved.crop_x = Some(0.0);
        saved.crop_y = Some(0.0);
        saved.crop_w = Some(natural.x);
        saved.crop_h = Some(natural.y);
    }
    if !saved.px_scale.is_some_and(|scale| scale > 0.0) {
        let natural = natural.max(Vec2::ONE);
        saved.px_scale = Some((saved.w / natural.x).max(saved.h / natural.y).max(1e-4));
    }
    if saved.orig_name.is_none() {
        saved.orig_name = Some(String::new());
    }
    if saved.hash.is_none() {
        saved.hash = Some(String::new());
    }
    saved
}

pub(crate) fn saved_from(tf: &Transform, node: &CanvasNode, sprite: &Sprite) -> SavedNode {
    let (src, orig_name, hash, img_w, img_h, kind, label, crop, px_scale) = match &node.kind {
        NodeKind::Image(img) => (
            Some(img.data.rel_path.clone()),
            Some(img.data.orig_name.clone()),
            Some(img.data.hash.clone()),
            Some(img.data.natural.x as i64),
            Some(img.data.natural.y as i64),
            "image",
            None,
            Some(img.data.crop),
            Some(img.data.px_scale),
        ),
        NodeKind::Frame(frame) => (
            None,
            None,
            None,
            None,
            None,
            "frame",
            Some(frame.label.clone()),
            None,
            None,
        ),
    };
    SavedNode {
        kind: kind.to_string(),
        x: tf.translation.x,
        y: tf.translation.y,
        w: node.size.x,
        h: node.size.y,
        rot: node.rotation,
        z: tf.translation.z,
        src,
        orig_name,
        hash,
        img_w,
        img_h,
        flip_x: sprite.flip_x,
        flip_y: sprite.flip_y,
        label,
        crop_x: crop.map(|c| c.min.x),
        crop_y: crop.map(|c| c.min.y),
        crop_w: crop.map(|c| c.size().x),
        crop_h: crop.map(|c| c.size().y),
        px_scale,
    }
}

/// Periodic autosave of the active tab plus a throttled local camera write.
pub fn autosave(
    time: Res<Time>,
    mut tabs: ResMut<Tabs>,
    view: Res<View>,
    mut q: Query<(Entity, &Transform, &mut CanvasNode, &Sprite, &TabOwner)>,
) {
    if tabs.stored.is_empty() {
        return;
    }
    let active = tabs.active.min(tabs.stored.len() - 1);
    let tabs = &mut *tabs;
    let clock = &mut tabs.clock;
    let tab = &mut tabs.stored[active];
    if !tab.loaded {
        return;
    }
    tab.state.seconds_since_save += time.delta_secs();
    if tab.state.seconds_since_save < AUTOSAVE_INTERVAL {
        return;
    }
    tab.state.seconds_since_save = 0.0;
    tab.view = (view.center, view.log_scale);

    if save_nodes(
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
    local::save_view(&tab.store.root, view.center, view.log_scale);
}

/// Stream preview textures in. Decoding runs on the async compute pool so a big
/// project loads across every core; the main thread only uploads finished
/// images and binds them.
pub fn pump_preview_loads(
    mut loader: ResMut<PreviewLoader>,
    mut images: ResMut<Assets<Image>>,
    mut tex_state: ResMut<GpuTextureState>,
    frame: Res<FrameCounter>,
    mut q: Query<(&mut CanvasNode, &mut Sprite), Without<crate::canvas::node::InactiveTab>>,
) {
    // Move due retries back into the queue.
    let now = Instant::now();
    let mut i = 0;
    while i < loader.retry.len() {
        if loader.retry[i].0 <= now {
            let (_, job) = loader.retry.swap_remove(i);
            if let Some(queue) = loader.queues.get_mut(&job.tab_id) {
                queue.push_front(job);
            }
        } else {
            i += 1;
        }
    }

    // Completed decodes.
    let mut i = 0;
    while i < loader.in_flight.len() {
        if let Some(regen) = check_ready(&mut loader.in_flight[i].1) {
            let _ = loader.in_flight.swap_remove(i);
            match regen.image {
                Some(image) => {
                    if let Ok((mut node, mut sprite)) = q.get_mut(regen.entity) {
                        let handle = images.add(image);
                        tex_state.touch(handle.id(), frame.0);
                        if let Some(img) = node.image_mut() {
                            img.data.preview = handle.clone();
                            img.data.preview_size = regen.size;
                            img.data.preview_pending = false;
                        }
                        let showing_full = node.image().map(|i| i.full.is_some()).unwrap_or(false);
                        if !showing_full {
                            sprite.image = handle;
                            if let Some(img) = node.image() {
                                sprite.rect = Some(img.active_rect());
                            }
                        }
                    }
                }
                None => {
                    // A missing source file usually means the blob is still on
                    // its way from another machine. Retry with a backoff. A
                    // file that exists but cannot be decoded may also be a
                    // partially replicated version, so give it a few attempts
                    // before treating it as permanently bad.
                    let exists = regen.image_path.exists();
                    let give_up = exists && regen.attempts >= 4;
                    if give_up {
                        if let Ok((mut node, _)) = q.get_mut(regen.entity)
                            && let Some(img) = node.image_mut()
                        {
                            img.data.preview_pending = false;
                        }
                        warn!(
                            "preview for {} not generated (file unreadable)",
                            regen.image_path.display()
                        );
                    } else {
                        let attempts = regen.attempts.saturating_add(1);
                        let delay = PREVIEW_RETRY_MIN
                            .saturating_mul(1u64 << attempts.min(4))
                            .min(PREVIEW_RETRY_MAX);
                        loader.retry.push((
                            Instant::now() + Duration::from_secs(delay),
                            PreviewJob {
                                entity: regen.entity,
                                tab_id: regen.tab_id,
                                image_path: regen.image_path,
                                cache_dir: regen.cache_dir,
                                hash: regen.hash,
                                attempts,
                            },
                        ));
                    }
                }
            }
        } else {
            i += 1;
        }
    }

    let Some(active) = loader.active else {
        return;
    };
    while loader.in_flight.len() < MAX_PREVIEW_IN_FLIGHT {
        let Some(job) = loader.queues.get_mut(&active).and_then(VecDeque::pop_front) else {
            break;
        };
        if q.get_mut(job.entity).is_err() {
            continue;
        }
        let PreviewJob {
            entity,
            tab_id,
            image_path,
            cache_dir,
            hash,
            attempts,
        } = job.clone();
        let task = AsyncComputeTaskPool::get().spawn(async move {
            let image_path_for_result = image_path.clone();
            let cache_dir_for_result = cache_dir.clone();
            let hash_for_result = hash.clone();
            if let Some(cached) = read_preview_cache(&cache_dir, &hash) {
                let size = UVec2::new(cached.width().max(1), cached.height().max(1));
                return Regen {
                    entity,
                    tab_id,
                    image_path: image_path_for_result,
                    cache_dir: cache_dir_for_result,
                    hash: hash_for_result,
                    attempts,
                    image: Some(to_bevy_image(cached)),
                    size,
                };
            }
            let loaded = std::fs::read(&image_path)
                .map_err(|err| format!("read failed: {err}"))
                .and_then(|bytes| {
                    image::load_from_memory(&bytes).map_err(|err| format!("decode failed: {err}"))
                });
            match loaded {
                Ok(dyn_img) => {
                    let preview = dyn_img.resize(
                        PREVIEW_MAX,
                        PREVIEW_MAX,
                        image::imageops::FilterType::CatmullRom,
                    );
                    let size = UVec2::new(preview.width().max(1), preview.height().max(1));
                    write_preview_cache(&cache_dir, &hash, &preview);
                    Regen {
                        entity,
                        tab_id,
                        image_path: image_path_for_result,
                        cache_dir: cache_dir_for_result,
                        hash: hash_for_result,
                        attempts,
                        image: Some(to_bevy_image(preview)),
                        size,
                    }
                }
                Err(err) => {
                    warn!(
                        "preview for {hash} not regenerated ({err}): {}",
                        image_path.display()
                    );
                    Regen {
                        entity,
                        tab_id,
                        image_path: image_path_for_result,
                        cache_dir: cache_dir_for_result,
                        hash: hash_for_result,
                        attempts,
                        image: None,
                        size: UVec2::ONE,
                    }
                }
            }
        });
        loader.in_flight.push((job, task));
    }
}

/// Persist local merged state and a checkpoint for this device on exit.
pub fn save_on_exit(tab: &super::tabs::OpenTab, device: &str) {
    local::save_state(&tab.store.root, &tab.merge, &tab.files);
    local::save_view(&tab.store.root, tab.view.0, tab.view.1);
    if let Err(err) = oplog::write_checkpoint(&tab.store.checkpoint_dir(), device, &tab.merge) {
        warn!("checkpoint write failed: {err:#}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(index: u32) -> PreviewJob {
        PreviewJob {
            entity: Entity::from_raw_u32(index).unwrap(),
            tab_id: index as u64,
            image_path: PathBuf::new(),
            cache_dir: PathBuf::new(),
            hash: index.to_string(),
            attempts: 0,
        }
    }

    #[test]
    fn inactive_previews_do_not_keep_the_app_busy() {
        let mut loader = PreviewLoader::default();
        loader.queues.entry(1).or_default().push_back(job(1));
        loader.activate(0);
        assert!(!loader.is_active());
        loader.activate(1);
        assert!(loader.is_active());
        loader.activate(0);
        assert!(!loader.is_active());
        assert_eq!(loader.queues[&1].len(), 1);
        loader.forget(1);
        assert!(!loader.queues.contains_key(&1));
    }

    #[test]
    fn switching_requeues_in_flight_previews_without_uploading_them() {
        let pool = bevy::tasks::TaskPoolBuilder::new().num_threads(1).build();
        let mut loader = PreviewLoader::default();
        loader.activate(0);
        loader
            .in_flight
            .push((job(1), pool.spawn(std::future::pending::<Regen>())));
        loader.activate(1);
        assert!(loader.in_flight.is_empty());
        assert_eq!(loader.queues[&0].len(), 1);
        assert!(!loader.is_active());
        loader.activate(0);
        assert!(loader.is_active());
        loader.forget(0);
        assert!(!loader.is_active());
    }
}

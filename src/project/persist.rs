//! Per-tab persistence: loading a project into the ECS, streaming preview
//! textures, and autosaving.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::time::Instant;

use bevy::prelude::*;
use bevy::tasks::{AsyncComputeTaskPool, Task, futures::check_ready};

use crate::canvas::camera::View;
use crate::canvas::frames;
use crate::canvas::lod::{FrameCounter, GpuTextureState};
use crate::canvas::manipulate::ZCounter;
use crate::canvas::node::{
    CanvasNode, ImageData, ImageNode, NodeKind, Pose, TabOwner, to_bevy_image,
};
use super::db::{NodeBatch, ProjectDb, SavedNode};
use super::import::{read_preview_cache, write_preview_cache};
use super::store::ProjectStore;
use super::tabs::Tabs;

/// Per-project save bookkeeping (last-known db state of every node).
#[derive(Default)]
pub struct PersistenceState {
    saved: HashMap<i64, SavedNode>,
    view_saved: Option<(f64, f64, f64)>,
    seconds_since_save: f32,
}

#[derive(Resource, Default)]
pub struct PreviewLoader {
    queues: HashMap<u64, VecDeque<PreviewJob>>,
    in_flight: Vec<(PreviewJob, Task<Regen>)>,
    active: Option<u64>,
}

impl PreviewLoader {
    /// True while previews are still queued or regenerating. The idle throttle
    /// keeps the loop at full speed until this drains: preview work is polled
    /// once per frame, so a sleeping loop would starve it.
    pub fn is_active(&self) -> bool {
        !self.in_flight.is_empty()
            || self.active.and_then(|id| self.queues.get(&id)).is_some_and(|q| !q.is_empty())
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
        if self.active == Some(tab_id) {
            self.in_flight.clear();
            self.active = None;
        }
    }
}

#[derive(Clone)]
struct PreviewJob {
    entity: Entity,
    image_path: PathBuf,
    cache_dir: PathBuf,
    hash: String,
}

struct Regen {
    entity: Entity,
    image: Option<Image>,
    size: UVec2,
}

const AUTOSAVE_INTERVAL: f32 = 1.5;
/// Cap on preview decodes in flight at once. Decoding runs on the async
/// compute pool, so this only bounds peak memory (each holds a decoded RGBA
/// image) while keeping every pool thread busy.
const MAX_PREVIEW_IN_FLIGHT: usize = 32;
/// Preview longest edge, pixels. The regenerator here and the pre-load size
/// guess in `node.rs` must both match `import::PREVIEW_MAX`.
pub const PREVIEW_MAX: u32 = 512;
const VIEW_EPS_POS: f64 = 0.5;
const VIEW_EPS_SCALE: f64 = 1e-4;

/// Solid dark-gray texture bound to restored nodes whose preview hasn't loaded
/// yet: the node reads as a dim card instead of an invisible sprite.
fn pending_placeholder() -> Image {
    to_bevy_image(image::DynamicImage::ImageRgba8(
        image::RgbaImage::from_pixel(
            1,
            1,
            image::Rgba([0x33, 0x33, 0x33, 0xFF]), // #333333, a step above the canvas
        ),
    ))
}

/// Load a project's nodes into the world (hidden when `visible` is false).
/// Returns the number of nodes and the stored camera, if any.
pub fn load_into_world(
    store: &ProjectStore,
    db: &ProjectDb,
    tab_id: u64,
    visible: bool,
    commands: &mut Commands,
    images: &mut Assets<Image>,
    zc: &mut ZCounter,
    loader: &mut PreviewLoader,
    state: &mut PersistenceState,
) -> anyhow::Result<(usize, Option<(f64, f64, f64)>)> {
    let loaded = db.load()?;
    let placeholder = images.add(pending_placeholder());
    let mut count = 0usize;
    let mut missing = 0usize;
    let mut max_z = 0.0f32;

    for row in &loaded.nodes {
        let s = &row.saved;
        if s.kind == "frame" {
            let pose = Pose {
                center: Vec2::new(s.x, s.y),
                size: Vec2::new(s.w, s.h),
                rot: s.rot,
            };
            frames::spawn_frame(
                commands,
                tab_id,
                visible,
                Some(row.id),
                s.label.clone().unwrap_or_else(|| "Frame".to_string()),
                pose,
                s.z,
            );
            state.saved.insert(row.id, s.clone());
            max_z = max_z.max(s.z);
            count += 1;
            continue;
        }
        if s.kind != "image" {
            continue;
        }
        let Some(rel_path) = s.src.clone() else {
            continue;
        };
        let natural = UVec2::new(
            s.img_w.unwrap_or(1).max(1) as u32,
            s.img_h.unwrap_or(1).max(1) as u32,
        );
        let hash = s.hash.clone().unwrap_or_default();
        let abs_path = store.root.join(&rel_path);
        // A missing original is the usual reason a node stays an empty
        // placeholder: report it up front instead of silently waiting.
        if !abs_path.exists() {
            missing += 1;
            if missing <= 5 {
                warn!("image file missing: {}", abs_path.display());
            }
        }
        // Crop/scale: legacy rows (columns NULL) default to the full image
        // scaled to the stored display size.
        let has_crop = matches!((s.crop_w, s.crop_h), (Some(w), Some(h)) if w > 0.0 && h > 0.0);
        let crop = if has_crop {
            Rect::from_corners(
                Vec2::new(s.crop_x.unwrap_or(0.0), s.crop_y.unwrap_or(0.0)),
                Vec2::new(
                    s.crop_x.unwrap_or(0.0) + s.crop_w.unwrap_or(0.0),
                    s.crop_y.unwrap_or(0.0) + s.crop_h.unwrap_or(0.0),
                ),
            )
        } else {
            ImageData::full_crop(natural)
        };
        let px_scale = s.px_scale.filter(|p| *p > 0.0).unwrap_or_else(|| {
            let nat = natural.as_vec2().max(Vec2::ONE);
            (s.w / nat.x).max(s.h / nat.y).max(1e-4)
        });
        let size = crop.size() * px_scale;
        let pose = Pose {
            center: Vec2::new(s.x, s.y),
            size,
            rot: s.rot,
        };
        // Best-guess preview size until the real cached preview loads (which
        // updates this via `pump_preview_loads`). Aspect-preserving, like the
        // preview generator.
        let preview_size = ImageData::preview_size_for(natural);
        let data = ImageData {
            rel_path: rel_path.clone(),
            abs_path: abs_path.clone(),
            orig_name: s.orig_name.clone().unwrap_or_default(),
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
                    db_id: Some(row.id),
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
                    // Preview tier (see `ImageData::tier_rect_of`).
                    rect: Some(ImageData::tier_rect_of(
                        crop,
                        natural,
                        preview_size,
                        false,
                    )),
                    flip_x: s.flip_x,
                    flip_y: s.flip_y,
                    ..default()
                },
                Transform {
                    translation: Vec3::new(pose.center.x, pose.center.y, s.z),
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

        loader.queues.entry(tab_id).or_default().push_back(PreviewJob {
            entity,
            image_path: abs_path,
            cache_dir: store.cache_dir(),
            hash,
        });
        state.saved.insert(row.id, s.clone());
        max_z = max_z.max(s.z);
        count += 1;
    }

    if missing > 0 {
        warn!(
            "{missing} image file(s) missing under {}",
            store.root.display()
        );
    }

    if let Some(view) = loaded.view {
        state.view_saved = Some(view);
    }
    zc.0 = zc.0.max(max_z);
    Ok((count, loaded.view))
}

/// Save all nodes belonging to `tab_id` (insert / update / delete) plus the
/// camera state, if anything changed, as one batched transaction (single
/// fsync), so a large selection never freezes the frame while dragging.
pub fn save_nodes(
    db: &ProjectDb,
    tab_id: u64,
    state: &mut PersistenceState,
    view: &View,
    q: &mut Query<(Entity, &Transform, &mut CanvasNode, &Sprite, &TabOwner)>,
) {
    let t0 = Instant::now();

    // Diff first, write once.
    let mut inserts: Vec<(Entity, SavedNode)> = Vec::new();
    let mut updates: Vec<(i64, SavedNode)> = Vec::new();
    let mut current: HashSet<i64> = HashSet::new();

    for (e, tf, node, sprite, owner) in q.iter_mut() {
        if owner.0 != tab_id {
            continue;
        }
        let saved = saved_from(tf, &node, sprite);
        match node.db_id {
            None => inserts.push((e, saved)),
            Some(id) => {
                current.insert(id);
                if state.saved.get(&id) != Some(&saved) {
                    updates.push((id, saved));
                }
            }
        }
    }

    let gone: Vec<i64> = state
        .saved
        .keys()
        .filter(|id| !current.contains(id))
        .copied()
        .collect();

    let v = (view.center.x, view.center.y, view.log_scale);
    let view_changed = match state.view_saved {
        Some(prev) => {
            (prev.0 - v.0).abs() > VIEW_EPS_POS
                || (prev.1 - v.1).abs() > VIEW_EPS_POS
                || (prev.2 - v.2).abs() > VIEW_EPS_SCALE
        }
        None => true,
    };

    if inserts.is_empty() && updates.is_empty() && gone.is_empty() && !view_changed {
        return;
    }

    let batch = NodeBatch {
        inserts: inserts.iter().map(|(_, n)| n).collect(),
        updates: updates.iter().map(|(id, n)| (*id, n)).collect(),
        deletes: gone.clone(),
        view: view_changed.then_some(v),
    };
    let new_ids = match db.apply_batch(&batch) {
        Ok(ids) => ids,
        Err(err) => {
            warn!("db save failed: {err:#}");
            return;
        }
    };

    // Bookkeeping only after the write succeeded.
    for ((e, saved), id) in inserts.into_iter().zip(new_ids) {
        if let Ok((_, _, mut node, _, _)) = q.get_mut(e) {
            node.db_id = Some(id);
        }
        state.saved.insert(id, saved);
    }
    for (id, saved) in updates {
        state.saved.insert(id, saved);
    }
    for id in gone {
        state.saved.remove(&id);
    }
    if view_changed {
        state.view_saved = Some(v);
    }

    // Canary for save hitches (logged at debug level).
    let ms = t0.elapsed().as_secs_f64() * 1000.0;
    if ms > 40.0 {
        debug!("save diag: {ms:.1} ms (slow save!)");
    }
}

fn saved_from(tf: &Transform, node: &CanvasNode, sprite: &Sprite) -> SavedNode {
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

/// Periodic autosave of the active tab.
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
    save_nodes(&tab.db, tab.id, &mut tab.state, &view, &mut q);
}

/// Stream preview textures in. Decoding runs on the async compute pool (cache
/// hits and misses alike) so a big project loads across every core; the main
/// thread only uploads finished images and binds them.
pub fn pump_preview_loads(
    mut loader: ResMut<PreviewLoader>,
    mut images: ResMut<Assets<Image>>,
    mut tex_state: ResMut<GpuTextureState>,
    frame: Res<FrameCounter>,
    mut q: Query<(&mut CanvasNode, &mut Sprite), Without<crate::canvas::node::InactiveTab>>,
) {
    // Completed decodes.
    let mut i = 0;
    while i < loader.in_flight.len() {
        if let Some(regen) = check_ready(&mut loader.in_flight[i].1) {
            let _ = loader.in_flight.swap_remove(i);
            match regen.image {
                Some(image) => {
                    if let Ok((mut node, mut sprite)) = q.get_mut(regen.entity) {
                        let handle = images.add(image);
                        // The upload is deferred by the per-frame byte budget;
                        // tracking it here keeps the idle throttle from napping
                        // while textures are still queued (see `has_pending`).
                        tex_state.touch(handle.id(), frame.0);
                        if let Some(img) = node.image_mut() {
                            img.data.preview = handle.clone();
                            img.data.preview_size = regen.size;
                            img.data.preview_pending = false;
                        }
                        // Keep the full-res texture bound if it already finished
                        // loading: switching the sprite to the preview while
                        // `img.full` is Some would desync the rect (it's derived
                        // from the tier the node believes it is on).
                        let showing_full =
                            node.image().map(|i| i.full.is_some()).unwrap_or(false);
                        if !showing_full {
                            sprite.image = handle;
                            if let Some(img) = node.image() {
                                sprite.rect = Some(img.active_rect());
                            }
                        }
                    }
                }
                None => {
                    // Decode failed (missing or corrupt original). Stop
                    // treating the node as pending so the dim outline (and the
                    // idle throttle's busy signal) can't stay pinned forever.
                    if let Ok((mut node, _)) = q.get_mut(regen.entity) {
                        if let Some(img) = node.image_mut() {
                            img.data.preview_pending = false;
                        }
                    }
                }
            }
        } else {
            i += 1;
        }
    }

    // Start decodes up to the in-flight cap. Cache hits and misses both run on
    // the async compute pool: the main thread only uploads finished images, so
    // a large project loads across all pool threads instead of a one-per-frame
    // trickle that leaves the CPU idle.
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
            image_path,
            cache_dir,
            hash,
        } = job.clone();
        let task = AsyncComputeTaskPool::get().spawn(async move {
            // Cache hit: decode the small preview.
            if let Some(cached) = read_preview_cache(&cache_dir, &hash) {
                let size = UVec2::new(cached.width().max(1), cached.height().max(1));
                return Regen {
                    entity,
                    image: Some(to_bevy_image(cached)),
                    size,
                };
            }
            // Cache miss: regenerate from the original and cache it.
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
                        image: None,
                        size: UVec2::ONE,
                    }
                }
            }
        });
        loader.in_flight.push((job, task));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(index: u32) -> PreviewJob {
        PreviewJob {
            entity: Entity::from_raw_u32(index).unwrap(),
            image_path: PathBuf::new(),
            cache_dir: PathBuf::new(),
            hash: index.to_string(),
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
        loader.in_flight.push((job(1), pool.spawn(std::future::pending::<Regen>())));
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

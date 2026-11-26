//! texture LOD: previews always, full-res streamed for the active tab only,
//! LRU-evicted under a VRAM budget.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use bevy::asset::AssetId;
use bevy::prelude::*;
use bevy::render::render_asset::RenderAssets;
use bevy::render::texture::GpuImage;
use bevy::render::{ExtractSchedule, MainWorld, RenderApp};
use bevy::sprite_render::SpriteMeshMaterial;
use bevy::tasks::{AsyncComputeTaskPool, futures::check_ready};
use bevy::window::{PrimaryWindow, RequestRedraw};

use super::camera::{View, ViewMotion, world_per_logical_px};
use super::hit::{aabb_of, rects_overlap};
use super::node::{CanvasNode, FullLoadTask, ImageNode, InactiveTab, Pose, TabOwner, to_bevy_image_with_usage};
use crate::project::tabs::Tabs;

/// viewport expansion when deciding what to load.
const LOAD_MARGIN: f32 = 1.4;
/// start loading full-res once the preview is stretched past this fraction of
/// native.
const LOAD_THRESHOLD: f32 = 0.85;

/// monotonic frame counter for lru bookkeeping.
#[derive(Resource, Default)]
pub struct FrameCounter(pub u64);

/// files that already failed to load, so each logs once instead of once per
/// retry frame; an entry clears on success.
#[derive(Resource, Default)]
pub struct FullLoadWarns(HashSet<PathBuf>);

/// cooldown for a full-res decode whose file isn't there yet; remote blobs can
/// arrive after the node that references them.
#[derive(Component)]
pub struct FullLoadBackoff {
    until: f64,
}

/// backstop cap on resident full-res vram; memory is normally reclaimed
/// proactively. override with `REF_TEX_BUDGET_MB`.
#[derive(Resource)]
pub struct TexBudget {
    pub max_bytes: u64,
}

impl Default for TexBudget {
    fn default() -> Self {
        let mb = std::env::var("REF_TEX_BUDGET_MB")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok());
        Self {
            max_bytes: mb
                .map(|m| m.saturating_mul(1024 * 1024))
                .unwrap_or(3 * 1024 * 1024 * 1024), // 3 GiB
        }
    }
}

/// tunables for proactive full-res reclaim, overridable without a rebuild.
#[derive(Resource)]
pub struct TexTuning {
    /// keep full-res while magnification is at least this; must stay below
    /// [`LOAD_THRESHOLD`] or the tier flaps at the boundary. `REF_TEX_KEEP_RATIO` (0.6).
    pub keep_ratio: f32,
    /// seconds a texture must go unwanted before it is freed.
    /// `REF_TEX_EVICT_SECS` (0.5).
    pub evict_secs: f64,
    /// stillness required before freeing (never free mid-gesture).
    /// `REF_TEX_MOTION_IDLE_SECS` (0.15).
    pub motion_idle_secs: f64,
    /// cap on frees per frame so shedding a backlog can't spike a frame.
    /// `REF_TEX_EVICT_PER_FRAME` (6).
    pub max_evictions_per_frame: usize,
}

impl Default for TexTuning {
    fn default() -> Self {
        let f = |key: &str| std::env::var(key).ok().and_then(|v| v.trim().parse::<f64>().ok());
        Self {
            keep_ratio: f("REF_TEX_KEEP_RATIO")
                .unwrap_or(0.6)
                .clamp(0.05, 0.8) as f32,
            evict_secs: f("REF_TEX_EVICT_SECS").unwrap_or(0.5).max(0.0),
            motion_idle_secs: f("REF_TEX_MOTION_IDLE_SECS").unwrap_or(0.15).max(0.0),
            max_evictions_per_frame: f("REF_TEX_EVICT_PER_FRAME")
                .map(|v| v.max(1.0) as usize)
                .unwrap_or(6),
        }
    }
}

/// frames a texture change must age before the render world is trusted to have
/// prepared it (extraction lags one frame, plus margin).
const SETTLE_GRACE: u64 = 2;

/// full-res uploads waiting on the render world; don't mutate an image until
/// its previous change settles in `RenderAssets<GpuImage>`.
#[derive(Resource, Default)]
pub struct GpuTextureState {
    /// handle id → main-world frame of the change awaiting confirmation.
    unsettled: HashMap<AssetId<Image>, u64>,
}

impl GpuTextureState {
    /// true while a changed texture is still queued for upload; the idle throttle
    /// reads this to keep rendering until uploads drain.
    pub fn has_pending(&self) -> bool {
        !self.unsettled.is_empty()
    }

    /// true when the asset's last change reached the gpu (or the asset is gone),
    /// so mutating it again can't race the render queue.
    fn settled(&self, id: AssetId<Image>) -> bool {
        !self.unsettled.contains_key(&id)
    }

    /// record a change (add or mutation) the render world must confirm.
    pub(crate) fn touch(&mut self, id: AssetId<Image>, frame: u64) {
        self.unsettled.insert(id, frame);
    }
}

/// settles handles once their gpu image exists or the asset was dropped.
pub fn sync_gpu_textures(
    mut main_world: ResMut<MainWorld>,
    gpu_images: Res<RenderAssets<GpuImage>>,
) {
    let now = main_world
        .get_resource::<FrameCounter>()
        .map(|f| f.0)
        .unwrap_or_default();
    let candidates: Vec<(AssetId<Image>, u64)> = {
        let Some(state) = main_world.get_resource::<GpuTextureState>() else {
            return;
        };
        if state.unsettled.is_empty() {
            return;
        }
        state.unsettled.iter().map(|(id, f)| (*id, *f)).collect()
    };
    let settled: Vec<AssetId<Image>> = candidates
        .into_iter()
        .filter(|(id, touched)| {
            if now.saturating_sub(*touched) < SETTLE_GRACE {
                return false;
            }
            // prepared or dropped (evicted / despawned): nothing left to wait for.
            gpu_images.get(*id).is_some()
                || main_world
                    .get_resource::<Assets<Image>>()
                    .is_some_and(|images| !images.contains(*id))
        })
        .map(|(id, _)| id)
        .collect();
    if settled.is_empty() {
        return;
    }
    if let Some(mut state) = main_world.get_resource_mut::<GpuTextureState>() {
        for id in settled {
            state.unsettled.remove(&id);
        }
    }
}

/// wire [`sync_gpu_textures`] into the render app (no-op when headless).
pub fn register_gpu_texture_sync(app: &mut App) {
    let Some(render_app) = app.get_sub_app_mut(RenderApp) else {
        return;
    };
    render_app.add_systems(ExtractSchedule, sync_gpu_textures.ambiguous_with_all());
}

fn node_visible(node: &CanvasNode, tf: &Transform, view_rect: Rect) -> bool {
    let pose = Pose {
        center: tf.translation.truncate(),
        size: node.size,
        rot: node.rotation,
    };
    rects_overlap(aabb_of(&pose), view_rect)
}

/// on-screen magnification relative to the preview (screen px per preview texel).
/// uses the *cropped* region, so zooming into a small crop needs full-res sooner.
fn full_ratio(img: &ImageNode, node_size: Vec2, wpp: f32) -> f32 {
    let screen_max = node_size.max_element() / wpp;
    let preview_ratio = img.data.preview_size.max_element().max(1) as f32
        / img.data.natural.max_element().max(1) as f32;
    let visible_texels = img.data.crop.size().max_element().max(1.0);
    let preview_max = (visible_texels * preview_ratio).max(1.0);
    screen_max / preview_max
}

pub fn tick_frame(mut frame: ResMut<FrameCounter>) {
    frame.0 += 1;
}

pub fn suspend_inactive_textures(
    mut commands: Commands,
    mut nodes: Query<(Entity, &mut CanvasNode, &mut Sprite), Added<InactiveTab>>,
) {
    for (entity, mut node, mut sprite) in &mut nodes {
        commands.entity(entity).remove::<FullLoadTask>();
        let Some(img) = node.image_mut() else { continue };
        if img.full.take().is_some() {
            sprite.image = img.data.preview.clone();
            sprite.rect = Some(img.data.tier_rect(false));
        }
        img.full_incoming = None;
        img.pending_cpu_free = None;
        img.sprite_material = None;
        img.resident_bytes = 0;
        img.sampler_nearest = false;
    }
}

#[derive(Component)]
pub struct ParkedMesh(Mesh2d);

// bound material rebuilds/teardown on large tab switches.
pub(crate) const TAB_MESHES_PER_FRAME: usize = 256;

// after sprite material updates, so inactive tabs leave the render workload.
pub fn park_inactive_meshes(
    mut commands: Commands,
    meshes: Query<(Entity, &Mesh2d), With<InactiveTab>>,
) {
    for (entity, mesh) in meshes.iter().take(TAB_MESHES_PER_FRAME) {
        commands.entity(entity)
            .insert(ParkedMesh(mesh.clone()))
            .remove::<(Mesh2d, MeshMaterial2d<SpriteMeshMaterial>)>();
    }
}

pub fn resume_active_meshes(
    mut commands: Commands,
    meshes: Query<(Entity, &ParkedMesh), Without<InactiveTab>>,
) {
    for (entity, mesh) in meshes.iter().take(TAB_MESHES_PER_FRAME) {
        commands.entity(entity).insert(mesh.0.clone()).remove::<ParkedMesh>();
    }
}

/// max full-res loads in flight; a dense zoom would otherwise queue a dozen
/// ~33 MB decodes/uploads at once.
const MAX_FULL_LOADS_IN_FLIGHT: usize = 3;

#[allow(clippy::too_many_arguments)]
pub fn ensure_full_textures(
    window: Single<&Window, With<PrimaryWindow>>,
    camera: Single<(&Camera, &GlobalTransform)>,
    view: Res<View>,
    tabs: Res<Tabs>,
    time: Res<Time>,
    tuning: Res<TexTuning>,
    resize: Res<crate::WindowResize>,
    mut commands: Commands,
    mut q: Query<(
        Entity,
        &Transform,
        &mut CanvasNode,
        Option<&FullLoadTask>,
        Option<&FullLoadBackoff>,
        &TabOwner,
    ), Without<InactiveTab>>,
) {
    // no new decodes mid-resize: they'd finish and upload during the drag,
    // hitching the frames that must stay smooth.
    if resize.is_active() {
        return;
    }
    // cap loads per node instead of returning here: visibility/lru bookkeeping
    // must keep running while the cap is saturated.
    let mut in_flight = q
        .iter()
        .filter(|(_, _, _, task, _, _)| task.is_some())
        .count();
    let now = time.elapsed_secs_f64();
    let Some(active_id) = tabs.active_id() else {
        return;
    };
    let (camera, cam_tf) = *camera;
    let Some(wpp) = world_per_logical_px(camera, cam_tf) else {
        return;
    };
    if wpp <= 0.0 {
        return;
    }
    let wpp = wpp as f32;
    let view_rect =
        Rect::from_center_size(view.center.as_vec2(), window.size() * wpp * LOAD_MARGIN);

    for (entity, tf, mut node, task, backoff, owner) in &mut q {
        if owner.0 != active_id {
            continue;
        }
        let node_size = node.size;
        let visible = node_visible(&node, tf, view_rect);
        let Some(img) = node.image_mut() else {
            continue;
        };
        // "load" needs the full threshold, "keep" only the lower hysteresis one,
        // so hovering at the boundary doesn't drop and immediately reload.
        let ratio = full_ratio(img, node_size, wpp);
        let load_needed = visible && ratio >= LOAD_THRESHOLD;
        let keep_needed = visible && ratio >= tuning.keep_ratio;
        if img.full.is_some() {
            // refresh the lru stamp while wanted; letting it lapse marks it for release.
            if keep_needed {
                img.last_needed = now;
            }
            continue;
        }
        if img.full_incoming.is_some() {
            // decode done, waiting on the gpu; drop it if no longer wanted instead
            // of binding a texture the user navigated away from.
            if keep_needed {
                img.last_needed = now;
            } else {
                img.full_incoming = None;
            }
            continue;
        }
        if task.is_some() || !load_needed {
            continue;
        }
        if backoff.is_some_and(|backoff| backoff.until > now) {
            continue;
        }
        if in_flight >= MAX_FULL_LOADS_IN_FLIGHT {
            continue;
        }
        in_flight += 1;

        img.last_needed = now;
        let path = img.data.abs_path.clone();
        debug!("full-res request: {}", path.display());
        let task = AsyncComputeTaskPool::get().spawn(async move {
            use bevy::asset::RenderAssetUsages;
            let bytes = std::fs::read(&path).map_err(|err| format!("read failed: {err}"))?;
            let dyn_img = image::load_from_memory(&bytes)
                .map_err(|err| format!("decode failed: {err}"))?;
            Ok(to_bevy_image_with_usage(dyn_img, RenderAssetUsages::all()))
        });
        commands.entity(entity).insert(FullLoadTask(task));
    }
}

pub fn collect_full_textures(
    frame: Res<FrameCounter>,
    time: Res<Time>,
    resize: Res<crate::WindowResize>,
    mut commands: Commands,
    mut images: ResMut<Assets<Image>>,
    mut tex_state: ResMut<GpuTextureState>,
    mut warns: ResMut<FullLoadWarns>,
    mut q: Query<(Entity, &mut CanvasNode, &mut FullLoadTask), Without<InactiveTab>>,
) {
    // hold finished decodes until the drag settles; the upload would hitch it.
    if resize.is_active() {
        return;
    }
    for (entity, mut node, mut task) in &mut q {
        if let Some(result) = check_ready(&mut task.0) {
            commands.entity(entity).remove::<FullLoadTask>();
            match result {
                Ok(image) => {
                    commands.entity(entity).remove::<FullLoadBackoff>();
                    if let Some(img) = node.image() {
                        warns.0.remove(&img.data.abs_path);
                    }
                    debug!("full-res decoded: {}x{}", image.width(), image.height());
                    let handle = images.add(image);
                    // defer binding until the upload is confirmed; see GpuTextureState
                    tex_state.touch(handle.id(), frame.0);
                    if let Some(img) = node.image_mut() {
                        img.full_incoming = Some(handle);
                    }
                }
                Err(err) => {
                    // retry after a cooldown: a file still being replicated
                    // shouldn't be read every frame.
                    commands.entity(entity).insert(FullLoadBackoff {
                        until: time.elapsed_secs_f64() + 2.0,
                    });
                    // retried every cooldown while visible; report each file once.
                    if let Some(img) = node.image() {
                        let path = img.data.abs_path.clone();
                        if warns.0.insert(path.clone()) {
                            warn!("full-res image failed to load ({err}): {}", path.display());
                        }
                    }
                }
            }
        }
    }
}

/// bind full-res only after its gpu image exists; earlier binds sample a placeholder.
pub fn bind_pending_full_textures(
    tex_state: Res<GpuTextureState>,
    mut q: Query<(&mut CanvasNode, &mut Sprite), Without<InactiveTab>>,
) {
    for (mut node, mut sprite) in &mut q {
        let Some(img) = node.image_mut() else {
            continue;
        };
        let Some(handle) = img.full_incoming.clone() else {
            continue;
        };
        // wait for the texture to reach the gpu.
        if !tex_state.settled(handle.id()) {
            continue;
        }
        img.resident_bytes = img.data.natural.x as u64 * img.data.natural.y as u64 * 4;
        img.sampler_nearest = false;
        img.full = Some(handle.clone());
        img.pending_cpu_free = Some(handle.clone());
        img.full_incoming = None;
        sprite.image = handle;
        // full-res tier: rect becomes the natural-space crop.
        if let Some(img) = node.image() {
            sprite.rect = Some(img.active_rect());
        }
    }
}

/// swap full-res back to preview once `last_needed` lapses; the tex budget is
/// only a backstop. frees wait for the view to settle and are rate-limited.
pub fn evict_full_textures(
    time: Res<Time>,
    tuning: Res<TexTuning>,
    budget: Res<TexBudget>,
    resize: Res<crate::WindowResize>,
    motion: Res<ViewMotion>,
    mut redraw: MessageWriter<RequestRedraw>,
    mut q: Query<(Entity, &mut CanvasNode, &mut Sprite), Without<InactiveTab>>,
) {
    // no evictions mid-drag: a shrinking window would free textures the next
    // growth step needs again (thrash), and frees hitch the drag.
    if resize.is_active() {
        return;
    }
    let now = time.elapsed_secs_f64();
    // free only while the view is still, unless over the backstop, where we must
    // shed regardless of motion to stay bounded.
    let still = now - motion.last_change() > tuning.motion_idle_secs;

    let mut total: u64 = 0;
    let mut candidates: Vec<(f64, Entity)> = Vec::new();
    for (e, node, _) in &q {
        let Some(img) = node.image() else {
            continue;
        };
        if img.full.is_some() {
            total += img.resident_bytes;
            candidates.push((img.last_needed, e));
        } else if img.full_incoming.is_some() {
            // decoded but not yet bound (e.g. tab switched away): release if stale;
            // not counted in `total`.
            candidates.push((img.last_needed, e));
        }
    }
    if total <= budget.max_bytes && !still {
        return;
    }
    candidates.sort_by(|a, b| a.0.total_cmp(&b.0));

    let mut evicted = 0usize;
    let mut backlog = false;
    for (last, e) in candidates {
        // oldest first: once the oldest is neither stale nor over budget, nothing
        // after it can be.
        if now - last <= tuning.evict_secs && total <= budget.max_bytes {
            break;
        }
        if evicted >= tuning.max_evictions_per_frame {
            backlog = true;
            break;
        }
        let Ok((_, mut node, mut sprite)) = q.get_mut(e) else {
            continue;
        };
        let Some(img) = node.image_mut() else {
            continue;
        };
        if img.full.is_some() {
            total = total.saturating_sub(img.resident_bytes);
            img.full = None;
            img.sampler_nearest = false;
            img.pending_cpu_free = None;
            img.resident_bytes = 0;
            sprite.image = img.data.preview.clone();
            // back on the preview tier: scale the crop into preview space.
            sprite.rect = Some(img.data.tier_rect(false));
            evicted += 1;
        } else if img.full_incoming.is_some() {
            img.full_incoming = None;
            evicted += 1;
        }
    }
    if backlog {
        // keep frames coming so the backlog drains promptly.
        redraw.write(RequestRedraw);
    }
}

/// periodic vram accounting at debug level, so reclaim can be watched
/// (`RUST_LOG=refcanvas=debug`).
pub fn log_vram_stats(frame: Res<FrameCounter>, q: Query<&CanvasNode>) {
    if frame.0 % 240 != 0 {
        return;
    }
    let (mut full_b, mut full_n, mut prev_b, mut prev_n) = (0u64, 0usize, 0u64, 0usize);
    for node in &q {
        if let Some(img) = node.image() {
            if img.full.is_some() {
                full_b += img.resident_bytes;
                full_n += 1;
            }
            if !img.data.preview_pending {
                prev_b += img.data.preview_size.x as u64 * img.data.preview_size.y as u64 * 4;
                prev_n += 1;
            }
        }
    }
    debug!(
        "vram: full {:.1} MiB across {full_n}, preview {:.1} MiB across {prev_n}",
        full_b as f64 / (1024.0 * 1024.0),
        prev_b as f64 / (1024.0 * 1024.0),
    );
}

/// once a full-res texture is confirmed on the gpu, drop its cpu copy.
/// the asset stays (`data: None`) so the sampler can still be flipped at runtime.
pub fn release_cpu_image_data(
    frame: Res<FrameCounter>,
    mut images: ResMut<Assets<Image>>,
    mut tex_state: ResMut<GpuTextureState>,
    mut q: Query<&mut CanvasNode, Without<InactiveTab>>,
) {
    for mut node in &mut q {
        let Some(img) = node.image_mut() else {
            continue;
        };
        let Some(handle) = img.pending_cpu_free.clone() else {
            continue;
        };
        // never free before the gpu texture exists: a re-prepare could replace
        // the queued upload and leave it empty (see GpuTextureState).
        if !tex_state.settled(handle.id()) {
            continue;
        }
        if let Some(mut image) = images.get_mut(&handle) {
            image.data = None;
            tex_state.touch(handle.id(), frame.0);
        }
        img.pending_cpu_free = None;
    }
}

/// full-res switches to nearest above this screen-px/texel ratio, with hysteresis below.
const NEAREST_AT: f32 = 2.0;
const NEAREST_BACK: f32 = 1.6;

// 2d sort keys are cached on queue; re-insert the material to pick up a z change.
pub fn requeue_on_z_change(
    mut commands: Commands,
    mut seen: Local<std::collections::HashMap<Entity, f32>>,
    sprites: Query<(Entity, &Transform, &MeshMaterial2d<SpriteMeshMaterial>), Without<InactiveTab>>,
) {
    for (entity, tf, material) in &sprites {
        let z = tf.translation.z;
        if seen.insert(entity, z) != Some(z) {
            // first sight or a changed `z`: force a re-queue.
            commands
                .entity(entity)
                .insert(MeshMaterial2d(material.0.clone()));
        }
    }
}

/// pixel peeping: past [`NEAREST_AT`] screen px per texel, switch full-res to
/// nearest so image pixels are crisp; back to linear below [`NEAREST_BACK`].
pub fn update_texture_samplers(
    window: Single<&Window, With<PrimaryWindow>>,
    camera: Single<(&Camera, &GlobalTransform)>,
    view: Res<View>,
    frame: Res<FrameCounter>,
    tabs: Res<Tabs>,
    mut images: ResMut<Assets<Image>>,
    mut sprite_materials: ResMut<Assets<SpriteMeshMaterial>>,
    mut tex_state: ResMut<GpuTextureState>,
    mut q: Query<(
        &Transform,
        &mut CanvasNode,
        &mut Sprite,
        Option<&mut MeshMaterial2d<SpriteMeshMaterial>>,
        &TabOwner,
    ), Without<InactiveTab>>,
) {
    let Some(active_id) = tabs.active_id() else {
        return;
    };
    let (camera, cam_tf) = *camera;
    let Some(wpp) = world_per_logical_px(camera, cam_tf) else {
        return;
    };
    if wpp <= 0.0 {
        return;
    }
    let wpp = wpp as f32;
    let view_rect =
        Rect::from_center_size(view.center.as_vec2(), window.size() * wpp * LOAD_MARGIN);

    for (tf, mut node, mut sprite, material, owner) in &mut q {
        if owner.0 != active_id {
            continue;
        }
        let node_size = node.size;
        let visible = node_visible(&node, tf, view_rect);
        let Some(img) = node.image_mut() else {
            continue;
        };
        // heal tier desync: a set full texture must be the one on screen
        if let Some(full) = img.full.as_ref() {
            if sprite.image != *full {
                debug!("tier repair: sprite rebound to full-res");
                sprite.image = full.clone();
                sprite.rect = Some(img.active_rect());
            }
        }
        // twice a second, log the tier state of any node magnified past 1x.
        if frame.0 % 120 == 0 {
            let ratio =
                (node_size.max_element() / wpp) / img.data.crop.size().max_element().max(1.0);
            if ratio > 1.0 {
                let drawn_full = img
                    .full
                    .as_ref()
                    .map(|f| f == &sprite.image)
                    .unwrap_or(false);
                debug!(
                    "ratio={ratio:.1} full={} drawn_full={} on_screen={} nearest={} scale={:.2} phys={:?}",
                    img.full.is_some(),
                    drawn_full,
                    visible,
                    img.sampler_nearest,
                    window.scale_factor(),
                    window.resolution.physical_size(),
                );
            }
        }
        let Some(full) = img.full.clone() else {
            // back on the preview tier: forget our material so the next
            // full-res bind builds a fresh one.
            if img.sprite_material.is_some() {
                img.sprite_material = None;
            }
            continue;
        };

        // samplers are baked into cached sprite materials; rebuild after a flip
        // or when the material isn't ours.
        if let Some(mut mat) = material {
            let ours = img.sprite_material.as_ref().is_some_and(|h| h == &mat.0);
            if !ours {
                if let Some(current) = sprite_materials.get(&mat.0).cloned() {
                    let fresh = sprite_materials.add(current);
                    mat.0 = fresh.clone();
                    img.sprite_material = Some(fresh);
                }
            }
        }
        // a sampler flip re-uploads while cpu data is attached; wait for the
        // free to avoid a second hitch.
        if img.pending_cpu_free.is_some() {
            continue;
        }
        // on-screen test here, not via `last_needed`, so flipping doesn't depend
        // on loader bookkeeping (which pauses while loads are in flight).
        if !visible {
            continue;
        }
        let screen_max = node_size.max_element() / wpp;
        let texel_max = img.data.crop.size().max_element().max(1.0);
        let ratio = screen_max / texel_max;
        let want_nearest = if img.sampler_nearest {
            ratio > NEAREST_BACK
        } else {
            ratio > NEAREST_AT
        };
        if want_nearest != img.sampler_nearest {
            // don't flip while the previous change is queued; it could
            // re-prepare empty. flips a frame or two later.
            if !tex_state.settled(full.id()) {
                continue;
            }
            if let Some(mut image) = images.get_mut(&full) {
                debug!(
                    "sampler flip -> {} (ratio={ratio:.1})",
                    if want_nearest { "nearest" } else { "linear" }
                );
                image.sampler = if want_nearest {
                    bevy::image::ImageSampler::nearest()
                } else {
                    bevy::image::ImageSampler::linear()
                };
                img.sampler_nearest = want_nearest;
                // material carries the sampler; rebuild next frame.
                img.sprite_material = None;
                tex_state.touch(full.id(), frame.0);
            } else {
                debug!("sampler flip skipped (asset missing)");
            }
        }
    }
}

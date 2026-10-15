//! Texture level-of-detail: previews always, full-res streamed on demand,
//! LRU-evicted under a VRAM budget. Only the active tab's nodes are streamed.

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

/// Viewport expansion factor when deciding what to load.
const LOAD_MARGIN: f32 = 1.4;
/// Start loading full-res once the preview is stretched past this fraction
/// of its native resolution.
const LOAD_THRESHOLD: f32 = 0.85;

/// Monotonic frame counter for LRU bookkeeping.
#[derive(Resource, Default)]
pub struct FrameCounter(pub u64);

/// Full-res files that already failed to load. Keeps a failure logged once per
/// file instead of once per retry frame; an entry clears on a successful load.
#[derive(Resource, Default)]
pub struct FullLoadWarns(HashSet<PathBuf>);

/// Backstop for resident full-res VRAM. Normally memory is reclaimed
/// proactively (a texture is dropped once it stops being needed, panned away,
/// zoomed out, or on an inactive tab); this only caps the actively-viewed
/// working set if it grows unusually large. Override with `REF_TEX_BUDGET_MB`.
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

/// Tunables for proactive full-res reclaim. All overridable without a rebuild.
#[derive(Resource)]
pub struct TexTuning {
    /// Keep full-res while the on-screen magnification ratio is at least this.
    /// Must stay below [`LOAD_THRESHOLD`] (the load trigger) so the tier can't
    /// flap at the boundary. `REF_TEX_KEEP_RATIO` (default 0.6).
    pub keep_ratio: f32,
    /// Seconds a texture must go unwanted before it is freed.
    /// `REF_TEX_EVICT_SECS` (default 0.5).
    pub evict_secs: f64,
    /// Seconds of stillness required before freeing (frees never run during a
    /// gesture). `REF_TEX_MOTION_IDLE_SECS` (default 0.15).
    pub motion_idle_secs: f64,
    /// Cap on frees per frame, so shedding a big backlog can't spike a frame.
    /// `REF_TEX_EVICT_PER_FRAME` (default 6).
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

/// Main-world frames a texture change must age before the render world is
/// trusted to have prepared it (extraction lags the change by one frame, plus
/// one frame of margin).
const SETTLE_GRACE: u64 = 2;

/// Tracks freshly added or changed full-res textures until the render world has
/// prepared them (a `GpuImage` exists for the handle).
///
/// Freeing `Image::data` or flipping `Image::sampler` is only safe once the
/// previous version of the asset is on the GPU. Bevy prepares render assets
/// under a per-frame byte budget (`RenderAssetBytesPerFrame`) and drops the
/// old GPU texture as soon as a new version of the asset is extracted, *before*
/// that version is prepared. If another change then replaces the still-queued
/// version (e.g. the CPU copy is freed while the first upload is still queued),
/// the final version has neither pixel data nor a previous texture to reuse, so
/// Bevy builds an **empty** texture, the image renders fully transparent until
/// restart. This state gates those mutations: nothing touches an image asset
/// until its previous change has been seen in `RenderAssets<GpuImage>`
/// (settled in [`sync_gpu_textures`]).
#[derive(Resource, Default)]
pub struct GpuTextureState {
    /// Handle id → main-world frame of the change awaiting confirmation.
    unsettled: HashMap<AssetId<Image>, u64>,
}

impl GpuTextureState {
    /// True while any changed texture is still waiting on the render world:
    /// queued for upload under the per-frame byte budget. The idle throttle
    /// reads this so the loop keeps rendering until uploads drain, instead of
    /// trickling at the budget once per heartbeat.
    pub fn has_pending(&self) -> bool {
        !self.unsettled.is_empty()
    }

    /// True when the asset's last change has reached the GPU (or the asset is
    /// gone), so mutating it again cannot race the render queue.
    fn settled(&self, id: AssetId<Image>) -> bool {
        !self.unsettled.contains_key(&id)
    }

    /// Record a change (add or mutation) the render world must confirm.
    pub(crate) fn touch(&mut self, id: AssetId<Image>, frame: u64) {
        self.unsettled.insert(id, frame);
    }
}

/// Render-world half of [`GpuTextureState`]: settles changed handles once their
/// `GpuImage` shows up (or the asset was dropped, e.g. evicted). Runs in
/// `ExtractSchedule`, the only point where the render world can touch the
/// stopped main world.
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
            // Prepared, or dropped (evicted / node despawned): nothing left
            // to wait for.
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

/// Wire [`sync_gpu_textures`] into the render app (no-op when headless).
pub fn register_gpu_texture_sync(app: &mut App) {
    let Some(render_app) = app.get_sub_app_mut(RenderApp) else {
        return;
    };
    render_app.add_systems(ExtractSchedule, sync_gpu_textures.ambiguous_with_all());
}

/// Whether the node's world AABB overlaps the (expanded) view rect.
fn node_visible(node: &CanvasNode, tf: &Transform, view_rect: Rect) -> bool {
    let pose = Pose {
        center: tf.translation.truncate(),
        size: node.size,
        rot: node.rotation,
    };
    rects_overlap(aabb_of(&pose), view_rect)
}

/// On-screen magnification of an image, relative to its preview texture
/// (screen px per preview texel). Full-res is loaded once this passes
/// [`LOAD_THRESHOLD`] and kept until it falls below
/// [`TexTuning::keep_ratio`], the gap is the hysteresis that stops the tier
/// flapping at the boundary. Uses the *cropped* region, so zooming into a small
/// crop needs the full-res sooner.
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

// Bound material rebuilds and teardown during large tab switches.
pub(crate) const TAB_MESHES_PER_FRAME: usize = 256;

// Run after sprite material updates so inactive tabs leave the render workload.
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

/// Never more than this many full-res loads in flight: zooming into a dense
/// area would otherwise queue a dozen ~33 MB decodes/uploads at once.
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
        &TabOwner,
    ), Without<InactiveTab>>,
) {
    // A live window resize churns visibility every frame. Don't start new
    // decodes mid-drag: they would finish and upload while the user is still
    // dragging, hitching the very frames that must stay smooth. Resumes the
    // moment the drag settles.
    if resize.is_active() {
        return;
    }
    // Cap concurrent full-res loads (see MAX_FULL_LOADS_IN_FLIGHT). Enforced
    // per-node inside the loop instead of returning here: the visibility/LRU
    // bookkeeping below must keep running even while the cap is saturated.
    let mut in_flight = q.iter().filter(|(_, _, _, task, _)| task.is_some()).count();
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

    for (entity, tf, mut node, task, owner) in &mut q {
        if owner.0 != active_id {
            continue;
        }
        let node_size = node.size;
        let visible = node_visible(&node, tf, view_rect);
        let Some(img) = node.image_mut() else {
            continue;
        };
        // "Load" needs the full preview threshold; "keep" only the lower
        // hysteresis threshold, so a texture isn't dropped and immediately
        // reloaded when the user hovers around the boundary.
        let ratio = full_ratio(img, node_size, wpp);
        let load_needed = visible && ratio >= LOAD_THRESHOLD;
        let keep_needed = visible && ratio >= tuning.keep_ratio;
        if img.full.is_some() {
            // Refresh the LRU stamp while it's still wanted; letting it lapse
            // is what marks it for proactive release.
            if keep_needed {
                img.last_needed = now;
            }
            continue;
        }
        if img.full_incoming.is_some() {
            // Decode finished; waiting for the GPU before it is bound. If it's
            // no longer wanted, drop it instead of binding a texture the user
            // has already navigated away from.
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
    resize: Res<crate::WindowResize>,
    mut commands: Commands,
    mut images: ResMut<Assets<Image>>,
    mut tex_state: ResMut<GpuTextureState>,
    mut warns: ResMut<FullLoadWarns>,
    mut q: Query<(Entity, &mut CanvasNode, &mut FullLoadTask), Without<InactiveTab>>,
) {
    // Hold finished decodes until the drag settles: a full-res upload is a
    // multi-frame cost that would hitch the resize itself.
    if resize.is_active() {
        return;
    }
    for (entity, mut node, mut task) in &mut q {
        if let Some(result) = check_ready(&mut task.0) {
            commands.entity(entity).remove::<FullLoadTask>();
            match result {
                Ok(image) => {
                    if let Some(img) = node.image() {
                        warns.0.remove(&img.data.abs_path);
                    }
                    debug!("full-res decoded: {}x{}", image.width(), image.height());
                    let handle = images.add(image);
                    // The upload may be deferred by the per-frame byte budget:
                    // mark it in flight until the render world confirms it
                    // (see [`GpuTextureState`]). Do NOT bind it to the sprite
                    // yet; see [`bind_pending_full_textures`].
                    tex_state.touch(handle.id(), frame.0);
                    if let Some(img) = node.image_mut() {
                        img.full_incoming = Some(handle);
                    }
                }
                Err(err) => {
                    // Retried every frame while visible: report each file once.
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

/// Promote a decoded full-res texture to the visible tier once its `GpuImage`
/// has actually reached the render world (see `GpuTextureState`). Deferring the
/// sprite rebind this way keeps Bevy's sprite material from being created
/// against an `Image` that isn't prepared yet, which would leave the mesh
/// drawn with a placeholder material-slot binding (slab 0, slot 0) for a frame,
/// sampling an arbitrary *other* image.
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
        // Wait until the render world has the texture on the GPU.
        if !tex_state.settled(handle.id()) {
            continue;
        }
        img.resident_bytes = img.data.natural.x as u64 * img.data.natural.y as u64 * 4;
        img.sampler_nearest = false;
        img.full = Some(handle.clone());
        img.pending_cpu_free = Some(handle.clone());
        img.full_incoming = None;
        sprite.image = handle;
        // Switched to the full-res tier: the rect becomes the natural-space
        // crop (full-res textures are natural-sized).
        if let Some(img) = node.image() {
            sprite.rect = Some(img.active_rect());
        }
    }
}

/// Proactive release of full-res textures, so VRAM tracks what's actually
/// being looked at instead of only accumulating. A texture whose `last_needed`
/// stamp has lapsed past the grace window (panned away, zoomed out, or on an
/// inactive tab) is swapped back to its preview. The `TexBudget` remains only
/// as a backstop for the actively-viewed working set.
///
/// Frees are deferred while the view is moving (so fast zoom/pan never hitches
/// on a texture teardown or material rebuild) and rate-limited per frame (so
/// shedding a big backlog can't spike a frame). Nodes still stamped this frame
/// are never evicted, so there's no reload thrash.
pub fn evict_full_textures(
    time: Res<Time>,
    tuning: Res<TexTuning>,
    budget: Res<TexBudget>,
    resize: Res<crate::WindowResize>,
    motion: Res<ViewMotion>,
    mut redraw: MessageWriter<RequestRedraw>,
    mut q: Query<(Entity, &mut CanvasNode, &mut Sprite), Without<InactiveTab>>,
) {
    // Don't evict mid-drag: a shrinking window would free textures that the
    // next growth step needs again (thrash), and the frees hitch the drag.
    if resize.is_active() {
        return;
    }
    let now = time.elapsed_secs_f64();
    // Only free while the view is still, unless we're over the backstop, in
    // which case we must shed regardless of motion to stay bounded.
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
            // Decoded but not yet bound (e.g. the tab was switched away):
            // release it if it has gone stale. It isn't counted in `total`.
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
        // Oldest first: once the oldest is neither stale nor over budget,
        // nothing after it can be either.
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
            // Back on the preview tier: scale the crop into preview space.
            sprite.rect = Some(img.data.tier_rect(false));
            evicted += 1;
        } else if img.full_incoming.is_some() {
            img.full_incoming = None;
            evicted += 1;
        }
    }
    if backlog {
        // Keep frames coming so the backlog drains promptly instead of one
        // batch per idle heartbeat.
        redraw.write(RequestRedraw);
    }
}

/// Periodic VRAM accounting at debug level: resident full-res and preview
/// bytes, so reclaim can be watched (`RUST_LOG=refcanvas=debug`).
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

/// Once a freshly uploaded full-res texture is on the GPU (confirmed via
/// [`GpuTextureState`]), drop its CPU copy to reclaim memory. The main-world
/// asset stays (with `data: None`) so the sampler can still be flipped at
/// runtime.
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
        // Never free before the texture exists on the GPU: the now data-less
        // re-prepare could replace the still-queued first upload and leave an
        // empty (transparent) texture. See [`GpuTextureState`].
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

/// Screen px per image texel where the full-res tier flips to nearest sampling
/// when zooming in (pixel peeping), and back to linear below (hysteresis, so
/// browsing stays smooth). Tweak these numbers and save, the runner
/// hot-patches the flip. Lower `NEAREST_AT` = switches to pixely sooner.
const NEAREST_AT: f32 = 2.0;
const NEAREST_BACK: f32 = 1.6;

/// Bevy 0.20 precalculates 2D transparent sort keys when an item is queued and
/// never refreshes them on transform changes (`recalculate_sort_keys` is a
/// no-op for `Transparent2d`), so changing a node's `z` alone does not reorder
/// anything until the entity happens to be re-queued. Re-insert the entity's
/// material handle whenever its `z` changes: that marks `MeshMaterial2d` as
/// changed, which flags the entity for re-specialization and re-queues it with
/// its current sort key.
pub fn requeue_on_z_change(
    mut commands: Commands,
    mut seen: Local<std::collections::HashMap<Entity, f32>>,
    sprites: Query<(Entity, &Transform, &MeshMaterial2d<SpriteMeshMaterial>), Without<InactiveTab>>,
) {
    for (entity, tf, material) in &sprites {
        let z = tf.translation.z;
        if seen.insert(entity, z) != Some(z) {
            // First sight or a changed `z`: force a re-queue.
            commands
                .entity(entity)
                .insert(MeshMaterial2d(material.0.clone()));
        }
    }
}

/// Pixel peeping: once a full-res texture is magnified past [`NEAREST_AT`]
/// screen px per texel, switch it to nearest sampling so individual image
/// pixels are crisp. Flip back to linear below [`NEAREST_BACK`].
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
        // Heal a tier desync: whenever a full-res texture exists it must be
        // the one on screen. (Restore/preview races can leave the sprite
        // rebound to the preview while `full` stays set, the visible symptom
        // is a soft, blocky preview under the pixel-peeping zoom.)
        if let Some(full) = img.full.as_ref() {
            if sprite.image != *full {
                debug!("tier repair: sprite rebound to full-res");
                sprite.image = full.clone();
                sprite.rect = Some(img.active_rect());
            }
        }
        // Twice a second, for any node magnified past 1x, report which tier
        // is bound, whether it is on screen, and the current sampling mode,
        // useful when debugging the pixel-peeping chain at debug log level.
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
            // Back on the preview tier: forget our material so the next
            // full-res bind builds a fresh one.
            if img.sprite_material.is_some() {
                img.sprite_material = None;
            }
            continue;
        };

        // Own the material. Bevy renders `Sprite`s through cached
        // `SpriteMeshMaterial`s that bake the image sampler in when the
        // material is prepared, flipping `Image::sampler` alone never
        // reaches the screen. Rebuild a fresh material whenever the entity
        // no longer carries ours (the sprite changed and Bevy re-inserted its
        // cached one) or right after a flip. A frame passes between the flip
        // and this rebuild, so the `GpuImage` already has the new sampler.
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
        // A sampler flip re-prepares the render asset: that is free once the
        // CPU copy is gone (Bevy reuses the texture and only swaps the sampler),
        // but it re-uploads the whole image while the data is still attached.
        // `release_cpu_image_data` clears it a few frames after the load, wait
        // for that instead of causing a second copy of the first-zoom hitch.
        if img.pending_cpu_free.is_some() {
            continue;
        }
        // On-screen? Tested here, not via `last_needed`, so flipping
        // never depends on the loader's per-frame bookkeeping (which pauses
        // while loads are in flight).
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
            // Don't flip while the previous change to this image is still in
            // the render queue: the flip's re-prepare could replace it and be
            // prepared without a previous texture (empty texture; see
            // [`GpuTextureState`]). It flips a frame or two later.
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
                // The material carries the sampler; rebuild it next frame.
                img.sprite_material = None;
                tex_state.touch(full.id(), frame.0);
            } else {
                debug!("sampler flip skipped (asset missing)");
            }
        }
    }
}

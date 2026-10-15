//! Reference image import.
//!
//! Sources: OS drag & drop (via `FileDragAndDrop`), Ctrl+I dialog, the system
//! clipboard paste (Ctrl+V, files or raw image pixels, see `crate::clipboard`),
//! and context menus. Workflow per file, on worker threads:
//!
//! 1. read bytes, hash them (blake3, 16 hex chars)
//! 2. decode dimensions, copy bytes into `<root>/images/<hash>.<ext>`
//! 3. build + cache a preview (`<root>/cache/<hash>_512.{png|jpg}`)
//! 4. hand the decoded preview pixels back for GPU upload

use std::collections::VecDeque;
use std::path::{Path, PathBuf};

use bevy::prelude::*;
use bevy::tasks::{AsyncComputeTaskPool, Task, futures::check_ready};
use bevy::window::{FileDragAndDrop, PrimaryWindow};
use bevy_egui::EguiContexts;

use crate::canvas::camera::View;
use crate::canvas::manipulate::{ZCounter, spawn_image_node};
use crate::canvas::node::{ImageData, NodeSnapshot, Pose, to_bevy_image};
use crate::canvas::undo::{NodeData, NodeRecord, UndoEntry, UndoStack};
use crate::project::tabs::Tabs;

use super::store::ProjectStore;

/// Preview longest edge, pixels. Canonical preview cap: `persist`'s
/// regenerator and `node`'s pre-load size guess must both match it.
const PREVIEW_MAX: u32 = 512;
/// Max concurrent import decodes.
const MAX_IN_FLIGHT: usize = 6;
/// Max images pulled from one dropped folder (safety valve).
const MAX_FOLDER_FILES: usize = 1000;
/// Spacing of the auto-arranged drop grid, world units.
const DROP_GRID_CELL: f32 = 520.0;

/// Where an import comes from.
enum ImportSource {
    /// A file on disk (drag & drop, dialog, clipboard `text/uri-list`, …).
    File(PathBuf),
    /// Pixels read from the system clipboard: there is no source file, so
    /// they are encoded as PNG and stored like any other import.
    Pixels { name: String, image: image::RgbaImage },
}

impl ImportSource {
    /// Deterministic ordering within one batch.
    fn sort_key(&self) -> PathBuf {
        match self {
            ImportSource::File(path) => path.clone(),
            ImportSource::Pixels { name, .. } => PathBuf::from(name),
        }
    }
}

#[derive(Resource, Default)]
pub struct ImportQueue {
    pending: VecDeque<(usize, ImportSource)>,
    in_flight: Vec<Task<Incoming>>,
    batch_center: Vec2,
    batch_total: usize,
}

/// Dev helper: `refcanvas --import <folder>` imports a folder on startup.
#[derive(Resource)]
pub struct StartupImport(pub PathBuf);

struct Incoming {
    index: usize,
    result: anyhow::Result<ImportedImage>,
}

struct ImportedImage {
    rel_path: String,
    abs_path: PathBuf,
    orig_name: String,
    hash: String,
    natural: UVec2,
    preview_size: UVec2,
    preview: Image,
}

impl ImportQueue {
    pub fn is_active(&self) -> bool {
        !self.pending.is_empty() || !self.in_flight.is_empty()
    }

    /// (done, total) for the current batch, if any.
    pub fn progress(&self) -> Option<(usize, usize)> {
        if !self.is_active() || self.batch_total == 0 {
            return None;
        }
        let done = self
            .batch_total
            .saturating_sub(self.pending.len() + self.in_flight.len());
        Some((done.min(self.batch_total), self.batch_total))
    }
}

/// Queue paths for import, arranging multiple images in a grid centered on
/// `spawn_center` (world space). Folders are scanned for images.
pub fn enqueue_paths(queue: &mut ImportQueue, spawn_center: Vec2, paths: Vec<PathBuf>) {
    let mut sources = Vec::new();
    for p in paths {
        if p.is_dir() {
            let mut files = Vec::new();
            collect_images(&p, 0, &mut files);
            sources.extend(files.into_iter().map(ImportSource::File));
        } else if is_supported_image(&p) {
            sources.push(ImportSource::File(p));
        }
    }
    enqueue_sources(queue, spawn_center, sources);
}

/// Queue pixels pasted from the system clipboard as a single import.
pub fn enqueue_clipboard_image(
    queue: &mut ImportQueue,
    spawn_center: Vec2,
    image: image::RgbaImage,
) {
    enqueue_sources(
        queue,
        spawn_center,
        vec![ImportSource::Pixels {
            name: "clipboard.png".to_string(),
            image,
        }],
    );
}

fn enqueue_sources(queue: &mut ImportQueue, spawn_center: Vec2, mut sources: Vec<ImportSource>) {
    if sources.is_empty() {
        return;
    }
    sources.sort_by_cached_key(|s| s.sort_key()); // deterministic batch order
    sources.truncate(MAX_FOLDER_FILES);

    if !queue.is_active() {
        queue.batch_center = spawn_center;
        queue.batch_total = 0;
    }
    for source in sources {
        let idx = queue.batch_total;
        queue.batch_total += 1;
        queue.pending.push_back((idx, source));
    }
}

fn is_supported_image(p: &Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .map(|e| {
            matches!(
                e.to_ascii_lowercase().as_str(),
                "png" | "jpg" | "jpeg" | "webp" | "bmp" | "gif" | "tga" | "tif" | "tiff"
            )
        })
        .unwrap_or(false)
}

fn collect_images(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if depth > 4 || out.len() >= MAX_FOLDER_FILES {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut items: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| !p.file_name().is_some_and(|n| n.to_string_lossy().starts_with('.')))
        .collect();
    items.sort();
    for p in items {
        if out.len() >= MAX_FOLDER_FILES {
            return;
        }
        if p.is_dir() {
            collect_images(&p, depth + 1, out);
        } else if is_supported_image(&p) {
            out.push(p);
        }
    }
}

// ---------------------------------------------------------------------------
// Systems
// ---------------------------------------------------------------------------

pub fn handle_dropped_files(
    mut messages: MessageReader<FileDragAndDrop>,
    window: Single<&Window, With<PrimaryWindow>>,
    camera: Single<(&Camera, &GlobalTransform)>,
    view: Res<View>,
    mut queue: ResMut<ImportQueue>,
) {
    let mut paths = Vec::new();
    for msg in messages.read() {
        if let FileDragAndDrop::DroppedFile { path_buf, .. } = msg {
            paths.push(path_buf.clone());
        }
    }
    if paths.is_empty() {
        return;
    }
    let (camera, cam_tf) = *camera;
    let spawn_center = window
        .cursor_position()
        .and_then(|c| camera.viewport_to_world_2d(cam_tf, c).ok())
        .unwrap_or_else(|| view.center.as_vec2());
    enqueue_paths(&mut queue, spawn_center, paths);
}

/// Dev helper: import a folder given on the command line (`--import <dir>`).
pub fn startup_import(
    startup: Option<Res<StartupImport>>,
    mut queue: ResMut<ImportQueue>,
    mut commands: Commands,
) {
    let Some(startup) = startup else {
        return;
    };
    info!("startup import: {}", startup.0.display());
    enqueue_paths(&mut queue, Vec2::ZERO, vec![startup.0.clone()]);
    commands.remove_resource::<StartupImport>();
}

/// Ctrl+I: pick files with the native dialog (blocking modal).
pub fn dialog_import(
    keys: Res<ButtonInput<KeyCode>>,
    view: Res<View>,
    mut queue: ResMut<ImportQueue>,
    mut contexts: EguiContexts,
    chord_mem: Res<crate::canvas::ChordMemory>,
    chord_frame: Res<crate::canvas::lod::FrameCounter>,
) {
    let ctrl = crate::canvas::ctrl_down(&keys, &chord_mem, chord_frame.0);
    if !(ctrl && keys.just_pressed(KeyCode::KeyI)) {
        return;
    }
    let egui_busy = crate::canvas::egui_keyboard_busy(&mut contexts);
    if egui_busy {
        return;
    }
    let picked = rfd::FileDialog::new()
        .set_title("Import reference images")
        .add_filter(
            "Images",
            &["png", "jpg", "jpeg", "webp", "bmp", "gif", "tga", "tif", "tiff"],
        )
        .pick_files();
    if let Some(paths) = picked {
        enqueue_paths(&mut queue, view.center.as_vec2(), paths);
    }
}

pub fn pump_import_queue(mut queue: ResMut<ImportQueue>, store: Res<ProjectStore>) {
    while queue.in_flight.len() < MAX_IN_FLIGHT {
        let Some((index, source)) = queue.pending.pop_front() else {
            break;
        };
        let store = store.clone();
        let task = AsyncComputeTaskPool::get().spawn(async move {
            let result = import_one(&store, source);
            Incoming { index, result }
        });
        queue.in_flight.push(task);
    }
}

pub fn collect_imports(
    mut queue: ResMut<ImportQueue>,
    mut commands: Commands,
    mut images: ResMut<Assets<Image>>,
    mut zc: ResMut<ZCounter>,
    mut undo: ResMut<UndoStack>,
    tabs: Res<Tabs>,
) {
    let mut done: Vec<Incoming> = Vec::new();
    let mut i = 0;
    while i < queue.in_flight.len() {
        if let Some(result) = check_ready(&mut queue.in_flight[i]) {
            let _ = queue.in_flight.swap_remove(i);
            done.push(result);
        } else {
            i += 1;
        }
    }
    if done.is_empty() {
        return;
    }
    done.sort_by_key(|d| d.index);

    let tab_id = tabs.active_id().unwrap_or(0);
    let total = queue.batch_total.max(1);
    let mut imported_records: Vec<NodeRecord> = Vec::new();
    for incoming in done {
        match incoming.result {
            Ok(imported) => {
                let handle = images.add(imported.preview);
                let crop = ImageData::full_crop(imported.natural);
                let px_scale = ImageData::default_px_scale(imported.natural);
                let data = ImageData {
                    rel_path: imported.rel_path,
                    abs_path: imported.abs_path,
                    orig_name: imported.orig_name,
                    hash: imported.hash,
                    natural: imported.natural,
                    preview: handle,
                    preview_size: imported.preview_size,
                    preview_pending: false, // the preview arrives with the node
                    crop,
                    px_scale,
                };
                let size = crop.size() * px_scale;
                let center = queue.batch_center + grid_cell_offset(incoming.index, total);
                let pose = Pose {
                    center,
                    size,
                    rot: 0.0,
                };
                let data_for_record = data.clone();
                let (entity, z) = spawn_image_node(
                    &mut commands,
                    &mut zc,
                    tab_id,
                    true,
                    data,
                    pose,
                );
                imported_records.push(NodeRecord {
                    entity,
                    tab_id,
                    data: NodeData::Image(data_for_record),
                    snap: NodeSnapshot {
                        pose,
                        z,
                        flip_x: false,
                        flip_y: false,
                        crop,
                        px_scale,
                    },
                });
            }
            Err(err) => warn!("import failed: {err:#}"),
        }
    }
    if !imported_records.is_empty() {
        undo.push(UndoEntry::Added {
            items: imported_records,
        });
    }
}

fn grid_cell_offset(index: usize, total: usize) -> Vec2 {
    if total <= 1 {
        return Vec2::ZERO;
    }
    let cols = (total as f32).sqrt().ceil() as usize;
    let rows = total.div_ceil(cols);
    let col = index % cols;
    let row = index / cols;
    Vec2::new(
        (col as f32 - (cols as f32 - 1.0) * 0.5) * DROP_GRID_CELL,
        -((row as f32) - (rows as f32 - 1.0) * 0.5) * DROP_GRID_CELL,
    )
}

// ---------------------------------------------------------------------------
// Worker
// ---------------------------------------------------------------------------

fn import_one(store: &ProjectStore, source: ImportSource) -> anyhow::Result<ImportedImage> {
    let (bytes, dyn_img, ext, orig_name) = match source {
        ImportSource::File(path) => {
            let bytes = std::fs::read(&path)?;
            let ext = path
                .extension()
                .and_then(|e| e.to_str())
                .map(|s| s.to_ascii_lowercase())
                .unwrap_or_else(|| "png".to_string());
            let orig_name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "image".to_string());
            let dyn_img = image::load_from_memory(&bytes)?;
            (bytes, dyn_img, ext, orig_name)
        }
        ImportSource::Pixels { name, image } => {
            // Clipboard pixels have no source file: store them as PNG.
            let bytes = encode_png(&image)?;
            (
                bytes,
                image::DynamicImage::ImageRgba8(image),
                "png".to_string(),
                name,
            )
        }
    };
    let natural = UVec2::new(dyn_img.width().max(1), dyn_img.height().max(1));

    let hash = blake3::hash(&bytes).to_hex().to_string();
    let short = hash[..16].to_string();
    let rel_path = format!("images/{short}.{ext}");

    let dest = store.root.join(&rel_path);
    if !dest.exists() {
        std::fs::write(&dest, &bytes)?;
    }

    let preview_img = dyn_img.resize(
        PREVIEW_MAX,
        PREVIEW_MAX,
        image::imageops::FilterType::CatmullRom,
    );
    let preview_size = UVec2::new(preview_img.width().max(1), preview_img.height().max(1));
    write_preview_cache(&store.cache_dir(), &short, &preview_img);

    Ok(ImportedImage {
        rel_path,
        abs_path: dest,
        orig_name,
        hash: short,
        natural,
        preview_size,
        preview: to_bevy_image(preview_img),
    })
}

/// PNG bytes for pixels that have no source file (clipboard paste).
fn encode_png(image: &image::RgbaImage) -> anyhow::Result<Vec<u8>> {
    use image::ImageEncoder as _;
    let mut bytes = Vec::new();
    image::codecs::png::PngEncoder::new(&mut bytes).write_image(
        image.as_raw(),
        image.width(),
        image.height(),
        image::ExtendedColorType::Rgba8,
    )?;
    Ok(bytes)
}

/// Write a preview for `hash` into `dir` as PNG (alpha) or JPEG (opaque).
pub fn write_preview_cache(dir: &std::path::Path, short: &str, img: &image::DynamicImage) {
    let _ = std::fs::create_dir_all(dir);
    if img.color().has_alpha() {
        let _ = img.save_with_format(dir.join(format!("{short}_512.png")), image::ImageFormat::Png);
    } else {
        let path = dir.join(format!("{short}_512.jpg"));
        if let Ok(file) = std::fs::File::create(path) {
            let mut writer = std::io::BufWriter::new(file);
            let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut writer, 85);
            let _ = encoder.encode_image(img);
        }
    }
}

/// Read a cached preview for `hash` from `dir`, if present.
pub fn read_preview_cache(dir: &std::path::Path, short: &str) -> Option<image::DynamicImage> {
    for ext in ["png", "jpg"] {
        let path = dir.join(format!("{short}_512.{ext}"));
        if path.exists() {
            match image::open(&path) {
                Ok(img) => return Some(img),
                // Present but unreadable (partial copy, truncation, …):
                // regenerating from the original still works, but say why.
                Err(err) => warn!("preview cache unreadable ({}): {err}", path.display()),
            }
        }
    }
    None
}

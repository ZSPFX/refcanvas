//! The system clipboard (X11 + Wayland + macOS + Windows).
//!
//! Ctrl+C writes the selected images there as a flattened PNG ("what you see
//! is what you copy"; see `canvas::export`), Ctrl+V reads whatever it holds:
//! image data (screenshots, browsers, image editors, …), files copied in a file
//! manager (`text/uri-list`), or paths copied as text.
//!
//! A copy made *inside* refcanvas also remembers each image's placement
//! (rotation, flips, size, z) keyed by the exported pixels' hash. The system
//! clipboard itself only carries pixels, so this local history is what lets a
//! round-trip paste keep the transform instead of landing axis-aligned.
//!
//! `arboard` handles both display servers: on Wayland it speaks the
//! data-control protocol (`wl-copy`/`wl-paste`) and falls back to X11
//! (Xwayland) when the compositor doesn't offer it; on X11 it owns the
//! selection through its own server thread. One `arboard::Clipboard` handle
//! is kept alive for the app's lifetime, on X11 the data is only served
//! while some handle exists, and the slow parts (decoding, PNG encoding,
//! blocking selection reads) run on worker threads.

use std::borrow::Cow;
use std::collections::VecDeque;
use std::path::PathBuf;

use bevy::prelude::*;
use bevy::tasks::{AsyncComputeTaskPool, Task, futures::check_ready};

use crate::canvas::export::{self, ExportNode};
use crate::project::import::{self, ImportQueue, PastedPlacement};

/// How many copied images are remembered for metadata restoration.
const COPY_HISTORY: usize = 16;
/// Side of the square fingerprint used to recognise our own copy after the
/// system re-encodes it. Exact hashes do not survive a clipboard round-trip
/// (macOS converts through TIFF, for example), so compare a coarse image.
const FINGERPRINT_SIDE: u32 = 12;
/// Mean per-channel difference (0-255) still accepted as "the same image".
const FINGERPRINT_TOLERANCE: u32 = 10;

/// Placement recorded for a copy, keyed by the pixels it produced.
#[derive(Clone, Debug)]
struct CopyPayload {
    /// World size of the flattened image. Rotation, crop and flips are baked
    /// into the exported pixels by `export::flatten`, so only the display size
    /// (and stacking order) may be restored without applying them twice.
    size: [f32; 2],
    z: f32,
}

pub struct ClipboardPlugin;

impl Plugin for ClipboardPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<SystemClipboard>()
            .add_systems(Update, (poll_copy, poll_paste).chain());
    }
}

#[derive(Resource, Default)]
pub struct SystemClipboard {
    /// Long-lived handle: keeps arboard's X11 selection server alive, and
    /// warms up the connection so the first Ctrl+C/V isn't slower.
    conn: Option<arboard::Clipboard>,
    /// In-flight Ctrl+C export (decode → flatten → PNG).
    copy: Option<Task<Option<(Fingerprint, CopyPayload)>>>,
    /// In-flight Ctrl+V read.
    paste: Option<Task<PastePayload>>,
    /// Where the in-flight paste should land (world space, captured at the
    /// keypress so a slow read doesn't move it).
    paste_at: Vec2,
    /// Recent in-app copies, keyed by a coarse visual fingerprint.
    /// A paste whose pixels match gets its transform back.
    history: VecDeque<(Fingerprint, CopyPayload)>,
}

impl SystemClipboard {
    /// True while a copy/paste worker is still running: the loop must keep
    /// ticking (`main.rs::idle_throttle`) so `poll_copy`/`poll_paste` can
    /// collect the result promptly.
    pub fn is_active(&self) -> bool {
        self.copy.is_some() || self.paste.is_some()
    }

    /// Open (once) and reuse the persistent clipboard handle.
    fn connection(&mut self) -> Option<&mut arboard::Clipboard> {
        if self.conn.is_none() {
            match arboard::Clipboard::new() {
                Ok(cb) => self.conn = Some(cb),
                Err(err) => warn!("clipboard: cannot open the system clipboard: {err}"),
            }
        }
        self.conn.as_mut()
    }
}

/// What a clipboard read found, in paste-priority order.
enum PastePayload {
    Files(Vec<PathBuf>),
    Image(image::RgbaImage),
    Empty,
}

/// Ctrl+C: copy the selected images to the system clipboard as one PNG.
/// Returns immediately, decoding/encoding runs on a worker.
pub fn copy_selection(sc: &mut SystemClipboard, nodes: Vec<ExportNode>) {
    sc.connection();
    if nodes.is_empty() || sc.copy.is_some() {
        return;
    }
    // Record placement now, on the main thread; the worker only touches pixels.
    let payload = CopyPayload::from_export(&nodes);
    let task = AsyncComputeTaskPool::get().spawn(async move {
        match export::flatten(&nodes) {
            Ok(img) => {
                let fingerprint = Fingerprint::of(&img);
                set_image(img);
                Some((fingerprint, payload))
            }
            Err(err) => {
                warn!("clipboard: image export failed: {err:#}");
                None
            }
        }
    });
    sc.copy = Some(task);
}

impl CopyPayload {
    fn from_export(nodes: &[ExportNode]) -> Self {
        let bounds = nodes
            .iter()
            .map(|node| crate::canvas::hit::aabb_of(&node.pose))
            .reduce(|a, b| a.union(b));
        let size = bounds.map(|bounds| bounds.size()).unwrap_or(Vec2::ZERO);
        let z = nodes
            .iter()
            .map(|node| node.z)
            .fold(f32::INFINITY, f32::min);
        Self {
            size: size.to_array(),
            z: if z.is_finite() { z } else { 0.0 },
        }
    }
}

/// A tiny sampled copy of the image, robust to the platform's re-encoding.
#[derive(Clone, Debug)]
struct Fingerprint {
    size: [u32; 2],
    samples: Vec<u8>,
}

impl Fingerprint {
    fn of(image: &image::RgbaImage) -> Self {
        let (w, h) = image.dimensions();
        let side = FINGERPRINT_SIDE;
        let mut samples = Vec::with_capacity((side * side * 4) as usize);
        for gy in 0..side {
            for gx in 0..side {
                let x = (gx * (w.saturating_sub(1))) / side.saturating_sub(1).max(1);
                let y = (gy * (h.saturating_sub(1))) / side.saturating_sub(1).max(1);
                samples.extend_from_slice(&image.get_pixel(x, y).0);
            }
        }
        Self {
            size: [w, h],
            samples,
        }
    }

    /// Same dimensions and a very similar picture. Comparing samples keeps
    /// the match stable across clipboard re-encodes without risking a false
    /// positive from a completely different image.
    fn matches(&self, other: &Self) -> bool {
        if self.size != other.size || self.samples.len() != other.samples.len() {
            return false;
        }
        let total: u32 = self
            .samples
            .iter()
            .zip(&other.samples)
            .map(|(a, b)| a.abs_diff(*b) as u32)
            .sum();
        let mean = total / self.samples.len().max(1) as u32;
        mean <= FINGERPRINT_TOLERANCE
    }
}

/// Ctrl+V: read the clipboard on a worker; [`poll_paste`] imports the result
/// at `at` (world space).
pub fn request_paste(sc: &mut SystemClipboard, at: Vec2) {
    sc.connection();
    if sc.paste.is_some() {
        return; // one read at a time; the keypress still counts as handled
    }
    sc.paste_at = at;
    let task = AsyncComputeTaskPool::get().spawn(async move { read_clipboard() });
    sc.paste = Some(task);
}

fn poll_copy(mut sc: ResMut<SystemClipboard>) {
    let finished = sc.copy.as_mut().and_then(|task| check_ready(task));
    let Some(entry) = finished else {
        return;
    };
    sc.copy = None;
    let Some((fingerprint, payload)) = entry else {
        return;
    };
    sc.history
        .retain(|(known, _)| !known.matches(&fingerprint));
    sc.history.push_front((fingerprint, payload));
    sc.history.truncate(COPY_HISTORY);
}

fn poll_paste(mut sc: ResMut<SystemClipboard>, mut queue: ResMut<ImportQueue>) {
    let payload = {
        let Some(task) = sc.paste.as_mut() else {
            return;
        };
        check_ready(task)
    };
    let Some(payload) = payload else {
        return;
    };
    sc.paste = None;
    let at = sc.paste_at;
    match payload {
        PastePayload::Files(paths) => {
            info!("clipboard: pasted {} file(s)", paths.len());
            import::enqueue_paths(&mut queue, at, paths);
        }
        PastePayload::Image(image) => {
            info!("clipboard: pasted image ({}×{})", image.width(), image.height());
            let fingerprint = Fingerprint::of(&image);
            let payload = sc
                .history
                .iter()
                .find(|(known, _)| known.matches(&fingerprint))
                .map(|(_, payload)| payload.clone());
            import::enqueue_clipboard_image(&mut queue, at, image, placement_for(payload));
        }
        PastePayload::Empty => {
            info!("clipboard: nothing to paste, copy an image or files first");
        }
    }
}

/// Put an RGBA image on the clipboard. Runs on a worker: arboard encodes it
/// as PNG, and on Wayland spawns the serving thread.
fn set_image(img: image::RgbaImage) {
    let (w, h) = img.dimensions();
    let mut cb = match arboard::Clipboard::new() {
        Ok(cb) => cb,
        Err(err) => {
            warn!("clipboard: cannot open the system clipboard: {err}");
            return;
        }
    };
    let data = arboard::ImageData {
        width: w as usize,
        height: h as usize,
        bytes: Cow::Owned(img.into_raw()),
    };
    match cb.set_image(data) {
        Ok(()) => info!("clipboard: copied image ({w}×{h})"),
        Err(err) => warn!("clipboard: failed to copy image: {err}"),
    }
}

/// Worker: whatever the clipboard offers, preferring files (a manager copy)
/// over raw image data over text that names existing files.
fn read_clipboard() -> PastePayload {
    let mut cb = match arboard::Clipboard::new() {
        Ok(cb) => cb,
        Err(err) => {
            warn!("clipboard: cannot open the system clipboard: {err}");
            return PastePayload::Empty;
        }
    };

    // Files copied in a file manager (Dolphin, Nautilus, Nemo, Thunar, …).
    if let Ok(paths) = cb.get().file_list() {
        let files = clean_paths(paths);
        if !files.is_empty() {
            return PastePayload::Files(files);
        }
    }

    // Raw image data (screenshots, "Copy image" in a browser, image editors, …).
    // Raw image data (screenshots, "Copy image" in a browser, image editors, …).
    match cb.get_image() {
        Ok(img) => {
            let (w, h) = (img.width as u32, img.height as u32);
            match image::RgbaImage::from_raw(w, h, img.bytes.into_owned()) {
                Some(image) => return PastePayload::Image(image),
                None => warn!("clipboard: malformed image data ({w}×{h})"),
            }
        }
        Err(err) => trace!("clipboard: no image data ({err})"),
    }

    // Last resort: text that names files ("Copy location", a path pasted
    // into a text editor, …).
    if let Ok(text) = cb.get_text() {
        let files = clean_paths(
            text.lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(PathBuf::from)
                .collect(),
        );
        if !files.is_empty() {
            return PastePayload::Files(files);
        }
    }

    PastePayload::Empty
}

/// Turn the remembered copy into a placement at the paste point.
fn placement_for(payload: Option<CopyPayload>) -> Option<PastedPlacement> {
    let payload = payload?;
    Some(PastedPlacement {
        size: Vec2::from_array(payload.size),
        z: payload.z,
    })
}

/// Keep only paths that actually exist, and drop stray carriage returns:
/// `text/uri-list` is CRLF-terminated on some desktops, while arboard only
/// splits on `\n`, leaving the `\r` in the path.
fn clean_paths(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    paths
        .into_iter()
        .map(trim_cr)
        .filter(|p| p.exists())
        .collect()
}

#[cfg(unix)]
fn trim_cr(p: PathBuf) -> PathBuf {
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    let mut bytes = p.as_os_str().as_bytes().to_vec();
    while bytes.last() == Some(&b'\r') {
        bytes.pop();
    }
    PathBuf::from(std::ffi::OsString::from_vec(bytes))
}

#[cfg(not(unix))]
fn trim_cr(p: PathBuf) -> PathBuf {
    p
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canvas::node::Pose;

    fn image(color: [u8; 4]) -> image::RgbaImage {
        image::RgbaImage::from_pixel(64, 64, image::Rgba(color))
    }

    #[test]
    fn fingerprint_survives_small_reencoding_changes() {
        let original = image([120, 60, 30, 255]);
        let mut reencoded = original.clone();
        for pixel in reencoded.pixels_mut() {
            pixel.0[0] = pixel.0[0].saturating_add(2);
            pixel.0[1] = pixel.0[1].saturating_sub(1);
        }
        assert!(Fingerprint::of(&original).matches(&Fingerprint::of(&reencoded)));
    }

    #[test]
    fn fingerprint_rejects_a_different_picture() {
        assert!(!Fingerprint::of(&image([120, 60, 30, 255]))
            .matches(&Fingerprint::of(&image([30, 200, 30, 255]))));
    }

    fn export(center: Vec2, size: Vec2, rot: f32, z: f32) -> ExportNode {
        ExportNode {
            path: PathBuf::new(),
            crop: Rect::from_corners(Vec2::ZERO, size),
            flip_x: false,
            flip_y: false,
            pose: Pose { center, size, rot },
            px_scale: 1.0,
            z,
        }
    }

    #[test]
    fn copied_size_matches_the_flattened_bounds() {
        // A rotated image is flattened into its axis-aligned bounds, so the
        // restore size must be the AABB, not the pre-rotation size.
        let node = export(Vec2::ZERO, Vec2::splat(100.0), std::f32::consts::FRAC_PI_4, 3.0);
        let payload = CopyPayload::from_export(&[node]);
        let expected = 100.0 * 2.0_f32.sqrt();
        assert!((payload.size[0] - expected).abs() < 0.01);
        assert!((payload.size[1] - expected).abs() < 0.01);
        assert_eq!(payload.z, 3.0);
    }

    #[test]
    fn copied_size_spans_the_whole_selection() {
        let payload = CopyPayload::from_export(&[
            export(Vec2::new(-100.0, 0.0), Vec2::splat(50.0), 0.0, 2.0),
            export(Vec2::new(100.0, 0.0), Vec2::splat(50.0), 0.0, 1.0),
        ]);
        assert!((payload.size[0] - 250.0).abs() < 0.01);
        assert_eq!(payload.z, 1.0);
    }

    #[test]
    fn fingerprint_rejects_a_different_size() {
        let small = image::RgbaImage::from_pixel(32, 32, image::Rgba([0, 0, 0, 255]));
        assert!(!Fingerprint::of(&small).matches(&Fingerprint::of(&image([0, 0, 0, 255]))));
    }
}

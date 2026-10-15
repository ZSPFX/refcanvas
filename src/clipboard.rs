//! The system clipboard (X11 + Wayland).
//!
//! Ctrl+C writes the selected images there as a flattened PNG ("what you see
//! is what you copy"; see `canvas::export`), Ctrl+V reads whatever it holds:
//! image data (screenshots, browsers, GIMP, …), files copied in a file
//! manager (`text/uri-list`), or paths copied as text.
//!
//! `arboard` handles both display servers: on Wayland it speaks the
//! data-control protocol (`wl-copy`/`wl-paste`) and falls back to X11
//! (Xwayland) when the compositor doesn't offer it; on X11 it owns the
//! selection through its own server thread. One `arboard::Clipboard` handle
//! is kept alive for the app's lifetime, on X11 the data is only served
//! while some handle exists, and the slow parts (decoding, PNG encoding,
//! blocking selection reads) run on worker threads.

use std::borrow::Cow;
use std::path::PathBuf;

use bevy::prelude::*;
use bevy::tasks::{AsyncComputeTaskPool, Task, futures::check_ready};

use crate::canvas::export::{self, ExportNode};
use crate::project::import::{self, ImportQueue};

pub struct ClipboardPlugin;

impl Plugin for ClipboardPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<SystemClipboard>()
            .add_systems(Update, (poll_copy, poll_paste));
    }
}

#[derive(Resource, Default)]
pub struct SystemClipboard {
    /// Long-lived handle: keeps arboard's X11 selection server alive, and
    /// warms up the connection so the first Ctrl+C/V isn't slower.
    conn: Option<arboard::Clipboard>,
    /// In-flight Ctrl+C export (decode → flatten → PNG → clipboard).
    copy: Option<Task<()>>,
    /// In-flight Ctrl+V read.
    paste: Option<Task<PastePayload>>,
    /// Where the in-flight paste should land (world space, captured at the
    /// keypress so a slow read doesn't move it).
    paste_at: Vec2,
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
    let task = AsyncComputeTaskPool::get().spawn(async move {
        match export::flatten(&nodes) {
            Ok(img) => set_image(img),
            Err(err) => warn!("clipboard: image export failed: {err:#}"),
        }
    });
    sc.copy = Some(task);
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
    let ready = sc
        .copy
        .as_mut()
        .is_some_and(|task| check_ready(task).is_some());
    if ready {
        sc.copy = None;
    }
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
        PastePayload::Image(img) => {
            info!("clipboard: pasted image ({}×{})", img.width(), img.height());
            import::enqueue_clipboard_image(&mut queue, at, img);
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

    // Raw image data (screenshots, "Copy image" in a browser, GIMP, …).
    match cb.get_image() {
        Ok(img) => {
            let (w, h) = (img.width as u32, img.height as u32);
            match image::RgbaImage::from_raw(w, h, img.bytes.into_owned()) {
                Some(img) => return PastePayload::Image(img),
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

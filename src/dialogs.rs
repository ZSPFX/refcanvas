//! Native pickers and dialogs that never block the update loop.
//!
//! `rfd`'s synchronous pickers spin their own platform event loop. On macOS
//! that is an AppKit run loop nested inside a Bevy tick, so the window stops
//! painting until the dialog closes and the app looks frozen. The async
//! variants return a future instead; this module polls it once per frame, so
//! the app keeps rendering while the picker is open on every platform.
//!
//! The futures are polled by hand rather than spawned on a task pool: they
//! must be driven on the platform's main thread, and a pool's local executor
//! does not tick there. For the same reason [`Dialogs`] is a *non-send*
//! resource: rfd's futures hold AppKit objects, and polling them from a
//! worker thread can hang the process.

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};

use bevy::prelude::*;
use bevy::winit::WINIT_WINDOWS;

/// Extensions the image pickers accept, matching `import::is_supported_image`.
pub const IMAGE_EXTENSIONS: &[&str] = &[
    "png", "jpg", "jpeg", "webp", "bmp", "gif", "tga", "tif", "tiff",
];
/// Title used by every image picker.
pub const IMAGE_PICKER_TITLE: &str = "Import reference images";

/// Which dialog a message result belongs to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MessageId {
    UpdateAvailable { version: String, url: String },
}

type BoxedFuture<T> = Pin<Box<dyn Future<Output = T>>>;

/// Native dialogs requested by the app.
///
/// Non-send: see the module docs. Inserted with `init_non_send`,
/// accessed through `NonSendMut`.
#[derive(Default)]
pub struct Dialogs {
    /// At most one dialog at a time: they are modal on every platform.
    active: Option<Active>,
}

enum Active {
    Folder(BoxedFuture<Option<PathBuf>>),
    Files(BoxedFuture<Vec<PathBuf>>),
    Message(BoxedFuture<(MessageId, rfd::MessageDialogResult)>),
}

/// Attach the dialog to the Bevy window.
///
/// Without a parent, rfd guesses from `NSApp.mainWindow()`. When that lookup
/// fails it prints "unsupported environment" and runs the panel as a blocking
/// `runModal()`, which freezes the app. Passing the window keeps it an async
/// sheet. The result is ignored on macOS when no window exists yet; the
/// pickers then fall back to their own parent detection.
fn with_parent(dialog: rfd::AsyncFileDialog) -> rfd::AsyncFileDialog {
    WINIT_WINDOWS.with_borrow(|windows| match windows.windows.values().next() {
        Some(window) => dialog.set_parent(&**window),
        None => dialog,
    })
}

fn message_with_parent(dialog: rfd::AsyncMessageDialog) -> rfd::AsyncMessageDialog {
    WINIT_WINDOWS.with_borrow(|windows| match windows.windows.values().next() {
        Some(window) => dialog.set_parent(&**window),
        None => dialog,
    })
}

/// Finished dialog results, drained by the systems that act on them.
#[derive(Resource, Default)]
pub struct DialogResults {
    pub folders: Vec<PathBuf>,
    pub files: Vec<PathBuf>,
    pub messages: Vec<(MessageId, rfd::MessageDialogResult)>,
}

impl Dialogs {
    /// True while a dialog is on screen, so callers do not stack another.
    pub fn is_open(&self) -> bool {
        self.active.is_some()
    }

    /// Show a folder picker. Ignored while another dialog is open.
    pub fn pick_folder(&mut self, title: &str) {
        if self.is_open() {
            return;
        }
        let title = title.to_string();
        let dialog = with_parent(rfd::AsyncFileDialog::new().set_title(title));
        let future = async move {
            dialog
                .pick_folder()
                .await
                .map(|handle| handle.path().to_path_buf())
        };
        self.active = Some(Active::Folder(Box::pin(future)));
    }

    /// Show the standard image picker.
    pub fn pick_images(&mut self) {
        self.pick_files(IMAGE_PICKER_TITLE, IMAGE_EXTENSIONS);
    }

    /// Show a multi-file picker filtered to the given extensions.
    pub fn pick_files(&mut self, title: &str, extensions: &[&str]) {
        if self.is_open() {
            return;
        }
        let title = title.to_string();
        let filter: Vec<String> = extensions.iter().map(|e| e.to_string()).collect();
        let dialog = with_parent(
            rfd::AsyncFileDialog::new()
                .set_title(title)
                .add_filter("Images", &filter_extensions(&filter)),
        );
        let future = async move {
            dialog
                .pick_files()
                .await
                .map(|handles| {
                    handles
                        .into_iter()
                        .map(|handle| handle.path().to_path_buf())
                        .collect()
                })
                .unwrap_or_default()
        };
        self.active = Some(Active::Files(Box::pin(future)));
    }

    /// Show a yes/no message box.
    pub fn ask(&mut self, title: &str, text: String, id: MessageId) {
        if self.is_open() {
            return;
        }
        let title = title.to_string();
        let dialog = message_with_parent(
            rfd::AsyncMessageDialog::new()
                .set_title(title)
                .set_description(text)
                .set_level(rfd::MessageLevel::Info)
                .set_buttons(rfd::MessageButtons::YesNo),
        );
        let future = async move {
            let result = dialog.show().await;
            (id, result)
        };
        self.active = Some(Active::Message(Box::pin(future)));
    }
}

pub struct DialogsPlugin;

impl Plugin for DialogsPlugin {
    fn build(&self, app: &mut App) {
        app.init_non_send::<Dialogs>()
            .init_resource::<DialogResults>()
            .add_systems(Update, poll_dialogs);
    }
}

fn poll_dialogs(mut dialogs: NonSendMut<Dialogs>, mut results: ResMut<DialogResults>) {
    let Some(active) = dialogs.active.as_mut() else {
        return;
    };
    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);
    match active {
        Active::Folder(future) => {
            if let Poll::Ready(result) = future.as_mut().poll(&mut cx) {
                dialogs.active = None;
                results.folders.extend(result);
            }
        }
        Active::Files(future) => {
            if let Poll::Ready(files) = future.as_mut().poll(&mut cx) {
                dialogs.active = None;
                results.files.extend(files);
            }
        }
        Active::Message(future) => {
            if let Poll::Ready((id, result)) = future.as_mut().poll(&mut cx) {
                dialogs.active = None;
                results.messages.push((id, result));
            }
        }
    }
}

/// The dialog futures are re-polled every frame, so a waker that does nothing
/// is enough and avoids waking the task pool for no reason.
struct NoopWake;

impl Wake for NoopWake {
    fn wake(self: Arc<Self>) {}
    fn wake_by_ref(self: &Arc<Self>) {}
}

fn noop_waker() -> Waker {
    static WAKER: std::sync::LazyLock<Waker> =
        std::sync::LazyLock::new(|| Waker::from(Arc::new(NoopWake)));
    WAKER.clone()
}

/// The extension list as `&str`, borrowed from an owned vector so it can be
/// handed to rfd from an async block.
fn filter_extensions(extensions: &[String]) -> Vec<&str> {
    extensions.iter().map(String::as_str).collect()
}

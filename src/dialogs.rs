//! native pickers that never block the loop: poll rfd's async futures by hand on
//! the main thread (sync variants freeze rendering; they hold appkit objects).

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};

use bevy::prelude::*;
use bevy::winit::WINIT_WINDOWS;

/// extensions the image pickers accept, matching `import::is_supported_image`.
pub const IMAGE_EXTENSIONS: &[&str] = &[
    "png", "jpg", "jpeg", "webp", "bmp", "gif", "tga", "tif", "tiff",
];
pub const IMAGE_PICKER_TITLE: &str = "Import reference images";

/// which dialog a message result belongs to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MessageId {
    UpdateAvailable { version: String, url: String },
}

type BoxedFuture<T> = Pin<Box<dyn Future<Output = T>>>;

/// native dialogs; non-send, see module docs.
#[derive(Default)]
pub struct Dialogs {
    /// at most one dialog at a time: they are modal everywhere.
    active: Option<Active>,
}

enum Active {
    Folder(BoxedFuture<Option<PathBuf>>),
    Files(BoxedFuture<Vec<PathBuf>>),
    Message(BoxedFuture<(MessageId, rfd::MessageDialogResult)>),
}

/// attach the dialog to the bevy window; without a parent rfd may run it as a
/// blocking `runModal()` and freeze the app.
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

/// finished dialog results, drained by the systems that act on them.
#[derive(Resource, Default)]
pub struct DialogResults {
    pub folders: Vec<PathBuf>,
    pub files: Vec<PathBuf>,
    pub messages: Vec<(MessageId, rfd::MessageDialogResult)>,
}

impl Dialogs {
    /// true while a dialog is on screen, so callers do not stack another.
    pub fn is_open(&self) -> bool {
        self.active.is_some()
    }

    /// show a folder picker; ignored while another dialog is open.
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

    pub fn pick_images(&mut self) {
        self.pick_files(IMAGE_PICKER_TITLE, IMAGE_EXTENSIONS);
    }

    /// multi-file picker filtered to the given extensions.
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

    /// show a yes/no message box.
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

/// polled every frame, so a no-op waker is enough.
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

/// extension list as `&str` for the async block.
fn filter_extensions(extensions: &[String]) -> Vec<&str> {
    extensions.iter().map(String::as_str).collect()
}

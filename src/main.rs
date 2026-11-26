#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

//! refcanvas, a GPU-accelerated infinite canvas for reference images.

mod canvas;
mod clipboard;
mod dialogs;
mod instance;
mod project;
mod theme;
mod ui;
mod update;

use std::num::NonZeroU32;
use std::time::{Duration, Instant};

use bevy::app::{TaskPoolOptions, TaskPoolPlugin, TaskPoolThreadAssignmentPolicy};
use bevy::ecs::system::NonSendMarker;
use bevy::prelude::*;
use bevy::window::{
    MonitorSelection, PresentMode, PrimaryWindow, WindowPosition, WindowResized, WindowResolution,
};
use bevy::winit::{UpdateMode, WINIT_WINDOWS, WinitSettings};
use winit::window::Icon;

use canvas::CanvasPlugin;
use canvas::node::{CanvasNode, FullLoadTask};
use clipboard::SystemClipboard;
use project::import::ImportQueue;
use project::persist::PreviewLoader;
use theme::ThemePlugin;
use ui::UiPlugin;

/// canvas/ui background
pub const CANVAS_BG: Color = Color::srgb(0.118, 0.118, 0.118); // #1e1e1e

// no vsync; our limiter paces the loop
const BASE_PRESENT_MODE: PresentMode = PresentMode::AutoNoVsync;

// REF_PRESENT_MODE=immediate|mailbox|fifo|fifo-relaxed
fn present_mode() -> PresentMode {
    match std::env::var("REF_PRESENT_MODE").as_deref().map(str::trim) {
        Ok("immediate") => PresentMode::Immediate,
        Ok("mailbox") => PresentMode::Mailbox,
        Ok("fifo") => PresentMode::Fifo,
        Ok("fifo-relaxed") | Ok("fifo_relaxed") => PresentMode::FifoRelaxed,
        _ => BASE_PRESENT_MODE,
    }
}

// true when winit picked wayland
pub(crate) fn on_wayland() -> bool {
    match std::env::var("WINIT_UNIX_BACKEND").as_deref() {
        Ok("x11") => false,
        Ok("wayland") => true,
        _ => std::env::var_os("WAYLAND_DISPLAY").is_some(),
    }
}

// wayland has no always-on-top; use the x11 compat backend when available
#[cfg(target_os = "linux")]
fn prefer_x11_for_always_on_top() {
    if !project::local::load_always_on_top() {
        return;
    }
    let Some(display) = std::env::var_os("DISPLAY") else {
        return;
    };
    if std::env::var_os("WAYLAND_DISPLAY").is_none()
        || std::env::var_os("WINIT_UNIX_BACKEND").is_some()
    {
        return;
    }
    // forcing x11 without the socket just fails
    let display = display.to_string_lossy();
    if let Some(rest) = display.strip_prefix(':') {
        let number = rest.split('.').next().unwrap_or("0");
        let socket = format!("/tmp/.X11-unix/X{number}");
        if !std::path::Path::new(&socket).exists() {
            eprintln!(
                "refcanvas: always-on-top needs X11 compatibility, but {socket} is missing"
            );
            return;
        }
    }
    eprintln!("refcanvas: always-on-top enabled; using the X11 compatibility backend");
    // SAFETY: this runs before Bevy or winit starts any threads.
    unsafe {
        std::env::set_var("WINIT_UNIX_BACKEND", "x11");
    }
}

// swapchain images = value + 1
const DEFAULT_FRAME_LATENCY: u32 = 4;

/// swapchain depth; more images help wayland under uncapped presentation
fn frame_latency() -> NonZeroU32 {
    let n = std::env::var("REF_FRAME_LATENCY")
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
        .unwrap_or(DEFAULT_FRAME_LATENCY);
    NonZeroU32::new(n.clamp(1, 16)).unwrap_or(NonZeroU32::MIN)
}

// 1 ms timer keeps the fps limiter from oversleeping
#[cfg(target_os = "windows")]
struct TimerResolution;

#[cfg(target_os = "windows")]
impl TimerResolution {
    fn acquire() -> Self {
        unsafe {
            windows_sys::Win32::Media::timeBeginPeriod(1);
        }
        Self
    }
}

#[cfg(target_os = "windows")]
impl Drop for TimerResolution {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::Media::timeEndPeriod(1);
        }
    }
}

fn main() {
    #[cfg(target_os = "windows")]
    let _timer_resolution = TimerResolution::acquire();

    #[cfg(target_os = "linux")]
    prefer_x11_for_always_on_top();

    // one window per user
    let _instance = if instance::enabled() {
        match instance::acquire() {
            Ok(Some(guard)) => Some(guard),
            Ok(None) => {
                eprintln!("refcanvas is already running, only one window is allowed");
                return;
            }
            Err(err) => {
                eprintln!("refcanvas: cannot enforce a single instance: {err}");
                None
            }
        }
    } else {
        None
    };

    let mut app = App::new();
    app.add_plugins(
        DefaultPlugins
            .set(TaskPoolPlugin {
                task_pool_options: TaskPoolOptions {
                    // leave room for frame work
                    async_compute: TaskPoolThreadAssignmentPolicy {
                        min_threads: 1,
                        max_threads: 8,
                        percent: 0.4,
                        on_thread_spawn: None,
                        on_thread_destroy: None,
                    },
                    ..default()
                },
            })
            .set(WindowPlugin {
                primary_window: Some(Window {
                    title: "refcanvas".into(),
                    // app id / WM_CLASS for icon matching
                    name: Some("refcanvas".into()),
                    resolution: WindowResolution::new(1600, 900),
                    // wayland can't place windows absolutely
                    position: if on_wayland() {
                        WindowPosition::Automatic
                    } else {
                        WindowPosition::Centered(MonitorSelection::Primary)
                    },
                    present_mode: present_mode(),
                    desired_maximum_frame_latency: Some(frame_latency()),
                    ..default()
                }),
                ..default()
            })
            .set(bevy::log::LogPlugin {
                // silence swapchain churn during live resize
                filter: "wgpu=error,naga=warn,bevy_render::view::window=error".into(),
                ..default()
            }),
    )
    .insert_resource(ClearColor(CANVAS_BG))
    // spread big texture uploads over frames
    .insert_resource(bevy::render::render_asset::RenderAssetBytesPerFrame {
        max_bytes: Some(8 << 20), // 8 MiB per frame
    })
    // overridden by idle_throttle after the first update
    .insert_resource(WinitSettings {
        focused_mode: UpdateMode::Continuous,
        unfocused_mode: UpdateMode::Continuous,
    })
    .add_plugins(bevy_egui::EguiPlugin::default())
    .add_plugins((
        CanvasPlugin,
        UiPlugin,
        dialogs::DialogsPlugin,
        project::ProjectPlugin,
        clipboard::ClipboardPlugin,
        ThemePlugin,
        update::UpdatePlugin,
    ));

    // dev helper: refcanvas --import <folder>
    let args: Vec<String> = std::env::args().collect();
    if let Some(pos) = args.iter().position(|a| a == "--import") {
        if let Some(dir) = args.get(pos + 1) {
            app.insert_resource(project::import::StartupImport(std::path::PathBuf::from(
                dir,
            )));
        }
    }

    app.init_resource::<FramePacer>()
        .init_resource::<WindowResize>()
        .add_systems(
            Update,
            (
                smooth_resize,
                idle_throttle.after(canvas::CanvasSet),
                set_window_icon,
            ),
        )
        .add_systems(Last, limit_fps);
    app.run();
}

/// set the window icon; x11/windows only, wayland uses the desktop app id
fn set_window_icon(_: NonSendMarker, mut applied: Local<bool>) {
    if *applied {
        return;
    }

    // embedded; keeps the release binary standalone
    let Ok(rgba) = image::load_from_memory(include_bytes!("../assets/icons/refcanvas-256.png"))
        .map(image::DynamicImage::into_rgba8)
    else {
        warn!("embedded window icon failed to decode");
        return;
    };
    let (width, height) = rgba.dimensions();
    let Ok(icon) = Icon::from_rgba(rgba.into_raw(), width, height) else {
        warn!("embedded window icon is not a valid RGBA buffer");
        return;
    };

    WINIT_WINDOWS.with_borrow(|windows| {
        // window usually exists by now; retry next frame if not
        if windows.windows.is_empty() {
            return;
        }
        for window in windows.windows.values() {
            window.set_window_icon(Some(icon.clone()));
        }
        *applied = true;
    });
}

const TARGET_FPS: f32 = 120.0;

#[derive(Resource)]
pub struct FramePacer {
    next_frame: Option<Instant>,
    /// benchmark mode bypasses the cap
    pub cap_enabled: bool,
}

impl Default for FramePacer {
    fn default() -> Self {
        Self {
            next_frame: None,
            cap_enabled: true,
        }
    }
}

/// cap at target fps unless resizing, touching, flinging or benchmarking
fn limit_fps(
    mut pacer: ResMut<FramePacer>,
    resize: Res<WindowResize>,
    touch: Res<canvas::touch::Pointer>,
    inertia: Res<canvas::touch::Inertia>,
) {
    if !pacer.cap_enabled || resize.is_active() || touch.touch_active || inertia.is_active() {
        // don't throttle benchmarks, gestures or live resizes
        pacer.next_frame = None; // fresh interval when resuming
        return;
    }
    let target = Duration::from_secs_f32(1.0 / TARGET_FPS);
    let now = Instant::now();
    if let Some(next) = pacer.next_frame {
        if next > now {
            let remaining = next - now;
            if let Some(sleep) = remaining.checked_sub(Duration::from_micros(200)) {
                if !sleep.is_zero() {
                    std::thread::sleep(sleep);
                }
            }
            while Instant::now() < next {
                std::hint::spin_loop();
            }
        }
    }
    pacer.next_frame = Some(Instant::now() + target);
}

/// idle loop wake interval
pub(crate) const IDLE_HEARTBEAT: Duration = Duration::from_secs(1);

/// drop to the heartbeat when nothing is loading; every loader is polled per
/// frame, so sleep only when the queues are drained
fn idle_throttle(
    mut settings: ResMut<WinitSettings>,
    import: Res<ImportQueue>,
    previews: Res<PreviewLoader>,
    textures: Res<canvas::lod::GpuTextureState>,
    tab_requests: Res<project::tabs::TabRequests>,
    tabs: Res<project::tabs::Tabs>,
    clipboard: Res<SystemClipboard>,
    pacer: Res<FramePacer>,
    resize: Res<WindowResize>,
    touch: Res<canvas::touch::Pointer>,
    inertia: Res<canvas::touch::Inertia>,
    loads: Query<(), With<FullLoadTask>>,
    nodes: Query<&CanvasNode, Without<canvas::node::InactiveTab>>,
    parking: Query<(), (With<Mesh2d>, With<canvas::node::InactiveTab>)>,
    resuming: Query<
        (),
        (
            With<canvas::lod::ParkedMesh>,
            Without<canvas::node::InactiveTab>,
        ),
    >,
) {
    // resize, gestures, loaders, sync, clipboard and uploads all count
    let busy = resize.is_active()
        || touch.touch_active
        || inertia.is_active()
        || import.is_active()
        || previews.is_active()
        || textures.has_pending()
        || nodes.iter().any(node_texture_pending)
        || !tab_requests.queue.is_empty()
        || project::sync::has_pending(&tabs)
        || clipboard.is_active()
        || !loads.is_empty()
        || !parking.is_empty()
        || !resuming.is_empty()
        || !pacer.cap_enabled;
    let idle = if busy {
        UpdateMode::Continuous
    } else {
        UpdateMode::reactive_low_power(IDLE_HEARTBEAT)
    };
    settings.focused_mode = idle;
    settings.unfocused_mode = idle;
}

/// true while a node still needs a texture upload or promote
fn node_texture_pending(node: &CanvasNode) -> bool {
    node.image().is_some_and(|img| {
        img.data.preview_pending || img.full_incoming.is_some() || img.pending_cpu_free.is_some()
    })
}

// resize stays active this long after the last event
const RESIZE_IDLE: Duration = Duration::from_millis(300);

// resize presentation state
#[derive(Resource, Default)]
pub struct WindowResize {
    last: Option<Instant>,
    active: bool,
}

impl WindowResize {
    pub fn is_active(&self) -> bool {
        self.active
    }
}

/// switch to fifo and bypass the cap while resizing; revert after a short idle
fn smooth_resize(
    mut messages: MessageReader<WindowResized>,
    mut resize: ResMut<WindowResize>,
    mut window: Single<&mut Window, With<PrimaryWindow>>,
) {
    let now = Instant::now();
    if messages.read().last().is_some() {
        resize.last = Some(now);
    }
    let resizing = resize
        .last
        .is_some_and(|t| now.duration_since(t) < RESIZE_IDLE);
    if resizing == resize.active {
        return; // no transition
    }
    resize.active = resizing;
    // fifo fixes x11 swapchain churn; wayland already paces, leave it alone
    let target = if resizing && !on_wayland() {
        PresentMode::Fifo
    } else {
        present_mode()
    };
    if window.present_mode != target {
        window.present_mode = target;
    }
}

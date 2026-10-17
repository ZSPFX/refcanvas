//! refcanvas, a GPU-accelerated infinite canvas for reference images.

mod canvas;
mod clipboard;
mod instance;
mod project;
mod theme;
mod ui;

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

/// Canvas / UI background color (Obsidian-like dark).
pub const CANVAS_BG: Color = Color::srgb(0.118, 0.118, 0.118); // #1e1e1e

/// Present mode while idle: no V-Sync, uncapped (our own limiter paces the
/// loop, and the FPS readout stays live). During a window resize the app
/// switches to `Fifo`; see [`smooth_resize`].
const BASE_PRESENT_MODE: PresentMode = PresentMode::AutoNoVsync;

/// Present mode for the primary window, overridable without a rebuild:
/// `REF_PRESENT_MODE=immediate|mailbox|fifo|fifo-relaxed`.
///
/// The default (`AutoNoVsync`) resolves to Vulkan *Immediate* where the
/// platform offers it. On X11 that is a true "present and never wait"; on
/// Wayland the surface usually only offers Fifo/Mailbox, so throughput is
/// governed by how fast the compositor recycles buffers; see
/// [`frame_latency`] for the knob that scales with that.
fn present_mode() -> PresentMode {
    match std::env::var("REF_PRESENT_MODE").as_deref().map(str::trim) {
        Ok("immediate") => PresentMode::Immediate,
        Ok("mailbox") => PresentMode::Mailbox,
        Ok("fifo") => PresentMode::Fifo,
        Ok("fifo-relaxed") | Ok("fifo_relaxed") => PresentMode::FifoRelaxed,
        _ => BASE_PRESENT_MODE,
    }
}

/// Is winit on a native Wayland surface? It picks Wayland whenever
/// `WAYLAND_DISPLAY` is set (an Xwayland session with that variable cleared
/// counts as X11). Used to skip X11-only workarounds and the monitor hint.
fn on_wayland() -> bool {
    std::env::var_os("WAYLAND_DISPLAY").is_some()
}

/// Swapchain images = this + 1. Override with `REF_FRAME_LATENCY=<n>`.
const DEFAULT_FRAME_LATENCY: u32 = 4;

/// Swap chain depth hint, overridable without a rebuild:
/// `REF_FRAME_LATENCY=<n>` (clamped 1..=16).
///
/// wgpu-hal turns this into the Vulkan image count (`native.rs`:
/// `min_image_count(config.maximum_frame_latency + 1)`), so the default 2
/// Bevy uses gives only 3 images. That matters on Wayland, where the client
/// stalls whenever the compositor hasn't recycled a buffer yet: uncapped FPS
/// ends up proportional to the number of images in flight (~images × the
/// compositor's release rate). More images cost a little VRAM and are free
/// latency-wise while presenting Immediately; only Fifo (used briefly during
/// window resizes) actually queues them.
fn frame_latency() -> NonZeroU32 {
    let n = std::env::var("REF_FRAME_LATENCY")
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
        .unwrap_or(DEFAULT_FRAME_LATENCY);
    NonZeroU32::new(n.clamp(1, 16)).unwrap_or(NonZeroU32::MIN)
}

fn main() {
    // Only allow one window per user at a time.
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
                    // Shorten image decode bursts while leaving capacity for frame work.
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
                    // Wayland app_id / X11 WM_CLASS, which the compositor and
                    // taskbar match against the installed refcanvas.desktop to
                    // pick the app icon (Wayland has no per-window icon
                    // protocol, and Bevy exposes no window-icon API).
                    name: Some("refcanvas".into()),
                    resolution: WindowResolution::new(1600, 900),
                    // Always open centered on the primary monitor; otherwise
                    // the window manager may spawn it under the cursor.
                    // Wayland has no absolute positioning, so asking for a
                    // monitor there only makes winit warn.
                    position: if on_wayland() {
                        WindowPosition::Automatic
                    } else {
                        WindowPosition::Centered(MonitorSelection::Primary)
                    },
                    // No V-Sync: capped at TARGET_FPS by our own frame limiter
                    // (V-Sync added noticeable input latency, and pinned the FPS
                    // readout to the 75 Hz panel).
                    present_mode: present_mode(),
                    // Swap chain depth: images = value + 1 (see `frame_latency`).
                    desired_maximum_frame_latency: Some(frame_latency()),
                    ..default()
                }),
                ..default()
            })
            .set(bevy::log::LogPlugin {
                // On X11 + NVIDIA the swap chain cannot always follow a live resize:
                // Bevy safely skips those frames and logs a warning each time
                // ("Couldn't get swap chain texture ... Outdated"), which floods the
                // console during a drag. Keep errors from that module, drop the
                // resize chatter.
                filter: "wgpu=error,naga=warn,bevy_render::view::window=error".into(),
                ..default()
            }),
    )
    .insert_resource(ClearColor(CANVAS_BG))
    // GPU upload budget. A full-res 4K image is ~33 MB RGBA; with the default
    // (unlimited) budget it lands in a single frame, which is exactly the hitch
    // you feel the first time an image is zoomed to full resolution. Spreading
    // it over a few frames makes the transition invisible.
    .insert_resource(bevy::render::render_asset::RenderAssetBytesPerFrame {
        max_bytes: Some(8 << 20), // 8 MiB per frame
    })
    // Frame-rate modes are maintained per-frame by [`idle_throttle`]; these
    // only cover the frames before the first `Update`.
    .insert_resource(WinitSettings {
        focused_mode: UpdateMode::Continuous,
        unfocused_mode: UpdateMode::Continuous,
    })
    .add_plugins(bevy_egui::EguiPlugin::default())
    .add_plugins((
        CanvasPlugin,
        UiPlugin,
        project::ProjectPlugin,
        clipboard::ClipboardPlugin,
        ThemePlugin,
    ));

    // Dev helper: `refcanvas --import <folder>`
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
        .add_systems(Update, (smooth_resize, idle_throttle, set_window_icon))
        .add_systems(Last, limit_fps);
    app.run();
}

/// Applies the app icon to every window (title bar and task bar).
///
/// Windows and X11 honour this. **Wayland ignores window icons**: winit's
/// `set_window_icon` is a no-op there, because xdg-shell has no icon protocol.
/// On Wayland the icon instead comes from the compositor matching the window's
/// app id — see `Window::name` — against the installed `refcanvas.desktop`,
/// whose `Icon=refcanvas` resolves through the icon theme.
///
/// Bevy 0.20 has no window-icon API, and `WinitWindows` is a thread-local now
/// rather than the `NonSend` resource older versions exposed (bevy#17667), so
/// this borrows it directly. `NonSendMarker` pins the system to the main thread,
/// where the winit event loop — and therefore that thread-local — lives.
fn set_window_icon(_: NonSendMarker, mut applied: Local<bool>) {
    if *applied {
        return;
    }

    // Embedded, so the release binary stays standalone (like the grid shader).
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
        // The runner creates the window before the first update, but if it isn't
        // there yet just try again on the next frame.
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
    /// When `false`, the frame limiter is bypassed (benchmark mode).
    /// Toggled by clicking the FPS readout in the HUD.
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

/// Paces the main loop to at most [`TARGET_FPS`] to avoid burning GPU/power
/// while V-Sync is off. Sleeps most of the wait, spins only the last 200µs
/// (keeps the pacing accurate without a full busy-wait).
///
/// The limiter steps aside while the window is being resized (see
/// [`smooth_resize`]): a sleeping loop answers resize steps late, which is
/// exactly what makes live resizing judder.
fn limit_fps(mut pacer: ResMut<FramePacer>, resize: Res<WindowResize>) {
    if !pacer.cap_enabled || resize.is_active() {
        // Benchmark mode, or a live resize: render as fast as we can.
        pacer.next_frame = None; // start a fresh interval when resuming
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

/// Heartbeat interval while idle: the loop still wakes ~once per second (for
/// autosave and similar slow state), but renders almost nothing. Also read by
/// the HUD's fps readout, which shows this rate while the loop naps.
pub(crate) const IDLE_HEARTBEAT: Duration = Duration::from_secs(1);

/// Drops the loop to ~1 fps whenever there is nothing to do, focused or not,
/// but only when there is no background work in flight.
///
/// Every loading pipeline here is polled once per frame (the preview
/// streamer's per-frame decode budget, the import queue, LOD uploads), so a
/// sleeping loop starves them: start-up/project loads would crawl ~100×
/// slower. While any of that work is in flight the app runs at full speed;
/// the moment the queues drain it drops to the heartbeat. Any window event
/// (click, key, cursor) wakes a sleeping loop instantly, and an in-flight
/// view ease requests redraws (`camera::view_smooth`) so zooming stays smooth
/// without needing constant frames.
///
/// Texture uploads count as in flight too. Bevy prepares render assets under
/// a per-frame byte budget (`RenderAssetBytesPerFrame`), so a big project has
/// thousands of decoded previews still queued on the GPU after the decoders
/// finish. Without this the loop would nap the moment the queues drain and the
/// uploads would then trickle at the budget once per heartbeat; see
/// `GpuTextureState::has_pending`.
///
/// [`WinitSettings`] is re-read by the winit runner after every update, so
/// assigning here takes effect on the next frame.
fn idle_throttle(
    mut settings: ResMut<WinitSettings>,
    import: Res<ImportQueue>,
    previews: Res<PreviewLoader>,
    textures: Res<canvas::lod::GpuTextureState>,
    tab_requests: Res<project::tabs::TabRequests>,
    clipboard: Res<SystemClipboard>,
    pacer: Res<FramePacer>,
    resize: Res<WindowResize>,
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
    // A live resize counts as busy: on Wayland the client drives the resize,
    // so a sleeping loop makes the window lag seconds behind the pointer.
    // Benchmark mode is explicitly uncapped, and an in-flight clipboard
    // worker's result must be collected promptly.
    let busy = resize.is_active()
        || import.is_active()
        || previews.is_active()
        || textures.has_pending()
        || nodes.iter().any(node_texture_pending)
        || !tab_requests.queue.is_empty()
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

/// True while a visible node is still short of its final texture: its preview
/// hasn't decoded, a decoded full-res texture is waiting for the GPU, or its
/// CPU copy hasn't been freed yet. Complements `GpuTextureState::has_pending`
/// (which covers the upload queue itself) for nodes whose texture is in hand
/// but not yet promoted.
fn node_texture_pending(node: &CanvasNode) -> bool {
    node.image().is_some_and(|img| {
        img.data.preview_pending || img.full_incoming.is_some() || img.pending_cpu_free.is_some()
    })
}

/// How long after the last resize event the window still counts as "being
/// resized" (drag steps can pause briefly without ending the drag).
const RESIZE_IDLE: Duration = Duration::from_millis(300);

/// Window-resize responsiveness state. See [`smooth_resize`].
#[derive(Resource, Default)]
pub struct WindowResize {
    /// When the last resize event arrived.
    last: Option<Instant>,
    /// Whether the resize presentation mode is currently applied.
    active: bool,
}

impl WindowResize {
    pub fn is_active(&self) -> bool {
        self.active
    }
}

/// While the window is resized, two things are changed for the duration of the
/// drag (and reverted ~300 ms after the last resize event):
///
/// 1. `present_mode` switches to `Fifo`, so frames are presented in lock-step
///    with the compositor. With uncapped `Immediate` presenting, most presents
///    are thrown away (and race the swap-chain reconfigure that each resize
///    step triggers), which shows up as the `Couldn't get swap chain texture
///    ... Outdated` burst and as visible judder.
/// 2. The frame limiter is bypassed ([`limit_fps`]), so every resize step is
///    answered with a fresh frame immediately instead of up to a frame-cap
///    sleep later.
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
        return; // no transition: leave the window untouched
    }
    resize.active = resizing;
    // The Fifo switch tames X11's swap-chain churn during live resizes. On
    // Wayland the compositor already paces presentation, so switching only
    // deepens the queue and makes the content lag the drag, leave the base
    // mode alone there.
    let target = if resizing && !on_wayland() {
        PresentMode::Fifo
    } else {
        present_mode()
    };
    if window.present_mode != target {
        window.present_mode = target;
    }
}

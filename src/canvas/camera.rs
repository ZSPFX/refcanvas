//! Camera / viewport control.
//!
//! Navigation follows Maya conventions:
//! - Alt+MMB drag (or plain MMB drag, or Space+LMB drag): pan
//! - Alt+RMB drag: dolly (zoom), anchored at the cursor
//! - Mouse wheel: zoom at cursor
//!
//! The view state is stored in f64 so panning far away from the origin stays
//! precise; the Bevy camera transform is a f32 mirror of it.

use bevy::input::mouse::AccumulatedMouseScroll;
use bevy::math::DVec2;
use bevy::prelude::*;
use bevy::window::{PrimaryWindow, RequestRedraw};
use bevy_egui::EguiContexts;

use super::egui_pointer_busy;

/// Zoom-in limit: `log_scale = -7` → ≈1100x magnification (see individual texels).
const MIN_LOG_SCALE: f64 = -7.0;
/// Zoom-out limit: `log_scale = 11` → the whole project fits many times over.
const MAX_LOG_SCALE: f64 = 11.0;
/// Log-zoom change per wheel line.
const ZOOM_WHEEL_SENSITIVITY: f64 = 0.16;
/// Log-zoom change per logical pixel of horizontal Alt+RMB dolly drag.
const ZOOM_DRAG_SENSITIVITY: f64 = 0.006;
/// Time constant of the exponential view smoothing (seconds).
/// Kept very short: zoom still eases between wheel steps, but there should be
/// no perceptible input lag (panning is applied instantly regardless).
const SMOOTH_TAU: f64 = 0.016;
/// Canvas coordinate clamp (f32 rendering precision stays comfortable inside this).
const PAN_CLAMP: f64 = 100_000.0;

#[derive(Resource)]
pub struct View {
    /// Current camera center in world space.
    pub center: DVec2,
    /// Current zoom, as the natural log of "world units per logical pixel"
    /// (the orthographic projection scale is `e^log_scale`; Bevy sizes the
    /// projection from the logical viewport).
    pub log_scale: f64,
    /// Where the view is smoothly heading.
    pub target_center: DVec2,
    pub target_log_scale: f64,
    /// Screen anchor (logical px) that must stay pinned while smoothing.
    pub zoom_anchor: Option<Vec2>,
    pan_prev_cursor: Option<Vec2>,
    dolly_prev_x: Option<f32>,
    /// Sign of the current Alt+RMB dolly drag (+ = zooming in), while active.
    dolly_sign: Option<f32>,
}

impl Default for View {
    fn default() -> Self {
        Self {
            center: DVec2::ZERO,
            log_scale: 0.0,
            target_center: DVec2::ZERO,
            target_log_scale: 0.0,
            zoom_anchor: None,
            pan_prev_cursor: None,
            dolly_prev_x: None,
            dolly_sign: None,
        }
    }
}

impl View {
    /// World units per logical pixel.
    pub fn scale(&self) -> f64 {
        self.log_scale.exp()
    }

    pub fn zoom_percent(&self) -> f64 {
        100.0 / self.scale()
    }

    pub fn is_panning(&self) -> bool {
        self.pan_prev_cursor.is_some()
    }

    /// While an Alt+RMB dolly drag is active: its direction (+ = zooming in).
    pub fn dolly_sign(&self) -> Option<f32> {
        self.dolly_sign
    }

    /// Ask the view to smoothly move to a new center & zoom.
    pub fn fly_to(&mut self, center: DVec2, log_scale: f64) {
        self.target_center = center;
        self.target_log_scale = log_scale.clamp(MIN_LOG_SCALE, MAX_LOG_SCALE);
        self.zoom_anchor = None;
    }
}

/// Tracks when the view last moved, so heavy work (freeing full-res textures
/// and rebuilding their materials) is deferred until navigation stops,
/// keeping fast zoom/pan free of mid-gesture hitches.
#[derive(Resource, Default)]
pub struct ViewMotion {
    prev: Option<(DVec2, f64)>,
    last_change: f64,
}

impl ViewMotion {
    /// Seconds (since startup) at which the view last moved.
    pub fn last_change(&self) -> f64 {
        self.last_change
    }
}

/// Sub-pixel movement in logical px that still counts as "moving".
const MOTION_EPS_LOGICAL_PX: f64 = 0.25;
/// Log-scale change that counts as "moving".
const MOTION_EPS_LOG_SCALE: f64 = 1e-4;

/// Update [`ViewMotion`] from this frame's view change. Runs after
/// `view_smooth` so the group it measures includes both pan and zoom easing.
pub fn track_view_motion(time: Res<Time>, view: Res<View>, mut motion: ResMut<ViewMotion>) {
    let now = time.elapsed_secs_f64();
    let cur = (view.center, view.log_scale);
    match motion.prev {
        Some(prev) => {
            let moved = (cur.0 - prev.0).length() * view.scale() > MOTION_EPS_LOGICAL_PX
                || (cur.1 - prev.1).abs() > MOTION_EPS_LOG_SCALE;
            if moved {
                motion.last_change = now;
            }
        }
        // First frame: treat as at rest so start-up can shed nothing-yet.
        None => motion.last_change = now,
    }
    motion.prev = Some(cur);
}

pub fn setup_camera(mut commands: Commands) {
    // A wider near/far range than the 2D default so the grid quad (z = -1000)
    // and nodes stay comfortably inside the frustum.
    commands.spawn((
        Camera2d,
        Msaa::Sample2,
        Projection::Orthographic(OrthographicProjection {
            near: -8000.0,
            far: 8000.0,
            ..OrthographicProjection::default_2d()
        }),
        Transform::from_xyz(0.0, 0.0, 1000.0),
    ));
}

/// World units (x) per logical pixel, derived from the live projection.
pub fn world_per_logical_px(camera: &Camera, transform: &GlobalTransform) -> Option<f64> {
    let p0 = camera.viewport_to_world_2d(transform, Vec2::ZERO).ok()?;
    let p1 = camera
        .viewport_to_world_2d(transform, Vec2::new(1.0, 0.0))
        .ok()?;
    Some((p1.x - p0.x).abs() as f64)
}

/// World-space rect the camera currently shows, for a logical viewport `size`.
pub fn visible_world_rect(
    camera: &Camera,
    transform: &GlobalTransform,
    size: Vec2,
) -> Option<Rect> {
    let a = camera.viewport_to_world_2d(transform, Vec2::ZERO).ok()?;
    let b = camera.viewport_to_world_2d(transform, size).ok()?;
    Some(Rect::from_corners(a, b))
}

pub fn view_input(
    window: Single<&Window, With<PrimaryWindow>>,
    mouse_buttons: Res<ButtonInput<MouseButton>>,
    keys: Res<ButtonInput<KeyCode>>,
    scroll: Res<AccumulatedMouseScroll>,
    camera: Single<(&Camera, &GlobalTransform)>,
    mut view: ResMut<View>,
    mut contexts: EguiContexts,
) {
    let egui_busy = egui_pointer_busy(&mut contexts);

    let cursor = window.cursor_position();
    let (camera, cam_transform) = *camera;
    let wpp = world_per_logical_px(camera, cam_transform).unwrap_or(1.0);

    let alt = keys.pressed(KeyCode::AltLeft) || keys.pressed(KeyCode::AltRight);

    // ---- Zoom: mouse wheel (anchored at the cursor) ----
    if !egui_busy && cursor.is_some() && scroll.delta.y != 0.0 {
        let delta = scroll.delta.y as f64;
        view.target_log_scale = (view.target_log_scale - delta * ZOOM_WHEEL_SENSITIVITY)
            .clamp(MIN_LOG_SCALE, MAX_LOG_SCALE);
        view.zoom_anchor = cursor;
    }

    // ---- Zoom: Alt+RMB horizontal dolly (Maya) ----
    if !egui_busy && alt && mouse_buttons.pressed(MouseButton::Right) {
        if let Some(c) = cursor {
            if let Some(prev) = view.dolly_prev_x {
                let dx = c.x - prev;
                if dx != 0.0 {
                    view.dolly_sign = Some(dx.signum());
                }
                view.target_log_scale = (view.target_log_scale - dx as f64 * ZOOM_DRAG_SENSITIVITY)
                    .clamp(MIN_LOG_SCALE, MAX_LOG_SCALE);
            }
            view.dolly_prev_x = Some(c.x);
            if view.zoom_anchor.is_none() {
                view.zoom_anchor = Some(c);
            }
        }
    } else {
        view.dolly_prev_x = None;
        view.dolly_sign = None;
    }

    // ---- Pan: MMB drag, Space+LMB drag, or Alt+MMB drag ----
    let space = keys.pressed(KeyCode::Space);
    let panning = mouse_buttons.pressed(MouseButton::Middle)
        || (space && mouse_buttons.pressed(MouseButton::Left));
    if !egui_busy && panning {
        if let Some(c) = cursor {
            if let Some(prev) = view.pan_prev_cursor {
                let d = c - prev;
                let world_delta = DVec2::new(d.x as f64, -(d.y as f64)) * wpp;
                view.center -= world_delta;
                view.target_center = view.center;
                // A manual pan overrides a pending zoom anchor.
                view.zoom_anchor = None;
            }
            view.pan_prev_cursor = Some(c);
        }
    } else {
        view.pan_prev_cursor = None;
    }

    view.center = view
        .center
        .clamp(DVec2::splat(-PAN_CLAMP), DVec2::splat(PAN_CLAMP));
    view.target_center = view
        .target_center
        .clamp(DVec2::splat(-PAN_CLAMP), DVec2::splat(PAN_CLAMP));
}

/// Exponentially smooth the view toward its target and pin the zoom anchor.
pub fn view_smooth(
    time: Res<Time>,
    camera: Single<(
        &Camera,
        &mut Transform,
        &mut GlobalTransform,
        &mut Projection,
    )>,
    mut view: ResMut<View>,
    mut redraw: MessageWriter<RequestRedraw>,
) {
    // After a reactive-mode nap the first frame's delta carries the whole
    // sleep, which would flush the exponential ease in a single frame (arrow-
    // key hops snapped instead of gliding). Clamp the step to a frame-ish
    // slice; the redraw request below keeps the glide's remaining frames
    // coming.
    let dt = time.delta_secs_f64().min(1.0 / 60.0);
    let f = 1.0 - (-dt / SMOOTH_TAU).exp();

    let (camera, mut transform, mut global, mut projection) = camera.into_inner();

    // World point under the zoom anchor *before* this frame's update.
    let pinned = view.zoom_anchor.and_then(|anchor| {
        camera
            .viewport_to_world_2d(&global, anchor)
            .ok()
            .map(|w| (anchor, w))
    });

    view.log_scale += (view.target_log_scale - view.log_scale) * f;
    let new_center = view.center + (view.target_center - view.center) * f;
    view.center = new_center;

    apply_to_camera(&view, &mut transform, &mut projection);

    // Re-pin the anchor: shift the center so the same world point stays under it.
    if let Some((anchor, before)) = pinned {
        sync_global(&transform, &mut global);
        if let Ok(after) = camera.viewport_to_world_2d(&global, anchor) {
            let d = (before - after).as_dvec2();
            view.center += d;
            view.target_center += d;
            view.center = view
                .center
                .clamp(DVec2::splat(-PAN_CLAMP), DVec2::splat(PAN_CLAMP));
            view.target_center = view
                .target_center
                .clamp(DVec2::splat(-PAN_CLAMP), DVec2::splat(PAN_CLAMP));
            apply_to_camera(&view, &mut transform, &mut projection);
        }
    }

    // Done zooming? Drop the anchor.
    let settled = (view.target_log_scale - view.log_scale).abs() < 1e-3;
    if settled {
        view.zoom_anchor = None;
    }

    // Keep frames coming while the ease is still moving. The loop drops to a
    // reactive mode when idle, so after the last input event a wheel step or
    // an F-press would otherwise freeze mid-ease until the next event wakes
    // it. (Harmless while the loop is already running continuously.)
    let center_left = (view.target_center - view.center).length() * view.scale();
    if !settled || center_left > 0.05 {
        redraw.write(RequestRedraw);
    }

    sync_global(&transform, &mut global);
}

/// Mirror Transform into GlobalTransform so same-frame view math is current
/// (propagation normally happens in PostUpdate).
fn sync_global(transform: &Transform, global: &mut GlobalTransform) {
    *global = GlobalTransform::from(*transform);
}

fn apply_to_camera(view: &View, transform: &mut Transform, projection: &mut Projection) {
    transform.translation.x = view.center.x as f32;
    transform.translation.y = view.center.y as f32;
    if let Projection::Orthographic(ortho) = &mut *projection {
        ortho.scale = view.scale() as f32;
    }
}

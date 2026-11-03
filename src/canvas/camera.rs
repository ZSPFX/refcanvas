//! Camera / viewport control.
//!
//! Viewport navigation:
//! - Alt+MMB drag (or plain MMB drag, or Space+LMB drag): pan
//! - Alt+RMB drag: dolly (zoom), anchored at the cursor
//! - Mouse wheel: zoom at cursor
//! - Trackpad two-finger drag: pan (the system sends it as precise scrolling)
//! - Ctrl/Cmd + scroll: zoom, for trackpads and wheels alike
//!
//! The view state is stored in f64 so panning far away from the origin stays
//! precise; the Bevy camera transform is a f32 mirror of it.

use bevy::input::mouse::{AccumulatedMouseScroll, MouseScrollUnit};
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
/// Log-zoom change per unit of pinch magnification. A full-finger pinch reports
/// a magnification of roughly a few tenths per event, so this needs to be much
/// larger than the per-line wheel gain to feel comparable.
const PINCH_SENSITIVITY: f64 = 4.0;
/// Scroll pixels that count as one wheel line. Precision touchpads report
/// pixels (a gesture is tens to hundreds of them), wheels report lines; both
/// must reach the zoom curve at comparable magnitudes.
const SCROLL_PIXELS_PER_LINE: f32 = 100.0;
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
    /// True while a precise-scroll gesture (trackpad two-finger drag) pans.
    pub wheel_pan: bool,
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
            wheel_pan: false,
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

    /// Pan by a drag in logical screen pixels (y down).
    pub fn pan_pixels(&mut self, delta: Vec2, wpp: f64) {
        if delta == Vec2::ZERO {
            return;
        }
        let world_delta = DVec2::new(delta.x as f64, -(delta.y as f64)) * wpp;
        self.center -= world_delta;
        self.target_center = self.center;
        // A manual pan overrides a pending zoom anchor.
        self.zoom_anchor = None;
        self.clamp_pan();
    }

    /// Zoom by a log-scale delta, optionally pinned at a screen anchor.
    pub fn zoom_log(&mut self, dlog: f64, anchor: Option<Vec2>) {
        self.target_log_scale = (self.target_log_scale + dlog).clamp(MIN_LOG_SCALE, MAX_LOG_SCALE);
        self.zoom_anchor = anchor;
    }

    fn clamp_pan(&mut self) {
        self.center = self
            .center
            .clamp(DVec2::splat(-PAN_CLAMP), DVec2::splat(PAN_CLAMP));
        self.target_center = self
            .target_center
            .clamp(DVec2::splat(-PAN_CLAMP), DVec2::splat(PAN_CLAMP));
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

#[allow(clippy::too_many_arguments)]
pub fn view_input(
    window: Single<&Window, With<PrimaryWindow>>,
    mouse_buttons: Res<ButtonInput<MouseButton>>,
    keys: Res<ButtonInput<KeyCode>>,
    scroll: Res<AccumulatedMouseScroll>,
    gesture: Res<WheelGesture>,
    mut pinch: MessageReader<bevy::input::gestures::PinchGesture>,
    camera: Single<(&Camera, &GlobalTransform)>,
    pointer: Res<super::touch::Pointer>,
    mut view: ResMut<View>,
    mut contexts: EguiContexts,
) {
    let egui_busy = egui_pointer_busy(&mut contexts);
    // Touch navigation is handled by `touch_input`; ignore the mouse while a
    // touch is active, including the mouse events Windows promotes from touch.
    let mouse_busy = pointer.touch_active || egui_busy;

    let cursor = window.cursor_position();
    let (camera, cam_transform) = *camera;
    let wpp = world_per_logical_px(camera, cam_transform).unwrap_or(1.0);

    let alt = keys.pressed(KeyCode::AltLeft) || keys.pressed(KeyCode::AltRight);

    // ---- Scroll: wheel zooms, a trackpad two-finger drag pans ----
    if !mouse_busy && cursor.is_some() && scroll.delta != Vec2::ZERO {
        let ctrl = keys.pressed(KeyCode::ControlLeft)
            || keys.pressed(KeyCode::ControlRight)
            || keys.pressed(KeyCode::SuperLeft)
            || keys.pressed(KeyCode::SuperRight);
        let action = scroll_action(gesture.active, ctrl);
        match (scroll_unit_lines(&scroll), action) {
            (Some(lines), ScrollAction::Pan) => {
                // Deltas follow the finger: dragging down scrolls the content
                // up, which moves the camera the other way.
                view.wheel_pan = true;
                view.pan_pixels(lines, wpp);
            }
            (Some(lines), ScrollAction::Zoom) => {
                view.wheel_pan = false;
                view.zoom_log(-lines.y as f64 * ZOOM_WHEEL_SENSITIVITY, cursor);
            }
            (None, _) => {}
        }
    } else if scroll.delta == Vec2::ZERO {
        view.wheel_pan = false;
    }

    // ---- Zoom: Alt+RMB horizontal dolly ----
    if !mouse_busy && alt && mouse_buttons.pressed(MouseButton::Right) {
        if let Some(c) = cursor {
            if let Some(prev) = view.dolly_prev_x {
                let dx = c.x - prev;
                if dx != 0.0 {
                    view.dolly_sign = Some(dx.signum());
                }
                let anchor = view.zoom_anchor.or(Some(c));
                view.zoom_log(-dx as f64 * ZOOM_DRAG_SENSITIVITY, anchor);
            }
            view.dolly_prev_x = Some(c.x);
        }
    } else {
        view.dolly_prev_x = None;
        view.dolly_sign = None;
    }

    // ---- Zoom: trackpad pinch ----
    // winit reports magnification with the opposite sign to the wheel: a
    // spread (zoom in) is negative. The zoom is anchored at the cursor, like
    // the wheel.
    let magnification: f32 = pinch.read().map(|pinch| pinch.0).sum();
    if !mouse_busy && magnification != 0.0 {
        view.zoom_log(-(magnification as f64) * PINCH_SENSITIVITY, cursor);
    }

    // ---- Pan: MMB drag, Space+LMB drag, or Alt+MMB drag ----
    let space = keys.pressed(KeyCode::Space);
    let panning = mouse_buttons.pressed(MouseButton::Middle)
        || (space && mouse_buttons.pressed(MouseButton::Left));
    if !mouse_busy && panning {
        if let Some(c) = cursor {
            if let Some(prev) = view.pan_prev_cursor {
                view.pan_pixels(c - prev, wpp);
            }
            view.pan_prev_cursor = Some(c);
        }
    } else {
        view.pan_prev_cursor = None;
    }
}

/// A trackpad gesture that arrived as precise scrolling, tracked so the HUD
/// and cursor logic can treat it like a drag while it lasts.
///
/// Winit reports the gesture phases in `MouseWheel::phase` (`Started`,
/// `Moved`, `Ended`), while Bevy's accumulator only carries the summed delta.
/// Watching the phase keeps the pan latched across a frame with no new delta,
/// which is what makes a slow two-finger drag feel continuous.
#[derive(Resource, Default)]
pub struct WheelGesture {
    /// True between `Started` and `Ended` of a precise-scroll gesture.
    pub active: bool,
}

/// Track the precise-scroll gesture phase.
pub fn track_wheel_gesture(
    mut wheels: MessageReader<bevy::input::mouse::MouseWheel>,
    mut gesture: ResMut<WheelGesture>,
) {
    for wheel in wheels.read() {
        // Only a trackpad reports a real gesture lifecycle. A classic wheel
        // sends `Moved` for every notch, which must never latch a pan.
        match wheel.phase {
            bevy::input::touch::TouchPhase::Started => gesture.active = true,
            bevy::input::touch::TouchPhase::Ended | bevy::input::touch::TouchPhase::Canceled => {
                gesture.active = false
            }
            _ => {}
        }
    }
}

/// Why scroll input is being handled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ScrollAction {
    Pan,
    Zoom,
}

/// Trackpad gestures pan; anything with a zoom modifier, and any device that
/// does not report a gesture (a classic wheel), zooms.
fn scroll_action(gesture_active: bool, zoom_modifier: bool) -> ScrollAction {
    if gesture_active && !zoom_modifier {
        ScrollAction::Pan
    } else {
        ScrollAction::Zoom
    }
}

/// Scroll delta converted to wheel lines, so wheels and precision touchpads
/// share one sensitivity.
fn scroll_unit_lines(scroll: &AccumulatedMouseScroll) -> Option<Vec2> {
    let delta = scroll.delta;
    if delta == Vec2::ZERO {
        return None;
    }
    Some(match scroll.unit {
        MouseScrollUnit::Line => delta,
        MouseScrollUnit::Pixel => delta / SCROLL_PIXELS_PER_LINE,
    })
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

#[cfg(test)]
mod tests {
    use super::*;

    fn scroll(unit: MouseScrollUnit, x: f32, y: f32) -> AccumulatedMouseScroll {
        AccumulatedMouseScroll {
            unit,
            delta: Vec2::new(x, y),
        }
    }

    #[test]
    fn wheel_lines_pass_through() {
        let lines = scroll_unit_lines(&scroll(MouseScrollUnit::Line, 0.0, 1.0)).unwrap();
        assert_eq!(lines, Vec2::new(0.0, 1.0));
    }

    #[test]
    fn precision_pixels_are_normalized_to_lines() {
        // A trackpad gesture of 150 px must not be 150 wheel notches.
        let lines = scroll_unit_lines(&scroll(MouseScrollUnit::Pixel, 0.0, 150.0)).unwrap();
        assert!((lines.y - 1.5).abs() < 1e-6);
        // Same magnitude as a wheel line reaches the zoom curve identically.
        let wheel = scroll_unit_lines(&scroll(MouseScrollUnit::Line, 0.0, 100.0 / 100.0)).unwrap();
        assert_eq!(wheel.y, 1.0);
    }

    #[test]
    fn only_trackpad_gestures_pan() {
        // A classic wheel never reports a gesture: it must zoom.
        assert_eq!(scroll_action(false, false), ScrollAction::Zoom);
        // Two-finger trackpad drag pans.
        assert_eq!(scroll_action(true, false), ScrollAction::Pan);
        // Cmd/Ctrl always means zoom, even mid-gesture.
        assert_eq!(scroll_action(true, true), ScrollAction::Zoom);
        assert_eq!(scroll_action(false, true), ScrollAction::Zoom);
    }

    #[test]
    fn empty_scroll_is_none() {
        assert!(scroll_unit_lines(&scroll(MouseScrollUnit::Pixel, 0.0, 0.0)).is_none());
    }

    #[test]
    fn pan_respects_the_scroll_direction() {
        let mut view = View::default();
        // Precise scroll down (content scrolls up) moves the camera up.
        view.pan_pixels(Vec2::new(0.0, 100.0), 1.0);
        assert!(view.center.y > 0.0);
        assert_eq!(view.zoom_anchor, None);
    }
}

//! Touch input: one-finger pan, pinch zoom, and a pointer shared with the mouse.
//!
//! Windows touch arrives through winit (WM_POINTER / WM_TOUCH) as ordinary
//! Bevy `TouchInput` messages, so no platform-specific code is needed.

use std::time::{Duration, Instant};

use bevy::prelude::*;
use bevy::window::{PrimaryWindow, RequestRedraw};
use bevy_egui::EguiContexts;

use super::camera::{View, world_per_logical_px};
use super::frames::{FrameLabelHits, FrameRename};
use super::hit;
use super::manipulate::{self, ZCounter};
use super::node::{InactiveTab, TabOwner};
use super::selection::{self, Interaction, NodeQ, Selection, press_label};
use crate::project::tabs::Tabs;

/// Movement under this distance still counts as a tap, logical pixels.
const TAP_SLOP_PX: f32 = 12.0;
/// Longest press that still counts as a tap.
const TAP_MAX: Duration = Duration::from_millis(450);
/// Maximum delay between two taps.
const DOUBLE_TAP_MAX: Duration = Duration::from_millis(350);
/// Maximum distance between two taps, logical pixels.
const DOUBLE_TAP_DIST_PX: f32 = 36.0;
/// Movement before a touch becomes a node drag, logical pixels.
pub const TOUCH_DRAG_THRESHOLD_PX: f32 = 8.0;

/// Pan friction time constant, seconds.
const PAN_TAU: f64 = 0.24;
/// Zoom friction time constant, seconds.
const ZOOM_TAU: f64 = 0.18;
/// How quickly the velocity estimate follows the finger.
const VELOCITY_TAU: f32 = 0.045;
/// Smallest release velocity that starts a fling.
const PAN_START_MIN: f32 = 100.0;
/// Pan speed at which the fling stops, logical px/s.
const PAN_STOP: f32 = 20.0;
/// Upper bound on the release pan speed, logical px/s.
const PAN_MAX: f32 = 6000.0;
/// Smallest release zoom velocity that starts a fling.
const ZOOM_START_MIN: f64 = 0.10;
/// Zoom speed at which the fling stops, log scale per second.
const ZOOM_STOP: f64 = 0.02;
/// Upper bound on the release zoom speed, log scale per second.
const ZOOM_MAX: f64 = 2.0;
/// Time after the last real movement that still counts as a fling.
const RELEASE_GRACE: Duration = Duration::from_millis(100);
/// Zoom velocity decays this fast once a pinch falls back to one finger.
const HANDOFF_ZOOM_TAU: f64 = 0.10;

/// Kinetic follow-through after a touch gesture.
#[derive(Resource, Default)]
pub struct Inertia {
    pan: Vec2,
    zoom: f64,
    anchor: Option<Vec2>,
    active: bool,
}

impl Inertia {
    pub fn is_active(&self) -> bool {
        self.active
    }

    fn stop(&mut self) {
        *self = Self::default();
    }

    fn start(&mut self, pan: Vec2, zoom: f64, anchor: Option<Vec2>) {
        self.pan = pan;
        self.zoom = zoom;
        self.anchor = anchor;
        self.active = pan != Vec2::ZERO || zoom != 0.0;
    }
}

/// Primary canvas pointer, fed by the mouse or a single active touch.
///
/// The canvas systems read this instead of the mouse directly, so touch and
/// mouse share one interaction path. While a touch is down the mouse is
/// ignored, which also drops the mouse events Windows synthesizes from touch.
#[derive(Resource, Default)]
pub struct Pointer {
    pub screen: Option<Vec2>,
    pub pressed: bool,
    pub just_pressed: bool,
    pub just_released: bool,
    /// This frame's input came from touch.
    pub is_touch: bool,
    /// A touch gesture is in progress (including the release frame).
    pub touch_active: bool,
    /// The press is the second tap of a double tap.
    pub double_tap: bool,
    /// This touch press navigates the camera instead of editing a node.
    pub touch_pan: bool,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Mode {
    #[default]
    Idle,
    Single,
    Multi,
    Ignore,
}

/// Gesture state carried between frames.
#[derive(Resource, Default)]
pub struct TouchState {
    mode: Mode,
    primary: Option<u64>,
    secondary: Option<u64>,
    start: Option<Vec2>,
    started: Option<Instant>,
    last_pos: Option<Vec2>,
    press_node: Option<Entity>,
    pan: bool,
    moved: bool,
    double_tap: bool,
    last_mid: Option<Vec2>,
    last_dist: f32,
    last_tap: Option<(Instant, Vec2)>,
    pan_velocity: Vec2,
    zoom_velocity: f64,
    last_frame: Option<Instant>,
    last_motion: Option<Instant>,
    pinch_mid: Option<Vec2>,
    suppress_tap: bool,
}

impl TouchState {
    fn reset(&mut self) {
        *self = Self {
            last_tap: self.last_tap,
            ..default()
        };
    }
}

#[derive(bevy::ecs::system::SystemParam)]
pub struct TapCtx<'w> {
    pub zc: ResMut<'w, ZCounter>,
    pub rename: ResMut<'w, FrameRename>,
}

#[allow(clippy::too_many_arguments)]
pub fn touch_input(
    touches: Res<Touches>,
    window: Single<&Window, With<PrimaryWindow>>,
    mouse: Res<ButtonInput<MouseButton>>,
    mut contexts: EguiContexts,
    camera: Single<(&Camera, &GlobalTransform)>,
    mut view: ResMut<View>,
    mut pointer: ResMut<Pointer>,
    mut state: ResMut<TouchState>,
    mut interaction: ResMut<Interaction>,
    mut selection: ResMut<Selection>,
    mut inertia: ResMut<Inertia>,
    mut tap: TapCtx,
    tabs: Res<Tabs>,
    label_hits: Res<FrameLabelHits>,
    q_own: Query<(Entity, &TabOwner), Without<InactiveTab>>,
    mut q: NodeQ,
) {
    let egui_busy = super::egui_pointer_busy(&mut contexts);
    let now = Instant::now();

    pointer.just_pressed = false;
    pointer.just_released = false;
    pointer.double_tap = false;
    pointer.touch_pan = false;

    // A canceled touch (focus loss, gesture stolen by the system) never sends
    // an end event, so roll the drag back and drop the gesture.
    if touches.any_just_canceled() {
        selection::cancel_interaction(&mut interaction, &mut q);
        inertia.stop();
        state.reset();
        pointer.pressed = false;
    }

    let (camera, cam_tf) = *camera;
    let wpp = world_per_logical_px(camera, cam_tf).unwrap_or(1.0) as f32;

    let pressed: Vec<(u64, Vec2)> = touches
        .iter_just_pressed()
        .map(|t| (t.id(), t.position()))
        .collect();
    for (id, pos) in pressed {
        match state.mode {
            Mode::Idle => {
                let stopping = inertia.is_active();
                if stopping {
                    inertia.stop();
                }
                if egui_busy {
                    state.mode = Mode::Ignore;
                    state.suppress_tap = stopping;
                    pointer.screen = Some(pos);
                    continue;
                }
                let Ok(world) = camera.viewport_to_world_2d(cam_tf, pos) else {
                    state.mode = Mode::Ignore;
                    continue;
                };
                let keep = selection::owner_filter(tabs.active_id(), &q_own);
                let hit = selection::pick_topmost(
                    world,
                    &q,
                    &keep,
                    &label_hits,
                    hit::TOUCH_NODE_SLOP_PX * wpp,
                );
                let on_handle = selection::selection_frame_of(&q, &selection)
                    .and_then(|frame| {
                        let (handle_px, rotate_px) = hit::touch_radii(&frame, wpp);
                        hit::hit_frame_with_radii(world, &frame, wpp, handle_px, rotate_px)
                    })
                    .is_some();

                let double_tap = state.last_tap.is_some_and(|(t, p)| {
                    now.duration_since(t) <= DOUBLE_TAP_MAX && p.distance(pos) <= DOUBLE_TAP_DIST_PX
                });
                if double_tap {
                    state.last_tap = None;
                }

                // A drag only edits an image after it was selected by a tap.
                // Every other single-finger press navigates the camera. A tap
                // that stops a fling also becomes a pan, so it cannot select.
                let on_selected_node = hit.is_some_and(|(entity, _)| selection.contains(entity));
                let pan = stopping || (!double_tap && !on_handle && !on_selected_node);

                state.mode = Mode::Single;
                state.primary = Some(id);
                state.start = Some(pos);
                state.started = Some(now);
                state.last_pos = Some(pos);
                state.press_node = hit.map(|(entity, _)| entity);
                state.pan = pan;
                state.moved = false;
                state.double_tap = double_tap;
                state.pan_velocity = Vec2::ZERO;
                state.zoom_velocity = 0.0;
                state.last_frame = Some(now);
                state.last_motion = None;
                state.pinch_mid = None;
                state.suppress_tap = stopping;

                pointer.screen = Some(pos);
                pointer.pressed = true;
                pointer.just_pressed = true;
                pointer.double_tap = double_tap;
                pointer.touch_pan = pan;
            }
            Mode::Single => {
                // A second finger always means navigation, cancel edits.
                state.mode = Mode::Multi;
                state.secondary = Some(id);
                state.last_mid = None;
                state.last_dist = 0.0;
                state.double_tap = false;
                state.last_tap = None;
                state.pan = false;
                state.pan_velocity = Vec2::ZERO;
                state.zoom_velocity = 0.0;
                state.last_frame = Some(now);
                state.last_motion = None;
                state.pinch_mid = None;
                pointer.screen = Some(pos);
                pointer.pressed = false;
                pointer.just_pressed = false;
                pointer.double_tap = false;
                pointer.touch_pan = false;
                selection::cancel_interaction(&mut interaction, &mut q);
            }
            Mode::Multi | Mode::Ignore => {}
        }
    }

    match state.mode {
        Mode::Single => {
            if let Some(pos) = state
                .primary
                .and_then(|id| touches.get_pressed(id))
                .map(|t| t.position())
            {
                let dt = state
                    .last_frame
                    .map(|t| now.duration_since(t).as_secs_f32())
                    .unwrap_or(0.0)
                    .min(0.1);
                if dt > 0.001 {
                    if let Some(prev) = state.last_pos {
                        let delta = pos - prev;
                        let instant = if delta.length_squared() > 0.0001 {
                            delta / dt
                        } else {
                            Vec2::ZERO
                        };
                        state.pan_velocity = smooth_vec(state.pan_velocity, instant, dt);
                        if delta.length_squared() > 0.01 {
                            state.last_motion = Some(now);
                        }
                    }
                    // A pinch that fell back to one finger loses its zoom
                    // momentum quickly; the remaining finger drives panning.
                    state.zoom_velocity *= (-dt as f64 / HANDOFF_ZOOM_TAU).exp();
                }
                state.last_frame = Some(now);

                let was_moved = state.moved;
                if !state.moved && pos.distance(state.start.unwrap_or(pos)) > TAP_SLOP_PX {
                    state.moved = true;
                }
                if state.pan && state.moved {
                    if !was_moved {
                        // Include the movement spent crossing the tap slop.
                        if let Some(start) = state.start {
                            view.pan_pixels(pos - start, wpp as f64);
                        }
                    } else if let Some(prev) = state.last_pos {
                        view.pan_pixels(pos - prev, wpp as f64);
                    }
                }
                state.last_pos = Some(pos);
                pointer.screen = Some(pos);
                pointer.pressed = true;
            }
        }
        Mode::Multi => {
            let a = state.primary.and_then(|id| touches.get_pressed(id));
            let b = state.secondary.and_then(|id| touches.get_pressed(id));
            if let (Some(a), Some(b)) = (a, b) {
                let pa = a.position();
                let pb = b.position();
                let mid = (pa + pb) * 0.5;
                let dist = pa.distance(pb);
                let dt = state
                    .last_frame
                    .map(|t| now.duration_since(t).as_secs_f32())
                    .unwrap_or(0.0)
                    .min(0.1);
                if let Some(prev_mid) = state.last_mid {
                    if dt > 0.001 {
                        let delta = mid - prev_mid;
                        state.pan_velocity = smooth_vec(state.pan_velocity, delta / dt, dt);
                        let mut moved = delta.length_squared() > 0.01;
                        if state.last_dist > 0.5 && dist > 0.5 {
                            let zoom_inst = -((dist / state.last_dist).ln() as f64) / dt as f64;
                            state.zoom_velocity = smooth_f64(state.zoom_velocity, zoom_inst, dt);
                            moved |= (dist - state.last_dist).abs() > 0.01;
                        }
                        if moved {
                            state.moved = true;
                            state.last_motion = Some(now);
                        }
                    }
                    view.pan_pixels(mid - prev_mid, wpp as f64);
                    if state.last_dist > 0.5 && dist > 0.5 {
                        let dlog = -(dist / state.last_dist).ln() as f64;
                        view.zoom_log(dlog, Some(mid));
                    }
                }
                state.last_frame = Some(now);
                state.last_mid = Some(mid);
                state.last_dist = dist;
                state.pinch_mid = Some(mid);
                pointer.screen = Some(mid);
            }
        }
        Mode::Idle | Mode::Ignore => {}
    }

    let released: Vec<(u64, Vec2)> = touches
        .iter_just_released()
        .map(|t| (t.id(), t.position()))
        .collect();
    let mut released_primary = None;
    for (id, pos) in released {
        if state.mode == Mode::Single && state.primary == Some(id) {
            released_primary = Some(pos);
        }
    }
    if let Some(pos) = released_primary {
        let is_tap = !state.moved
            && state
                .started
                .is_some_and(|t| now.duration_since(t) <= TAP_MAX);
        if is_tap {
            if state.pan && !state.suppress_tap {
                if let Some(entity) = state.press_node {
                    let started = state.start.unwrap_or(pos);
                    let world = camera
                        .viewport_to_world_2d(cam_tf, started)
                        .unwrap_or_default();
                    if press_label(
                        entity,
                        world,
                        &label_hits,
                        &mut tap.rename,
                        &mut selection,
                        &q,
                    ) {
                        *interaction = Interaction::Idle;
                    } else {
                        if !selection.contains(entity) {
                            selection.nodes = vec![entity];
                        }
                        let is_frame = q
                            .get(entity)
                            .map(|(_, _, _, node)| node.kind.is_frame())
                            .unwrap_or(true);
                        if !is_frame {
                            manipulate::raise(entity, &mut q, &mut tap.zc);
                        }
                    }
                } else {
                    selection.clear();
                }
            }
            if !state.double_tap && !state.suppress_tap {
                state.last_tap = Some((now, state.start.unwrap_or(pos)));
            }
        }
        // Only camera pans fling. Node drags and handle edits stay put.
        if state.pan {
            start_inertia(now, &state, &mut inertia);
        }
        pointer.just_released = true;
        pointer.pressed = false;
        pointer.screen = Some(pos);
        state.reset();
    }

    // Lifting one finger after a pinch hands the gesture back to the
    // remaining finger as a pan, re-anchored so the view does not jump.
    if state.mode == Mode::Multi {
        let mut current = touches.iter();
        if let (Some(touch), None) = (current.next(), current.next()) {
            let pos = touch.position();
            state.mode = Mode::Single;
            state.primary = Some(touch.id());
            state.secondary = None;
            state.start = Some(pos);
            state.started = Some(now);
            state.last_pos = Some(pos);
            state.press_node = None;
            state.pan = true;
            state.moved = true;
            state.double_tap = false;
            state.last_mid = None;
            state.last_dist = 0.0;
            state.last_frame = Some(now);
            state.last_motion = Some(now);
            pointer.screen = Some(pos);
            pointer.pressed = true;
        }
    }

    if touches.iter().next().is_none() {
        match state.mode {
            Mode::Single | Mode::Multi | Mode::Ignore => {
                if state.mode == Mode::Multi {
                    start_inertia(now, &state, &mut inertia);
                }
                selection::cancel_interaction(&mut interaction, &mut q);
                state.reset();
                pointer.pressed = false;
            }
            Mode::Idle => {}
        }
    }

    let touch_active =
        state.mode != Mode::Idle || touches.iter().next().is_some() || touches.any_just_released();
    pointer.touch_active = touch_active;
    pointer.is_touch = touch_active;

    if !touch_active {
        pointer.screen = window.cursor_position();
        pointer.pressed = mouse.pressed(MouseButton::Left);
        pointer.just_pressed = mouse.just_pressed(MouseButton::Left);
        pointer.just_released = mouse.just_released(MouseButton::Left);
        pointer.double_tap = false;
    }
}

fn smooth_vec(current: Vec2, target: Vec2, dt: f32) -> Vec2 {
    let alpha = 1.0 - (-dt / VELOCITY_TAU).exp();
    current + (target - current) * alpha
}

fn smooth_f64(current: f64, target: f64, dt: f32) -> f64 {
    let alpha = 1.0 - (-dt as f64 / VELOCITY_TAU as f64).exp();
    current + (target - current) * alpha
}

fn start_inertia(now: Instant, state: &TouchState, inertia: &mut Inertia) {
    if !state.moved
        || !state
            .last_motion
            .is_some_and(|t| now.duration_since(t) <= RELEASE_GRACE)
    {
        return;
    }

    let mut pan = state.pan_velocity;
    if pan.length() > PAN_MAX {
        pan = pan.normalize() * PAN_MAX;
    }
    if pan.length() < PAN_START_MIN {
        pan = Vec2::ZERO;
    }

    let mut zoom = state.zoom_velocity.clamp(-ZOOM_MAX, ZOOM_MAX);
    if zoom.abs() < ZOOM_START_MIN || state.pinch_mid.is_none() {
        zoom = 0.0;
    }

    if pan != Vec2::ZERO || zoom != 0.0 {
        inertia.start(pan, zoom, state.pinch_mid);
    }
}

/// Continue a released pan or pinch with browser-like friction. A new touch
/// stops it immediately in `touch_input`.
pub fn apply_inertia(
    time: Res<Time>,
    camera: Single<(&Camera, &GlobalTransform)>,
    pointer: Res<Pointer>,
    mut view: ResMut<View>,
    mut inertia: ResMut<Inertia>,
    mut redraw: MessageWriter<RequestRedraw>,
) {
    if !inertia.active || pointer.touch_active {
        return;
    }

    let dt = time.delta_secs_f64().min(1.0 / 30.0);
    if dt <= 0.0 {
        return;
    }
    let (camera, cam_tf) = *camera;
    let wpp = world_per_logical_px(camera, cam_tf).unwrap_or(1.0) as f32;

    if inertia.pan.length_squared() > 0.01 {
        view.pan_pixels(inertia.pan * dt as f32, wpp as f64);
    }
    if inertia.zoom.abs() > ZOOM_STOP * 0.5 {
        view.zoom_log(inertia.zoom * dt, inertia.anchor);
    }

    inertia.pan *= (-dt / PAN_TAU).exp() as f32;
    inertia.zoom *= (-dt / ZOOM_TAU).exp();

    if inertia.pan.length() < PAN_STOP && inertia.zoom.abs() < ZOOM_STOP {
        inertia.stop();
    } else {
        redraw.write(RequestRedraw);
    }
}

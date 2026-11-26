//! the canvas: camera/view control, dot grid, nodes, selection & interaction.

pub mod camera;
pub mod cursor;
pub mod export;
pub mod frames;
pub mod gizmos;
pub mod grid;
pub mod hit;
pub mod keyboard;
pub mod lod;
pub mod manipulate;
pub mod node;
pub mod selection;
pub mod snap;
pub mod touch;
pub mod undo;

use bevy::prelude::*;
use bevy_egui::{EguiContexts, egui};

/// last frame each modifier was seen; chords can arrive split across frames
/// (especially synthetic input), so a modifier released within a few frames counts.
#[derive(Resource, Default)]
pub struct ChordMemory {
    ctrl_last: u64,
    cmd_last: u64,
    shift_last: u64,
    alt_last: u64,
}

const CHORD_GRACE_FRAMES: u64 = 4;

pub fn track_chords(
    keys: Res<ButtonInput<KeyCode>>,
    frame: Res<lod::FrameCounter>,
    mut mem: ResMut<ChordMemory>,
) {
    let f = frame.0;
    if keys.any_pressed([KeyCode::ControlLeft, KeyCode::ControlRight])
        || keys.any_just_pressed([KeyCode::ControlLeft, KeyCode::ControlRight])
        || keys.any_just_released([KeyCode::ControlLeft, KeyCode::ControlRight])
    {
        mem.ctrl_last = f;
    }
    if keys.any_pressed([KeyCode::SuperLeft, KeyCode::SuperRight])
        || keys.any_just_pressed([KeyCode::SuperLeft, KeyCode::SuperRight])
        || keys.any_just_released([KeyCode::SuperLeft, KeyCode::SuperRight])
    {
        mem.cmd_last = f;
    }
    if keys.any_pressed([KeyCode::ShiftLeft, KeyCode::ShiftRight])
        || keys.any_just_pressed([KeyCode::ShiftLeft, KeyCode::ShiftRight])
        || keys.any_just_released([KeyCode::ShiftLeft, KeyCode::ShiftRight])
    {
        mem.shift_last = f;
    }
    if keys.any_pressed([KeyCode::AltLeft, KeyCode::AltRight])
        || keys.any_just_pressed([KeyCode::AltLeft, KeyCode::AltRight])
        || keys.any_just_released([KeyCode::AltLeft, KeyCode::AltRight])
    {
        mem.alt_last = f;
    }
}

pub fn ctrl_down(keys: &ButtonInput<KeyCode>, mem: &ChordMemory, frame: u64) -> bool {
    keys.any_pressed([KeyCode::ControlLeft, KeyCode::ControlRight])
        || frame.saturating_sub(mem.ctrl_last) <= CHORD_GRACE_FRAMES
}

/// platform command modifier for shortcuts: super under `cfg(target_os = "macos")`,
/// control elsewhere. drag modifiers keep using [`ctrl_down`] so they stay physical.
#[cfg(target_os = "macos")]
pub fn command_down(keys: &ButtonInput<KeyCode>, mem: &ChordMemory, frame: u64) -> bool {
    keys.any_pressed([KeyCode::SuperLeft, KeyCode::SuperRight])
        || frame.saturating_sub(mem.cmd_last) <= CHORD_GRACE_FRAMES
}

#[cfg(not(target_os = "macos"))]
pub fn command_down(keys: &ButtonInput<KeyCode>, mem: &ChordMemory, frame: u64) -> bool {
    ctrl_down(keys, mem, frame)
}

pub fn shift_down(keys: &ButtonInput<KeyCode>, mem: &ChordMemory, frame: u64) -> bool {
    keys.any_pressed([KeyCode::ShiftLeft, KeyCode::ShiftRight])
        || frame.saturating_sub(mem.shift_last) <= CHORD_GRACE_FRAMES
}

pub fn alt_down(keys: &ButtonInput<KeyCode>, mem: &ChordMemory, frame: u64) -> bool {
    keys.any_pressed([KeyCode::AltLeft, KeyCode::AltRight])
        || frame.saturating_sub(mem.alt_last) <= CHORD_GRACE_FRAMES
}

/// egui `Id` the tab bar publishes its rect under, so canvas input can ignore
/// the pointer while it's over the bar.
pub const TAB_BAR_RECT_ID: &str = "refcanvas_tabs_bar";

/// true while egui is using the pointer (hovering ui, dragging a slider/menu…)
/// or the pointer is over the tab bar.
pub fn egui_pointer_busy(contexts: &mut EguiContexts) -> bool {
    let Ok(ctx) = contexts.ctx_mut() else {
        return false;
    };
    if ctx.egui_wants_pointer_input() {
        return true;
    }
    // the tab bar's padding isn't egui-interactive, so gate on its rect
    let pointer = ctx.input(|i| i.pointer.interact_pos());
    let bar = ctx.data_mut(|d| d.get_persisted::<egui::Rect>(egui::Id::new(TAB_BAR_RECT_ID)));
    matches!((pointer, bar), (Some(p), Some(r)) if r.contains(p))
}

/// true while egui should receive keyboard input (focused field etc.).
pub fn egui_keyboard_busy(contexts: &mut EguiContexts) -> bool {
    contexts
        .ctx_mut()
        .map(|c| c.egui_wants_keyboard_input() || c.text_edit_focused())
        .unwrap_or(false)
}

/// interaction stage of the canvas (input, camera, selection), in order.
#[derive(SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
pub struct CanvasSet;

/// post-interaction stage (culling, lod, gizmos, grid), in order.
#[derive(SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
pub struct CanvasLateSet;

pub struct CanvasPlugin;

impl Plugin for CanvasPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(grid::GridPlugin)
            .init_resource::<camera::View>()
            .init_resource::<selection::Selection>()
            .init_resource::<selection::Interaction>()
            .init_resource::<manipulate::ZCounter>()
            .init_resource::<grid::GridFade>()
            .init_resource::<undo::UndoStack>()
            .init_resource::<camera::ViewMotion>()
            .init_resource::<camera::WheelGesture>()
            .init_resource::<lod::FrameCounter>()
            .init_resource::<lod::GpuTextureState>()
            .init_resource::<lod::TexTuning>()
            .init_resource::<lod::FullLoadWarns>()
            .init_resource::<frames::FrameLabelHits>()
            .init_resource::<frames::FrameRename>()
            .init_resource::<ChordMemory>()
            .init_resource::<touch::Pointer>()
            .init_resource::<touch::TouchState>()
            .init_resource::<touch::Inertia>()
            .init_resource::<lod::TexBudget>()
            .add_systems(Last, lod::park_inactive_meshes)
            .add_systems(
                Startup,
                (
                    camera::setup_camera,
                    gizmos::configure_gizmos,
                    gizmos::spawn_marquee_fill,
                ),
            )
            .add_systems(
                Update,
                (
                    lod::tick_frame,
                    track_chords,
                    touch::touch_input,
                    touch::apply_inertia,
                    camera::track_wheel_gesture,
                    camera::view_input,
                    // before `view_smooth`: focus/arrow hops must set their target
                    // in the same frame, or the idle loop naps before the glide starts.
                    keyboard::shortcuts,
                    camera::view_smooth,
                    camera::track_view_motion,
                    undo::undo_redo,
                    selection::pointer_events,
                    selection::apply_interaction,
                )
                    .chain()
                    .in_set(CanvasSet),
            )
            .add_systems(
                Update,
                (
                    selection::prune_selection,
                    lod::ensure_full_textures,
                    lod::collect_full_textures,
                    lod::bind_pending_full_textures,
                    lod::evict_full_textures,
                    lod::log_vram_stats,
                    lod::release_cpu_image_data,
                    lod::update_texture_samplers,
                    lod::requeue_on_z_change,
                    gizmos::draw_pending_loads,
                    gizmos::update_marquee_fill,
                    gizmos::draw_selection,
                    frames::update_frame_colors,
                    frames::update_frame_labels,
                    frames::draw_frame_borders,
                    grid::update_grid,
                )
                    .chain()
                    .in_set(CanvasLateSet)
                    .after(CanvasSet),
            )
            // cursor shapes run after egui's, so canvas cursors win over the
            // canvas and egui's win over its ui.
            .add_systems(
                PostUpdate,
                cursor::update_cursor.after(bevy_egui::EguiPostUpdateSet::PostProcessOutput),
            );

        // render-world half of the texture-upload bookkeeping (see
        // `lod::GpuTextureState`); no-op without a render app.
        lod::register_gpu_texture_sync(app);
    }
}

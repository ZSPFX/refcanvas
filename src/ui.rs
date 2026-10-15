//! egui overlay: tab bar, context menus, status HUD.

use bevy::prelude::*;
use bevy::time::Real;
use bevy::window::PrimaryWindow;
use bevy_egui::{
    EguiContexts, EguiPrimaryContextPass,
    egui::{self, LayerId, UiBuilder},
};

use crate::canvas::camera::View;
use crate::canvas::frames::{FrameLabelHits, FrameRename};
use crate::canvas::grid::GridFade;
use crate::canvas::hit;
use crate::canvas::manipulate::{self, ZCounter};
use crate::canvas::node::{FrameLabel, NodeKind, NodeSnapshot, Pose};
use crate::canvas::selection::{self, Interaction, NodeQ, Selection};
use crate::canvas::undo::{UndoEntry, UndoStack, snapshot_of};
use crate::clipboard::SystemClipboard;
use crate::project::import::{ImportQueue, enqueue_paths};
use crate::project::tabs::{TabRequest, TabRequests, Tabs};
use crate::theme::Accent;

pub struct UiPlugin;

impl Plugin for UiPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<ContextMenu>().add_systems(
            EguiPrimaryContextPass,
            (
                apply_accent_style,
                context_menu_input,
                tab_bar,
                hud,
                context_menu_ui,
                frame_rename_ui,
            )
                .chain(),
        );
    }
}

/// Our own right-click menu state (canvas interactions are not egui widgets).
#[derive(Resource, Default)]
struct ContextMenu {
    open: bool,
    pos: Vec2,
    node: Option<Entity>,
    world: Vec2,
}

#[derive(Default)]
struct FpsCounter {
    frames: u32,
    elapsed: f32,
    fps: f32,
}

// ---------------------------------------------------------------------------
// Accent
// ---------------------------------------------------------------------------

/// Tint egui's selection/hover visuals with the desktop accent. egui 0.36
/// paints a selected tab / menu entry from `selection.bg_fill` +
/// `selection.stroke.color` (`widget_style::button_style`), and hovers from
/// `widgets.hovered.weak_bg_fill`, those are the fields set here. Applied on
/// change only; the accent itself is polled by [`crate::theme`].
fn apply_accent_style(
    mut contexts: EguiContexts,
    accent: Res<Accent>,
    mut applied: Local<Option<[f32; 3]>>,
) -> Result {
    let rgb = accent.rgb();
    if *applied == Some(rgb) {
        return Ok(());
    }
    let ctx = contexts.ctx_mut()?;
    let tint = |a: u8| {
        egui::Color32::from_rgba_unmultiplied(
            (rgb[0] * 255.0).round() as u8,
            (rgb[1] * 255.0).round() as u8,
            (rgb[2] * 255.0).round() as u8,
            a,
        )
    };
    let on_accent = if accent.is_light() {
        egui::Color32::from_rgb(18, 18, 18)
    } else {
        egui::Color32::WHITE
    };
    // NB: egui 0.36 has no `Context::style_mut`; `all_styles_mut` applies to
    // both the dark and light styles, so the tint sticks whichever is active.
    ctx.all_styles_mut(|style| {
        // Selected tab / selected menu entry.
        style.visuals.selection.bg_fill = tint(255);
        style.visuals.selection.stroke.color = on_accent;
        // Hover / press on other widgets: a subtle tint instead of gray.
        style.visuals.widgets.hovered.weak_bg_fill = tint(64);
        style.visuals.widgets.active.weak_bg_fill = tint(96);
    });
    *applied = Some(rgb);
    Ok(())
}

// ---------------------------------------------------------------------------
// Context menu
// ---------------------------------------------------------------------------

enum MenuAction {
    BringToFront,
    SendToBack,
    ResetRotation,
    FlipH,
    FlipV,
    Reveal,
    Delete,
    ImportDialog,
    PasteFromClipboard,
    SelectAll,
    FrameAll,
    ResetView,
    ToggleGrid,
    Align(AlignKind),
    Distribute(bool),
    ResetCrop,
    ResetSize,
}

#[derive(Clone, Copy)]
enum AlignKind {
    Left,
    CenterH,
    Right,
    Top,
    MiddleV,
    Bottom,
}

#[allow(clippy::too_many_arguments)]
fn context_menu_input(
    window: Single<&Window, With<PrimaryWindow>>,
    mouse: Res<ButtonInput<MouseButton>>,
    keys: Res<ButtonInput<KeyCode>>,
    camera: Single<(&Camera, &GlobalTransform)>,
    mut contexts: EguiContexts,
    mut menu: ResMut<ContextMenu>,
    mut selection: ResMut<Selection>,
    tabs: Res<Tabs>,
    label_hits: Res<crate::canvas::frames::FrameLabelHits>,
    q_own: Query<(Entity, &crate::canvas::node::TabOwner), Without<crate::canvas::node::InactiveTab>>,
    q: NodeQ,
) {
    // Close on Escape or on a click *outside* the menu. (Clicks inside the
    // menu must reach egui, or menu items could never be activated.)
    let pointer_over_egui = crate::canvas::egui_pointer_busy(&mut contexts);
    if keys.just_pressed(KeyCode::Escape)
        || (mouse.just_pressed(MouseButton::Left) && !pointer_over_egui)
    {
        menu.open = false;
    }
    let alt = keys.any_pressed([KeyCode::AltLeft, KeyCode::AltRight]);
    if !mouse.just_pressed(MouseButton::Right)
        || alt
        || crate::canvas::egui_pointer_busy(&mut contexts)
    {
        return;
    }
    let Some(cursor) = window.cursor_position() else {
        return;
    };
    let (camera, cam_tf) = *camera;
    let Ok(world) = camera.viewport_to_world_2d(cam_tf, cursor) else {
        return;
    };
    let keep = selection::owner_filter(tabs.active_id(), &q_own);
    let node = selection::pick_topmost(world, &q, &keep, &label_hits).map(|(e, _)| e);
    if let Some(e) = node {
        if !selection.contains(e) {
            selection.nodes = vec![e];
        }
    } else {
        selection.clear();
    }
    menu.open = true;
    menu.pos = cursor;
    menu.node = node;
    menu.world = world;
}

#[allow(clippy::too_many_arguments)]
fn context_menu_ui(
    mut contexts: EguiContexts,
    mut menu: ResMut<ContextMenu>,
    mut selection: ResMut<Selection>,
    mut zc: ResMut<ZCounter>,
    mut commands: Commands,
    mut q: NodeQ,
    tabs: Res<Tabs>,
    mut view: ResMut<View>,
    window: Single<&Window, With<PrimaryWindow>>,
    mut grid_fade: ResMut<GridFade>,
    mut system_clipboard: ResMut<SystemClipboard>,
    mut import_queue: ResMut<ImportQueue>,
    mut interaction: ResMut<Interaction>,
    mut undo: ResMut<UndoStack>,
    q_own: Query<(Entity, &crate::canvas::node::TabOwner), Without<crate::canvas::node::InactiveTab>>,
) -> Result {
    if !menu.open {
        return Ok(());
    }
    let ctx = contexts.ctx_mut()?;
    let mut action: Option<MenuAction> = None;
    let pos = menu.pos;
    let has_node = menu.node.is_some();

    egui::Area::new(egui::Id::new("refcanvas_ctx"))
        .fixed_pos(egui::pos2(pos.x, pos.y))
        .order(egui::Order::Foreground)
        .show(ctx, |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                ui.set_min_width(190.0);
                if has_node {
                    if ui.button("Bring to Front").clicked() {
                        action = Some(MenuAction::BringToFront);
                    }
                    if ui.button("Send to Back").clicked() {
                        action = Some(MenuAction::SendToBack);
                    }
                    if ui.button("Reset Rotation").clicked() {
                        action = Some(MenuAction::ResetRotation);
                    }
                    if ui.button("Reset Crop").clicked() {
                        action = Some(MenuAction::ResetCrop);
                    }
                    if ui.button("Reset Size").clicked() {
                        action = Some(MenuAction::ResetSize);
                    }
                    ui.horizontal(|ui| {
                        if ui.button("Flip H").clicked() {
                            action = Some(MenuAction::FlipH);
                        }
                        if ui.button("Flip V").clicked() {
                            action = Some(MenuAction::FlipV);
                        }
                    });
                    if selection.nodes.len() >= 2 {
                        ui.separator();
                        ui.horizontal(|ui| {
                            if ui.button("Left").clicked() {
                                action = Some(MenuAction::Align(AlignKind::Left));
                            }
                            if ui.button("Center").clicked() {
                                action = Some(MenuAction::Align(AlignKind::CenterH));
                            }
                            if ui.button("Right").clicked() {
                                action = Some(MenuAction::Align(AlignKind::Right));
                            }
                        });
                        ui.horizontal(|ui| {
                            if ui.button("Top").clicked() {
                                action = Some(MenuAction::Align(AlignKind::Top));
                            }
                            if ui.button("Middle").clicked() {
                                action = Some(MenuAction::Align(AlignKind::MiddleV));
                            }
                            if ui.button("Bottom").clicked() {
                                action = Some(MenuAction::Align(AlignKind::Bottom));
                            }
                        });
                        if selection.nodes.len() >= 3 {
                            ui.horizontal(|ui| {
                                if ui.button("Distribute H").clicked() {
                                    action = Some(MenuAction::Distribute(true));
                                }
                                if ui.button("Distribute V").clicked() {
                                    action = Some(MenuAction::Distribute(false));
                                }
                            });
                        }
                    }
                    ui.separator();
                    if ui.button("Show in File Manager").clicked() {
                        action = Some(MenuAction::Reveal);
                    }
                    ui.separator();
                    if ui.button("Delete").clicked() {
                        action = Some(MenuAction::Delete);
                    }
                } else {
                    if ui.button("Import images…").clicked() {
                        action = Some(MenuAction::ImportDialog);
                    }
                    if ui.button("Paste from clipboard").clicked() {
                        action = Some(MenuAction::PasteFromClipboard);
                    }
                    ui.separator();
                    if ui.button("Select All").clicked() {
                        action = Some(MenuAction::SelectAll);
                    }
                    if ui.button("Frame All").clicked() {
                        action = Some(MenuAction::FrameAll);
                    }
                    if ui.button("Reset Zoom (100%)").clicked() {
                        action = Some(MenuAction::ResetView);
                    }
                    if ui.button("Toggle Grid").clicked() {
                        action = Some(MenuAction::ToggleGrid);
                    }
                }
            });
        });

    if let Some(action) = action {
        menu.open = false;
        match action {
            MenuAction::BringToFront => {
                let before = capture_snapshots(&q, &selection);
                for e in selection.nodes.clone() {
                    if let Ok((_, mut tf, _, _)) = q.get_mut(e) {
                        tf.translation.z = zc.next();
                    }
                }
                commit_changed(&mut undo, &q, before);
            }
            MenuAction::SendToBack => {
                let before = capture_snapshots(&q, &selection);
                let min_z = q
                    .iter()
                    .map(|(_, tf, _, _)| tf.translation.z)
                    .fold(f32::INFINITY, f32::min);
                let mut z = min_z - 1.0;
                for e in selection.nodes.clone() {
                    if let Ok((_, mut tf, _, _)) = q.get_mut(e) {
                        tf.translation.z = z;
                        z -= 1.0;
                    }
                }
                commit_changed(&mut undo, &q, before);
            }
            MenuAction::ResetRotation => {
                let before = capture_snapshots(&q, &selection);
                for e in selection.nodes.clone() {
                    if let Ok((_, mut tf, _, mut node)) = q.get_mut(e) {
                        node.rotation = 0.0;
                        tf.rotation = Quat::IDENTITY;
                    }
                }
                commit_changed(&mut undo, &q, before);
            }
            MenuAction::FlipH => {
                let before = capture_snapshots(&q, &selection);
                for e in selection.nodes.clone() {
                    if let Ok((_, _, mut sprite, _)) = q.get_mut(e) {
                        sprite.flip_x = !sprite.flip_x;
                    }
                }
                commit_changed(&mut undo, &q, before);
            }
            MenuAction::FlipV => {
                let before = capture_snapshots(&q, &selection);
                for e in selection.nodes.clone() {
                    if let Ok((_, _, mut sprite, _)) = q.get_mut(e) {
                        sprite.flip_y = !sprite.flip_y;
                    }
                }
                commit_changed(&mut undo, &q, before);
            }
            MenuAction::Reveal => {
                if let Some(target) = menu.node {
                    if let Ok((_, _, _, node)) = q.get(target) {
                        if let Some(img) = node.image() {
                            reveal_in_file_manager(&img.data.abs_path);
                        }
                    }
                }
            }
            MenuAction::Delete => {
                let tab_id = tabs.active_id().unwrap_or(0);
                manipulate::delete_selection_undoable(
                    &mut selection,
                    &mut undo,
                    &q,
                    tab_id,
                    &mut commands,
                );
                *interaction = Interaction::Idle;
            }
            MenuAction::ImportDialog => {
                let picked = rfd::FileDialog::new()
                    .set_title("Import reference images")
                    .add_filter(
                        "Images",
                        &[
                            "png", "jpg", "jpeg", "webp", "bmp", "gif", "tga", "tif", "tiff",
                        ],
                    )
                    .pick_files();
                if let Some(paths) = picked {
                    enqueue_paths(&mut import_queue, view.center.as_vec2(), paths);
                }
            }
            MenuAction::PasteFromClipboard => {
                crate::clipboard::request_paste(&mut system_clipboard, menu.world);
            }
            MenuAction::SelectAll => {
                let keep = selection::owner_filter(tabs.active_id(), &q_own);
                manipulate::select_all(&q, keep, &mut selection);
            }
            MenuAction::FrameAll => {
                let keep = selection::owner_filter(tabs.active_id(), &q_own);
                let poses: Vec<_> = q
                    .iter()
                    .filter(|(e, ..)| keep(*e))
                    .map(|(_, tf, _, node)| manipulate::current_pose(tf, node))
                    .collect();
                if !poses.is_empty() {
                    manipulate::focus_poses(&poses, window.size(), &mut view);
                }
            }
            MenuAction::ResetView => {
                let center = view.center;
                view.fly_to(center, 0.0);
            }
            MenuAction::ToggleGrid => {
                grid_fade.0 = if grid_fade.0 > 0.5 { 0.0 } else { 1.0 };
            }
            MenuAction::ResetCrop => {
                let before = capture_snapshots(&q, &selection);
                for e in selection.nodes.clone() {
                    if let Ok((_, mut tf, mut sprite, mut node)) = q.get_mut(e) {
                        let size = if let Some(img) = node.image_mut() {
                            img.data.crop =
                                crate::canvas::node::ImageData::full_crop(img.data.natural);
                            img.data.display_size()
                        } else {
                            node.size
                        };
                        let pose = Pose {
                            size,
                            ..manipulate::current_pose(&tf, &node)
                        };
                        manipulate::set_pose(&mut tf, Some(&mut sprite), &mut node, &pose);
                    }
                }
                commit_changed(&mut undo, &q, before);
            }
            MenuAction::ResetSize => {
                let before = capture_snapshots(&q, &selection);
                for e in selection.nodes.clone() {
                    if let Ok((_, mut tf, mut sprite, mut node)) = q.get_mut(e) {
                        let size = if let Some(img) = node.image_mut() {
                            img.data.px_scale =
                                crate::canvas::node::ImageData::default_px_scale(img.data.natural);
                            img.data.display_size()
                        } else {
                            node.size
                        };
                        let pose = Pose {
                            size,
                            ..manipulate::current_pose(&tf, &node)
                        };
                        manipulate::set_pose(&mut tf, Some(&mut sprite), &mut node, &pose);
                    }
                }
                commit_changed(&mut undo, &q, before);
            }
            MenuAction::Align(kind) => {
                align_selection(kind, &mut q, &selection, &mut undo);
            }
            MenuAction::Distribute(horizontal) => {
                distribute_selection(horizontal, &mut q, &selection, &mut undo);
            }
        }
    }
    Ok(())
}

fn align_selection(kind: AlignKind, q: &mut NodeQ, selection: &Selection, undo: &mut UndoStack) {
    let before = capture_snapshots(q, selection);
    let mut items: Vec<(Entity, Pose)> = selection
        .nodes
        .iter()
        .filter_map(|e| {
            q.get(*e)
                .ok()
                .map(|(_, tf, _, n)| (*e, manipulate::current_pose(tf, n)))
        })
        .collect();
    let Some(union) = manipulate::union_aabb(&items.iter().map(|(_, p)| *p).collect::<Vec<_>>())
    else {
        return;
    };
    for (e, pose) in items.drain(..) {
        let aabb = crate::canvas::hit::aabb_of(&pose);
        let delta = match kind {
            AlignKind::Left => Vec2::new(union.min.x - aabb.min.x, 0.0),
            AlignKind::CenterH => Vec2::new(union.center().x - aabb.center().x, 0.0),
            AlignKind::Right => Vec2::new(union.max.x - aabb.max.x, 0.0),
            AlignKind::Top => Vec2::new(0.0, union.max.y - aabb.max.y),
            AlignKind::MiddleV => Vec2::new(0.0, union.center().y - aabb.center().y),
            AlignKind::Bottom => Vec2::new(0.0, union.min.y - aabb.min.y),
        };
        if let Ok((_, mut tf, mut sprite, mut node)) = q.get_mut(e) {
            let new_pose = Pose {
                center: pose.center + delta,
                ..pose
            };
            manipulate::set_pose(&mut tf, Some(&mut sprite), &mut node, &new_pose);
        }
    }
    commit_changed(undo, q, before);
}

fn distribute_selection(
    horizontal: bool,
    q: &mut NodeQ,
    selection: &Selection,
    undo: &mut UndoStack,
) {
    let before = capture_snapshots(q, selection);
    let mut items: Vec<(Entity, Pose)> = selection
        .nodes
        .iter()
        .filter_map(|e| {
            q.get(*e)
                .ok()
                .map(|(_, tf, _, n)| (*e, manipulate::current_pose(tf, n)))
        })
        .collect();
    if items.len() < 3 {
        return;
    }
    let poses: Vec<Pose> = items.iter().map(|(_, p)| *p).collect();
    let Some(union) = manipulate::union_aabb(&poses) else {
        return;
    };
    let key = |p: &Pose| if horizontal { p.center.x } else { p.center.y };
    items.sort_by(|a, b| key(&a.1).total_cmp(&key(&b.1)));

    let size_of = |p: &Pose| {
        let r = crate::canvas::hit::aabb_of(p);
        if horizontal { r.size().x } else { r.size().y }
    };
    let total: f32 = items.iter().map(|(_, p)| size_of(p)).sum();
    let span = if horizontal {
        union.size().x
    } else {
        union.size().y
    };
    let gap = ((span - total) / (items.len() - 1) as f32).max(0.0);

    let mut cursor = if horizontal { union.min.x } else { union.min.y };
    for (e, pose) in items.drain(..) {
        let s = size_of(&pose);
        let target = cursor + s * 0.5;
        cursor += s + gap;
        let delta = if horizontal {
            Vec2::new(target - pose.center.x, 0.0)
        } else {
            Vec2::new(0.0, target - pose.center.y)
        };
        if let Ok((_, mut tf, mut sprite, mut node)) = q.get_mut(e) {
            let new_pose = Pose {
                center: pose.center + delta,
                ..pose
            };
            manipulate::set_pose(&mut tf, Some(&mut sprite), &mut node, &new_pose);
        }
    }
    commit_changed(undo, q, before);
}

fn capture_snapshots(q: &NodeQ, selection: &Selection) -> Vec<(Entity, NodeSnapshot)> {
    selection
        .nodes
        .iter()
        .filter_map(|e| {
            q.get(*e)
                .ok()
                .map(|(_, tf, sp, n)| (*e, snapshot_of(tf, n, sp)))
        })
        .collect()
}

fn commit_changed(undo: &mut UndoStack, q: &NodeQ, before: Vec<(Entity, NodeSnapshot)>) {
    let mut items = Vec::new();
    for (e, b) in before {
        if let Ok((_, tf, sp, n)) = q.get(e) {
            let a = snapshot_of(tf, n, sp);
            if a != b {
                items.push((e, b, a));
            }
        }
    }
    if !items.is_empty() {
        undo.push(UndoEntry::Changed { items });
    }
}

/// Open a folder in the system file manager.
fn open_folder(dir: &std::path::Path) {
    #[cfg(target_os = "windows")]
    let _ = std::process::Command::new("explorer").arg(dir).spawn();
    #[cfg(not(target_os = "windows"))]
    let _ = std::process::Command::new("xdg-open").arg(dir).spawn();
}

/// Reveal a file in the file manager, *selecting* it. On Linux this goes
/// through the freedesktop `org.freedesktop.FileManager1` D-Bus interface
/// (Nautilus, Dolphin, Nemo, Caja, Thunar, …); if no file manager answers,
/// fall back to opening the parent folder without a selection.
fn reveal_in_file_manager(path: &std::path::Path) {
    #[cfg(target_os = "windows")]
    reveal_windows(path);

    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open")
        .arg("-R")
        .arg(path)
        .spawn();

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let selected = std::process::Command::new("dbus-send")
            .args([
                "--session",
                // Wait for the reply: a missing file manager must be an error
                // (→ fallback), not a fire-and-forget success.
                "--print-reply",
                "--dest=org.freedesktop.FileManager1",
                "/org/freedesktop/FileManager1",
                "org.freedesktop.FileManager1.ShowItems",
            ])
            .arg(format!("array:string:{}", file_uri(path)))
            .arg("string:") // startup id
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !selected {
            open_folder(path.parent().unwrap_or(path));
        }
    }
}

/// Windows: `explorer /select,` is unreliable, the already-running shell
/// often drops the argument and opens Documents instead, so call the shell
/// API itself (the same one the `showfile` crate wraps, via the `windows-sys`
/// copy already in the dependency tree). Runs on its own COM-initialized
/// thread: the main thread's apartment belongs to winit/wgpu.
#[cfg(target_os = "windows")]
fn reveal_windows(path: &std::path::Path) {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::System::Com::{
        COINIT_APARTMENTTHREADED, CoInitializeEx, CoTaskMemFree, CoUninitialize,
    };
    use windows_sys::Win32::UI::Shell::{SHOpenFolderAndSelectItems, SHParseDisplayName};

    // The shell protocol wants native separators (a session carried over
    // from Linux can still hold `/` in the root).
    let path: std::path::PathBuf = path.components().collect();
    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    wide.push(0);

    std::thread::spawn(move || unsafe {
        let hr = CoInitializeEx(std::ptr::null(), COINIT_APARTMENTTHREADED as u32);
        let mut pidl = std::ptr::null_mut();
        // With `cidl == 0` and a null `apidl`, the parsed PIDL doubles as the
        // item to select, the documented "show one file" call. (Passing the
        // file's PIDL as the folder one is the standard shortcut.)
        if SHParseDisplayName(
            wide.as_ptr(),
            std::ptr::null_mut(),
            &mut pidl,
            0,
            std::ptr::null_mut(),
        ) >= 0
            && !pidl.is_null()
        {
            SHOpenFolderAndSelectItems(pidl, 0, std::ptr::null(), 0);
            CoTaskMemFree(pidl as *const _);
        } else {
            warn!("could not resolve path for Explorer: {}", path.display());
        }
        if hr >= 0 {
            CoUninitialize();
        }
    });
}

/// `file://` URI for a path, percent-encoding everything outside the
/// unreserved set (spaces, `#`, non-ASCII, …), `ShowItems` wants real URIs.
#[cfg(all(unix, not(target_os = "macos")))]
fn file_uri(path: &std::path::Path) -> String {
    use std::os::unix::ffi::OsStrExt;

    let mut uri = String::from("file://");
    for &b in path.as_os_str().as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~' | b'/') {
            uri.push(b as char);
        } else {
            uri.push_str(&format!("%{b:02X}"));
        }
    }
    uri
}

// ---------------------------------------------------------------------------
// Frame rename
// ---------------------------------------------------------------------------

/// Text field opened by double-clicking a frame's label (see [`FrameRename`]):
/// Enter or clicking away commits, Escape cancels. Positioned over the label
/// every frame, so it follows pans/zooms.
fn frame_rename_ui(
    mut contexts: EguiContexts,
    mut rename: ResMut<FrameRename>,
    camera: Single<(&Camera, &GlobalTransform)>,
    view: Res<View>,
    label_hits: Res<FrameLabelHits>,
    tabs: Res<Tabs>,
    q_own: Query<(Entity, &crate::canvas::node::TabOwner), Without<crate::canvas::node::InactiveTab>>,
    mut q: NodeQ,
    mut labels: Query<(&FrameLabel, &mut Text2d)>,
) -> Result {
    let Some(frame) = rename.frame else {
        return Ok(());
    };
    let ctx = contexts.ctx_mut()?;

    // The frame can disappear mid-edit (delete, tab close, undo).
    let Ok((_, tf, _, mut node)) = q.get_mut(frame) else {
        rename.clear();
        return Ok(());
    };
    // Switching tabs abandons the edit (the frame is hidden there anyway).
    let frame_tab = q_own.get(frame).ok().map(|(_, owner)| owner.0);
    if frame_tab != tabs.active_id() {
        rename.clear();
        return Ok(());
    }

    let wpp = (view.scale() as f32).max(1e-4);
    // Anchor at the label's top-left; if the label is hidden (frame too small
    // on screen), fall back to the frame's own top-left.
    let (anchor, width) = match label_hits.get(frame) {
        Some(hit_pose) => (
            hit::corner_world(hit_pose, 0),
            (hit_pose.size.x / wpp).max(110.0),
        ),
        None => {
            let pose = manipulate::current_pose(&tf, &node);
            (
                hit::corner_world(&pose, 0),
                (node.size.x / wpp).clamp(80.0, 240.0),
            )
        }
    };
    let (camera, cam_tf) = *camera;
    // Off-screen label: keep the editor alive, just don't draw it.
    let Ok(pos) = camera.world_to_viewport(cam_tf, anchor.extend(0.0)) else {
        return Ok(());
    };

    let mut commit = false;
    let mut cancel = false;
    egui::Area::new(egui::Id::new("refcanvas_frame_rename"))
        .fixed_pos(egui::pos2(pos.x, pos.y))
        .order(egui::Order::Foreground)
        .show(ctx, |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                let resp = ui.add(
                    egui::TextEdit::singleline(&mut rename.text)
                        .desired_width(width)
                        .hint_text("Frame name"),
                );
                if rename.focus_me {
                    resp.request_focus();
                    rename.focus_me = false;
                }
                if resp.has_focus() {
                    rename.had_focus = true;
                }
                // Escape cancels (reverts); Enter or clicking away commits.
                if rename.had_focus {
                    if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                        cancel = true;
                    } else if resp.lost_focus() {
                        commit = true;
                    }
                }
            });
        });

    if cancel {
        rename.clear();
    } else if commit {
        if let NodeKind::Frame(frame_node) = &mut node.kind {
            let name = rename.text.trim();
            if !name.is_empty() {
                frame_node.label = name.to_string();
            }
            let label = frame_node.label.clone();
            for (target, mut text) in &mut labels {
                if target.0 == frame {
                    text.0 = label.clone();
                }
            }
        }
        rename.clear();
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tab bar
// ---------------------------------------------------------------------------

fn tab_bar(
    mut contexts: EguiContexts,
    tabs: Res<Tabs>,
    mut requests: ResMut<TabRequests>,
) -> Result {
    let ctx = contexts.ctx_mut()?;
    let mut pending: Vec<TabRequest> = Vec::new();
    let mut open_new = false;
    let mut open_existing = false;

    let mut root = egui::Ui::new(
        ctx.clone(),
        "refcanvas_root".into(),
        UiBuilder::new()
            .layer_id(LayerId::background())
            .max_rect(ctx.viewport_rect()),
    );

    // Symmetric, explicit vertical padding so the row sits dead-centre in the
    // bar instead of relying on the frame margin plus manual spacing.
    let tabs_frame =
        egui::Frame::side_top_panel(root.style()).inner_margin(egui::Margin::symmetric(8, 4));
    let tabs_panel = egui::Panel::top("refcanvas_tabs").frame(tabs_frame).show(&mut root, |ui| {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            // Track the tallest tab so the "+" button can match it exactly.
            let mut row_h = ui.spacing().interact_size.y;
            let active = tabs.active_id();
            for (id, name) in tabs.summaries() {
                let selected = Some(id) == active;

                // Hover/press feedback is the accent tint only: no outline
                // stroke and no expansion around the tab.
                let resp = ui
                    .scope(|ui| {
                        let v = ui.visuals_mut();
                        v.widgets.hovered.bg_stroke = egui::Stroke::NONE;
                        v.widgets.hovered.expansion = 0.0;
                        v.widgets.active.bg_stroke = egui::Stroke::NONE;
                        v.widgets.active.expansion = 0.0;
                        ui.selectable_label(selected, truncate(&name, 24))
                    })
                    .inner; // .inner = the label's Response, so resp.clicked() still works
                row_h = row_h.max(resp.rect.height());

                if resp.clicked() {
                    pending.push(TabRequest::Switch(id));
                }
                if resp.middle_clicked() {
                    pending.push(TabRequest::Close(id));
                }
                resp.context_menu(|ui| {
                    if ui
                        .add_enabled(tabs.len() > 1, egui::Button::new("Close tab"))
                        .clicked()
                    {
                        pending.push(TabRequest::Close(id));
                        ui.close();
                    }
                    if let Some(root) = tabs.root_of(id) {
                        if ui
                            .button("Open project folder")
                            .on_hover_text(root.display().to_string())
                            .clicked()
                        {
                            open_folder(&root);
                            ui.close();
                        }
                    }
                });
                ui.add_space(3.0);
            }
            // "+" is a square button with a hand-painted plus: the stock
            // `menu_button` glyph is neither 1:1 nor optically centered.
            let side = row_h;
            let (resp, _) = egui::containers::menu::MenuButton::from_button(
                egui::Button::new("").min_size(egui::vec2(side, side)),
            )
            .ui(ui, |ui| {
                if ui.button("New project…").clicked() {
                    open_new = true;
                    ui.close();
                }
                if ui.button("Open project…").clicked() {
                    open_existing = true;
                    ui.close();
                }
            });
            {
                let c = resp.rect.center();
                let arm = side * 0.26;
                let width = (side * 0.10).clamp(1.0, 2.2);
                let stroke = egui::Stroke::new(width, ui.style().interact(&resp).fg_stroke.color);
                let p = ui.painter();
                p.line_segment([c - egui::vec2(arm, 0.0), c + egui::vec2(arm, 0.0)], stroke);
                p.line_segment([c - egui::vec2(0.0, arm), c + egui::vec2(0.0, arm)], stroke);
            }
        });
    });
    // Publish the bar's rect so the canvas ignores pointer input over it.
    ctx.data_mut(|d| {
        d.insert_persisted(
            egui::Id::new(crate::canvas::TAB_BAR_RECT_ID),
            tabs_panel.response.rect,
        )
    });

    if open_new {
        if let Some(dir) = rfd::FileDialog::new()
            .set_title("New project: choose a folder")
            .pick_folder()
        {
            pending.push(TabRequest::Open(dir));
        }
    }
    if open_existing {
        if let Some(file) = rfd::FileDialog::new()
            .set_title("Open project")
            .add_filter("refcanvas project", &["db"])
            .pick_file()
        {
            pending.push(TabRequest::Open(file));
        }
    }

    requests.queue.extend(pending);
    Ok(())
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

// ---------------------------------------------------------------------------
// Status HUD
// ---------------------------------------------------------------------------

fn hud(
    mut contexts: EguiContexts,
    view: Res<View>,
    selection: Res<Selection>,
    queue: Res<ImportQueue>,
    real: Res<Time<Real>>,
    mut pacer: ResMut<crate::FramePacer>,
    mut fps: Local<FpsCounter>,
) -> Result {
    // Real (unclamped) time so the low-power nap is recognizable: a wake-up
    // frame arrives with the whole sleep as its delta. Such a gap means the
    // loop is sleeping at its heartbeat, so put the heartbeat's target rate
    // on the readout directly, no averaging, no easing down to it, and
    // start the next window fresh so the wake-up frames count cleanly.
    // Anything shorter measures normally (short window + light smoothing:
    // ~0.15 s to settle on real changes).
    let dt = real.delta_secs();
    if dt >= crate::IDLE_HEARTBEAT.as_secs_f32() * 0.9 {
        fps.fps = 1.0 / crate::IDLE_HEARTBEAT.as_secs_f32();
        fps.frames = 0;
        fps.elapsed = 0.0;
    } else {
        fps.frames += 1;
        fps.elapsed += dt;
        if fps.elapsed >= 0.1 {
            let instant = fps.frames as f32 / fps.elapsed;
            fps.fps = if fps.fps > 0.0 {
                fps.fps * 0.25 + instant * 0.75
            } else {
                instant
            };
            fps.frames = 0;
            fps.elapsed = 0.0;
        }
    }

    let ctx = contexts.ctx_mut()?;
    let text = |s: String| egui::RichText::new(s).monospace().size(12.0);

    egui::Area::new(egui::Id::new("refcanvas_hud"))
        .anchor(egui::Align2::RIGHT_BOTTOM, egui::vec2(-10.0, -8.0))
        // The area background is click-through (won't steal canvas drags),
        // but the area itself stays hit-testable so child widgets, like the
        // clickable fps readout, can receive input. (`interactable(false)`
        // would make egui skip this entire layer in its hit-test.)
        .sense(egui::Sense::hover())
        .show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 12.0;
                // Fixed-width numbers: the area is right-anchored, and egui
                // re-anchors an area a frame after its size changes, a width
                // change would show as a visible one-frame wobble. Padded to
                // the widest value each label can reach (zoom: 100·e⁷ ≈
                // 109647%, fps: 4 digits).
                ui.label(text(format!("{:>6.0}%", view.zoom_percent())));
                // Click the fps readout to toggle benchmark mode:
                // black chip + bright white text + the 120 fps cap lifted.
                let bench = !pacer.cap_enabled;
                let fps_rt = if bench {
                    text(format!("{:>4.0} fps", fps.fps)).color(egui::Color32::WHITE)
                } else {
                    text(format!("{:>4.0} fps", fps.fps)).weak()
                };
                let chip = if bench {
                    egui::Color32::BLACK
                } else {
                    egui::Color32::TRANSPARENT
                };
                let resp = egui::Frame::NONE
                    .fill(chip)
                    .inner_margin(egui::Margin::symmetric(4, 2))
                    .show(ui, |ui| {
                        ui.add(
                            egui::Label::new(fps_rt)
                                // Selection would give an I-beam cursor and allow
                                // dragging the text; we only want clicks.
                                .selectable(false)
                                .sense(egui::Sense::click()),
                        )
                    })
                    .inner;
                if resp.clicked() {
                    pacer.cap_enabled = !pacer.cap_enabled;
                }
                let _ = resp
                    .on_hover_cursor(egui::CursorIcon::PointingHand)
                    .on_hover_text(if bench {
                        "benchmark mode, click to re-cap to 120 fps"
                    } else {
                        "click: uncap FPS (benchmark)"
                    });
            });
        });

    egui::Area::new(egui::Id::new("refcanvas_status"))
        .anchor(egui::Align2::LEFT_BOTTOM, egui::vec2(10.0, -8.0))
        .interactable(false)
        .show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 12.0;
                ui.label(text(format!("v{}", env!("CARGO_PKG_VERSION"))).weak());
                if !selection.nodes.is_empty() && selection.nodes.len() > 1 {
                    ui.label(text(format!("{} selected", selection.nodes.len())).weak());
                }
                if let Some((done, total)) = queue.progress() {
                    ui.label(text(format!("importing {done}/{total}")));
                }
            });
        });

    Ok(())
}

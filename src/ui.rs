//! egui overlay: tab bar, context menus, status HUD.

use bevy::ecs::system::NonSendMarker;
use bevy::math::DVec2;
use bevy::prelude::*;
use bevy::time::Real;
use bevy::window::PrimaryWindow;
use bevy::winit::WINIT_WINDOWS;
use bevy_egui::{
    EguiContexts, EguiPrimaryContextPass,
    egui::{self, LayerId, UiBuilder},
};
use std::path::PathBuf;
use winit::window::WindowLevel;

use crate::canvas::camera::View;
use crate::canvas::frames::{self, FrameLabelHits, FrameRename};
use crate::canvas::hit;
use crate::canvas::manipulate::{self, ZCounter};
use crate::canvas::node::{FrameLabel, NodeKind, NodeSnapshot, Pose};
use crate::canvas::selection::{self, Interaction, NodeQ, Selection};
use crate::canvas::undo::{UndoEntry, UndoStack, snapshot_of};
use crate::clipboard::SystemClipboard;
use crate::project::import::ImportQueue;
use crate::project::tabs::{TabRequest, TabRequests, Tabs};
use crate::theme::Accent;

pub struct UiPlugin;

impl Plugin for UiPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<ContextMenu>()
            .init_resource::<TabMenu>()
            .init_resource::<AlwaysOnTop>()
            .add_systems(Update, apply_always_on_top)
            .add_systems(
                EguiPrimaryContextPass,
                (
                    apply_accent_style,
                    close_on_navigate,
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

/// window stays above others.
#[derive(Resource)]
pub struct AlwaysOnTop(pub bool);

impl Default for AlwaysOnTop {
    fn default() -> Self {
        let supported = !crate::on_wayland();
        Self(supported && crate::project::local::load_always_on_top())
    }
}

/// set the level on the main thread; native wayland has no request, so the
/// saved preference only applies after an x11/xwayland launch.
fn apply_always_on_top(
    _: NonSendMarker,
    state: Res<AlwaysOnTop>,
    mut applied: Local<Option<bool>>,
) {
    if *applied == Some(state.0) {
        return;
    }
    WINIT_WINDOWS.with_borrow(|windows| {
        if windows.windows.is_empty() {
            return;
        }
        let level = if state.0 {
            WindowLevel::AlwaysOnTop
        } else {
            WindowLevel::Normal
        };
        for window in windows.windows.values() {
            window.set_window_level(level);
        }
        *applied = Some(state.0);
    });
}

/// right-click menu state; canvas isn't made of egui widgets.
#[derive(Resource, Default)]
struct ContextMenu {
    open: bool,
    pos: Vec2,
    node: Option<Entity>,
    world: Vec2,
    reset_open: bool,
    reset_rect: Option<egui::Rect>,
    reset_menu_rect: Option<egui::Rect>,
}

#[derive(Resource, Default)]
struct TabMenu {
    open: bool,
    pos: egui::Pos2,
    target: Option<TabMenuTarget>,
}

#[derive(Clone, Copy)]
enum TabMenuTarget {
    Tab(u64),
    Project,
}

#[derive(Clone)]
enum TabMenuAction {
    Close(u64),
    OpenFolder(PathBuf),
    NewProject,
    OpenProject,
}

#[derive(Default)]
struct TabDrag {
    id: Option<u64>,
    grab: f32,
}

#[derive(Default)]
struct FpsCounter {
    frames: u32,
    elapsed: f32,
    fps: f32,
}

fn readable_text(bg: egui::Color32) -> egui::Color32 {
    let lin = |c: u8| {
        let c = c as f32 / 255.0;
        if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    let l = 0.2126 * lin(bg.r()) + 0.7152 * lin(bg.g()) + 0.0722 * lin(bg.b());
    let black = (l + 0.05) / 0.05;
    let white = 1.05 / (l + 0.05);
    if black >= white {
        egui::Color32::from_rgb(18, 18, 18)
    } else {
        egui::Color32::WHITE
    }
}

/// close overlays as soon as the camera starts moving.
fn close_on_navigate(
    view: Res<View>,
    mut last: Local<Option<(DVec2, f64)>>,
    mut menu: ResMut<ContextMenu>,
    mut tab_menu: ResMut<TabMenu>,
    mut rename: ResMut<FrameRename>,
) {
    let current = (view.target_center, view.target_log_scale);
    if let Some(prev) = *last
        && prev != current
    {
        menu.open = false;
        menu.reset_open = false;
        menu.reset_rect = None;
        menu.reset_menu_rect = None;
        tab_menu.open = false;
        if rename.frame.is_some() {
            rename.clear();
        }
    }
    *last = Some(current);
}

/// tint egui selection/hover visuals with the accent; applied on change only.
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
    let on_accent = readable_text(tint(255));
    // no style_mut here; all_styles_mut covers whichever theme is active.
    ctx.all_styles_mut(|style| {
        style.visuals.selection.bg_fill = tint(255);
        style.visuals.selection.stroke.color = on_accent;
        style.visuals.widgets.hovered.weak_bg_fill = tint(64);
        style.visuals.widgets.active.weak_bg_fill = tint(96);
    });
    *applied = Some(rgb);
    Ok(())
}

#[derive(Clone)]
enum MenuAction {
    ResetRotation,
    FlipH,
    FlipV,
    Group,
    Reveal,
    Delete,
    ImportDialog,
    PasteFromClipboard,
    ResetCrop,
    ResetSize,
}

#[allow(clippy::too_many_arguments)]
fn context_menu_input(
    window: Single<&Window, With<PrimaryWindow>>,
    mouse: Res<ButtonInput<MouseButton>>,
    pointer: Res<crate::canvas::touch::Pointer>,
    keys: Res<ButtonInput<KeyCode>>,
    camera: Single<(&Camera, &GlobalTransform)>,
    mut contexts: EguiContexts,
    mut menu: ResMut<ContextMenu>,
    mut selection: ResMut<Selection>,
    tabs: Res<Tabs>,
    label_hits: Res<crate::canvas::frames::FrameLabelHits>,
    q_own: Query<
        (Entity, &crate::canvas::node::TabOwner),
        Without<crate::canvas::node::InactiveTab>,
    >,
    q: NodeQ,
) {
    // clicks inside the menu belong to egui.
    let pointer_over_egui = crate::canvas::egui_pointer_busy(&mut contexts);
    if keys.just_pressed(KeyCode::Escape) || (pointer.just_pressed && !pointer_over_egui) {
        menu.open = false;
        menu.reset_open = false;
        menu.reset_rect = None;
        menu.reset_menu_rect = None;
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
    let node = selection::pick_topmost(world, &q, &keep, &label_hits, 0.0).map(|(e, _)| e);
    if let Some(e) = node {
        if !selection.contains(e) {
            selection.nodes = vec![e];
        }
    } else {
        selection.clear();
    }
    if tabs.active_read_only()
        && !node.is_some_and(|e| {
            q.get(e)
                .ok()
                .is_some_and(|(_, _, _, node)| node.image().is_some())
        })
    {
        menu.open = false;
        return;
    }
    menu.open = true;
    menu.pos = cursor;
    menu.node = node;
    menu.world = world;
    menu.reset_open = false;
    menu.reset_rect = None;
    menu.reset_menu_rect = None;
}

fn menu_row_h(ui: &egui::Ui) -> f32 {
    (ui.text_style_height(&egui::TextStyle::Button) + 10.0).max(ui.spacing().interact_size.y)
}

fn menu_row<A: Clone>(
    ui: &mut egui::Ui,
    hits: &mut Vec<(A, egui::Rect)>,
    label: &str,
    action: Option<A>,
) -> bool {
    menu_cell(
        ui,
        hits,
        label,
        action,
        ui.available_width(),
        menu_row_h(ui),
    )
}

fn menu_cell<A: Clone>(
    ui: &mut egui::Ui,
    hits: &mut Vec<(A, egui::Rect)>,
    label: &str,
    action: Option<A>,
    width: f32,
    inset: f32,
) -> bool {
    let (rect, resp) =
        ui.allocate_exact_size(egui::vec2(width, menu_row_h(ui)), egui::Sense::click());
    paint_menu_row(ui, &resp, label, None, inset);
    let clicked = resp.clicked();
    if let Some(action) = action {
        hits.push((action, rect));
    }
    clicked
}

fn menu_pair<A: Clone>(
    ui: &mut egui::Ui,
    hits: &mut Vec<(A, egui::Rect)>,
    action: &mut Option<A>,
    left: (&str, A),
    right: (&str, A),
) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        let width = (ui.available_width() - ui.spacing().item_spacing.x) * 0.5;
        let gutter = menu_row_h(ui);
        let (left_label, left_action) = left;
        let (right_label, right_action) = right;
        if menu_cell(
            ui,
            hits,
            left_label,
            Some(left_action.clone()),
            width,
            gutter,
        ) {
            *action = Some(left_action);
        }
        if menu_cell(
            ui,
            hits,
            right_label,
            Some(right_action.clone()),
            width,
            6.0,
        ) {
            *action = Some(right_action);
        }
    });
}

fn submenu_row(ui: &mut egui::Ui, label: &str) -> egui::Response {
    let gutter = menu_row_h(ui);
    let (_, resp) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), gutter),
        egui::Sense::click(),
    );
    paint_menu_row(ui, &resp, label, Some("⏵"), gutter);
    resp
}

fn paint_menu_row(
    ui: &egui::Ui,
    resp: &egui::Response,
    label: &str,
    arrow: Option<&str>,
    inset: f32,
) {
    let rect = resp.rect;
    let hot = resp.contains_pointer() || resp.is_pointer_button_down_on();
    if hot {
        let fill = ui.visuals().widgets.hovered.weak_bg_fill;
        ui.painter()
            .rect_filled(rect, egui::CornerRadius::same(2), fill);
    }
    let font = egui::TextStyle::Button.resolve(ui.style());
    let color = if hot {
        ui.visuals().strong_text_color()
    } else {
        ui.visuals().text_color()
    };
    ui.painter().text(
        egui::pos2(rect.left() + inset, rect.center().y),
        egui::Align2::LEFT_CENTER,
        label,
        font.clone(),
        color,
    );
    if let Some(arrow) = arrow {
        ui.painter().text(
            egui::pos2(rect.right() - 6.0, rect.center().y),
            egui::Align2::RIGHT_CENTER,
            arrow,
            font,
            color,
        );
    }
}

// hover-safe wedge between a row and its submenu
fn submenu_bridge(pos: egui::Pos2, reset: Option<egui::Rect>, submenu: Option<egui::Rect>) -> bool {
    let Some(reset) = reset else {
        return false;
    };
    let Some(submenu) = submenu else {
        return false;
    };
    let reset = egui::Rect::from_min_max(
        egui::pos2(reset.left(), reset.top() - 8.0),
        egui::pos2(reset.right() + 6.0, reset.bottom() + 8.0),
    );
    let submenu = submenu.expand(8.0);
    if reset.contains(pos) || submenu.contains(pos) {
        return true;
    }
    let a = reset.right_top();
    let b = reset.right_bottom();
    in_triangle(pos, a, b, submenu.left_top()) || in_triangle(pos, a, b, submenu.left_bottom())
}

fn in_triangle(p: egui::Pos2, a: egui::Pos2, b: egui::Pos2, c: egui::Pos2) -> bool {
    let cross = |a: egui::Pos2, b: egui::Pos2, p: egui::Pos2| {
        (b.x - a.x) * (p.y - a.y) - (b.y - a.y) * (p.x - a.x)
    };
    let d1 = cross(a, b, p);
    let d2 = cross(b, c, p);
    let d3 = cross(c, a, p);
    let neg = d1 < 0.0 || d2 < 0.0 || d3 < 0.0;
    let pos = d1 > 0.0 || d2 > 0.0 || d3 > 0.0;
    !(neg && pos)
}

fn tab_menu_ui(
    ctx: &egui::Context,
    menu: &mut TabMenu,
    tabs: &Tabs,
    just_opened: bool,
) -> Option<TabMenuAction> {
    if !menu.open {
        return None;
    }
    let Some(target) = menu.target else {
        menu.open = false;
        return None;
    };
    let mut action: Option<TabMenuAction> = None;
    let mut hits: Vec<(TabMenuAction, egui::Rect)> = Vec::new();
    let area = egui::Area::new(egui::Id::new("refcanvas_tab_menu"))
        .fixed_pos(menu.pos)
        .order(egui::Order::Foreground)
        .show(ctx, |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                ui.set_width(190.0);
                ui.spacing_mut().item_spacing.y = 0.0;
                ui.with_layout(
                    egui::Layout::top_down_justified(egui::Align::Min),
                    |ui| match target {
                        TabMenuTarget::Tab(id) => {
                            if tabs.len() > 1
                                && menu_row(
                                    ui,
                                    &mut hits,
                                    "Close tab",
                                    Some(TabMenuAction::Close(id)),
                                )
                            {
                                action = Some(TabMenuAction::Close(id));
                            }
                            if let Some(root) = tabs.root_of(id) {
                                let open = TabMenuAction::OpenFolder(root.clone());
                                if menu_row(
                                    ui,
                                    &mut hits,
                                    "Open project folder",
                                    Some(open.clone()),
                                ) {
                                    action = Some(open);
                                }
                            }
                        }
                        TabMenuTarget::Project => {
                            if menu_row(
                                ui,
                                &mut hits,
                                "New project…",
                                Some(TabMenuAction::NewProject),
                            ) {
                                action = Some(TabMenuAction::NewProject);
                            }
                            if menu_row(
                                ui,
                                &mut hits,
                                "Open project…",
                                Some(TabMenuAction::OpenProject),
                            ) {
                                action = Some(TabMenuAction::OpenProject);
                            }
                        }
                    },
                );
            });
        });
    let rect = area.response.rect;
    let pointer = ctx.input(|i| i.pointer.interact_pos());
    if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
        menu.open = false;
    }
    if !just_opened {
        let outside = pointer.is_some_and(|p| !rect.contains(p));
        if outside
            && (ctx.input(|i| i.pointer.primary_pressed())
                || ctx.input(|i| i.pointer.button_pressed(egui::PointerButton::Secondary)))
        {
            menu.open = false;
        }
    }
    if ctx.input(|i| i.pointer.button_released(egui::PointerButton::Secondary))
        && let Some(p) = pointer
        && p.distance(menu.pos) > 4.0
    {
        if let Some((act, _)) = hits.iter().rev().find(|(_, r)| r.contains(p)) {
            action = Some(act.clone());
        }
        menu.open = false;
    }
    if action.is_some() {
        menu.open = false;
    }
    action
}

#[allow(clippy::too_many_arguments)]
fn context_menu_ui(
    mut contexts: EguiContexts,
    mut menu: ResMut<ContextMenu>,
    mut selection: ResMut<Selection>,
    mut commands: Commands,
    mut q: NodeQ,
    tabs: Res<Tabs>,
    mut system_clipboard: ResMut<SystemClipboard>,
    mut interaction: ResMut<Interaction>,
    mut undo: ResMut<UndoStack>,
    mut dialogs: NonSendMut<crate::dialogs::Dialogs>,
    mut zc: ResMut<ZCounter>,
    view: Res<View>,
) -> Result {
    if !menu.open {
        return Ok(());
    }
    let read_only = tabs.active_read_only();
    if read_only
        && !menu.node.is_some_and(|e| {
            q.get(e)
                .ok()
                .is_some_and(|(_, _, _, node)| node.image().is_some())
        })
    {
        menu.open = false;
        menu.reset_open = false;
        menu.reset_rect = None;
        menu.reset_menu_rect = None;
        return Ok(());
    }
    let ctx = contexts.ctx_mut()?;
    let mut action: Option<MenuAction> = None;
    let mut hits: Vec<(MenuAction, egui::Rect)> = Vec::new();
    let pos = menu.pos;
    let has_node = menu.node.is_some();
    let pointer = ctx.input(|i| i.pointer.interact_pos());
    let keep_reset =
        pointer.is_some_and(|p| submenu_bridge(p, menu.reset_rect, menu.reset_menu_rect));
    let mut reset_rect: Option<egui::Rect> = None;
    let mut reset_hover = false;
    let mut menu_margin = egui::Margin::ZERO;

    let menu_area = egui::Area::new(egui::Id::new("refcanvas_ctx"))
        .fixed_pos(egui::pos2(pos.x, pos.y))
        .order(egui::Order::Foreground)
        .info(egui::UiStackInfo::new(egui::UiKind::Popup))
        .show(ctx, |ui| {
            menu_margin = ui.style().spacing.menu_margin;
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                ui.set_width(190.0);
                ui.spacing_mut().item_spacing.y = 0.0;
                ui.with_layout(egui::Layout::top_down_justified(egui::Align::Min), |ui| {
                    if has_node {
                        if read_only {
                            if menu_row(
                                ui,
                                &mut hits,
                                "Show in File Manager",
                                Some(MenuAction::Reveal),
                            ) {
                                action = Some(MenuAction::Reveal);
                            }
                        } else {
                            let reset = submenu_row(ui, "Reset");
                            reset_rect = Some(reset.rect);
                            reset_hover = reset.contains_pointer();
                            menu_pair(
                                ui,
                                &mut hits,
                                &mut action,
                                ("Flip H", MenuAction::FlipH),
                                ("Flip V", MenuAction::FlipV),
                            );
                            if !selection.nodes.is_empty() {
                                let label =
                                    format!("Group ({})", format_count(selection.nodes.len()));
                                if menu_row(ui, &mut hits, &label, Some(MenuAction::Group)) {
                                    action = Some(MenuAction::Group);
                                }
                            }
                            ui.separator();
                            if menu_row(
                                ui,
                                &mut hits,
                                "Show in File Manager",
                                Some(MenuAction::Reveal),
                            ) {
                                action = Some(MenuAction::Reveal);
                            }
                            ui.separator();
                            if menu_row(ui, &mut hits, "Delete", Some(MenuAction::Delete)) {
                                action = Some(MenuAction::Delete);
                            }
                        }
                    } else if !read_only {
                        if menu_row(
                            ui,
                            &mut hits,
                            "Import images…",
                            Some(MenuAction::ImportDialog),
                        ) {
                            action = Some(MenuAction::ImportDialog);
                        }
                        if menu_row(
                            ui,
                            &mut hits,
                            "Paste from clipboard",
                            Some(MenuAction::PasteFromClipboard),
                        ) {
                            action = Some(MenuAction::PasteFromClipboard);
                        }
                        ui.separator();
                        menu_row(ui, &mut hits, "Settings", None);
                    }
                });
            });
        });

    menu.reset_rect = reset_rect;
    menu.reset_open = reset_hover || (menu.reset_open && keep_reset);
    if menu.reset_open
        && let Some(reset) = reset_rect
    {
        let x = menu_area.response.rect.right() + 2.0;
        let y = reset.top() - menu_margin.top as f32;
        let submenu = egui::Area::new(egui::Id::new("refcanvas_ctx_reset"))
            .fixed_pos(egui::pos2(x, y))
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.set_width(130.0);
                    ui.spacing_mut().item_spacing.y = 0.0;
                    ui.with_layout(egui::Layout::top_down_justified(egui::Align::Min), |ui| {
                        if menu_row(ui, &mut hits, "Rotation", Some(MenuAction::ResetRotation)) {
                            action = Some(MenuAction::ResetRotation);
                        }
                        if menu_row(ui, &mut hits, "Crop", Some(MenuAction::ResetCrop)) {
                            action = Some(MenuAction::ResetCrop);
                        }
                        if menu_row(ui, &mut hits, "Size", Some(MenuAction::ResetSize)) {
                            action = Some(MenuAction::ResetSize);
                        }
                    });
                });
            });
        menu.reset_menu_rect = Some(submenu.response.rect);
    } else {
        menu.reset_menu_rect = None;
    }

    let released = ctx.input(|i| i.pointer.button_released(egui::PointerButton::Secondary));
    if released && let Some(p) = ctx.input(|i| i.pointer.interact_pos()) {
        let opened = egui::pos2(menu.pos.x, menu.pos.y);
        if p.distance(opened) > 4.0 {
            if let Some((act, _)) = hits.iter().rev().find(|(_, rect)| rect.contains(p)) {
                action = Some(act.clone());
            }
            menu.open = false;
            menu.reset_open = false;
            menu.reset_menu_rect = None;
        }
    }

    if let Some(action) = action {
        menu.open = false;
        match action {
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
            MenuAction::Group => {
                let tab_id = tabs.active_id().unwrap_or(0);
                frames::group_selection(
                    &selection,
                    &q,
                    tab_id,
                    view.scale() as f32,
                    &mut zc,
                    &mut commands,
                    &mut undo,
                );
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
                dialogs.pick_images();
            }
            MenuAction::PasteFromClipboard => {
                crate::clipboard::request_paste(&mut system_clipboard, menu.world);
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
        }
    }
    Ok(())
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

fn open_folder(dir: &std::path::Path) {
    #[cfg(target_os = "windows")]
    let _ = std::process::Command::new("explorer").arg(dir).spawn();
    // macos uses `open` instead of xdg-open.
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(dir).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let _ = std::process::Command::new("xdg-open").arg(dir).spawn();
}

/// reveal and select a file; linux uses the freedesktop dbus interface, with a
/// fallback to just opening the parent folder.
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
                // wait for the reply so a missing file manager falls back.
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

/// windows: `explorer /select,` often drops the argument, so call the shell api
/// directly; own thread because the main one's COM apartment belongs to winit.
#[cfg(target_os = "windows")]
fn reveal_windows(path: &std::path::Path) {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::System::Com::{
        COINIT_APARTMENTTHREADED, CoInitializeEx, CoTaskMemFree, CoUninitialize,
    };
    use windows_sys::Win32::UI::Shell::{SHOpenFolderAndSelectItems, SHParseDisplayName};

    // shell wants native separators; a session from linux can still hold '/'.
    let path: std::path::PathBuf = path.components().collect();
    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    wide.push(0);

    std::thread::spawn(move || unsafe {
        let hr = CoInitializeEx(std::ptr::null(), COINIT_APARTMENTTHREADED as u32);
        let mut pidl = std::ptr::null_mut();
        // cidl 0 + null apidl: the parsed pidl is the item to select.
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

/// `file://` uri, percent-encoding everything outside the unreserved set;
/// ShowItems wants real uris.
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

/// rename field over a frame label: enter or click-away commits, escape cancels.
fn frame_rename_ui(
    mut contexts: EguiContexts,
    mut rename: ResMut<FrameRename>,
    camera: Single<(&Camera, &GlobalTransform)>,
    view: Res<View>,
    label_hits: Res<FrameLabelHits>,
    tabs: Res<Tabs>,
    q_own: Query<
        (Entity, &crate::canvas::node::TabOwner),
        Without<crate::canvas::node::InactiveTab>,
    >,
    mut q: NodeQ,
    mut labels: Query<(&FrameLabel, &mut Text2d)>,
) -> Result {
    let Some(frame) = rename.frame else {
        return Ok(());
    };
    if tabs.active_read_only() {
        rename.clear();
        return Ok(());
    }
    let ctx = contexts.ctx_mut()?;

    // frame can vanish mid-edit (delete, tab close, undo).
    let Ok((_, tf, _, mut node)) = q.get_mut(frame) else {
        rename.clear();
        return Ok(());
    };
    // switching tabs abandons the edit.
    let frame_tab = q_own.get(frame).ok().map(|(_, owner)| owner.0);
    if frame_tab != tabs.active_id() {
        rename.clear();
        return Ok(());
    }

    let wpp = (view.scale() as f32).max(1e-4);
    // label top-left; fall back to the frame's when hidden.
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
    let pos = match rename.screen_pos {
        Some(pos) => pos,
        None => match camera.world_to_viewport(cam_tf, anchor.extend(0.0)) {
            Ok(pos) => pos,
            // off-screen: keep the edit alive, just skip drawing.
            Err(_) => return Ok(()),
        },
    };

    let mut commit = false;
    let mut cancel = false;
    let mut area = egui::Area::new(egui::Id::new("refcanvas_frame_rename"))
        .fixed_pos(egui::pos2(pos.x, pos.y))
        .order(egui::Order::Foreground);
    if rename.screen_pos.is_some() {
        area = area
            .pivot(egui::Align2::LEFT_CENTER)
            .fixed_pos(egui::pos2(pos.x - width * 0.3, pos.y));
    }
    area.show(ctx, |ui| {
        egui::Frame::popup(ui.style()).show(ui, |ui| {
            let output = egui::TextEdit::singleline(&mut rename.text)
                .desired_width(width)
                .hint_text("Frame name")
                .show(ui);
            let resp = output.response;
            if rename.focus_me {
                resp.request_focus();
                let end = egui::text::CCursor::new(rename.text.chars().count());
                let mut state = output.state;
                state
                    .cursor
                    .set_char_range(Some(egui::text::CCursorRange::one(end)));
                state.store(ctx, resp.id);
                rename.focus_me = false;
            }
            if resp.has_focus() {
                rename.had_focus = true;
            }
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

#[derive(Clone)]
struct PinIcon {
    texture: egui::TextureHandle,
}

fn pin_icon(ctx: &egui::Context) -> PinIcon {
    let id = egui::Id::new("refcanvas_pin_icon");
    if let Some(icon) = ctx.data(|data| data.get_temp::<PinIcon>(id)) {
        return icon;
    }
    let image = image::load_from_memory(include_bytes!("../assets/icons/thumbtack-white.png"))
        .expect("embedded thumbtack icon")
        .into_rgba8();
    let size = [image.width() as usize, image.height() as usize];
    let color = egui::ColorImage::from_rgba_unmultiplied(size, image.as_raw());
    let icon = PinIcon {
        texture: ctx.load_texture("refcanvas_pin", color, egui::TextureOptions::LINEAR),
    };
    ctx.data_mut(|data| data.insert_temp(id, icon.clone()));
    icon
}

fn tab_bar(
    mut contexts: EguiContexts,
    mut tabs: ResMut<Tabs>,
    mut requests: ResMut<TabRequests>,
    mut always_on_top: ResMut<AlwaysOnTop>,
    mut dialogs: NonSendMut<crate::dialogs::Dialogs>,
    mut dialog_results: ResMut<crate::dialogs::DialogResults>,
    // folder picker reports only a path, so remember which menu asked.
    mut folder_pick_is_new: Local<bool>,
    mut tab_menu: ResMut<TabMenu>,
    mut drag: Local<TabDrag>,
) -> Result {
    let ctx = contexts.ctx_mut()?;
    let mut pending: Vec<TabRequest> = Vec::new();
    let mut open_new = false;
    let mut open_existing = false;
    let mut just_opened = false;

    let mut root = egui::Ui::new(
        ctx.clone(),
        "refcanvas_root".into(),
        UiBuilder::new()
            .layer_id(LayerId::background())
            .max_rect(ctx.viewport_rect()),
    );

    // explicit symmetric padding so the row sits centred.
    let tabs_frame =
        egui::Frame::side_top_panel(root.style()).inner_margin(egui::Margin::symmetric(8, 4));
    let tabs_panel = egui::Panel::top("refcanvas_tabs")
        .frame(tabs_frame)
        .show(&mut root, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 3.0;
                // tallest tab sets the "+" button size.
                let mut row_h = ui.spacing().interact_size.y;
                let active = tabs.active_id();
                let spacing = 3.0;
                let start_x = ui.cursor().min.x;
                let top = ui.cursor().min.y;
                let pointer = ctx.input(|i| i.pointer.interact_pos());
                let primary_down =
                    ctx.input(|i| i.pointer.button_down(egui::PointerButton::Primary));
                if !primary_down {
                    drag.id = None;
                }
                let mut list = tabs.summaries();
                let mut widths: Vec<f32> = list.iter().map(|(_, n)| tab_width(ui, n)).collect();
                let total_w = |widths: &[f32]| {
                    widths.iter().sum::<f32>() + spacing * widths.len().saturating_sub(1) as f32
                };

                if drag.id.is_none()
                    && ctx.input(|i| i.pointer.button_pressed(egui::PointerButton::Primary))
                    && let Some(p) = pointer
                {
                    let mut x = start_x;
                    for ((id, _), w) in list.iter().zip(&widths) {
                        let rect =
                            egui::Rect::from_min_size(egui::pos2(x, top), egui::vec2(*w, row_h));
                        if rect.contains(p) {
                            drag.id = Some(*id);
                            drag.grab = p.x - x;
                            break;
                        }
                        x += w + spacing;
                    }
                }

                // follow the pointer, then let the other tabs slide to their new slot.
                let mut drag_x = None;
                if let (Some(id), Some(p)) = (drag.id, pointer)
                    && let Some(from) = list.iter().position(|(tid, _)| *tid == id)
                {
                    let max_x = (start_x + total_w(&widths) - widths[from]).max(start_x);
                    let x = (p.x - drag.grab).clamp(start_x, max_x);
                    let mut to = 0;
                    let mut slot = start_x;
                    for (i, w) in widths.iter().enumerate() {
                        if i == from {
                            slot += w + spacing;
                            continue;
                        }
                        if slot + w * 0.5 < p.x {
                            to += 1;
                        }
                        slot += w + spacing;
                    }
                    drag_x = Some(x);
                    if to != from {
                        tabs.move_tab(from, to);
                        list = tabs.summaries();
                        widths = list.iter().map(|(_, n)| tab_width(ui, n)).collect();
                    }
                }

                let anim = |id: u64| egui::Id::new(("refcanvas_tab_slide", id));
                let dragged = drag.id;
                let mut responses: Vec<(u64, egui::Response)> = Vec::new();
                let mut x = start_x;
                for ((id, name), w) in list.iter().zip(&widths) {
                    let slot = x;
                    x += w + spacing;
                    if Some(*id) == dragged {
                        continue;
                    }
                    let shown = ctx.animate_value_with_time(anim(*id), slot, 0.12);
                    let selected = Some(*id) == active;
                    responses.push((*id, tab_button(ui, *id, name, *w, row_h, shown, selected)));
                }
                if let Some(id) = dragged
                    && let Some(i) = list.iter().position(|(tid, _)| *tid == id)
                {
                    let slot = start_x + widths[..i].iter().sum::<f32>() + spacing * i as f32;
                    let shown = ctx.animate_value_with_time(anim(id), drag_x.unwrap_or(slot), 0.04);
                    let selected = Some(id) == active;
                    responses.push((
                        id,
                        tab_button(ui, id, &list[i].1, widths[i], row_h, shown, selected),
                    ));
                }
                let row = egui::Rect::from_min_size(
                    egui::pos2(start_x, top),
                    egui::vec2(total_w(&widths), row_h),
                );
                ui.advance_cursor_after_rect(row);

                for (id, resp) in responses {
                    row_h = row_h.max(resp.rect.height());
                    if resp.clicked() {
                        pending.push(TabRequest::Switch(id));
                    }
                    if resp.middle_clicked() {
                        pending.push(TabRequest::Close(id));
                    }
                    if resp.contains_pointer()
                        && ui.input(|i| i.pointer.button_pressed(egui::PointerButton::Secondary))
                    {
                        tab_menu.open = true;
                        tab_menu.target = Some(TabMenuTarget::Tab(id));
                        tab_menu.pos = resp.interact_pointer_pos().unwrap_or(resp.rect.center());
                        just_opened = true;
                    }
                }
                // pin toggles always-on-top, "+" opens the project menu; the plus
                // is hand-drawn because the stock glyph isn't optically centred.
                let side = row_h;
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    // flat hover: the default stroke shrinks content and grows the row.
                    let visuals = ui.visuals_mut();
                    visuals.widgets.hovered.bg_stroke = egui::Stroke::NONE;
                    visuals.widgets.active.bg_stroke = egui::Stroke::NONE;
                    visuals.widgets.hovered.expansion = 0.0;
                    visuals.widgets.active.expansion = 0.0;

                    let wayland = crate::on_wayland();
                    let can_pin = !wayland || std::env::var_os("DISPLAY").is_some();
                    let icon = pin_icon(ctx);
                    let pinned = always_on_top.0;
                    let button = egui::Button::new("")
                        .min_size(egui::vec2(side, side))
                        .frame_when_inactive(pinned);
                    let button = if pinned {
                        button.fill(ui.visuals().selection.bg_fill)
                    } else {
                        button
                    };
                    let pin_resp = ui.add_enabled(can_pin, button);
                    let tint = if !pin_resp.enabled() {
                        ui.visuals().weak_text_color()
                    } else if pinned {
                        egui::Color32::from_rgb(18, 18, 18)
                    } else {
                        ui.style().interact(&pin_resp).fg_stroke.color
                    };
                    let icon_rect = egui::Rect::from_center_size(
                        pin_resp.rect.center(),
                        egui::vec2(side * 0.54, side * 0.54),
                    );
                    ui.painter().image(
                        icon.texture.id(),
                        icon_rect,
                        egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                        tint,
                    );
                    let pin_resp = pin_resp.on_hover_text(if !can_pin {
                        "Always on top is not available on native Wayland"
                    } else if wayland {
                        "Always on top\nRestart to apply on Wayland"
                    } else {
                        "Always on top"
                    });
                    if pin_resp.clicked() {
                        always_on_top.0 = !always_on_top.0;
                        crate::project::local::save_always_on_top(always_on_top.0);
                    }

                    let resp = ui.add(
                        egui::Button::new("")
                            .min_size(egui::vec2(side, side))
                            .frame_when_inactive(false),
                    );
                    if resp.clicked() {
                        tab_menu.open = true;
                        tab_menu.target = Some(TabMenuTarget::Project);
                        tab_menu.pos = egui::pos2(resp.rect.left(), resp.rect.bottom() + 2.0);
                        just_opened = true;
                    }
                    {
                        let c = resp.rect.center();
                        let arm = side * 0.26;
                        let width = (side * 0.10).clamp(1.0, 2.2);
                        let stroke =
                            egui::Stroke::new(width, ui.style().interact(&resp).fg_stroke.color);
                        let p = ui.painter();
                        p.line_segment(
                            [c - egui::vec2(arm, 0.0), c + egui::vec2(arm, 0.0)],
                            stroke,
                        );
                        p.line_segment(
                            [c - egui::vec2(0.0, arm), c + egui::vec2(0.0, arm)],
                            stroke,
                        );
                    }
                });
            });
        });
    // canvas ignores pointer input over the bar.
    ctx.data_mut(|d| {
        d.insert_persisted(
            egui::Id::new(crate::canvas::TAB_BAR_RECT_ID),
            tabs_panel.response.rect,
        )
    });

    if let Some(action) = tab_menu_ui(ctx, &mut tab_menu, &tabs, just_opened) {
        match action {
            TabMenuAction::Close(id) => pending.push(TabRequest::Close(id)),
            TabMenuAction::OpenFolder(root) => open_folder(&root),
            TabMenuAction::NewProject => open_new = true,
            TabMenuAction::OpenProject => open_existing = true,
        }
    }

    if let Some(dir) = dialog_results.folders.pop() {
        dialog_results.folders.clear();
        let new_project = *folder_pick_is_new;
        *folder_pick_is_new = false;
        pending.push(if new_project {
            TabRequest::OpenNew(dir)
        } else {
            TabRequest::Open(dir)
        });
    }
    if !dialogs.is_open() {
        if open_new {
            dialogs.pick_folder("New project: choose a folder");
            *folder_pick_is_new = true;
        }
        if open_existing {
            dialogs.pick_folder("Open project: choose a folder");
        }
    }

    requests.queue.extend(pending);
    Ok(())
}

fn format_count(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn tab_width(ui: &egui::Ui, name: &str) -> f32 {
    let font = egui::TextStyle::Button.resolve(ui.style());
    let text = truncate(name, 24);
    let text_w = ui
        .painter()
        .layout_no_wrap(text, font, ui.visuals().text_color())
        .size()
        .x;
    (text_w + 18.0).max(56.0)
}

fn tab_button(
    ui: &mut egui::Ui,
    id: u64,
    name: &str,
    width: f32,
    row_h: f32,
    x: f32,
    selected: bool,
) -> egui::Response {
    let rect =
        egui::Rect::from_min_size(egui::pos2(x, ui.cursor().min.y), egui::vec2(width, row_h));
    let text_color = if selected {
        readable_text(ui.visuals().selection.bg_fill)
    } else {
        ui.visuals().text_color()
    };
    let label = egui::RichText::new(truncate(name, 24)).color(text_color);
    let mut tab_ui = ui.new_child(
        UiBuilder::new()
            .id(egui::Id::new(("refcanvas_tab", id)))
            .max_rect(rect)
            .layout(egui::Layout::centered_and_justified(
                egui::Direction::TopDown,
            )),
    );
    tab_ui.spacing_mut().button_padding = egui::vec2(9.0, 2.0);
    // flat hover: the default stroke shrinks content and grows the row.
    let v = tab_ui.visuals_mut();
    let corner = v.widgets.inactive.corner_radius;
    v.widgets.hovered.bg_stroke = egui::Stroke::NONE;
    v.widgets.hovered.expansion = 0.0;
    v.widgets.hovered.corner_radius = corner;
    v.widgets.active.bg_stroke = egui::Stroke::NONE;
    v.widgets.active.expansion = 0.0;
    v.widgets.active.corner_radius = corner;
    v.widgets.open.corner_radius = corner;
    tab_ui.add(
        egui::Button::selectable(selected, label)
            .min_size(egui::vec2(width, row_h))
            .sense(egui::Sense::click_and_drag()),
    )
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

fn hud(
    mut contexts: EguiContexts,
    view: Res<View>,
    selection: Res<Selection>,
    queue: Res<ImportQueue>,
    update: Res<crate::update::UpdateStatus>,
    tabs: Res<Tabs>,
    real: Res<Time<Real>>,
    mut pacer: ResMut<crate::FramePacer>,
    mut fps: Local<FpsCounter>,
) -> Result {
    // real time: a wake-up frame carries the whole nap as its delta, so snap
    // to the heartbeat rate and start the window fresh.
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
        // click-through background, but hit-testable for the fps click.
        .sense(egui::Sense::hover())
        .show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 12.0;
                // fixed-width numbers: right-anchored areas re-anchor a frame late,
                // so a width change wobbles. padded to the widest value.
                ui.label(text(format!("{:>6.0}%", view.zoom_percent())));
                // click fps to toggle benchmark mode (uncapped).
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
                                // selection would add an I-beam and allow dragging.
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
                if let Some(badge) = update.badge_text() {
                    egui::Frame::NONE
                        .fill(egui::Color32::from_rgb(255, 210, 0))
                        .corner_radius(0)
                        .inner_margin(egui::Margin::symmetric(4, 1))
                        .show(ui, |ui| {
                            ui.label(
                                egui::RichText::new(badge)
                                    .monospace()
                                    .size(12.0)
                                    .color(egui::Color32::BLACK),
                            );
                        });
                } else {
                    ui.label(text(format!("v{}", env!("CARGO_PKG_VERSION"))).weak());
                }
                if !selection.nodes.is_empty() && selection.nodes.len() > 1 {
                    ui.label(text(format!("{} selected", selection.nodes.len())).weak());
                }
                if let Some((done, total)) = queue.progress() {
                    ui.label(text(format!("importing {done}/{total}")));
                }
                if let Some(tab) = tabs.stored.get(tabs.active)
                    && tab.merge.read_only
                {
                    egui::Frame::NONE
                        .fill(egui::Color32::from_rgb(190, 38, 38))
                        .corner_radius(0)
                        .inner_margin(egui::Margin::symmetric(4, 1))
                        .show(ui, |ui| {
                            ui.label(
                                egui::RichText::new("(read-only)")
                                    .monospace()
                                    .size(12.0)
                                    .color(egui::Color32::WHITE),
                            );
                        });

                    egui::Frame::NONE
                        .fill(egui::Color32::from_rgb(80, 20, 20))
                        .corner_radius(0)
                        .inner_margin(egui::Margin::symmetric(4, 1))
                        .show(ui, |ui| {
                            ui.label(
                                egui::RichText::new(
                                    "This project data structure is newer than your client; an update is required to prevent data corruption...",
                                )
                                .monospace()
                                .size(12.0)
                                .color(egui::Color32::WHITE),
                            );
                        });
                    // ui.label(text("update RefCanvas to edit".to_string()).weak());
                }
            });
        });

    Ok(())
}

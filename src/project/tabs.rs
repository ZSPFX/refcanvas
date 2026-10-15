//! Open projects as tabs (Chrome-style), with session restore.
//!
//! One project = one folder containing `project.db`, `images/`, `cache/`.
//! Restored tabs load on first activation. Visited tabs keep hidden nodes and
//! previews, with background work and undo history parked per tab.

use std::path::{Path, PathBuf};

use bevy::math::DVec2;
use bevy::prelude::*;
use bevy_egui::EguiContexts;

use crate::canvas::camera::View;
use crate::canvas::egui_keyboard_busy;
use crate::canvas::manipulate::ZCounter;
use crate::canvas::node::{CanvasNode, InactiveTab, TabOwner};
use crate::canvas::undo::UndoStack;
use crate::canvas::selection::{Interaction, Selection};

use super::db::ProjectDb;
use super::import::ImportQueue;
use super::persist::{self, PersistenceState, PreviewLoader};
use super::store::ProjectStore;

#[derive(Resource, Default)]
pub struct Tabs {
    pub(crate) stored: Vec<OpenTab>,
    pub(crate) active: usize,
    next_id: u64,
}

pub struct OpenTab {
    pub id: u64,
    pub name: String,
    pub store: ProjectStore,
    pub db: ProjectDb,
    pub state: PersistenceState,
    pub loaded: bool,
    undo: UndoStack,
    imports: ImportQueue,
    /// Camera center + log scale.
    pub view: (DVec2, f64),
}

impl Tabs {
    pub fn active_id(&self) -> Option<u64> {
        self.stored.get(self.active).map(|t| t.id)
    }

    pub fn index_of(&self, id: u64) -> Option<usize> {
        self.stored.iter().position(|t| t.id == id)
    }

    pub fn len(&self) -> usize {
        self.stored.len()
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.stored.is_empty()
    }

    /// (id, name) for every open tab, in tab-bar order.
    pub fn summaries(&self) -> Vec<(u64, String)> {
        self.stored.iter().map(|t| (t.id, t.name.clone())).collect()
    }

    /// Root folder of the tab with the given id.
    pub fn root_of(&self, id: u64) -> Option<PathBuf> {
        self.stored
            .iter()
            .find(|t| t.id == id)
            .map(|t| t.store.root.clone())
    }

    fn new_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }
}

#[derive(Resource, Default)]
pub struct TabRequests {
    pub queue: Vec<TabRequest>,
}

pub enum TabRequest {
    /// Open (or create) the project rooted at this path (folder or .db file).
    Open(PathBuf),
    Close(u64),
    Switch(u64),
    Cycle(i32),
    SaveNow,
}

// ---------------------------------------------------------------------------
// Session (which tabs were open)
// ---------------------------------------------------------------------------

#[derive(serde::Serialize, serde::Deserialize, Default)]
struct Session {
    tabs: Vec<String>,
    active: usize,
}

#[cfg(not(test))]
fn session_path() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|b| b.data_dir().join("refcanvas").join("session.json"))
}

#[cfg(test)]
fn session_path() -> Option<PathBuf> {
    None
}

fn load_session() -> Session {
    let Some(path) = session_path() else {
        return Session::default();
    };
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save_session(tabs: &Tabs) {
    let Some(path) = session_path() else {
        return;
    };
    let session = Session {
        tabs: tabs
            .stored
            .iter()
            .map(|t| t.store.root.to_string_lossy().into_owned())
            .collect(),
        active: tabs.active,
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    match serde_json::to_string_pretty(&session) {
        Ok(json) => {
            if let Err(err) = std::fs::write(&path, json) {
                warn!("session save failed: {err}");
            }
        }
        Err(err) => warn!("session serialize failed: {err}"),
    }
}

// ---------------------------------------------------------------------------
// Systems
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
pub fn startup_tabs(
    mut commands: Commands,
    mut tabs: ResMut<Tabs>,
    mut view: ResMut<View>,
    mut store: ResMut<ProjectStore>,
    mut zc: ResMut<ZCounter>,
    mut images: ResMut<Assets<Image>>,
    mut loader: ResMut<PreviewLoader>,
    mut q_vis: Query<(Entity, &TabOwner, &mut Visibility)>,
) {
    restore_session(load_session(), &mut commands, &mut tabs, &mut view,
        &mut store, &mut zc, &mut images, &mut loader, &mut q_vis);
}

#[allow(clippy::too_many_arguments)]
fn restore_session(
    session: Session,
    commands: &mut Commands,
    tabs: &mut Tabs,
    view: &mut View,
    store: &mut ProjectStore,
    zc: &mut ZCounter,
    images: &mut Assets<Image>,
    loader: &mut PreviewLoader,
    q_vis: &mut Query<(Entity, &TabOwner, &mut Visibility)>,
) {
    let mut roots: Vec<PathBuf> = session
        .tabs
        .iter()
        .map(PathBuf::from)
        .filter(|p| p.is_dir())
        .collect();
    if roots.is_empty() {
        roots.push(store.root.clone());
    }
    let wanted_root = session.tabs.get(session.active).map(PathBuf::from);
    for root in &roots {
        let id = tabs.new_id();
        match open_tab_at(root, id) {
            Ok(tab) => tabs.stored.push(tab),
            Err(err) => warn!("failed to open {}: {err:#}", root.display()),
        }
    }
    if tabs.stored.is_empty() {
        error!("no project could be opened");
        return;
    }
    let preferred = tabs.stored.iter().position(|tab| Some(&tab.store.root) == wanted_root.as_ref())
        .unwrap_or(0);
    let mut activated = false;
    for index in std::iter::once(preferred).chain((0..tabs.len()).filter(|i| *i != preferred)) {
        let tab = &mut tabs.stored[index];
        match ensure_loaded(tab, commands, images, zc, loader) {
            Ok(()) => {
                tabs.active = index;
                activated = true;
                break;
            }
            Err(err) => warn!("failed to load {}: {err:#}", tab.store.root.display()),
        }
    }
    if !activated {
        error!("no project could be loaded");
        return;
    }
    apply_active(tabs, view, q_vis, commands, loader);
    if let Some(s) = tabs.stored.get(tabs.active) {
        *store = s.store.clone();
        info!("active project: {}", s.store.root.display());
    }
    save_session(tabs);
}

/// Handle shortcuts that manage tabs (Ctrl+T / Ctrl+O / Ctrl+W / Ctrl+Tab / Ctrl+S).
pub fn tab_shortcuts(
    keys: Res<ButtonInput<KeyCode>>,
    mut contexts: EguiContexts,
    tabs: Res<Tabs>,
    mut requests: ResMut<TabRequests>,
    chord_mem: Res<crate::canvas::ChordMemory>,
    chord_frame: Res<crate::canvas::lod::FrameCounter>,
) {
    if egui_keyboard_busy(&mut contexts) {
        return;
    }
    if !crate::canvas::ctrl_down(&keys, &chord_mem, chord_frame.0) {
        return;
    }
    let shift = crate::canvas::shift_down(&keys, &chord_mem, chord_frame.0);

    if keys.just_pressed(KeyCode::KeyT) {
        if let Some(dir) = rfd::FileDialog::new()
            .set_title("New project: choose a folder")
            .pick_folder()
        {
            requests.queue.push(TabRequest::Open(dir));
        }
    }
    if keys.just_pressed(KeyCode::KeyO) {
        if let Some(file) = rfd::FileDialog::new()
            .set_title("Open project")
            .add_filter("refcanvas project", &["db"])
            .pick_file()
        {
            requests.queue.push(TabRequest::Open(file));
        }
    }
    if keys.just_pressed(KeyCode::KeyW) {
        if let Some(id) = tabs.active_id() {
            requests.queue.push(TabRequest::Close(id));
        }
    }
    if keys.just_pressed(KeyCode::Tab) {
        requests.queue.push(TabRequest::Cycle(if shift { -1 } else { 1 }));
    }
    if keys.just_pressed(KeyCode::KeyS) {
        requests.queue.push(TabRequest::SaveNow);
    }
}

#[allow(clippy::too_many_arguments)]
pub fn process_requests(
    mut requests: ResMut<TabRequests>,
    mut tabs: ResMut<Tabs>,
    mut view: ResMut<View>,
    mut store: ResMut<ProjectStore>,
    mut images: ResMut<Assets<Image>>,
    mut zc: ResMut<ZCounter>,
    mut loader: ResMut<PreviewLoader>,
    mut commands: Commands,
    mut q_nodes: Query<(Entity, &Transform, &mut CanvasNode, &Sprite, &TabOwner)>,
    mut q_vis: Query<(Entity, &TabOwner, &mut Visibility)>,
    mut selection: ResMut<Selection>,
    mut interaction: ResMut<Interaction>,
    mut undo: ResMut<UndoStack>,
    mut imports: ResMut<ImportQueue>,
) {
    if requests.queue.is_empty() {
        return;
    }
    // Apply deferred spawns before processing another switch or save.
    let request = requests.queue.remove(0);
    let mut closing = None;
    let next = match request {
        TabRequest::Open(path) => {
            let root = if path.is_dir() {
                path
            } else {
                path.parent().map(|p| p.to_path_buf()).unwrap_or(path)
            };
            if let Some(index) = tabs.stored.iter().position(|t| t.store.root == root) {
                index
            } else {
                let id = tabs.new_id();
                match open_tab_at(&root, id) {
                    Ok(tab) => {
                        tabs.stored.push(tab);
                        tabs.stored.len() - 1
                    }
                    Err(err) => {
                        warn!("failed to open {}: {err:#}", root.display());
                        return;
                    }
                }
            }
        }
        TabRequest::Switch(id) => {
            let Some(index) = tabs.index_of(id) else {
                return;
            };
            index
        }
        TabRequest::Cycle(dir) => {
            if tabs.len() <= 1 {
                return;
            }
            (tabs.active as i32 + dir).rem_euclid(tabs.len() as i32) as usize
        }
        TabRequest::Close(id) => {
            if tabs.len() <= 1 {
                return;
            }
            let Some(index) = tabs.index_of(id) else {
                return;
            };
            closing = Some(index);
            if index != tabs.active {
                tabs.active
            } else if index + 1 < tabs.len() {
                index + 1
            } else {
                index - 1
            }
        }
        TabRequest::SaveNow => {
            save_active(&mut tabs, &view, &mut q_nodes);
            return;
        }
    };

    if next != tabs.active || !tabs.stored[next].loaded {
        let tab = &mut tabs.stored[next];
        if let Err(err) = ensure_loaded(tab, &mut commands, &mut images, &mut zc, &mut loader) {
            warn!("failed to load {}: {err:#}", tab.store.root.display());
            return;
        }
        let previous = tabs.active;
        if previous != next {
            save_active(&mut tabs, &view, &mut q_nodes);
            std::mem::swap(&mut tabs.stored[previous].undo, &mut *undo);
            std::mem::swap(&mut tabs.stored[previous].imports, &mut *imports);
            std::mem::swap(&mut tabs.stored[next].undo, &mut *undo);
            std::mem::swap(&mut tabs.stored[next].imports, &mut *imports);
        }
        tabs.active = next;
        apply_active(&tabs, &mut view, &mut q_vis, &mut commands, &mut loader);
        selection.clear();
        *interaction = Interaction::Idle;
        *store = tabs.stored[next].store.clone();
    }

    if let Some(index) = closing {
        let id = tabs.stored[index].id;
        for (entity, _, _, _, owner) in &q_nodes {
            if owner.0 == id {
                commands.entity(entity).despawn();
            }
        }
        loader.forget(id);
        tabs.stored.remove(index);
        if index < tabs.active {
            tabs.active -= 1;
        }
    }
    save_session(&tabs);
}

pub fn on_exit(
    mut exit: MessageReader<AppExit>,
    mut tabs: ResMut<Tabs>,
    view: Res<View>,
    mut q: Query<(Entity, &Transform, &mut CanvasNode, &Sprite, &TabOwner)>,
) {
    if exit.read().next().is_none() || tabs.stored.is_empty() {
        return;
    }
    save_active(&mut tabs, &view, &mut q);
    save_session(&tabs);
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn open_tab_at(root: &Path, id: u64) -> anyhow::Result<OpenTab> {
    let store = ProjectStore {
        root: root.to_path_buf(),
    };
    store.ensure_dirs()?;
    let db = ProjectDb::open(&store.db_path())?;
    let name = root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "project".to_string());
    Ok(OpenTab {
        id,
        name,
        store,
        db,
        state: PersistenceState::default(),
        loaded: false,
        undo: UndoStack::default(),
        imports: ImportQueue::default(),
        view: (DVec2::ZERO, 0.0),
    })
}

fn ensure_loaded(
    tab: &mut OpenTab,
    commands: &mut Commands,
    images: &mut Assets<Image>,
    zc: &mut ZCounter,
    loader: &mut PreviewLoader,
) -> anyhow::Result<()> {
    if tab.loaded {
        return Ok(());
    }
    let (_count, cam) = persist::load_into_world(
        &tab.store,
        &tab.db,
        tab.id,
        true,
        commands,
        images,
        zc,
        loader,
        &mut tab.state,
    )?;
    if let Some((x, y, ls)) = cam {
        tab.view = (DVec2::new(x, y), ls);
    }
    tab.loaded = true;
    Ok(())
}

/// Persist the active tab (nodes + camera).
fn save_active(
    tabs: &mut Tabs,
    view: &View,
    q: &mut Query<(Entity, &Transform, &mut CanvasNode, &Sprite, &TabOwner)>,
) {
    if tabs.stored.is_empty() {
        return;
    }
    let idx = tabs.active.min(tabs.stored.len() - 1);
    let tab = &mut tabs.stored[idx];
    if !tab.loaded {
        return;
    }
    tab.view = (view.center, view.log_scale);
    persist::save_nodes(&tab.db, tab.id, &mut tab.state, view, q);
}

/// Apply the active tab's camera and node visibility.
fn apply_active(
    tabs: &Tabs,
    view: &mut View,
    q_vis: &mut Query<(Entity, &TabOwner, &mut Visibility)>,
    commands: &mut Commands,
    loader: &mut PreviewLoader,
) {
    let Some(tab) = tabs.stored.get(tabs.active) else {
        return;
    };
    view.center = tab.view.0;
    view.target_center = tab.view.0;
    view.log_scale = tab.view.1;
    view.target_log_scale = tab.view.1;
    view.zoom_anchor = None;
    let active_id = tab.id;
    loader.activate(active_id);
    for (entity, owner, mut vis) in q_vis.iter_mut() {
        if owner.0 == active_id {
            *vis = Visibility::Inherited;
            commands.entity(entity).remove::<InactiveTab>();
        } else {
            *vis = Visibility::Hidden;
            commands.entity(entity).insert(InactiveTab);
        }
    }
}

#[cfg(test)]
#[path = "tabs_tests.rs"]
mod tests;

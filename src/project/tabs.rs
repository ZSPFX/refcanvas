//! Open projects as tabs, with session restore.
//!
//! One project = one folder containing append-only canvas journals,
//! checkpoints, `images/` and `cache/`. Restored tabs load on first activation.
//! Visited tabs keep hidden nodes and previews, with background work and undo
//! history parked per tab.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use bevy::math::DVec2;
use bevy::prelude::*;
use bevy_egui::EguiContexts;

use crate::canvas::camera::View;
use crate::canvas::egui_keyboard_busy;
use crate::canvas::manipulate::ZCounter;
use crate::canvas::node::{CanvasNode, InactiveTab, TabOwner};
use crate::canvas::selection::{Interaction, Selection};
use crate::canvas::undo::UndoStack;

use super::db;
use super::import::ImportQueue;
use super::local;
use super::oplog::{self, Change, Clock, FileMark, Journal, MergeState, Op};
use super::persist::{self, PersistenceState, PreviewLoader};
use super::store::ProjectStore;

#[derive(Resource)]
pub struct Tabs {
    pub(crate) stored: Vec<OpenTab>,
    pub(crate) active: usize,
    next_id: u64,
    /// Project folders remembered from the session but not available yet
    /// (offline network share, unmounted drive, sync still arriving). They are
    /// retried in the background and preserved in `session.json`.
    pub(crate) unavailable: Vec<PathBuf>,
    pub(crate) clock: Clock,
}

impl Default for Tabs {
    fn default() -> Self {
        Self {
            stored: Vec::new(),
            active: 0,
            next_id: 1,
            unavailable: Vec::new(),
            clock: local::load_clock(),
        }
    }
}

pub struct OpenTab {
    pub id: u64,
    pub name: String,
    pub store: ProjectStore,
    pub journal: Journal,
    pub merge: MergeState,
    pub files: HashMap<String, FileMark>,
    /// Remote changes waiting for the pointer interaction to finish.
    pub pending: Vec<Change>,
    pub state: PersistenceState,
    pub state_dirty: bool,
    pub checkpoint_dirty: bool,
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
    /// Create a project in a folder that is not open yet: activates it even
    /// when it is already a tab, so the picker always answers visibly.
    OpenNew(PathBuf),
    /// Re-open a remembered project without changing the active tab.
    OpenRestored(PathBuf),
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
    let mut roots: Vec<String> = tabs
        .stored
        .iter()
        .map(|t| t.store.root.to_string_lossy().into_owned())
        .collect();
    for root in &tabs.unavailable {
        let root = root.to_string_lossy().into_owned();
        if !roots.iter().any(|existing| existing == &root) {
            roots.push(root);
        }
    }
    let active_root = tabs
        .stored
        .get(tabs.active)
        .map(|t| t.store.root.to_string_lossy().into_owned());
    let active = active_root
        .and_then(|root| roots.iter().position(|candidate| candidate == &root))
        .unwrap_or(0);
    let session = Session {
        tabs: roots,
        active,
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
    restore_session(
        load_session(),
        &mut commands,
        &mut tabs,
        &mut view,
        &mut store,
        &mut zc,
        &mut images,
        &mut loader,
        &mut q_vis,
    );
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
    let mut roots: Vec<PathBuf> = Vec::new();
    let mut unavailable: Vec<PathBuf> = Vec::new();
    for root in session.tabs.iter().map(PathBuf::from) {
        if root.is_dir() {
            roots.push(root);
        } else {
            unavailable.push(root);
        }
    }
    if roots.is_empty() {
        roots.push(store.root.clone());
    }
    tabs.unavailable = unavailable;
    let wanted_root = session.tabs.get(session.active).map(PathBuf::from);
    for root in &roots {
        let id = tabs.new_id();
        match open_tab_at(root, id, &mut tabs.clock) {
            Ok(tab) => tabs.stored.push(tab),
            Err(err) => {
                warn!("failed to open {}: {err:#}", root.display());
                tabs.unavailable.push(root.clone());
            }
        }
    }
    if tabs.stored.is_empty() {
        error!("no project could be opened");
        return;
    }
    let preferred = tabs
        .stored
        .iter()
        .position(|tab| Some(&tab.store.root) == wanted_root.as_ref())
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
    mut dialogs: ResMut<crate::dialogs::Dialogs>,
    mut dialog_results: ResMut<crate::dialogs::DialogResults>,
    mut pick_is_new: Local<bool>,
    chord_mem: Res<crate::canvas::ChordMemory>,
    chord_frame: Res<crate::canvas::lod::FrameCounter>,
) {
    if egui_keyboard_busy(&mut contexts) {
        return;
    }
    if let Some(dir) = dialog_results.folders.pop() {
        dialog_results.folders.clear();
        let is_new = *pick_is_new;
        *pick_is_new = false;
        requests.queue.push(if is_new {
            TabRequest::OpenNew(dir)
        } else {
            TabRequest::Open(dir)
        });
    }
    if !crate::canvas::ctrl_down(&keys, &chord_mem, chord_frame.0) {
        return;
    }
    let shift = crate::canvas::shift_down(&keys, &chord_mem, chord_frame.0);

    if keys.just_pressed(KeyCode::KeyT) {
        dialogs.pick_folder("New project: choose a folder");
        *pick_is_new = true;
    }
    if keys.just_pressed(KeyCode::KeyO) {
        dialogs.pick_folder("Open project: choose a folder");
        *pick_is_new = false;
    }
    if keys.just_pressed(KeyCode::KeyW) {
        if let Some(id) = tabs.active_id() {
            requests.queue.push(TabRequest::Close(id));
        }
    }
    if keys.just_pressed(KeyCode::Tab) {
        requests
            .queue
            .push(TabRequest::Cycle(if shift { -1 } else { 1 }));
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
            tabs.unavailable.retain(|pending| pending != &root);
            if let Some(index) = tabs.stored.iter().position(|t| t.store.root == root) {
                index
            } else {
                let id = tabs.new_id();
                match open_tab_at(&root, id, &mut tabs.clock) {
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
        TabRequest::OpenNew(path) => {
            let root = if path.is_dir() {
                path
            } else {
                path.parent().map(|p| p.to_path_buf()).unwrap_or(path)
            };
            tabs.unavailable.retain(|pending| pending != &root);
            if let Some(index) = tabs.stored.iter().position(|t| t.store.root == root) {
                index
            } else {
                let id = tabs.new_id();
                match open_tab_at(&root, id, &mut tabs.clock) {
                    Ok(tab) => {
                        tabs.stored.push(tab);
                        tabs.stored.len() - 1
                    }
                    Err(err) => {
                        warn!("failed to create project at {}: {err:#}", root.display());
                        return;
                    }
                }
            }
        }
        TabRequest::OpenRestored(path) => {
            let root = if path.is_dir() {
                path
            } else {
                path.parent().map(|p| p.to_path_buf()).unwrap_or(path)
            };
            if tabs.stored.iter().any(|tab| tab.store.root == root) {
                save_session(&tabs);
                return;
            }
            let id = tabs.new_id();
            match open_tab_at(&root, id, &mut tabs.clock) {
                Ok(tab) => tabs.stored.push(tab),
                Err(err) => {
                    warn!("failed to restore {}: {err:#}", root.display());
                    if !tabs.unavailable.iter().any(|pending| pending == &root) {
                        tabs.unavailable.push(root);
                    }
                }
            }
            save_session(&tabs);
            return;
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
            save_session(&tabs);
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
        let tab = tabs.stored.remove(index);
        if tab.state_dirty || tab.checkpoint_dirty {
            local::save_state(&tab.store.root, &tab.merge, &tab.files);
        }
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
    let device = tabs.clock.device_id().to_string();
    for tab in &tabs.stored {
        persist::save_on_exit(tab, &device);
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

pub fn open_tab_at(root: &Path, id: u64, clock: &mut Clock) -> anyhow::Result<OpenTab> {
    let store = ProjectStore {
        root: root.to_path_buf(),
    };
    store.ensure_dirs()?;

    let cached = local::load_state(root).unwrap_or_default();
    let first_open = cached.root.is_empty()
        && !oplog::has_journals(&store.journal_dir())
        && !oplog::has_checkpoints(&store.checkpoint_dir());
    let mut journal = Journal::create(&store.journal_dir(), clock.device_id())?;

    let mut merge = cached.merge;
    let mut files = cached.files;

    if let Err(err) = oplog::read_project(&store.root, &mut merge, &mut files) {
        warn!("initial project read failed: {err:#}");
    }

    // Only open the legacy database when this project has never been used with
    // journals, or when a partial first journal left the merged state empty.
    // Re-running migration is safe: legacy node ids are content-derived, so
    // duplicates collapse in the merge.
    if (first_open || merge.is_empty()) && store.db_path().is_file() {
        match db::load(&store.db_path()) {
            Ok(rows) if !rows.is_empty() => {
                let count = migrate_legacy(&mut journal, &rows, clock)?;
                info!("migrated {count} legacy node(s)");
                if let Err(err) = oplog::read_project(&store.root, &mut merge, &mut files) {
                    warn!("project read after migration failed: {err:#}");
                }
            }
            Ok(_) => {}
            Err(err) => debug!("legacy database not migrated: {err:#}"),
        }
    }

    if let Some(rev) = merge.max_seen() {
        clock.observe(rev);
    }

    let name = root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "project".to_string());
    let view = local::load_view(root).unwrap_or((DVec2::ZERO, 0.0));
    let tab = OpenTab {
        id,
        name,
        store,
        journal,
        merge,
        files,
        pending: Vec::new(),
        state: PersistenceState::default(),
        state_dirty: false,
        checkpoint_dirty: false,
        loaded: false,
        undo: UndoStack::default(),
        imports: ImportQueue::default(),
        view,
    };
    local::save_state(&tab.store.root, &tab.merge, &tab.files);
    Ok(tab)
}

fn migrate_legacy(
    journal: &mut Journal,
    rows: &[(i64, super::oplog::SavedNode)],
    clock: &mut Clock,
) -> anyhow::Result<usize> {
    if rows.is_empty() {
        return Ok(0);
    }
    let mut ops = Vec::with_capacity(rows.len());
    for (row, saved) in rows {
        ops.push(Op {
            id: db::legacy_id(*row, saved),
            rev: clock.tick(),
            deleted: false,
            node: Some(saved.clone()),
        });
    }
    journal.append(ops)?;
    Ok(rows.len())
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
    tab.state.saved.clear();
    persist::load_into_world(
        &tab.store,
        &tab.merge,
        tab.id,
        true,
        commands,
        images,
        zc,
        loader,
        &mut tab.state,
    )?;
    tab.loaded = true;
    tab.pending.clear();
    Ok(())
}

/// Persist the active tab's nodes + camera.
fn save_active(
    tabs: &mut Tabs,
    view: &View,
    q: &mut Query<(Entity, &Transform, &mut CanvasNode, &Sprite, &TabOwner)>,
) {
    if tabs.stored.is_empty() {
        return;
    }
    let idx = tabs.active.min(tabs.stored.len() - 1);
    let tabs = &mut *tabs;
    let clock = &mut tabs.clock;
    let tab = &mut tabs.stored[idx];
    if !tab.loaded {
        return;
    }
    tab.view = (view.center, view.log_scale);
    if persist::save_nodes(
        tab.id,
        &mut tab.journal,
        &mut tab.merge,
        &mut tab.state,
        clock,
        q,
    ) {
        tab.state_dirty = true;
        tab.checkpoint_dirty = true;
    }
    local::save_view(&tab.store.root, view.center, view.log_scale);
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

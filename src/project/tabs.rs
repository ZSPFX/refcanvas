//! projects as tabs, with session restore: restored tabs load on first activation,
//! visited tabs keep nodes, previews, background work and undo parked per tab.

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

use super::import::{self, ImportQueue};
use super::local;
use super::oplog::{self, Change, Clock, FileMark, Journal, MergeState};
use super::persist::{self, PersistenceState, PreviewLoader};
use super::store::ProjectStore;

#[derive(Resource)]
pub struct Tabs {
    pub(crate) stored: Vec<OpenTab>,
    pub(crate) active: usize,
    next_id: u64,
    /// remembered roots not available yet (unmounted drive, offline share);
    /// retried in the background and kept in `session.json`.
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
    pub journal: Option<Journal>,
    pub merge: MergeState,
    pub files: HashMap<String, FileMark>,
    /// remote changes waiting for the pointer to go idle.
    pub pending: Vec<Change>,
    pub state: PersistenceState,
    pub state_dirty: bool,
    pub checkpoint_dirty: bool,
    pub opened: bool,
    pub loaded: bool,
    undo: UndoStack,
    imports: ImportQueue,
    /// camera center and log scale.
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

    /// (id, name) per tab, in tab-bar order.
    pub fn summaries(&self) -> Vec<(u64, String)> {
        self.stored.iter().map(|t| (t.id, t.name.clone())).collect()
    }

    pub fn root_of(&self, id: u64) -> Option<PathBuf> {
        self.stored
            .iter()
            .find(|t| t.id == id)
            .map(|t| t.store.root.clone())
    }

    pub fn move_tab(&mut self, from: usize, to: usize) {
        if from >= self.stored.len() || to >= self.stored.len() || from == to {
            return;
        }
        let active = self.active_id();
        let tab = self.stored.remove(from);
        self.stored.insert(to, tab);
        if let Some(id) = active
            && let Some(index) = self.index_of(id)
        {
            self.active = index;
        }
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
    Open(PathBuf),
    /// open-new always activates, even when the folder is already a tab, so the
    /// picker answers visibly.
    OpenNew(PathBuf),
    /// re-open a remembered project without changing the active tab.
    OpenRestored(PathBuf),
    Close(u64),
    Switch(u64),
    Cycle(i32),
    SaveNow,
}

// session: which tabs were open

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

// systems

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
        tabs.stored.push(placeholder_tab(root, id));
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
        let Tabs { stored, clock, .. } = &mut *tabs;
        let tab = &mut stored[index];
        if let Err(err) = open_tab(tab, clock) {
            warn!("failed to open {}: {err:#}", tab.store.root.display());
            continue;
        }
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

/// tab shortcuts: Ctrl+T/O/W/Tab/S.
pub fn tab_shortcuts(
    keys: Res<ButtonInput<KeyCode>>,
    mut contexts: EguiContexts,
    tabs: Res<Tabs>,
    mut requests: ResMut<TabRequests>,
    mut dialogs: NonSendMut<crate::dialogs::Dialogs>,
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
    if !crate::canvas::command_down(&keys, &chord_mem, chord_frame.0) {
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
    // apply deferred spawns before another switch or save.
    let request = requests.queue.remove(0);
    let mut closing = None;
    let next = match request {
        TabRequest::Open(root) => {
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
        TabRequest::OpenNew(root) => {
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
        TabRequest::OpenRestored(root) => {
            if tabs.stored.iter().any(|tab| tab.store.root == root) {
                save_session(&tabs);
                return;
            }
            let id = tabs.new_id();
            tabs.stored.push(placeholder_tab(&root, id));
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
        if !tabs.stored[next].opened {
            let Tabs { stored, clock, .. } = &mut *tabs;
            if let Err(err) = open_tab(&mut stored[next], clock) {
                warn!("failed to open {}: {err:#}", stored[next].store.root.display());
                return;
            }
        }
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
        if let Some(journal) = &tab.journal {
            journal.remove_if_empty();
        }
        if tab.opened && (tab.state_dirty || tab.checkpoint_dirty) {
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
        if !tab.opened {
            continue;
        }
        persist::save_on_exit(tab, &device);
        if let Some(journal) = &tab.journal {
            journal.remove_if_empty();
        }
    }
}

// helpers

fn tab_name(root: &Path) -> String {
    root.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "project".to_string())
}

fn placeholder_tab(root: &Path, id: u64) -> OpenTab {
    OpenTab {
        id,
        name: tab_name(root),
        store: ProjectStore {
            root: root.to_path_buf(),
        },
        journal: None,
        merge: MergeState::default(),
        files: HashMap::new(),
        pending: Vec::new(),
        state: PersistenceState::default(),
        state_dirty: false,
        checkpoint_dirty: false,
        opened: false,
        loaded: false,
        undo: UndoStack::default(),
        imports: ImportQueue::default(),
        view: (DVec2::ZERO, 0.0),
    }
}

fn open_tab(tab: &mut OpenTab, clock: &mut Clock) -> anyhow::Result<()> {
    if tab.opened {
        return Ok(());
    }
    let store = &tab.store;
    store.ensure_dirs()?;

    let cached = local::load_state(&store.root).unwrap_or_default();
    let journal = Journal::create(&store.journal_dir(), clock.device_id())?;
    oplog::prune_empty_journals(&store.journal_dir(), journal.path());

    let mut merge = cached.merge;
    let mut files = cached.files;

    let read_ok = match oplog::read_project(&store.root, &mut merge, &mut files) {
        Ok(_) => true,
        Err(err) => {
            warn!("initial project read failed: {err:#}");
            false
        }
    };
    if read_ok {
        let removed = import::sweep_orphaned_media(store, &merge, import::MEDIA_RETENTION);
        if removed > 0 {
            info!("removed {removed} orphaned media file(s)");
        }
    }

    if let Some(rev) = merge.max_seen() {
        clock.observe(rev);
    }

    tab.journal = Some(journal);
    tab.merge = merge;
    tab.files = files;
    tab.view = local::load_view(&store.root).unwrap_or((DVec2::ZERO, 0.0));
    tab.opened = true;
    local::save_state(&store.root, &tab.merge, &tab.files);
    Ok(())
}

pub fn open_tab_at(root: &Path, id: u64, clock: &mut Clock) -> anyhow::Result<OpenTab> {
    let mut tab = placeholder_tab(root, id);
    open_tab(&mut tab, clock)?;
    Ok(tab)
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
    if !tab.opened {
        anyhow::bail!("tab not opened");
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
    let Some(journal) = &mut tab.journal else {
        return;
    };
    tab.view = (view.center, view.log_scale);
    if persist::save_nodes(
        tab.id,
        journal,
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

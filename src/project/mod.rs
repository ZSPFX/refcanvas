//! Project data: on-disk store, SQLite persistence, tabs, import pipeline.

pub mod db;
pub mod import;
pub mod persist;
pub mod store;
pub mod tabs;

use bevy::prelude::*;

use crate::canvas::{CanvasLateSet, CanvasSet};

pub struct ProjectPlugin;

impl Plugin for ProjectPlugin {
    fn build(&self, app: &mut App) {
        configure_tab_switching(app);
        app.init_resource::<store::ProjectStore>()
            .init_resource::<import::ImportQueue>()
            .init_resource::<persist::PreviewLoader>()
            .init_resource::<tabs::Tabs>()
            .init_resource::<tabs::TabRequests>()
            .add_systems(
                Startup,
                (setup_project, tabs::startup_tabs, import::startup_import).chain(),
            )
            .add_systems(
                Update,
                tabs::tab_shortcuts.before(tabs::process_requests),
            )
            .add_systems(
                Update,
                (
                    import::handle_dropped_files,
                    import::dialog_import,
                    import::pump_import_queue,
                    import::collect_imports,
                    persist::pump_preview_loads,
                    persist::autosave,
                )
                    .chain()
                    .after(CanvasLateSet),
            )
            .add_systems(Last, tabs::on_exit);
    }
}

fn configure_tab_switching(app: &mut App) {
    // Restore the view before camera, LOD, and overlays use it this frame.
    app.add_systems(
        Update,
        (
            tabs::process_requests,
            crate::canvas::lod::resume_active_meshes,
            crate::canvas::lod::suspend_inactive_textures,
        )
            .chain()
            .before(CanvasSet),
    );
}

fn setup_project(store: Res<store::ProjectStore>) {
    match store.ensure_dirs() {
        Ok(()) => info!("default project root: {}", store.root.display()),
        Err(err) => warn!("failed to create project dirs at {:?}: {err}", store.root),
    }
}

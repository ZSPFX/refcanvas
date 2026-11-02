//! Where a project's data lives on disk.
//!
//! ```text
//! <root>/
//!   journal/       append-only canvas operations (shared, one writer per file)
//!   checkpoints/   immutable full-state snapshots (shared, one writer per file)
//!   project.db     legacy single-file database, read once and then left alone
//!   images/        originals, copied on import (content-hash dedup)
//!   cache/         generated previews (regenerated on demand)
//! ```

use bevy::prelude::*;
use std::path::PathBuf;

#[derive(Resource, Clone)]
pub struct ProjectStore {
    pub root: PathBuf,
}

impl Default for ProjectStore {
    fn default() -> Self {
        let base = directories::BaseDirs::new()
            .map(|b| b.data_dir().join("refcanvas"))
            .unwrap_or_else(std::env::temp_dir);
        Self {
            root: base.join("scratch-project"),
        }
    }
}

impl ProjectStore {
    pub fn images_dir(&self) -> PathBuf {
        self.root.join("images")
    }

    pub fn cache_dir(&self) -> PathBuf {
        self.root.join("cache")
    }

    pub fn journal_dir(&self) -> PathBuf {
        self.root.join("journal")
    }

    pub fn checkpoint_dir(&self) -> PathBuf {
        self.root.join("checkpoints")
    }

    /// Legacy single-file database. Only read during migration.
    pub fn db_path(&self) -> PathBuf {
        self.root.join("project.db")
    }

    pub fn ensure_dirs(&self) -> anyhow::Result<()> {
        std::fs::create_dir_all(self.images_dir())?;
        std::fs::create_dir_all(self.cache_dir())?;
        std::fs::create_dir_all(self.journal_dir())?;
        std::fs::create_dir_all(self.checkpoint_dir())?;
        Ok(())
    }
}

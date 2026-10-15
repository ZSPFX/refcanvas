//! SQLite persistence for one project.
//!
//! Deliberately syncthing-friendly: rollback journal (SQLite default), short
//! transactions, no long-lived sidecar files while idle.

use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};

#[derive(Clone, Debug, PartialEq)]
pub struct SavedNode {
    pub kind: String,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub rot: f32,
    pub z: f32,
    pub src: Option<String>,
    pub orig_name: Option<String>,
    pub hash: Option<String>,
    pub img_w: Option<i64>,
    pub img_h: Option<i64>,
    pub flip_x: bool,
    pub flip_y: bool,
    pub label: Option<String>,
    /// Image crop (natural texel coords) + display scale.
    pub crop_x: Option<f32>,
    pub crop_y: Option<f32>,
    pub crop_w: Option<f32>,
    pub crop_h: Option<f32>,
    pub px_scale: Option<f32>,
}

pub struct LoadedNode {
    pub id: i64,
    pub saved: SavedNode,
}

pub struct LoadedProject {
    pub nodes: Vec<LoadedNode>,
    /// Camera (center x, center y, log scale).
    pub view: Option<(f64, f64, f64)>,
}

/// A batch of node writes applied in a single transaction.
///
/// Batching is a latency requirement, not a style choice: with SQLite's default
/// rollback journal every committed statement fsyncs, so writing a 100+ node
/// selection row-by-row made *every* autosave freeze the app for ~0.5 s while
/// dragging (measured: 198 rows = 651 ms individually, 9 ms in one transaction).
pub struct NodeBatch<'a> {
    pub inserts: Vec<&'a SavedNode>,
    pub updates: Vec<(i64, &'a SavedNode)>,
    pub deletes: Vec<i64>,
    /// Camera state to persist alongside (`None` = unchanged).
    pub view: Option<(f64, f64, f64)>,
}

const INSERT_NODE_SQL: &str = "INSERT INTO nodes (kind, x, y, w, h, rot, z, src, orig_name, hash, img_w, img_h, flip_x, flip_y, label,
                            crop_x, crop_y, crop_w, crop_h, px_scale)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20)";

const UPDATE_NODE_SQL: &str = "UPDATE nodes SET x=?2, y=?3, w=?4, h=?5, rot=?6, z=?7, flip_x=?8, flip_y=?9, label=?10,
                          crop_x=?11, crop_y=?12, crop_w=?13, crop_h=?14, px_scale=?15
         WHERE id=?1";

pub struct ProjectDb {
    conn: Mutex<Connection>,
}

impl ProjectDb {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path).with_context(|| format!("opening {path:?}"))?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS meta (
                 key TEXT PRIMARY KEY,
                 value TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS nodes (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 kind TEXT NOT NULL DEFAULT 'image',
                 x REAL NOT NULL,
                 y REAL NOT NULL,
                 w REAL NOT NULL,
                 h REAL NOT NULL,
                 rot REAL NOT NULL DEFAULT 0,
                 z REAL NOT NULL DEFAULT 0,
                 src TEXT,
                 orig_name TEXT,
                 hash TEXT,
                 img_w INTEGER,
                 img_h INTEGER,
                 flip_x INTEGER NOT NULL DEFAULT 0,
                 flip_y INTEGER NOT NULL DEFAULT 0,
                 crop_x REAL,
                 crop_y REAL,
                 crop_w REAL,
                 crop_h REAL,
                 px_scale REAL,
                 parent_id INTEGER,
                 label TEXT,
                 color TEXT
             );",
        )?;

        // Migrate older projects: add columns that were introduced later.
        let existing: Vec<String> = {
            let mut stmt = conn.prepare("PRAGMA table_info(nodes)")?;
            let names = stmt
                .query_map([], |row| row.get::<_, String>(1))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            names
        };
        for (name, decl) in [
            ("crop_x", "REAL"),
            ("crop_y", "REAL"),
            ("crop_w", "REAL"),
            ("crop_h", "REAL"),
            ("px_scale", "REAL"),
            ("label", "TEXT"),
        ] {
            if !existing.iter().any(|n| n == name) {
                conn.execute(&format!("ALTER TABLE nodes ADD COLUMN {name} {decl}"), [])?;
            }
        }

        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub fn load(&self) -> Result<LoadedProject> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, kind, x, y, w, h, rot, z, src, orig_name, hash, img_w, img_h, flip_x, flip_y, label,
                    crop_x, crop_y, crop_w, crop_h, px_scale
             FROM nodes ORDER BY z ASC, id ASC",
        )?;
        let nodes = stmt
            .query_map([], |row| {
                Ok(LoadedNode {
                    id: row.get(0)?,
                    saved: SavedNode {
                        kind: row.get(1)?,
                        x: row.get(2)?,
                        y: row.get(3)?,
                        w: row.get(4)?,
                        h: row.get(5)?,
                        rot: row.get(6)?,
                        z: row.get(7)?,
                        src: row.get(8)?,
                        orig_name: row.get(9)?,
                        hash: row.get(10)?,
                        img_w: row.get(11)?,
                        img_h: row.get(12)?,
                        flip_x: row.get::<_, i64>(13)? != 0,
                        flip_y: row.get::<_, i64>(14)? != 0,
                        label: row.get(15)?,
                        crop_x: row.get(16)?,
                        crop_y: row.get(17)?,
                        crop_w: row.get(18)?,
                        crop_h: row.get(19)?,
                        px_scale: row.get(20)?,
                    },
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(stmt);

        let get = |key: &str| -> Result<Option<String>> {
            Ok(conn
                .query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| {
                    r.get::<_, String>(0)
                })
                .optional()?)
        };
        let view = match (get("view_x")?, get("view_y")?, get("view_log_scale")?) {
            (Some(x), Some(y), Some(ls)) => {
                match (x.parse::<f64>(), y.parse::<f64>(), ls.parse::<f64>()) {
                    (Ok(x), Ok(y), Ok(ls)) => Some((x, y, ls)),
                    _ => None,
                }
            }
            _ => None,
        };

        Ok(LoadedProject { nodes, view })
    }

    /// Apply a batch atomically (one commit, one fsync) and return the new row
    /// ids for `inserts`, in order.
    pub fn apply_batch(&self, batch: &NodeBatch<'_>) -> Result<Vec<i64>> {
        let conn = self.conn.lock().unwrap();
        let tx = conn.unchecked_transaction()?;
        let mut ids = Vec::with_capacity(batch.inserts.len());
        for n in &batch.inserts {
            tx.execute(
                INSERT_NODE_SQL,
                params![
                    n.kind,
                    n.x,
                    n.y,
                    n.w,
                    n.h,
                    n.rot,
                    n.z,
                    n.src,
                    n.orig_name,
                    n.hash,
                    n.img_w,
                    n.img_h,
                    n.flip_x as i64,
                    n.flip_y as i64,
                    n.label,
                    n.crop_x,
                    n.crop_y,
                    n.crop_w,
                    n.crop_h,
                    n.px_scale
                ],
            )?;
            ids.push(tx.last_insert_rowid());
        }
        for (id, n) in &batch.updates {
            tx.execute(
                UPDATE_NODE_SQL,
                params![
                    id,
                    n.x,
                    n.y,
                    n.w,
                    n.h,
                    n.rot,
                    n.z,
                    n.flip_x as i64,
                    n.flip_y as i64,
                    n.label,
                    n.crop_x,
                    n.crop_y,
                    n.crop_w,
                    n.crop_h,
                    n.px_scale
                ],
            )?;
        }
        for id in &batch.deletes {
            tx.execute("DELETE FROM nodes WHERE id = ?1", [id])?;
        }
        if let Some((x, y, ls)) = batch.view {
            for (key, value) in [
                ("view_x", x.to_string()),
                ("view_y", y.to_string()),
                ("view_log_scale", ls.to_string()),
                ("schema_version", "1".to_string()),
            ] {
                tx.execute(
                    "INSERT INTO meta (key, value) VALUES (?1, ?2)
                     ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                    params![key, value],
                )?;
            }
        }
        tx.commit()?;
        Ok(ids)
    }

    #[allow(dead_code)]
    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }
}

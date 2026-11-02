//! One-way migration from the old single-file project database.
//!
//! The file is read once when a project has no operation journal yet. It is
//! never written again: the journal is the shared source of truth from then on.
//! Legacy rows receive deterministic node ids derived from their row id and
//! content, so two machines migrating the same file converge instead of
//! creating duplicate nodes.

use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::Connection;
use uuid::Uuid;

use super::oplog::{NodeId, SavedNode};

pub fn load(path: &Path) -> Result<Vec<(i64, SavedNode)>> {
    let conn = Connection::open(path).with_context(|| format!("opening {path:?}"))?;
    ensure_columns(&conn)?;
    let mut stmt = conn.prepare(
        "SELECT id, kind, x, y, w, h, rot, z, src, orig_name, hash, img_w, img_h, flip_x, flip_y, label,
                crop_x, crop_y, crop_w, crop_h, px_scale
         FROM nodes ORDER BY id ASC",
    )?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                SavedNode {
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
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Projects from older builds may predate some columns. Add them so the shared
/// SELECT above works regardless of which version wrote the file.
fn ensure_columns(conn: &Connection) -> Result<()> {
    let mut stmt = conn.prepare("PRAGMA table_info(nodes)")?;
    let existing: Vec<String> = stmt
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
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
    Ok(())
}

/// Stable id for a migrated legacy row.
pub fn legacy_id(row: i64, saved: &SavedNode) -> NodeId {
    let payload = serde_json::to_vec(saved).unwrap_or_default();
    let mut key = format!("refcanvas-legacy\0{row}\0").into_bytes();
    key.extend_from_slice(&payload);
    Uuid::new_v5(&Uuid::NAMESPACE_OID, &key)
}

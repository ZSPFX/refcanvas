//! Shared-project operation log.
//!
//! A project folder may be replicated between machines by an external folder
//! sync tool. The only shared state this app writes is an append-only journal
//! per session, plus immutable full-state checkpoints. Each file has exactly
//! one writer, so replication can deliver files in any order and at any time
//! without creating a merge target that two machines write.
//!
//! Every row change is a last-writer-wins register keyed by a stable node id
//! and ordered by a hybrid logical clock. Merging is commutative, associative
//! and idempotent: replaying a file or receiving it late never changes the
//! result, and merged state is never written back into the shared folder.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use bevy::log::warn;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub type NodeId = Uuid;

// ---------------------------------------------------------------------------
// Clock
// ---------------------------------------------------------------------------

/// One operation revision: wall-clock milliseconds, a counter for equal
/// milliseconds, the originating device and a per-op nonce.
///
/// Ordering is derived field-by-field, so every machine resolves the same
/// winner even when two devices edit the same node concurrently.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Rev {
    pub w: u64,
    #[serde(default)]
    pub c: u32,
    pub d: String,
    #[serde(default)]
    pub n: u32,
}

impl Rev {
    pub fn device(&self) -> &str {
        &self.d
    }
}

static NONCE: AtomicU64 = AtomicU64::new(0);

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn nonce() -> u32 {
    let tick = NONCE.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut bytes = Vec::with_capacity(24);
    bytes.extend_from_slice(&tick.to_le_bytes());
    bytes.extend_from_slice(&nanos.to_le_bytes());
    bytes.extend_from_slice(&std::process::id().to_le_bytes());
    let hash = blake3::hash(&bytes);
    u32::from_le_bytes(hash.as_bytes()[..4].try_into().unwrap())
}

/// Hybrid logical clock. Ticks are monotonic even if the system clock jumps
/// backwards, and observing remote revisions keeps new local edits ordered
/// after everything this machine has already merged.
#[derive(Clone, Debug)]
pub struct Clock {
    device: String,
    last_w: u64,
    last_c: u32,
}

impl Clock {
    pub fn new(device: String, last_w: u64, last_c: u32) -> Self {
        Self {
            device,
            last_w,
            last_c,
        }
    }

    pub fn device_id(&self) -> &str {
        &self.device
    }

    pub fn last_w(&self) -> u64 {
        self.last_w
    }

    pub fn last_c(&self) -> u32 {
        self.last_c
    }

    pub fn tick(&mut self) -> Rev {
        let now = now_ms();
        if now > self.last_w {
            self.last_w = now;
            self.last_c = 0;
        } else {
            self.last_c = self.last_c.saturating_add(1);
        }
        Rev {
            w: self.last_w,
            c: self.last_c,
            d: self.device.clone(),
            n: nonce(),
        }
    }

    pub fn observe(&mut self, rev: &Rev) {
        if (rev.w, rev.c) > (self.last_w, self.last_c) {
            self.last_w = rev.w;
            self.last_c = rev.c;
        }
    }
}

// ---------------------------------------------------------------------------
// Stored node state
// ---------------------------------------------------------------------------

/// The complete persisted state of one canvas node. This is the payload of an
/// upsert op and of a checkpoint row.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Default)]
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
    pub crop_x: Option<f32>,
    pub crop_y: Option<f32>,
    pub crop_w: Option<f32>,
    pub crop_h: Option<f32>,
    pub px_scale: Option<f32>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VersionedNode {
    pub rev: Rev,
    #[serde(default)]
    pub deleted: bool,
    #[serde(default)]
    pub node: Option<SavedNode>,
}

/// A change that must be applied to the live scene.
#[derive(Clone, Debug)]
pub enum Change {
    Upsert(NodeId),
    Remove(NodeId),
}

// ---------------------------------------------------------------------------
// Journal wire format
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Op {
    pub id: NodeId,
    pub rev: Rev,
    #[serde(default)]
    pub deleted: bool,
    #[serde(default)]
    pub node: Option<SavedNode>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Batch {
    #[serde(default = "wire_version")]
    pub v: u32,
    pub device: String,
    pub session: String,
    pub ops: Vec<Op>,
}

fn wire_version() -> u32 {
    1
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Checkpoint {
    #[serde(default = "wire_version")]
    v: u32,
    device: String,
    created_ms: u64,
    #[serde(default)]
    seen: HashMap<String, Rev>,
    #[serde(default)]
    nodes: HashMap<NodeId, VersionedNode>,
}

// ---------------------------------------------------------------------------
// Merge state
// ---------------------------------------------------------------------------

/// Merged view of every journal and checkpoint seen so far, plus the highest
/// revision merged per device. Serializable so it can be cached locally.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct MergeState {
    #[serde(default)]
    pub nodes: HashMap<NodeId, VersionedNode>,
    #[serde(default)]
    pub seen: HashMap<String, Rev>,
}

impl MergeState {
    pub fn merge(&mut self, id: NodeId, rev: Rev, deleted: bool, node: Option<SavedNode>) -> bool {
        if !self.wins(id, &rev) {
            return false;
        }
        self.seen_rev(&rev);
        self.nodes.insert(id, VersionedNode { rev, deleted, node });
        true
    }

    fn merge_versioned(&mut self, id: NodeId, value: VersionedNode) -> bool {
        if !self.wins(id, &value.rev) {
            return false;
        }
        self.seen_rev(&value.rev);
        self.nodes.insert(id, value);
        true
    }

    fn wins(&self, id: NodeId, rev: &Rev) -> bool {
        match self.nodes.get(&id) {
            Some(current) => rev > &current.rev,
            None => true,
        }
    }

    /// Record that this revision has been inspected, even when it lost to a
    /// newer value. This keeps checkpoint watermarks accurate and lets later
    /// passes skip already-seen journal rows.
    pub fn observe(&mut self, rev: &Rev) {
        self.seen_rev(rev);
    }

    fn seen_rev(&mut self, rev: &Rev) {
        match self.seen.get(rev.device()) {
            Some(current) if current >= rev => {}
            _ => {
                self.seen.insert(rev.device().to_string(), rev.clone());
            }
        }
    }

    pub fn covered(&self, rev: &Rev) -> bool {
        self.seen
            .get(rev.device())
            .is_some_and(|current| rev <= current)
    }

    pub fn max_seen(&self) -> Option<&Rev> {
        self.seen.values().max()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Visible nodes in a stable order (z, then id) for scene loading.
    pub fn visible_nodes(&self) -> Vec<(NodeId, &SavedNode)> {
        let mut out: Vec<(NodeId, &SavedNode)> = self
            .nodes
            .iter()
            .filter_map(|(id, value)| {
                if value.deleted {
                    None
                } else {
                    value.node.as_ref().map(|node| (*id, node))
                }
            })
            .collect();
        out.sort_by(|(a_id, a), (b_id, b)| {
            a.z.partial_cmp(&b.z)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a_id.cmp(b_id))
        });
        out
    }
}

// ---------------------------------------------------------------------------
// Journal writer
// ---------------------------------------------------------------------------

pub struct Journal {
    path: PathBuf,
    device: String,
    session: String,
}

impl Journal {
    /// Create this session's (empty) journal and close it immediately. Keeping
    /// the name visible from startup lets peers discover the session without
    /// waiting for a directory-cache refresh, while no handle is held between
    /// batches.
    pub fn create(dir: &Path, device: &str) -> Result<Self> {
        std::fs::create_dir_all(dir)?;
        let safe: String = device
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
            .collect();
        let session = Uuid::now_v7().simple().to_string();
        let path = dir.join(format!("{safe}-{session}.jsonl"));
        let file = OpenOptions::new()
            .create_new(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("creating journal {path:?}"))?;
        drop(file);
        Ok(Self {
            path,
            device: safe,
            session,
        })
    }

    #[allow(dead_code)]
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn append(&mut self, ops: Vec<Op>) -> Result<bool> {
        if ops.is_empty() {
            return Ok(false);
        }
        let batch = Batch {
            v: wire_version(),
            device: self.device.clone(),
            session: self.session.clone(),
            ops,
        };
        let mut line = serde_json::to_vec(&batch)?;
        line.push(b'\n');
        // Open/write/close for every batch. A long-lived handle on a network
        // share can hold a lease that keeps metadata and the appended data
        // invisible to other clients until the process exits.
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .with_context(|| format!("opening journal {:?}", self.path))?;
        file.write_all(&line)?;
        file.sync_data()?;
        drop(file);
        Ok(true)
    }
}

/// True when the folder already contains any journal (including conflict
/// copies), i.e. it has been opened by an op-log aware build before.
pub fn has_journals(dir: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    entries.filter_map(|e| e.ok()).any(|e| {
        let name = e.file_name();
        let name = name.to_string_lossy();
        name.contains(".jsonl") && e.metadata().map(|meta| meta.len() > 0).unwrap_or(false)
    })
}

/// True when this folder already has a full-state checkpoint. Used to avoid
/// re-importing a legacy database after journals have been cleaned up.
pub fn has_checkpoints(dir: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    entries.filter_map(|e| e.ok()).any(|e| {
        let name = e.file_name();
        let name = name.to_string_lossy();
        name.contains(".json") && e.metadata().map(|meta| meta.len() > 0).unwrap_or(false)
    })
}

// ---------------------------------------------------------------------------
// Incremental reader
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileMark {
    /// Offset just past the last complete line (journals) or the file length
    /// (checkpoints).
    pub len: u64,
    pub mtime_ns: u128,
    /// Hash of the first block, used to notice a same-length replacement.
    #[serde(default)]
    pub head: u64,
}

type Marks = HashMap<String, FileMark>;

fn mtime_ns(meta: &std::fs::Metadata) -> u128 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

fn head_hash(path: &Path) -> std::io::Result<u64> {
    let mut file = File::open(path)?;
    let mut buf = [0u8; 256];
    let n = file.read(&mut buf)?;
    let hash = blake3::hash(&buf[..n]);
    Ok(u64::from_le_bytes(hash.as_bytes()[..8].try_into().unwrap()))
}

/// Read new ops from every journal (including conflict copies) and merge them.
pub fn read_project(root: &Path, state: &mut MergeState, marks: &mut Marks) -> Result<Vec<Change>> {
    let mut changes = Vec::new();
    read_checkpoints(&root.join("checkpoints"), state, marks, &mut changes)?;
    read_journals(&root.join("journal"), state, marks, &mut changes)?;
    Ok(changes)
}

fn read_journals(
    dir: &Path,
    state: &mut MergeState,
    marks: &mut Marks,
    changes: &mut Vec<Change>,
) -> Result<()> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Ok(());
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_file())
        .collect();
    paths.sort();

    for path in paths {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if !name.contains(".jsonl") {
            continue;
        }
        let key = format!("journal/{name}");
        let mark = marks.get(&key).copied();
        // Open every poll instead of trusting cached size/mtime. Network
        // filesystems can report stale metadata for a file another client is
        // writing; the open/fstat/read path is what forces revalidation.
        let mut file = match File::open(&path) {
            Ok(file) => file,
            Err(err) => {
                warn!("journal open failed ({}): {err}", path.display());
                continue;
            }
        };
        let Ok(meta) = file.metadata() else {
            continue;
        };
        let len = meta.len();
        let mtime = mtime_ns(&meta);
        let start = match mark {
            Some(m) if len >= m.len => m.len,
            _ => 0,
        };
        if start > 0 {
            let _ = file.seek(SeekFrom::Start(start));
        }
        // Stream line by line: a long editing session can produce a large
        // journal, and only the new tail is interesting after the first read.
        let mut reader = BufReader::new(file);
        let mut consumed = 0u64;
        loop {
            let mut line = Vec::new();
            let read = match reader.read_until(b'\n', &mut line) {
                Ok(read) => read,
                Err(err) => {
                    warn!("journal read failed ({}): {err}", path.display());
                    break;
                }
            };
            if read == 0 {
                break;
            }
            if line.last() != Some(&b'\n') {
                // Incomplete tail: leave it unconsumed so a later complete
                // version of the file is read from the right offset.
                break;
            }
            consumed += read as u64;
            line.pop();
            if line.is_empty() {
                continue;
            }
            match serde_json::from_slice::<Batch>(&line) {
                Ok(batch) => merge_batch(state, batch, changes),
                Err(err) => warn!("journal line skipped ({}): {err}", path.display()),
            }
        }
        marks.insert(
            key,
            FileMark {
                len: start + consumed,
                mtime_ns: mtime,
                head: 0,
            },
        );
    }

    marks.retain(|key, _| {
        key.starts_with("checkpoints/") || {
            let name = key.trim_start_matches("journal/");
            dir.join(name).is_file()
        }
    });
    Ok(())
}

fn merge_batch(state: &mut MergeState, batch: Batch, changes: &mut Vec<Change>) {
    for op in batch.ops {
        if state.covered(&op.rev) {
            continue;
        }
        state.observe(&op.rev);
        if state.merge(op.id, op.rev.clone(), op.deleted, op.node.clone()) {
            changes.push(if op.deleted {
                Change::Remove(op.id)
            } else {
                Change::Upsert(op.id)
            });
        }
    }
}

fn read_checkpoints(
    dir: &Path,
    state: &mut MergeState,
    marks: &mut Marks,
    changes: &mut Vec<Change>,
) -> Result<()> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Ok(());
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_file())
        .collect();
    paths.sort();

    for path in paths {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if !name.contains(".json") {
            continue;
        }
        let key = format!("checkpoints/{name}");
        let Ok(meta) = std::fs::metadata(&path) else {
            marks.remove(&key);
            continue;
        };
        let len = meta.len();
        let mtime = mtime_ns(&meta);
        if marks
            .get(&key)
            .is_some_and(|m| m.len == len && m.mtime_ns == mtime)
        {
            continue;
        }
        let head = head_hash(&path).unwrap_or(0);
        let parsed = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Checkpoint>(&bytes).ok());
        match parsed {
            Some(checkpoint) => {
                for (device, rev) in checkpoint.seen {
                    match state.seen.get(&device) {
                        Some(current) if current >= &rev => {}
                        _ => {
                            state.seen.insert(device, rev);
                        }
                    }
                }
                let mut rows: Vec<(NodeId, VersionedNode)> = checkpoint.nodes.into_iter().collect();
                rows.sort_by_key(|(id, _)| *id);
                for (id, value) in rows {
                    let deleted = value.deleted;
                    if state.merge_versioned(id, value) {
                        changes.push(if deleted {
                            Change::Remove(id)
                        } else {
                            Change::Upsert(id)
                        });
                    }
                }
            }
            None => warn!("checkpoint unreadable ({}), will retry", path.display()),
        }
        marks.insert(
            key,
            FileMark {
                len,
                mtime_ns: mtime,
                head,
            },
        );
    }

    marks.retain(|key, _| {
        key.starts_with("journal/") || {
            let name = key.trim_start_matches("checkpoints/");
            dir.join(name).is_file()
        }
    });
    Ok(())
}

// ---------------------------------------------------------------------------
// Checkpoint writer
// ---------------------------------------------------------------------------

/// Write a full-state checkpoint owned by this device. Immutable once written,
/// so replication never needs to merge two versions of the same file.
pub fn write_checkpoint(dir: &Path, device: &str, state: &MergeState) -> Result<Option<PathBuf>> {
    if state.is_empty() {
        return Ok(None);
    }
    std::fs::create_dir_all(dir)?;
    let safe: String = device
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    let checkpoint = Checkpoint {
        v: wire_version(),
        device: safe.clone(),
        created_ms: now_ms(),
        seen: state.seen.clone(),
        nodes: state.nodes.clone(),
    };
    let bytes = serde_json::to_vec(&checkpoint)?;
    let name = format!("{safe}-{:013}-{:04x}.json", now_ms(), nonce() & 0xFFFF);
    let path = dir.join(&name);
    let tmp = dir.join(format!(".{name}.tmp"));
    std::fs::write(&tmp, &bytes)?;
    std::fs::rename(&tmp, &path)?;
    prune_own_checkpoints(dir, &safe, 3);
    Ok(Some(path))
}

fn prune_own_checkpoints(dir: &Path, device: &str, keep: usize) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let prefix = format!("{device}-");
    let mut own: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|path| {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            name.starts_with(&prefix)
                && name.ends_with(".json")
                && !name.contains(".sync-conflict-")
        })
        .collect();
    own.sort();
    while own.len() > keep {
        let path = own.remove(0);
        let _ = std::fs::remove_file(path);
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn node(x: f32) -> SavedNode {
        SavedNode {
            kind: "image".to_string(),
            x,
            w: 10.0,
            h: 10.0,
            ..Default::default()
        }
    }

    fn clock(device: &str) -> Clock {
        Clock::new(device.to_string(), 0, 0)
    }

    #[test]
    fn merge_is_order_independent_and_idempotent() {
        let id = Uuid::now_v7();
        let mut a = clock("a");
        let mut b = clock("b");
        let ra = a.tick();
        let rb = b.tick();
        let node_a = node(1.0);
        let node_b = node(2.0);

        let mut left = MergeState::default();
        left.merge(id, ra.clone(), false, Some(node_a.clone()));
        left.merge(id, rb.clone(), false, Some(node_b.clone()));
        let mut right = MergeState::default();
        right.merge(id, rb, false, Some(node_b));
        right.merge(id, ra, false, Some(node_a));
        let third = clock("c").tick();
        left.merge(id, third.clone(), false, Some(node(9.0)));
        right.merge(id, third, false, Some(node(9.0)));

        assert_eq!(left.nodes[&id].rev, right.nodes[&id].rev);
        let winner = left.nodes[&id].node.clone().unwrap();
        // Replaying the winning op changes nothing.
        let rev = left.nodes[&id].rev.clone();
        assert!(!left.merge(id, rev, false, Some(winner.clone())));
        assert_eq!(left.nodes[&id].node.clone().unwrap(), winner);
    }

    #[test]
    fn tombstone_wins_only_when_newer() {
        let id = Uuid::now_v7();
        let mut a = clock("a");
        let move_rev = a.tick();
        let delete_rev = a.tick();
        let mut state = MergeState::default();
        assert!(state.merge(id, move_rev.clone(), false, Some(node(3.0))));
        assert!(state.merge(id, delete_rev.clone(), true, None));
        assert!(state.nodes[&id].deleted);
        assert!(!state.merge(id, move_rev, false, Some(node(4.0))));
        assert!(state.nodes[&id].deleted);
        assert!(state.visible_nodes().is_empty());
    }

    #[test]
    fn merge_state_round_trips_through_json() {
        let mut state = MergeState::default();
        let id = Uuid::now_v7();
        let mut c = clock("dev");
        state.merge(id, c.tick(), false, Some(node(5.0)));
        let bytes = serde_json::to_vec(&state).unwrap();
        let restored: MergeState = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(restored.nodes[&id].node.as_ref().unwrap().x, 5.0);
        assert_eq!(restored.seen.len(), 1);
    }

    #[test]
    fn conflict_copies_are_merged_and_left_in_place() {
        let root = std::env::temp_dir().join(format!(
            "refcanvas-conflict-{}-{}",
            std::process::id(),
            NONCE.fetch_add(1, Ordering::Relaxed)
        ));
        let dir = root.join("journal");
        std::fs::create_dir_all(&dir).unwrap();
        let id = Uuid::now_v7();
        let mut c = clock("dev");

        let mut first = Journal::create(&dir, "dev").unwrap();
        let canonical = first.path().to_path_buf();
        first
            .append(vec![Op {
                id,
                rev: c.tick(),
                deleted: false,
                node: Some(node(1.0)),
            }])
            .unwrap();
        drop(first);

        let mut later = Journal::create(&dir, "dev").unwrap();
        later
            .append(vec![Op {
                id,
                rev: c.tick(),
                deleted: false,
                node: Some(node(2.0)),
            }])
            .unwrap();
        let name = canonical
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let conflict = dir.join(format!("{name}.sync-conflict-20260101-000000-ABCDEFG"));
        std::fs::copy(later.path(), &conflict).unwrap();
        drop(later);

        let mut state = MergeState::default();
        let mut marks = Marks::default();
        read_project(&root, &mut state, &mut marks).unwrap();
        assert_eq!(state.nodes[&id].node.as_ref().unwrap().x, 2.0);
        assert!(conflict.exists(), "conflict copies are never deleted");

        // A second pass is a no-op.
        let changes = read_project(&root, &mut state, &mut marks).unwrap();
        assert!(changes.is_empty());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_new_machine_bootstraps_from_a_checkpoint() {
        let root = std::env::temp_dir().join(format!(
            "refcanvas-checkpoint-{}-{}",
            std::process::id(),
            NONCE.fetch_add(1, Ordering::Relaxed)
        ));
        let mut source = MergeState::default();
        let id = Uuid::now_v7();
        let mut c = clock("writer");
        source.merge(id, c.tick(), false, Some(node(4.0)));
        write_checkpoint(&root.join("checkpoints"), "writer", &source)
            .unwrap()
            .unwrap();

        let mut fresh = MergeState::default();
        let mut marks = Marks::default();
        read_project(&root, &mut fresh, &mut marks).unwrap();
        assert_eq!(fresh.nodes[&id].node.as_ref().unwrap().x, 4.0);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn two_devices_converge_regardless_of_delivery_order() {
        let root = std::env::temp_dir().join(format!(
            "refcanvas-peers-{}-{}",
            std::process::id(),
            NONCE.fetch_add(1, Ordering::Relaxed)
        ));
        let a_dir = root.join("a/journal");
        let b_dir = root.join("b/journal");

        let contested = Uuid::now_v7();
        let mut a_clock = clock("a");
        let mut b_clock = clock("b");
        let a_rev = a_clock.tick();
        let b_rev = b_clock.tick();

        let mut a_journal = Journal::create(&a_dir, "a").unwrap();
        a_journal
            .append(vec![Op {
                id: contested,
                rev: a_rev.clone(),
                deleted: false,
                node: Some(node(1.0)),
            }])
            .unwrap();
        let a_file = a_journal.path().to_path_buf();
        drop(a_journal);

        let mut b_journal = Journal::create(&b_dir, "b").unwrap();
        b_journal
            .append(vec![Op {
                id: contested,
                rev: b_rev.clone(),
                deleted: false,
                node: Some(node(2.0)),
            }])
            .unwrap();
        let b_file = b_journal.path().to_path_buf();
        drop(b_journal);

        // Deliver in both directions (and at all, in either order).
        std::fs::copy(&a_file, b_dir.join(a_file.file_name().unwrap())).unwrap();
        std::fs::copy(&b_file, a_dir.join(b_file.file_name().unwrap())).unwrap();

        let mut a_state = MergeState::default();
        let mut a_marks = Marks::default();
        read_project(&root.join("a"), &mut a_state, &mut a_marks).unwrap();
        let mut b_state = MergeState::default();
        let mut b_marks = Marks::default();
        read_project(&root.join("b"), &mut b_state, &mut b_marks).unwrap();

        let winner = if a_rev > b_rev { &a_rev } else { &b_rev };
        assert_eq!(a_state.nodes[&contested].rev, *winner);
        assert_eq!(b_state.nodes[&contested].rev, *winner);
        assert_eq!(
            a_state.nodes[&contested].node,
            b_state.nodes[&contested].node
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn journal_file_is_created_empty_and_closed_between_batches() {
        let root = std::env::temp_dir().join(format!(
            "refcanvas-lazy-journal-{}-{}",
            std::process::id(),
            NONCE.fetch_add(1, Ordering::Relaxed)
        ));
        let dir = root.join("journal");
        let mut journal = Journal::create(&dir, "dev").unwrap();
        assert!(journal.path().is_file());
        assert_eq!(std::fs::metadata(journal.path()).unwrap().len(), 0);
        let mut c = clock("dev");
        journal
            .append(vec![Op {
                id: Uuid::now_v7(),
                rev: c.tick(),
                deleted: false,
                node: Some(node(1.0)),
            }])
            .unwrap();
        assert!(std::fs::metadata(journal.path()).unwrap().len() > 0);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn journal_round_trip_ignores_a_partial_tail() {
        let root = std::env::temp_dir().join(format!(
            "refcanvas-oplog-{}-{}",
            std::process::id(),
            NONCE.fetch_add(1, Ordering::Relaxed)
        ));
        let dir = root.join("journal");
        let mut state = MergeState::default();
        let mut marks = Marks::default();
        let id = Uuid::now_v7();
        let mut c = clock("dev");
        let path;
        let first_line;
        {
            let mut journal = Journal::create(&dir, "dev").unwrap();
            path = journal.path().to_path_buf();
            journal
                .append(vec![Op {
                    id,
                    rev: c.tick(),
                    deleted: false,
                    node: Some(node(7.0)),
                }])
                .unwrap();
            drop(journal);
            first_line = std::fs::read(&path).unwrap();
            // Simulate a replicated file that was scanned mid-write.
            let mut bytes = first_line.clone();
            bytes.extend_from_slice(br#"{"v":1,"device":"dev""#);
            std::fs::write(&path, bytes).unwrap();
        }
        read_project(&root, &mut state, &mut marks).unwrap();
        assert_eq!(state.nodes.len(), 1);
        assert_eq!(state.nodes[&id].node.as_ref().unwrap().x, 7.0);

        // The source finishes and the complete file replaces the partial copy.
        let second = Uuid::now_v7();
        let batch = Batch {
            v: wire_version(),
            device: "dev".to_string(),
            session: "s".to_string(),
            ops: vec![Op {
                id: second,
                rev: c.tick(),
                deleted: false,
                node: Some(node(8.0)),
            }],
        };
        let mut complete = first_line;
        complete.extend(serde_json::to_vec(&batch).unwrap());
        complete.push(b'\n');
        std::fs::write(&path, complete).unwrap();
        read_project(&root, &mut state, &mut marks).unwrap();
        assert_eq!(state.nodes.len(), 2);
        let _ = std::fs::remove_dir_all(root);
    }
}

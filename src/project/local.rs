//! Per-machine local state: identity, camera, merged-state cache and cleanup.
//!
//! None of this lives in the project folder. Two artists sharing a project keep
//! separate camera positions and separate caches, and panning never produces
//! shared-folder writes.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bevy::math::DVec2;
use serde::{Deserialize, Serialize};

use super::oplog::{Clock, FileMark, MergeState};

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(not(test))]
fn data_root() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|b| b.data_dir().join("refcanvas"))
}

#[cfg(test)]
fn data_root() -> Option<PathBuf> {
    None
}

pub fn projects_root() -> Option<PathBuf> {
    data_root().map(|root| root.join("projects"))
}

/// Stable per-installation identity. Stored outside the project folder so two
/// machines never share one clock or one device id.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct DeviceFile {
    id: String,
    #[serde(default)]
    last_w: u64,
    #[serde(default)]
    last_c: u32,
}

#[cfg(not(test))]
fn device_path() -> Option<PathBuf> {
    data_root().map(|root| root.join("device.json"))
}

#[cfg(test)]
fn device_path() -> Option<PathBuf> {
    None
}

#[cfg(not(test))]
pub fn device_id() -> String {
    let Some(path) = device_path() else {
        return "local".to_string();
    };
    if let Ok(bytes) = std::fs::read(&path)
        && let Ok(file) = serde_json::from_slice::<DeviceFile>(&bytes)
    {
        return file.id;
    }
    let id = uuid::Uuid::new_v4().simple().to_string()[..16].to_string();
    let file = DeviceFile {
        id: id.clone(),
        last_w: 0,
        last_c: 0,
    };
    write_json(&path, &file);
    id
}

#[cfg(test)]
pub fn device_id() -> String {
    "test-device".to_string()
}

pub fn load_clock() -> Clock {
    #[cfg(not(test))]
    {
        let (id, last_w, last_c) = device_path()
            .and_then(|path| std::fs::read(path).ok())
            .and_then(|bytes| serde_json::from_slice::<DeviceFile>(&bytes).ok())
            .map(|file| (file.id, file.last_w, file.last_c))
            .unwrap_or_else(|| (device_id(), 0, 0));
        Clock::new(id, last_w, last_c)
    }
    #[cfg(test)]
    {
        Clock::new(device_id(), 0, 0)
    }
}

pub fn save_clock(clock: &Clock) {
    let Some(path) = device_path() else {
        return;
    };
    let file = DeviceFile {
        id: clock.device_id().to_string(),
        last_w: clock.last_w(),
        last_c: clock.last_c(),
    };
    write_json(&path, &file);
}

fn write_json<T: Serialize>(path: &Path, value: &T) {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);

    let Some(parent) = path.parent() else {
        return;
    };
    let _ = std::fs::create_dir_all(parent);
    let Ok(bytes) = serde_json::to_vec_pretty(value) else {
        return;
    };
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = parent.join(format!(
        ".{name}.{}-{}.tmp",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    if std::fs::write(&tmp, bytes).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

// ---------------------------------------------------------------------------
// Window preferences
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct UiPrefs {
    #[serde(default)]
    always_on_top: bool,
}

fn prefs_path() -> Option<PathBuf> {
    data_root().map(|root| root.join("ui.json"))
}

pub fn load_always_on_top() -> bool {
    let Some(path) = prefs_path() else {
        return false;
    };
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<UiPrefs>(&bytes).ok())
        .map(|prefs| prefs.always_on_top)
        .unwrap_or(false)
}

pub fn save_always_on_top(on_top: bool) {
    let Some(path) = prefs_path() else {
        return;
    };
    write_json(
        &path,
        &UiPrefs {
            always_on_top: on_top,
        },
    );
}

// ---------------------------------------------------------------------------
// Project path key
// ---------------------------------------------------------------------------

/// Canonical absolute path used as the local-state identity. Keeping the
/// original string in the metadata file makes a hash collision detectable.
pub fn canonical_root(root: &Path) -> String {
    let canonical = std::fs::canonicalize(root)
        .or_else(|_| std::path::absolute(root))
        .unwrap_or_else(|_| root.to_path_buf());
    let canonical: PathBuf = canonical.components().collect();
    let text = canonical.to_string_lossy().into_owned();
    #[cfg(windows)]
    let text = {
        let mut text = text;
        text = text.to_lowercase();
        text
    };
    text
}

fn project_key(root: &Path) -> String {
    let hash = blake3::hash(canonical_root(root).as_bytes());
    hash.to_hex()[..32].to_string()
}

pub fn project_dir(root: &Path) -> Option<PathBuf> {
    projects_root().map(|projects| projects.join(project_key(root)))
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct ProjectMeta {
    root: String,
    #[serde(default)]
    missing_since_ms: Option<u64>,
    #[serde(default)]
    last_seen_ms: u64,
}

fn read_meta(dir: &Path) -> Option<ProjectMeta> {
    let bytes = std::fs::read(dir.join("meta.json")).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn write_meta(dir: &Path, meta: &ProjectMeta) {
    write_json(&dir.join("meta.json"), meta);
}

fn touch_meta(root: &Path, dir: &Path) {
    let canonical = canonical_root(root);
    let mut meta = read_meta(dir).unwrap_or_default();
    if meta.root != canonical {
        meta.root = canonical;
    }
    meta.last_seen_ms = now_ms();
    meta.missing_since_ms = None;
    write_meta(dir, &meta);
}

// ---------------------------------------------------------------------------
// Cached merged state
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CachedState {
    #[serde(default)]
    pub root: String,
    #[serde(default)]
    pub merge: MergeState,
    #[serde(default)]
    pub files: HashMap<String, FileMark>,
}

pub fn load_state(root: &Path) -> Option<CachedState> {
    let dir = project_dir(root)?;
    let bytes = std::fs::read(dir.join("state.json")).ok()?;
    let cached: CachedState = serde_json::from_slice(&bytes).ok()?;
    let canonical = canonical_root(root);
    if !cached.root.is_empty() && cached.root != canonical {
        return None;
    }
    Some(CachedState {
        root: canonical,
        ..cached
    })
}

pub fn save_state(root: &Path, merge: &MergeState, files: &HashMap<String, FileMark>) {
    let Some(dir) = project_dir(root) else {
        return;
    };
    let _ = std::fs::create_dir_all(&dir);
    let cached = CachedState {
        root: canonical_root(root),
        merge: merge.clone(),
        files: files.clone(),
    };
    write_json(&dir.join("state.json"), &cached);
    touch_meta(root, &dir);
}

/// Write a snapshot of local state on a worker thread. The journal is already
/// durable, so a missed or slow cache write only costs replay time later.
pub fn save_state_async(root: PathBuf, merge: MergeState, files: HashMap<String, FileMark>) {
    std::thread::spawn(move || save_state(&root, &merge, &files));
}

// ---------------------------------------------------------------------------
// Camera
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ViewState {
    #[serde(default = "wire_version")]
    schema: u32,
    root: String,
    center: [f64; 2],
    log_scale: f64,
    updated_ms: u64,
}

fn wire_version() -> u32 {
    1
}

pub fn load_view(root: &Path) -> Option<(DVec2, f64)> {
    let dir = project_dir(root)?;
    let bytes = std::fs::read(dir.join("view.json")).ok()?;
    let view: ViewState = serde_json::from_slice(&bytes).ok()?;
    if view.schema != wire_version() || view.root != canonical_root(root) {
        return None;
    }
    Some((DVec2::new(view.center[0], view.center[1]), view.log_scale))
}

pub fn save_view(root: &Path, center: DVec2, log_scale: f64) {
    let Some(dir) = project_dir(root) else {
        return;
    };
    let _ = std::fs::create_dir_all(&dir);
    let view = ViewState {
        schema: wire_version(),
        root: canonical_root(root),
        center: [center.x, center.y],
        log_scale,
        updated_ms: now_ms(),
    };
    write_json(&dir.join("view.json"), &view);
    touch_meta(root, &dir);
}

// ---------------------------------------------------------------------------
// Cleanup
// ---------------------------------------------------------------------------

/// Drop local state for a project that no longer exists.
#[allow(dead_code)]
pub fn forget_project(root: &Path) {
    if let Some(dir) = project_dir(root) {
        let _ = std::fs::remove_dir_all(dir);
    }
}

/// Remove local state whose project folder has been gone for longer than
/// `grace`. A missing folder can simply mean an unmounted drive or an offline
/// network share, so nothing is deleted on first sight.
pub fn sweep_missing(grace: Duration) {
    let Some(projects) = projects_root() else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(&projects) else {
        return;
    };
    let now = now_ms();
    let grace_ms = grace.as_millis() as u64;
    for entry in entries.filter_map(|e| e.ok()) {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        let Some(mut meta) = read_meta(&dir) else {
            continue;
        };
        if meta.root.is_empty() {
            continue;
        }
        if Path::new(&meta.root).is_dir() {
            if meta.missing_since_ms.take().is_some() {
                write_meta(&dir, &meta);
            }
            continue;
        }
        match meta.missing_since_ms {
            Some(since) if now.saturating_sub(since) >= grace_ms => {
                let _ = std::fs::remove_dir_all(&dir);
            }
            Some(_) => {}
            None => {
                meta.missing_since_ms = Some(now);
                write_meta(&dir, &meta);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_keys_are_stable_and_distinct() {
        let a = project_key(Path::new("/tmp/refcanvas-a"));
        let b = project_key(Path::new("/tmp/refcanvas-b"));
        assert_eq!(a, project_key(Path::new("/tmp/refcanvas-a/")));
        assert_ne!(a, b);
    }
}

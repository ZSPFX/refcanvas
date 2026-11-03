//! Startup update check.
//!
//! One background request per launch. The result is polled from the main loop;
//! a newer release shows a native dialog, and declining it leaves a badge in
//! the status bar. Nothing is downloaded or installed automatically.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use bevy::prelude::*;
use bevy_egui::{EguiContexts, egui};
use semver::Version;
use serde::{Deserialize, Serialize};

use crate::dialogs::{DialogResults, Dialogs, MessageId};
use crate::project::local;

const API_URL: &str = "https://api.github.com/repos/ZSPFX/refcanvas/releases/latest";
const RELEASES_URL: &str = "https://github.com/ZSPFX/refcanvas/releases/latest";
const RELEASE_PREFIX: &str = "https://github.com/ZSPFX/refcanvas/";

pub struct UpdatePlugin;

impl Plugin for UpdatePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<UpdateStatus>()
            .add_systems(Startup, start_check)
            .add_systems(Update, (poll_check, answer_prompt).chain());
    }
}

/// What the status bar needs to render the declined-update badge.
#[derive(Resource, Default)]
pub struct UpdateStatus {
    pub latest: Option<Version>,
    pub declined: bool,
    stored: Stored,
    pending: Option<Arc<Mutex<Option<Result<Latest, String>>>>>,
    prompted_this_run: bool,
}

impl UpdateStatus {
    pub fn badge_text(&self) -> Option<String> {
        if !self.declined {
            return None;
        }
        let latest = self.latest.as_ref()?;
        Some(format!(
            "v{} (v{latest} latest. please update)",
            env!("CARGO_PKG_VERSION")
        ))
    }
}

#[derive(Clone, Debug)]
struct Latest {
    version: Version,
    url: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
struct Stored {
    #[serde(default)]
    latest_version: Option<String>,
    #[serde(default)]
    latest_url: Option<String>,
    #[serde(default)]
    prompted_version: Option<String>,
    #[serde(default)]
    declined_version: Option<String>,
}

impl Stored {
    fn load() -> Self {
        let Some(path) = state_path() else {
            return Self::default();
        };
        std::fs::read(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    fn save(&self) {
        let Some(path) = state_path() else {
            return;
        };
        local::write_json(&path, self);
    }
}

fn state_path() -> Option<std::path::PathBuf> {
    local::data_root().map(|root| root.join("update.json"))
}

fn current_version() -> Version {
    Version::parse(env!("CARGO_PKG_VERSION")).unwrap_or_else(|_| Version::new(0, 0, 0))
}

fn parse_tag(tag: &str) -> Result<Version, semver::Error> {
    Version::parse(tag.trim().trim_start_matches('v'))
}

fn disabled() -> bool {
    std::env::var("REF_NO_UPDATE_CHECK")
        .map(|value| !value.is_empty() && value != "0")
        .unwrap_or(false)
}

fn start_check(mut status: ResMut<UpdateStatus>) {
    status.stored = Stored::load();
    let current = current_version();

    if let Some(version) = status
        .stored
        .latest_version
        .as_deref()
        .and_then(|text| Version::parse(text).ok())
    {
        if version > current {
            status.declined =
                status.stored.declined_version.as_deref() == Some(version.to_string().as_str());
            status.latest = Some(version);
        }
    }

    if disabled() {
        return;
    }

    let slot = Arc::new(Mutex::new(None));
    status.pending = Some(slot.clone());
    std::thread::spawn(move || {
        let result = fetch_latest();
        if let Ok(mut slot) = slot.lock() {
            *slot = Some(result);
        }
    });
}

fn poll_check(mut status: ResMut<UpdateStatus>, mut dialogs: ResMut<Dialogs>) {
    let Some(slot) = status.pending.clone() else {
        return;
    };
    let result = slot.lock().ok().and_then(|mut slot| slot.take());
    let Some(result) = result else {
        return;
    };
    status.pending = None;

    let latest = match result {
        Ok(latest) => latest,
        Err(err) => {
            debug!("update check failed: {err}");
            return;
        }
    };

    let current = current_version();
    let latest_text = latest.version.to_string();

    if latest.version <= current {
        if status.stored.latest_version.is_some() {
            status.stored.latest_version = None;
            status.stored.latest_url = None;
            status.stored.prompted_version = None;
            status.stored.declined_version = None;
            status.stored.save();
        }
        status.latest = None;
        status.declined = false;
        return;
    }

    let was_declined = status.stored.declined_version.as_deref() == Some(latest_text.as_str());
    status.latest = Some(latest.version.clone());
    status.declined = was_declined;

    let already_prompted = status.stored.prompted_version.as_deref() == Some(latest_text.as_str());
    let should_prompt = !was_declined && !already_prompted && !status.prompted_this_run;

    status.stored.latest_version = Some(latest_text.clone());
    status.stored.latest_url = Some(latest.url.clone());

    if should_prompt {
        status.prompted_this_run = true;
        status.stored.prompted_version = Some(latest_text);
        status.stored.save();
        // The dialog is native and asynchronous: the answer arrives in
        // `answer_prompt` while the app keeps rendering.
        dialogs.ask(
            "Update available",
            prompt_text(&latest),
            MessageId::UpdateAvailable {
                version: latest.version.to_string(),
                url: latest.url,
            },
        );
    } else if status.stored.declined_version.as_deref() == Some(latest_text.as_str()) {
        status.declined = true;
    }
}

/// Handle the user's answer to the update prompt.
fn answer_prompt(
    mut status: ResMut<UpdateStatus>,
    mut results: ResMut<DialogResults>,
    mut contexts: EguiContexts,
) {
    let answers: Vec<(MessageId, rfd::MessageDialogResult)> = std::mem::take(&mut results.messages);
    for (id, answer) in answers {
        let MessageId::UpdateAvailable { version, url } = id;
        if matches!(answer, rfd::MessageDialogResult::Yes) {
            open_url(&mut contexts, &url);
        } else {
            status.stored.declined_version = Some(version);
            status.declined = true;
            status.stored.save();
        }
    }
}

fn prompt_text(latest: &Latest) -> String {
    format!(
        "refcanvas v{} is available. You have v{}. Open the release page to download it?",
        latest.version,
        env!("CARGO_PKG_VERSION")
    )
}

fn open_url(contexts: &mut EguiContexts, url: &str) {
    match contexts.ctx_mut() {
        Ok(ctx) => ctx.open_url(egui::OpenUrl::new_tab(url)),
        Err(err) => warn!("could not open update link: {err}"),
    }
}

fn fetch_latest() -> Result<Latest, String> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(6)))
        .build()
        .into();

    let body = agent
        .get(API_URL)
        .header(
            "User-Agent",
            concat!("refcanvas/", env!("CARGO_PKG_VERSION")),
        )
        .header("Accept", "application/vnd.github+json")
        .call()
        .map_err(|err| format!("request failed: {err}"))?
        .body_mut()
        .read_to_string()
        .map_err(|err| format!("response body failed: {err}"))?;

    #[derive(Deserialize)]
    struct Release {
        tag_name: String,
        html_url: String,
        #[serde(default)]
        prerelease: bool,
    }

    let release: Release =
        serde_json::from_str(&body).map_err(|err| format!("release json failed: {err}"))?;
    if release.prerelease {
        return Err("latest release is a pre-release".to_string());
    }
    let version = parse_tag(&release.tag_name).map_err(|err| format!("bad tag: {err}"))?;
    let url = if release.html_url.starts_with(RELEASE_PREFIX) {
        release.html_url
    } else {
        RELEASES_URL.to_string()
    };
    Ok(Latest { version, url })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags_are_normalized() {
        assert_eq!(parse_tag("v0.2.0").unwrap(), Version::new(0, 2, 0));
        assert_eq!(parse_tag("0.2.0").unwrap(), Version::new(0, 2, 0));
        assert_eq!(parse_tag(" v1.2.3 ").unwrap(), Version::new(1, 2, 3));
        assert!(parse_tag("not-a-version").is_err());
    }

    #[test]
    fn only_newer_versions_alert() {
        let current = Version::new(0, 1, 4);
        assert!(Version::new(0, 1, 5) > current);
        assert!(Version::new(0, 2, 0) > current);
        assert!(Version::new(0, 1, 4) <= current);
        assert!(Version::new(0, 1, 3) <= current);
    }
}

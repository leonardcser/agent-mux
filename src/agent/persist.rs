use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use fs2::FileExt;
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::agent::{Pane, PaneStatus, tmux::parse_target};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CachedPane {
    #[serde(rename = "paneID", default, skip_serializing_if = "String::is_empty")]
    pub pane_id: String,
    pub target: String,
    #[serde(
        rename = "windowName",
        default,
        skip_serializing_if = "String::is_empty"
    )]
    pub window_name: String,
    #[serde(default)]
    pub path: String,
    #[serde(rename = "shortPath", default)]
    pub short_path: String,
    #[serde(
        rename = "projectRoot",
        default,
        skip_serializing_if = "String::is_empty"
    )]
    pub project_root: String,
    #[serde(
        rename = "projectShort",
        default,
        skip_serializing_if = "String::is_empty"
    )]
    pub project_short: String,
    #[serde(
        rename = "projectBranch",
        default,
        skip_serializing_if = "String::is_empty"
    )]
    pub project_branch: String,
    #[serde(rename = "projectDirty", default, skip_serializing_if = "is_false")]
    pub project_dirty: bool,
    #[serde(
        rename = "gitBranch",
        default,
        skip_serializing_if = "String::is_empty"
    )]
    pub git_branch: String,
    #[serde(rename = "gitDirty", default, skip_serializing_if = "is_false")]
    pub git_dirty: bool,
    #[serde(default)]
    pub stashed: bool,
    #[serde(default, skip_serializing_if = "is_zero_usize")]
    pub order: usize,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub provider: String,
    #[serde(rename = "windowActive", default, skip_serializing_if = "is_false")]
    pub window_active: bool,
    #[serde(
        rename = "statusOverride",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub status_override: Option<i32>,
    #[serde(
        rename = "contentHash",
        default,
        skip_serializing_if = "String::is_empty"
    )]
    pub content_hash: String,
    #[serde(
        rename = "lastStatus",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub last_status: Option<i32>,
    #[serde(
        rename = "lastActive",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub last_active: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct State {
    #[serde(default)]
    pub version: i32,
    #[serde(default)]
    pub panes: Vec<CachedPane>,
    #[serde(rename = "lastPosition", default)]
    pub last_position: LastPosition,
    #[serde(rename = "sidebarWidth", default, skip_serializing_if = "is_zero_u16")]
    pub sidebar_width: u16,
    #[serde(rename = "updatedAt", default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Snapshot {
    #[serde(default)]
    pub version: i32,
    #[serde(default)]
    pub generation: u64,
    #[serde(default)]
    pub panes: Vec<CachedPane>,
    #[serde(rename = "updatedAt", default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Heartbeat {
    #[serde(default)]
    pub version: i32,
    #[serde(rename = "updatedAt", default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct UiState {
    #[serde(default)]
    pub version: i32,
    #[serde(default)]
    pub panes: std::collections::HashMap<String, UiPaneState>,
    #[serde(rename = "lastPosition", default)]
    pub last_position: LastPosition,
    #[serde(rename = "sidebarWidth", default, skip_serializing_if = "is_zero_u16")]
    pub sidebar_width: u16,
    #[serde(rename = "updatedAt", default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct UiPaneState {
    #[serde(default, skip_serializing_if = "is_false")]
    pub stashed: bool,
    #[serde(
        rename = "manualStatus",
        alias = "statusOverride",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub manual_status: Option<i32>,
    #[serde(
        rename = "manualStatusBaseHash",
        alias = "contentHash",
        default,
        skip_serializing_if = "String::is_empty"
    )]
    pub manual_status_base_hash: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LastPosition {
    #[serde(rename = "pane_id", default, skip_serializing_if = "String::is_empty")]
    pub pane_id: String,
    #[serde(
        rename = "pane_target",
        default,
        skip_serializing_if = "String::is_empty"
    )]
    pub pane_target: String,
    #[serde(default)]
    pub cursor: usize,
    #[serde(rename = "scroll_start", default)]
    pub scroll_start: usize,
}

fn is_false(v: &bool) -> bool {
    !*v
}
fn is_zero_usize(v: &usize) -> bool {
    *v == 0
}
fn is_zero_u16(v: &u16) -> bool {
    *v == 0
}

impl CachedPane {
    pub fn pane_key(&self) -> &str {
        if self.pane_id.is_empty() {
            &self.target
        } else {
            &self.pane_id
        }
    }
}

pub fn load_state() -> Option<State> {
    load_state_file(state_path())
}

pub fn load_snapshot() -> Option<Snapshot> {
    load_snapshot_file().or_else(|| {
        load_state().map(|state| Snapshot {
            version: state.version,
            generation: 1,
            panes: state.panes,
            updated_at: state.updated_at,
        })
    })
}

fn load_snapshot_file() -> Option<Snapshot> {
    load_json_file(snapshot_path()).filter(|snapshot: &Snapshot| snapshot.version == 1)
}

pub fn load_ui_state() -> UiState {
    load_json_file(ui_state_path())
        .filter(|state: &UiState| state.version == 1)
        .or_else(|| load_state().map(ui_state_from_legacy_state))
        .unwrap_or_default()
}

pub fn apply_ui_state(panes: &mut [Pane], ui_state: &UiState) {
    for pane in panes {
        if let Some(ui) = ui_state
            .panes
            .get(&pane.pane_id)
            .or_else(|| ui_state.panes.get(&pane.target))
        {
            apply_pane_ui_state(pane, ui);
        }
    }
}

pub fn apply_pane_ui_state(pane: &mut Pane, ui: &UiPaneState) {
    pane.stashed = ui.stashed;
    if let Some(status) = ui.manual_status {
        pane.status = display_status(
            pane.status,
            &pane.content_hash,
            PaneStatus::from_i32(status),
            &ui.manual_status_base_hash,
        );
    }
}

pub fn display_status(
    observed_status: PaneStatus,
    content_hash: &str,
    manual_status: PaneStatus,
    manual_status_base_hash: &str,
) -> PaneStatus {
    let same_content =
        manual_status_base_hash.is_empty() || manual_status_base_hash == content_hash;
    match manual_status {
        PaneStatus::Unread => PaneStatus::Unread,
        PaneStatus::Idle if same_content => PaneStatus::Idle,
        PaneStatus::Idle => observed_status,
        status if same_content => status,
        _ => observed_status,
    }
}

pub fn has_manual_status(ui_state: &UiState, pane_id: &str, pane_target: &str) -> bool {
    ui_state
        .panes
        .get(pane_id)
        .or_else(|| ui_state.panes.get(pane_target))
        .and_then(|ui| ui.manual_status)
        .is_some()
}

pub fn ui_pane_state_is_empty(ui: &UiPaneState) -> bool {
    !ui.stashed && ui.manual_status.is_none()
}

fn ui_state_from_legacy_state(state: State) -> UiState {
    let panes = state
        .panes
        .into_iter()
        .filter_map(|cp| {
            let key = cp.pane_key().to_string();
            let ui = UiPaneState {
                stashed: cp.stashed,
                manual_status: cp.status_override,
                manual_status_base_hash: cp.content_hash,
            };
            (ui.stashed || ui.manual_status.is_some()).then_some((key, ui))
        })
        .collect();
    UiState {
        version: state.version,
        panes,
        last_position: state.last_position,
        sidebar_width: state.sidebar_width,
        updated_at: state.updated_at,
    }
}

pub fn write_snapshot_if_changed(mut snapshot: Snapshot) -> Result<bool> {
    let lock_file = lock_file(snapshot_write_lock_path())?;
    let previous = load_snapshot_file();
    if previous
        .as_ref()
        .is_some_and(|previous| previous.generation > 0 && previous.panes == snapshot.panes)
    {
        drop(lock_file);
        return Ok(false);
    }

    snapshot.version = 1;
    snapshot.generation = previous
        .as_ref()
        .map(|previous| {
            let generation = previous.generation.max(1);
            if panes_equal_for_generation(&previous.panes, &snapshot.panes) {
                generation
            } else {
                generation + 1
            }
        })
        .unwrap_or(1);
    snapshot.updated_at = Some(Utc::now());
    write_json_file(snapshot_path(), &snapshot)?;
    drop(lock_file);
    Ok(true)
}

fn panes_equal_for_generation(a: &[CachedPane], b: &[CachedPane]) -> bool {
    comparable_panes(a) == comparable_panes(b)
}

fn comparable_panes(panes: &[CachedPane]) -> Vec<CachedPane> {
    panes
        .iter()
        .cloned()
        .map(|mut pane| {
            pane.stashed = false;
            pane.status_override = None;
            pane.window_active = false;
            pane.content_hash.clear();
            pane.last_active = None;
            pane
        })
        .collect()
}

pub fn write_heartbeat() -> Result<()> {
    let lock_file = lock_file(heartbeat_write_lock_path())?;
    write_json_file(
        heartbeat_path(),
        &Heartbeat {
            version: 1,
            updated_at: Some(Utc::now()),
        },
    )?;
    drop(lock_file);
    Ok(())
}

pub fn update_ui_state_if_changed(mut f: impl FnMut(&mut UiState)) -> Result<bool> {
    let lock_file = lock_file(ui_state_write_lock_path())?;
    let mut state = load_ui_state();
    let mut previous = state.clone();
    previous.updated_at = None;
    f(&mut state);
    let mut comparable = state.clone();
    comparable.updated_at = None;
    if comparable == previous {
        drop(lock_file);
        return Ok(false);
    }
    state.version = 1;
    state.updated_at = Some(Utc::now());
    write_json_file(ui_state_path(), &state)?;
    drop(lock_file);
    Ok(true)
}

pub fn update_ui_state(mut f: impl FnMut(&mut UiState)) -> Result<()> {
    let lock_file = lock_file(ui_state_write_lock_path())?;
    let mut state = load_ui_state();
    f(&mut state);
    state.version = 1;
    state.updated_at = Some(Utc::now());
    write_json_file(ui_state_path(), &state)?;
    drop(lock_file);
    Ok(())
}

fn load_state_file(path: PathBuf) -> Option<State> {
    let state: State = load_json_file(path)?;
    (state.version == 1).then_some(state)
}

fn load_json_file<T: DeserializeOwned>(path: PathBuf) -> Option<T> {
    let data = fs::read(path).ok()?;
    serde_json::from_slice(&data).ok()
}

fn lock_file(path: PathBuf) -> Result<File> {
    fs::create_dir_all(state_dir()).context("create state dir")?;
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(path)
        .context("open state lock")?;
    file.lock_exclusive().context("lock state")?;
    Ok(file)
}

fn write_json_file<T: Serialize>(path: PathBuf, value: &T) -> Result<()> {
    fs::create_dir_all(state_dir()).context("create state dir")?;
    let data = serde_json::to_vec_pretty(value).context("encode state")?;
    let file_name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("state.json");
    let tmp_path = state_dir().join(format!(".{file_name}-{}.tmp", std::process::id()));
    {
        let mut tmp = File::create(&tmp_path).context("create tmp state")?;
        tmp.write_all(&data).context("write tmp state")?;
        tmp.sync_all().ok();
    }
    fs::rename(&tmp_path, path).context("rename state")?;
    Ok(())
}

pub fn cache_panes(panes: &[Pane]) -> Vec<CachedPane> {
    panes
        .iter()
        .map(|p| CachedPane {
            pane_id: p.pane_id.clone(),
            target: p.target.clone(),
            window_name: p.window_name.clone(),
            path: p.path.clone(),
            short_path: p.short_path.clone(),
            project_root: p.project_root.clone(),
            project_short: p.project_short.clone(),
            project_branch: p.project_branch.clone(),
            project_dirty: p.project_dirty,
            git_branch: p.git_branch.clone(),
            git_dirty: p.git_dirty,
            order: p.order,
            provider: p.provider.clone(),
            window_active: p.window_active,
            last_active: p.last_active,
            ..CachedPane::default()
        })
        .collect()
}

pub fn panes_from_snapshot(snapshot: &Snapshot) -> Vec<Pane> {
    panes_from_cached(&snapshot.panes)
}

fn panes_from_cached(panes: &[CachedPane]) -> Vec<Pane> {
    panes
        .iter()
        .map(|cp| {
            let id = if cp.pane_id.is_empty() {
                cp.target.clone()
            } else {
                cp.pane_id.clone()
            };
            let (session, window, pane) = parse_target(&cp.target);
            Pane {
                pane_id: id,
                target: cp.target.clone(),
                session,
                window,
                window_name: cp.window_name.clone(),
                pane,
                path: cp.path.clone(),
                short_path: cp.short_path.clone(),
                project_root: cp.project_root.clone(),
                project_short: cp.project_short.clone(),
                project_branch: cp.project_branch.clone(),
                project_dirty: cp.project_dirty,
                git_branch: cp.git_branch.clone(),
                git_dirty: cp.git_dirty,
                stashed: cp.stashed,
                order: cp.order,
                provider: cp.provider.clone(),
                window_active: cp.window_active,
                content_hash: cp.content_hash.clone(),
                status: cp.last_status.map(PaneStatus::from_i32).unwrap_or_default(),
                last_active: cp.last_active,
                ..Pane::default()
            }
        })
        .collect()
}

pub fn state_dir() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".local/state/agent-mux")
}

pub fn state_path() -> PathBuf {
    state_dir().join("state.json")
}

pub fn snapshot_path() -> PathBuf {
    state_dir().join("snapshot.json")
}

pub fn ui_state_path() -> PathBuf {
    state_dir().join("ui_state.json")
}

pub fn heartbeat_path() -> PathBuf {
    state_dir().join("heartbeat.json")
}

pub fn snapshot_write_lock_path() -> PathBuf {
    state_dir().join("snapshot.lock")
}

pub fn ui_state_write_lock_path() -> PathBuf {
    state_dir().join("ui_state.lock")
}

pub fn heartbeat_write_lock_path() -> PathBuf {
    state_dir().join("heartbeat.lock")
}

#[cfg(test)]
mod tests {
    use super::{UiPaneState, UiState, apply_ui_state, display_status, has_manual_status};
    use crate::agent::{Pane, PaneStatus};

    fn pane(status: PaneStatus, content_hash: &str) -> Pane {
        Pane {
            pane_id: "%1".to_string(),
            target: "s:1.1".to_string(),
            status,
            content_hash: content_hash.to_string(),
            ..Pane::default()
        }
    }

    fn ui(status: PaneStatus, content_hash: &str) -> UiPaneState {
        UiPaneState {
            manual_status: Some(status.as_i32()),
            manual_status_base_hash: content_hash.to_string(),
            ..UiPaneState::default()
        }
    }

    #[test]
    fn manual_unread_overrides_observed_idle() {
        assert_eq!(
            display_status(PaneStatus::Idle, "new", PaneStatus::Unread, "old"),
            PaneStatus::Unread
        );
    }

    #[test]
    fn manual_read_holds_until_content_changes() {
        assert_eq!(
            display_status(PaneStatus::Unread, "same", PaneStatus::Idle, "same"),
            PaneStatus::Idle
        );
        assert_eq!(
            display_status(PaneStatus::Unread, "new", PaneStatus::Idle, "old"),
            PaneStatus::Unread
        );
    }

    #[test]
    fn applies_user_state_as_display_layer() {
        let mut panes = vec![pane(PaneStatus::Unread, "same")];
        let mut state = UiState::default();
        state
            .panes
            .insert("%1".to_string(), ui(PaneStatus::Idle, "same"));

        apply_ui_state(&mut panes, &state);

        assert_eq!(panes[0].status, PaneStatus::Idle);
        assert!(has_manual_status(&state, "%1", "s:1.1"));
    }
}

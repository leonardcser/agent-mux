use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use fs2::FileExt;
use serde::{Deserialize, Serialize};

use crate::agent::{Pane, PaneStatus, tmux::parse_target};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
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

fn load_state_file(path: PathBuf) -> Option<State> {
    let data = fs::read(path).ok()?;
    let state: State = serde_json::from_slice(&data).ok()?;
    (state.version == 1).then_some(state)
}

pub fn update_state(mut f: impl FnMut(&mut State)) -> Result<()> {
    let lock_file = lock_state_file()?;
    let mut state = load_state().unwrap_or_default();
    f(&mut state);
    write_state_file(state)?;
    drop(lock_file);
    Ok(())
}

fn lock_state_file() -> Result<File> {
    fs::create_dir_all(state_dir()).context("create state dir")?;
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(state_write_lock_path())
        .context("open state lock")?;
    file.lock_exclusive().context("lock state")?;
    Ok(file)
}

fn write_state_file(mut state: State) -> Result<()> {
    fs::create_dir_all(state_dir()).context("create state dir")?;
    state.version = 1;
    state.updated_at = Some(Utc::now());
    let data = serde_json::to_vec_pretty(&state).context("encode state")?;
    let tmp_path = state_dir().join(format!(".state-{}.tmp", std::process::id()));
    {
        let mut tmp = File::create(&tmp_path).context("create tmp state")?;
        tmp.write_all(&data).context("write tmp state")?;
        tmp.sync_all().ok();
    }
    fs::rename(&tmp_path, state_path()).context("rename state")?;
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
            stashed: p.stashed,
            order: p.order,
            provider: p.provider.clone(),
            last_active: p.last_active,
            ..CachedPane::default()
        })
        .collect()
}

pub fn panes_from_state(state: &State) -> Vec<Pane> {
    state
        .panes
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
                content_hash: cp.content_hash.clone(),
                status: cp.last_status.map(PaneStatus::from_i32).unwrap_or_default(),
                last_active: cp.last_active,
                ..Pane::default()
            }
        })
        .collect()
}

pub fn has_status_override(state: &State, pane_id: &str) -> bool {
    state
        .panes
        .iter()
        .any(|cp| cp.pane_key() == pane_id && cp.status_override.is_some())
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

pub fn state_write_lock_path() -> PathBuf {
    state_dir().join("state.lock")
}

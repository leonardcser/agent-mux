pub mod adapter;
pub mod git;
pub mod ipc;
pub mod persist;
pub mod provider;
pub mod reconcile;
pub mod tmux;
pub mod watch;

pub use reconcile::Reconciler;
pub use tmux::{
    capture_pane, kill_pane, list_panes, list_panes_fast, restart_watch, start_watch,
    switch_to_pane,
};

use std::fmt;

use chrono::{DateTime, Utc};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PaneId(String);

impl PaneId {
    pub fn parse(value: &str) -> Option<Self> {
        let id = value.strip_prefix('%')?;
        (!id.is_empty() && id.bytes().all(|byte| byte.is_ascii_digit()))
            .then(|| Self(value.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for PaneId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PaneStatus {
    #[default]
    Idle = 0,
    Busy = 1,
    Unread = 3,
}

impl PaneStatus {
    pub fn from_i32(value: i32) -> Self {
        match value {
            1 => Self::Busy,
            3 => Self::Unread,
            _ => Self::Idle,
        }
    }

    pub fn as_i32(self) -> i32 {
        self as i32
    }
}

#[derive(Debug, Clone)]
pub struct Pane {
    pub pane_id: PaneId,
    pub target: String,
    pub session: String,
    pub window: String,
    pub window_name: String,
    pub pane: String,
    pub path: String,
    pub short_path: String,
    pub project_root: String,
    pub project_short: String,
    pub project_branch: String,
    pub project_dirty: bool,
    pub git_branch: String,
    pub git_dirty: bool,
    #[allow(dead_code)]
    pub pid: i32,
    pub provider_pid: i32,
    pub status: PaneStatus,
    pub observed_busy: Option<bool>,
    pub content_hash: String,
    pub window_active: bool,
    pub width: u16,
    pub height: u16,
    pub last_active: Option<DateTime<Utc>>,
    pub stashed: bool,
    pub order: usize,
    pub provider: String,
}

impl Pane {
    pub fn new(pane_id: PaneId) -> Self {
        Self {
            pane_id,
            target: String::new(),
            session: String::new(),
            window: String::new(),
            window_name: String::new(),
            pane: String::new(),
            path: String::new(),
            short_path: String::new(),
            project_root: String::new(),
            project_short: String::new(),
            project_branch: String::new(),
            project_dirty: false,
            git_branch: String::new(),
            git_dirty: false,
            pid: 0,
            provider_pid: 0,
            status: PaneStatus::default(),
            observed_busy: None,
            content_hash: String::new(),
            window_active: false,
            width: 0,
            height: 0,
            last_active: None,
            stashed: false,
            order: 0,
            provider: String::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::PaneId;

    #[test]
    fn pane_id_only_accepts_stable_tmux_ids() {
        assert_eq!(PaneId::parse("%42").unwrap().as_str(), "%42");
        assert!(PaneId::parse("session:1.1").is_none());
        assert!(PaneId::parse("%agent").is_none());
        assert!(PaneId::parse("%").is_none());
        assert!(PaneId::parse("").is_none());
    }
}

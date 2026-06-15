pub mod git;
pub mod persist;
pub mod provider;
pub mod reconcile;
pub mod tmux;
pub mod watch;

pub use reconcile::Reconciler;
pub use tmux::{
    capture_pane, kill_pane, list_panes, list_panes_basic, restart_watch, start_watch,
    switch_to_pane,
};

use chrono::{DateTime, Utc};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneStatus {
    Idle = 0,
    Busy = 1,
    NeedsAttention = 2,
    Unread = 3,
}

impl Default for PaneStatus {
    fn default() -> Self {
        Self::Idle
    }
}

impl PaneStatus {
    pub fn from_i32(value: i32) -> Self {
        match value {
            1 => Self::Busy,
            2 => Self::NeedsAttention,
            3 => Self::Unread,
            _ => Self::Idle,
        }
    }

    pub fn as_i32(self) -> i32 {
        self as i32
    }
}

#[derive(Debug, Clone, Default)]
pub struct Pane {
    pub pane_id: String,
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
    pub status: PaneStatus,
    pub content_hash: String,
    pub content_moving: bool,
    pub heuristic_attention: bool,
    pub window_active: bool,
    pub last_active: Option<DateTime<Utc>>,
    pub stashed: bool,
    pub order: usize,
    pub provider: String,
}

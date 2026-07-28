use std::collections::HashMap;
use std::process::Command;

use serde::Deserialize;

use crate::agent::{Pane, PaneStatus};

use super::ProviderAdapter;

pub(super) struct SmeltAdapter;

impl ProviderAdapter for SmeltAdapter {
    fn observed_statuses(&self, _panes: &[Pane]) -> HashMap<u32, PaneStatus> {
        smelt_statuses()
            .into_iter()
            .map(|(pid, status)| (pid, map_status(&status)))
            .collect()
    }
}

pub(super) static SMELT_ADAPTER: SmeltAdapter = SmeltAdapter;

#[derive(Debug, Clone, Deserialize)]
struct SmeltStatus {
    pid: u32,
    state: SmeltState,
    #[serde(default)]
    reason: Option<SmeltReason>,
    #[serde(default)]
    focus: Option<SmeltFocus>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum SmeltState {
    Idle,
    Busy,
    NeedsAttention,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum SmeltReason {
    Permission,
    Question,
    TurnComplete,
    Error,
    Auth,
    Setup,
    Interrupted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum SmeltFocus {
    Focused,
    Unfocused,
    Unknown,
}

fn smelt_statuses() -> HashMap<u32, SmeltStatus> {
    let _g = smelt_perf::perf::begin("provider.smelt_status_all");
    let Ok(out) = Command::new("smelt")
        .arg("status")
        .arg("--all")
        .arg("--json")
        .output()
    else {
        return HashMap::new();
    };
    if !out.status.success() {
        return HashMap::new();
    }
    let Ok(statuses) = serde_json::from_slice::<Vec<SmeltStatus>>(&out.stdout) else {
        return HashMap::new();
    };
    statuses
        .into_iter()
        .map(|status| (status.pid, status))
        .collect()
}

fn map_status(status: &SmeltStatus) -> PaneStatus {
    match status.state {
        SmeltState::Busy => PaneStatus::Busy,
        SmeltState::Idle => PaneStatus::Idle,
        SmeltState::NeedsAttention => match status.reason {
            Some(SmeltReason::TurnComplete) if status.focus == Some(SmeltFocus::Focused) => {
                PaneStatus::Idle
            }
            Some(SmeltReason::TurnComplete) => PaneStatus::Unread,
            _ => PaneStatus::NeedsAttention,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(
        state: SmeltState,
        reason: Option<SmeltReason>,
        focus: Option<SmeltFocus>,
    ) -> SmeltStatus {
        SmeltStatus {
            pid: 1,
            state,
            reason,
            focus,
        }
    }

    #[test]
    fn maps_busy_and_idle_directly() {
        assert_eq!(
            map_status(&status(SmeltState::Busy, None, None)),
            PaneStatus::Busy
        );
        assert_eq!(
            map_status(&status(SmeltState::Idle, None, None)),
            PaneStatus::Idle
        );
    }

    #[test]
    fn maps_unfocused_turn_complete_to_unread() {
        assert_eq!(
            map_status(&status(
                SmeltState::NeedsAttention,
                Some(SmeltReason::TurnComplete),
                Some(SmeltFocus::Unfocused)
            )),
            PaneStatus::Unread
        );
    }

    #[test]
    fn maps_blocked_attention_to_needs_attention() {
        assert_eq!(
            map_status(&status(
                SmeltState::NeedsAttention,
                Some(SmeltReason::Permission),
                Some(SmeltFocus::Focused)
            )),
            PaneStatus::NeedsAttention
        );
    }
}

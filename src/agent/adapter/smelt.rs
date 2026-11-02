use std::collections::HashMap;
use std::process::Command;

use serde::Deserialize;

use crate::agent::Pane;

use super::{ObservedActivity, ProviderAdapter};

pub(super) struct SmeltAdapter;

impl ProviderAdapter for SmeltAdapter {
    fn observed_activity(&self, _panes: &[Pane]) -> HashMap<u32, ObservedActivity> {
        smelt_statuses()
            .into_iter()
            .map(|(pid, status)| {
                (
                    pid,
                    ObservedActivity {
                        busy: status.state.is_busy(),
                        completed: matches!(status.state, SmeltState::Completed),
                    },
                )
            })
            .collect()
    }
}

pub(super) static SMELT_ADAPTER: SmeltAdapter = SmeltAdapter;

#[derive(Debug, Clone, Deserialize)]
struct SmeltStatus {
    pid: u32,
    state: SmeltState,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum SmeltState {
    Busy,
    Waiting,
    Completed,
    #[serde(other)]
    Inactive,
}

impl SmeltState {
    fn is_busy(self) -> bool {
        matches!(self, Self::Busy | Self::Waiting)
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn busy_and_waiting_states_are_active() {
        assert!(SmeltState::Busy.is_busy());
        assert!(SmeltState::Waiting.is_busy());
        assert!(!SmeltState::Completed.is_busy());
        assert!(!SmeltState::Inactive.is_busy());
    }

    #[test]
    fn inactive_provider_states_are_not_busy() {
        for state in ["idle", "needs_attention", "completed", "future_state"] {
            let json = format!(r#"{{"pid":1,"state":"{state}"}}"#);
            let status: SmeltStatus = serde_json::from_str(&json).unwrap();
            assert!(!status.state.is_busy());
        }
    }
}

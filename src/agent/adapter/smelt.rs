use std::collections::HashMap;
use std::process::Command;

use serde::Deserialize;

use crate::agent::Pane;

use super::ProviderAdapter;

pub(super) struct SmeltAdapter;

impl ProviderAdapter for SmeltAdapter {
    fn observed_activity(&self, _panes: &[Pane]) -> HashMap<u32, bool> {
        smelt_statuses()
            .into_iter()
            .map(|(pid, status)| (pid, status.state.is_busy()))
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
    #[serde(other)]
    Inactive,
}

impl SmeltState {
    fn is_busy(self) -> bool {
        matches!(self, Self::Busy)
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
    fn only_busy_state_is_active() {
        assert!(SmeltState::Busy.is_busy());
        assert!(!SmeltState::Inactive.is_busy());
    }

    #[test]
    fn all_non_busy_provider_states_are_inactive() {
        for state in ["idle", "needs_attention", "future_state"] {
            let json = format!(r#"{{"pid":1,"state":"{state}"}}"#);
            let status: SmeltStatus = serde_json::from_str(&json).unwrap();
            assert!(!status.state.is_busy());
        }
    }
}

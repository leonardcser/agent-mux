use std::collections::HashMap;
use std::process::Command;

use serde::Deserialize;

use crate::agent::{Activity, Pane};

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
                        activity: status.state.activity(),
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
    fn activity(self) -> Activity {
        match self {
            Self::Busy => Activity::Busy,
            Self::Waiting => Activity::Waiting,
            Self::Completed | Self::Inactive => Activity::Inactive,
        }
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

    fn activity(state: &str) -> Activity {
        let json = format!(r#"{{"pid":1,"state":"{state}"}}"#);
        serde_json::from_str::<SmeltStatus>(&json)
            .unwrap()
            .state
            .activity()
    }

    #[test]
    fn busy_and_waiting_are_distinct_activities() {
        assert_eq!(activity("busy"), Activity::Busy);
        assert_eq!(activity("waiting"), Activity::Waiting);
    }

    #[test]
    fn other_provider_states_are_inactive() {
        for state in ["idle", "needs_attention", "completed", "future_state"] {
            assert_eq!(activity(state), Activity::Inactive);
        }
    }
}

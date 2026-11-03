mod default;
mod smelt;

use std::collections::{HashMap, HashSet};

use crate::agent::{Activity, Pane};

use self::default::DEFAULT_ADAPTER;
use self::smelt::SMELT_ADAPTER;

#[derive(Clone, Copy)]
struct ObservedActivity {
    activity: Activity,
    completed: bool,
}

trait ProviderAdapter: Sync {
    fn observed_activity(&self, _panes: &[Pane]) -> HashMap<u32, ObservedActivity> {
        HashMap::new()
    }

    /// Whether the provider publishes its own activity. Panes it does not
    /// list are inactive rather than inferred from screen content, which
    /// also changes when the user scrolls or resizes.
    fn reports_activity(&self) -> bool {
        false
    }
}

fn adapter_for(provider: &str) -> &'static dyn ProviderAdapter {
    match provider {
        "smelt" => &SMELT_ADAPTER,
        "claude" | "codex" | "gemini" | "opencode" | "kimi" => &DEFAULT_ADAPTER,
        _ => &DEFAULT_ADAPTER,
    }
}

pub fn apply_provider_activity(panes: &mut [Pane]) {
    let providers: HashSet<String> = panes.iter().map(|pane| pane.provider.clone()).collect();

    for provider in providers {
        let adapter = adapter_for(&provider);
        let activity = adapter.observed_activity(panes);
        for pane in panes.iter_mut().filter(|pane| pane.provider == provider) {
            let observed = (pane.provider_pid > 0)
                .then(|| activity.get(&(pane.provider_pid as u32)))
                .flatten();
            if let Some(observed) = observed {
                pane.observed = Some(observed.activity);
                pane.completed = observed.completed;
            } else if adapter.reports_activity() {
                pane.observed = Some(Activity::Inactive);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::adapter_for;

    #[test]
    fn only_smelt_reports_its_own_activity() {
        assert!(adapter_for("smelt").reports_activity());
        for provider in ["claude", "codex", "gemini", "opencode", "kimi", "unknown"] {
            assert!(!adapter_for(provider).reports_activity());
        }
    }

    #[test]
    fn providers_without_overrides_share_the_default_adapter() {
        let default = adapter_for("unknown");
        for provider in ["claude", "codex", "gemini", "opencode", "kimi"] {
            assert!(std::ptr::eq(adapter_for(provider), default));
        }
    }
}

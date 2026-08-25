mod default;
mod smelt;

use std::collections::{HashMap, HashSet};

use crate::agent::Pane;

use self::default::DEFAULT_ADAPTER;
use self::smelt::SMELT_ADAPTER;

trait ProviderAdapter: Sync {
    fn observed_activity(&self, _panes: &[Pane]) -> HashMap<u32, bool> {
        HashMap::new()
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
        let activity = adapter_for(&provider).observed_activity(panes);
        for pane in panes.iter_mut().filter(|pane| pane.provider == provider) {
            if pane.provider_pid <= 0 {
                continue;
            }
            if let Some(busy) = activity.get(&(pane.provider_pid as u32)) {
                pane.observed_busy = Some(*busy);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::adapter_for;

    #[test]
    fn providers_without_overrides_share_the_default_adapter() {
        let default = adapter_for("unknown");
        for provider in ["claude", "codex", "gemini", "opencode", "kimi"] {
            assert!(std::ptr::eq(adapter_for(provider), default));
        }
    }
}

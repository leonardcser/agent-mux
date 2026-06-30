use std::collections::HashMap;

use chrono::{DateTime, Utc};

use crate::agent::persist::{CachedPane, Snapshot};
use crate::agent::{Pane, PaneStatus};

#[derive(Debug, Default)]
pub struct Reconciler {
    prev_content: HashMap<String, String>,
    unchanged_count: HashMap<String, usize>,
    prev_statuses: HashMap<String, PaneStatus>,
    prev_window_active: HashMap<String, bool>,
    last_active: HashMap<String, DateTime<Utc>>,
}

impl Reconciler {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn seed_from_snapshot(&mut self, snapshot: &Snapshot) {
        for cp in &snapshot.panes {
            let id = cp.pane_key().to_string();
            if !cp.content_hash.is_empty() {
                self.prev_content
                    .insert(id.clone(), cp.content_hash.clone());
            }
            if let Some(s) = cp.last_status {
                self.prev_statuses
                    .insert(id.clone(), PaneStatus::from_i32(s));
            }
            self.prev_window_active.insert(id.clone(), cp.window_active);
            if let Some(t) = cp.last_active {
                self.last_active.insert(id, t);
            }
        }
    }

    pub fn reconcile(&mut self, panes: &mut [Pane]) {
        let now = Utc::now();
        let mut alive = HashMap::new();
        for p in panes.iter_mut() {
            let id = p.pane_id.clone();
            alive.insert(id.clone(), true);
            let prev_status = self
                .prev_statuses
                .get(&id)
                .copied()
                .unwrap_or(PaneStatus::Idle);
            let raw_content_changed = !p.content_hash.is_empty()
                && self
                    .prev_content
                    .get(&id)
                    .is_none_or(|prev| *prev != p.content_hash);
            let focus_changed = self
                .prev_window_active
                .get(&id)
                .is_some_and(|prev| *prev != p.window_active);

            if let Some(observed_status) = p.observed_status {
                if observed_status == PaneStatus::Busy {
                    self.last_active.insert(id.clone(), now);
                    self.unchanged_count.insert(id.clone(), 0);
                }
                p.last_active = self.last_active.get(&id).copied();
                p.status = observed_status;
                self.track_pane(p);
                continue;
            }

            let content_changed = raw_content_changed && !focus_changed;
            let active_now = content_changed || p.content_moving;

            if active_now {
                self.last_active.insert(id.clone(), now);
                self.unchanged_count.insert(id.clone(), 0);
            } else if prev_status == PaneStatus::Busy {
                *self.unchanged_count.entry(id.clone()).or_default() += 1;
            }
            p.last_active = self.last_active.get(&id).copied();

            p.status = if active_now {
                if p.window_active && prev_status == PaneStatus::Idle {
                    PaneStatus::Idle
                } else {
                    PaneStatus::Busy
                }
            } else if prev_status == PaneStatus::Busy {
                if self.unchanged_count.get(&id).copied().unwrap_or_default() >= 2 {
                    if p.heuristic_attention {
                        PaneStatus::NeedsAttention
                    } else if p.window_active {
                        PaneStatus::Idle
                    } else {
                        PaneStatus::Unread
                    }
                } else {
                    PaneStatus::Busy
                }
            } else if p.heuristic_attention || prev_status == PaneStatus::NeedsAttention {
                PaneStatus::NeedsAttention
            } else if prev_status == PaneStatus::Unread {
                if p.window_active {
                    PaneStatus::Idle
                } else {
                    PaneStatus::Unread
                }
            } else {
                PaneStatus::Idle
            };

            self.track_pane(p);
        }

        self.prev_content.retain(|k, _| alive.contains_key(k));
        self.unchanged_count.retain(|k, _| alive.contains_key(k));
        self.prev_statuses.retain(|k, _| alive.contains_key(k));
        self.prev_window_active.retain(|k, _| alive.contains_key(k));
        self.last_active.retain(|k, _| alive.contains_key(k));
    }

    fn track_pane(&mut self, p: &Pane) {
        let id = p.pane_id.clone();
        if !p.content_hash.is_empty() {
            self.prev_content.insert(id.clone(), p.content_hash.clone());
        }
        self.prev_statuses.insert(id.clone(), p.status);
        self.prev_window_active.insert(id, p.window_active);
    }

    pub fn apply_to_cache(&self, panes: &mut [CachedPane]) {
        for cp in panes {
            let id = cp.pane_key().to_string();
            if let Some(h) = self.prev_content.get(&id) {
                cp.content_hash = h.clone();
            }
            if let Some(s) = self.prev_statuses.get(&id) {
                cp.last_status = Some(s.as_i32());
            }
            if let Some(active) = self.prev_window_active.get(&id) {
                cp.window_active = *active;
            }
            if let Some(t) = self.last_active.get(&id) {
                cp.last_active = Some(*t);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(status: PaneStatus, content_hash: &str, window_active: bool) -> Snapshot {
        Snapshot {
            version: 1,
            panes: vec![CachedPane {
                pane_id: "%1".to_string(),
                target: "s:1.1".to_string(),
                content_hash: content_hash.to_string(),
                last_status: Some(status.as_i32()),
                window_active,
                ..CachedPane::default()
            }],
            ..Snapshot::default()
        }
    }

    fn pane(content_hash: &str, window_active: bool, heuristic_attention: bool) -> Pane {
        Pane {
            pane_id: "%1".to_string(),
            target: "s:1.1".to_string(),
            content_hash: content_hash.to_string(),
            window_active,
            heuristic_attention,
            ..Pane::default()
        }
    }

    #[test]
    fn content_change_without_focus_change_marks_busy() {
        let mut reconciler = Reconciler::new();
        reconciler.seed_from_snapshot(&snapshot(PaneStatus::NeedsAttention, "old", false));
        let mut panes = vec![pane("new", false, true)];

        reconciler.reconcile(&mut panes);

        assert_eq!(panes[0].status, PaneStatus::Busy);
    }

    #[test]
    fn focus_change_content_redraw_does_not_mark_busy() {
        let mut reconciler = Reconciler::new();
        reconciler.seed_from_snapshot(&snapshot(PaneStatus::NeedsAttention, "old", true));
        let mut panes = vec![pane("new", false, true)];

        reconciler.reconcile(&mut panes);

        assert_eq!(panes[0].status, PaneStatus::NeedsAttention);
    }
}

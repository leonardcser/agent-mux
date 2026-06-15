use std::collections::HashMap;

use chrono::{DateTime, Utc};

use crate::agent::persist::{CachedPane, State};
use crate::agent::{Pane, PaneStatus};

#[derive(Debug, Clone)]
pub struct StatusOverride {
    status: PaneStatus,
    content_hash: String,
}

#[derive(Debug, Default)]
pub struct Reconciler {
    prev_content: HashMap<String, String>,
    unchanged_count: HashMap<String, usize>,
    prev_statuses: HashMap<String, PaneStatus>,
    overrides: HashMap<String, StatusOverride>,
    last_active: HashMap<String, DateTime<Utc>>,
}

impl Reconciler {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn seed_from_state(&mut self, state: &State) {
        for cp in &state.panes {
            let id = cp.pane_key().to_string();
            if !cp.content_hash.is_empty() {
                self.prev_content
                    .insert(id.clone(), cp.content_hash.clone());
            }
            if let Some(s) = cp.last_status {
                self.prev_statuses
                    .insert(id.clone(), PaneStatus::from_i32(s));
            }
            if let Some(s) = cp.status_override {
                self.overrides.insert(
                    id.clone(),
                    StatusOverride {
                        status: PaneStatus::from_i32(s),
                        content_hash: cp.content_hash.clone(),
                    },
                );
            }
            if let Some(t) = cp.last_active {
                self.last_active.insert(id, t);
            }
        }
    }

    pub fn merge_overrides(&mut self, state: &State) {
        for cp in &state.panes {
            let Some(status) = cp.status_override else {
                continue;
            };
            let id = cp.pane_key().to_string();
            let ov = StatusOverride {
                status: PaneStatus::from_i32(status),
                content_hash: cp.content_hash.clone(),
            };
            self.prev_statuses.insert(id.clone(), ov.status);
            self.overrides.insert(id, ov);
        }
    }

    pub fn merge_new_overrides(&mut self, prev: &State, fresh: &State) {
        let mut existing = HashMap::new();
        for cp in &prev.panes {
            if let Some(status) = cp.status_override {
                existing.insert(cp.pane_key().to_string(), status);
            }
        }
        for cp in &fresh.panes {
            let Some(status) = cp.status_override else {
                continue;
            };
            let id = cp.pane_key().to_string();
            if existing.get(&id).is_some_and(|old| *old == status) {
                continue;
            }
            let ov = StatusOverride {
                status: PaneStatus::from_i32(status),
                content_hash: cp.content_hash.clone(),
            };
            self.prev_statuses.insert(id.clone(), ov.status);
            self.overrides.insert(id, ov);
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
            let content_changed = !p.content_hash.is_empty()
                && self
                    .prev_content
                    .get(&id)
                    .is_none_or(|prev| *prev != p.content_hash);
            let active_now = content_changed || p.content_moving;

            if active_now {
                self.last_active.insert(id.clone(), now);
                self.unchanged_count.insert(id.clone(), 0);
            } else if prev_status == PaneStatus::Busy {
                *self.unchanged_count.entry(id.clone()).or_default() += 1;
            }
            p.last_active = self.last_active.get(&id).copied();

            if let Some(ov) = self.overrides.get(&id).cloned() {
                match ov.status {
                    PaneStatus::Unread => {
                        p.status = PaneStatus::Unread;
                        self.track_pane(p);
                        continue;
                    }
                    PaneStatus::Idle => {
                        if active_now && !p.window_active {
                            self.overrides.remove(&id);
                        } else {
                            p.status = PaneStatus::Idle;
                            self.track_pane(p);
                            continue;
                        }
                    }
                    _ => {
                        if active_now {
                            self.overrides.remove(&id);
                        } else {
                            p.status = ov.status;
                            self.track_pane(p);
                            continue;
                        }
                    }
                }
            }

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
            } else if p.heuristic_attention {
                PaneStatus::NeedsAttention
            } else if prev_status == PaneStatus::NeedsAttention {
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
        self.overrides.retain(|k, _| alive.contains_key(k));
        self.last_active.retain(|k, _| alive.contains_key(k));
    }

    fn track_pane(&mut self, p: &Pane) {
        let id = p.pane_id.clone();
        if !p.content_hash.is_empty() {
            self.prev_content.insert(id.clone(), p.content_hash.clone());
        }
        self.prev_statuses.insert(id, p.status);
    }

    pub fn apply_to_cache(&self, panes: &mut [CachedPane]) {
        for cp in panes {
            let id = cp.pane_key().to_string();
            if let Some(ov) = self.overrides.get(&id) {
                cp.status_override = Some(ov.status.as_i32());
                cp.content_hash = ov.content_hash.clone();
            }
            if let Some(h) = self.prev_content.get(&id) {
                cp.content_hash = h.clone();
            }
            if let Some(s) = self.prev_statuses.get(&id) {
                cp.last_status = Some(s.as_i32());
            }
            if let Some(t) = self.last_active.get(&id) {
                cp.last_active = Some(*t);
            }
        }
    }
}

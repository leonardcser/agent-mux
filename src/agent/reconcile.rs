use std::collections::HashMap;

use chrono::{DateTime, Utc};

use crate::agent::persist::{CachedPane, Snapshot};
use crate::agent::{Activity, Pane, PaneStatus};

const BUSY_UNCHANGED_POLLS: usize = 3;
// After a pane is resized, tmux reflows the existing content and the running
// program repaints itself over the next poll or two. Keep suppressing that
// churn until the content settles, capped so a genuinely busy pane recovers.
const RESIZE_SETTLE_POLLS: usize = 5;

fn activity_status(
    previous: PaneStatus,
    activity: Activity,
    focused: bool,
    newly_completed: bool,
) -> PaneStatus {
    if activity == Activity::Busy {
        PaneStatus::Busy
    } else if activity == Activity::Waiting {
        PaneStatus::Waiting
    } else if focused {
        PaneStatus::Idle
    } else if newly_completed || previous.is_working() || previous == PaneStatus::Unread {
        PaneStatus::Unread
    } else {
        PaneStatus::Idle
    }
}

#[derive(Debug, Default)]
pub struct Reconciler {
    prev_content: HashMap<String, String>,
    unchanged_count: HashMap<String, usize>,
    prev_statuses: HashMap<String, PaneStatus>,
    prev_completed: HashMap<String, bool>,
    prev_window_active: HashMap<String, bool>,
    prev_dimensions: HashMap<String, (u16, u16)>,
    resize_settling: HashMap<String, usize>,
    last_active: HashMap<String, DateTime<Utc>>,
    status_changed_at: HashMap<String, DateTime<Utc>>,
    park_epoch: HashMap<String, DateTime<Utc>>,
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
            self.prev_completed.insert(id.clone(), cp.completed);
            self.prev_window_active.insert(id.clone(), cp.window_active);
            if let Some(t) = cp.last_active {
                self.last_active.insert(id.clone(), t);
            }
            if let Some(t) = cp.status_changed_at.or(cp.last_active) {
                self.status_changed_at.insert(id.clone(), t);
            }
            if let Some(t) = cp.park_epoch.or(cp.status_changed_at).or(cp.last_active) {
                self.park_epoch.insert(id, t);
            }
        }
    }

    pub fn reconcile(&mut self, panes: &mut [Pane]) {
        let now = Utc::now();
        let mut alive = HashMap::new();
        for p in panes.iter_mut() {
            let id = p.pane_id.to_string();
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
            let dimensions_changed = self
                .prev_dimensions
                .get(&id)
                .is_some_and(|prev| *prev != (p.width, p.height));

            // A resize triggers a burst of content changes (tmux reflow, then the
            // program's own repaint) spread across several polls. Suppress that
            // burst until the content settles for one poll, bounded by a cap.
            let prev_settling = self.resize_settling.get(&id).copied().unwrap_or(0);
            let resize_suppressed = dimensions_changed || prev_settling > 0;
            let settling = if dimensions_changed {
                RESIZE_SETTLE_POLLS
            } else if prev_settling > 0 && raw_content_changed {
                prev_settling - 1
            } else {
                0
            };
            self.resize_settling.insert(id.clone(), settling);

            if let Some(observed) = p.observed {
                if observed == Activity::Busy {
                    self.last_active.insert(id.clone(), now);
                    self.unchanged_count.insert(id.clone(), 0);
                }
                p.last_active = self.last_active.get(&id).copied();
                let newly_completed =
                    p.completed && !self.prev_completed.get(&id).copied().unwrap_or(false);
                p.status = activity_status(prev_status, observed, p.window_active, newly_completed);
                self.track_pane(p, now);
                continue;
            }

            let content_changed = raw_content_changed && !focus_changed && !resize_suppressed;
            let active_now = content_changed;

            if active_now {
                self.last_active.insert(id.clone(), now);
                self.unchanged_count.insert(id.clone(), 0);
            } else if prev_status == PaneStatus::Busy {
                *self.unchanged_count.entry(id.clone()).or_default() += 1;
            }
            p.last_active = self.last_active.get(&id).copied();

            p.status = if !active_now
                && prev_status == PaneStatus::Busy
                && self.unchanged_count.get(&id).copied().unwrap_or_default() < BUSY_UNCHANGED_POLLS
            {
                PaneStatus::Busy
            } else {
                let activity = if active_now {
                    Activity::Busy
                } else {
                    Activity::Inactive
                };
                activity_status(prev_status, activity, p.window_active, false)
            };

            self.track_pane(p, now);
        }

        self.prev_content.retain(|k, _| alive.contains_key(k));
        self.unchanged_count.retain(|k, _| alive.contains_key(k));
        self.prev_statuses.retain(|k, _| alive.contains_key(k));
        self.prev_completed.retain(|k, _| alive.contains_key(k));
        self.prev_window_active.retain(|k, _| alive.contains_key(k));
        self.prev_dimensions.retain(|k, _| alive.contains_key(k));
        self.resize_settling.retain(|k, _| alive.contains_key(k));
        self.last_active.retain(|k, _| alive.contains_key(k));
        self.status_changed_at.retain(|k, _| alive.contains_key(k));
        self.park_epoch.retain(|k, _| alive.contains_key(k));
    }

    fn track_pane(&mut self, p: &mut Pane, now: DateTime<Utc>) {
        let id = p.pane_id.to_string();
        let previous = self.prev_statuses.get(&id).copied();
        let status_changed =
            previous != Some(p.status) || self.prev_completed.get(&id) != Some(&p.completed);
        let resumed = previous == Some(PaneStatus::Waiting) && p.status == PaneStatus::Busy;
        let changed_at = self.status_changed_at.entry(id.clone()).or_insert(now);
        if status_changed {
            *changed_at = now;
        }
        p.status_changed_at = Some(*changed_at);
        let epoch = self.park_epoch.entry(id.clone()).or_insert(now);
        if status_changed && !resumed {
            *epoch = now;
        }
        p.park_epoch = Some(*epoch);
        if !p.content_hash.is_empty() {
            self.prev_content.insert(id.clone(), p.content_hash.clone());
        }
        self.prev_statuses.insert(id.clone(), p.status);
        self.prev_completed.insert(id.clone(), p.completed);
        self.prev_window_active.insert(id.clone(), p.window_active);
        self.prev_dimensions.insert(id, (p.width, p.height));
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
            cp.completed = self.prev_completed.get(&id).copied().unwrap_or(false);
            cp.status_changed_at = self.status_changed_at.get(&id).copied();
            cp.park_epoch = self.park_epoch.get(&id).copied();
            if let Some(t) = self.last_active.get(&id) {
                cp.last_active = Some(*t);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::PaneId;

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

    fn pane(content_hash: &str, window_active: bool) -> Pane {
        Pane {
            target: "s:1.1".to_string(),
            content_hash: content_hash.to_string(),
            window_active,
            width: 80,
            height: 24,
            ..Pane::new(PaneId::parse("%1").unwrap())
        }
    }

    fn pane_with_dims(content_hash: &str, width: u16, height: u16) -> Pane {
        Pane {
            width,
            height,
            ..pane(content_hash, false)
        }
    }

    fn pane_with_observed(content_hash: &str, window_active: bool, busy: bool) -> Pane {
        Pane {
            observed: Some(if busy {
                Activity::Busy
            } else {
                Activity::Inactive
            }),
            ..pane(content_hash, window_active)
        }
    }

    #[test]
    fn state_change_time_ignores_output_and_survives_restart() {
        let started = DateTime::from_timestamp(100, 0).unwrap();
        let mut initial = snapshot(PaneStatus::Busy, "one", false);
        initial.panes[0].status_changed_at = Some(started);
        let mut reconciler = Reconciler::new();
        reconciler.seed_from_snapshot(&initial);
        let mut panes = vec![pane_with_observed("one", false, true)];
        reconciler.reconcile(&mut panes);
        panes[0].content_hash = "two".into();
        reconciler.reconcile(&mut panes);
        assert_eq!(panes[0].status_changed_at, Some(started));

        let mut cached = vec![CachedPane {
            pane_id: "%1".into(),
            ..Default::default()
        }];
        reconciler.apply_to_cache(&mut cached);
        let mut restarted = Reconciler::new();
        restarted.seed_from_snapshot(&Snapshot {
            panes: cached,
            ..Default::default()
        });
        restarted.reconcile(&mut panes);
        assert_eq!(panes[0].status_changed_at, Some(started));
        panes[0].observed = Some(Activity::Inactive);
        restarted.reconcile(&mut panes);
        assert!(panes[0].status_changed_at.unwrap() > started);
        assert_eq!(panes[0].status, PaneStatus::Unread);
    }

    #[test]
    fn completion_stays_distinct_from_read_state() {
        let mut reconciler = Reconciler::new();
        let mut panes = vec![pane_with_observed("same", false, false)];
        panes[0].completed = true;
        reconciler.reconcile(&mut panes);
        assert_eq!(panes[0].status, PaneStatus::Unread);
        panes[0].window_active = true;
        reconciler.reconcile(&mut panes);
        assert_eq!(panes[0].status, PaneStatus::Idle);
        assert!(panes[0].completed);
        let mut cached = crate::agent::persist::cache_panes(&panes);
        reconciler.apply_to_cache(&mut cached);
        let snapshot = Snapshot {
            panes: cached,
            ..Default::default()
        };
        let restored = crate::agent::persist::panes_from_snapshot(&snapshot);
        assert!(restored[0].completed);
        panes[0].window_active = false;
        reconciler.reconcile(&mut panes);
        assert_eq!(panes[0].status, PaneStatus::Idle);
        panes[0].completed = false;
        panes[0].observed = Some(Activity::Busy);
        reconciler.reconcile(&mut panes);
        assert_eq!(panes[0].status, PaneStatus::Busy);
        assert!(!panes[0].completed);
    }

    #[test]
    fn observed_busy_becoming_idle_marks_unfocused_pane_unread() {
        let mut reconciler = Reconciler::new();
        reconciler.seed_from_snapshot(&snapshot(PaneStatus::Busy, "same", false));
        let mut panes = vec![pane_with_observed("same", false, false)];

        reconciler.reconcile(&mut panes);

        assert_eq!(panes[0].status, PaneStatus::Unread);
    }

    #[test]
    fn observed_busy_becoming_idle_stays_read_when_focused() {
        let mut reconciler = Reconciler::new();
        reconciler.seed_from_snapshot(&snapshot(PaneStatus::Busy, "same", true));
        let mut panes = vec![pane_with_observed("same", true, false)];

        reconciler.reconcile(&mut panes);

        assert_eq!(panes[0].status, PaneStatus::Idle);
    }

    #[test]
    fn observed_idle_preserves_unread_until_focused() {
        let mut reconciler = Reconciler::new();
        reconciler.seed_from_snapshot(&snapshot(PaneStatus::Unread, "same", false));

        let mut panes = vec![pane_with_observed("same", false, false)];
        reconciler.reconcile(&mut panes);
        assert_eq!(panes[0].status, PaneStatus::Unread);

        let mut panes = vec![pane_with_observed("same", true, false)];
        reconciler.reconcile(&mut panes);
        assert_eq!(panes[0].status, PaneStatus::Idle);
    }

    #[test]
    fn observed_busy_marks_pane_busy() {
        let mut reconciler = Reconciler::new();
        reconciler.seed_from_snapshot(&snapshot(PaneStatus::Idle, "same", false));
        let mut panes = vec![pane_with_observed("same", false, true)];

        reconciler.reconcile(&mut panes);

        assert_eq!(panes[0].status, PaneStatus::Busy);
    }

    #[test]
    fn content_change_without_focus_change_marks_busy() {
        let mut reconciler = Reconciler::new();
        reconciler.seed_from_snapshot(&snapshot(PaneStatus::Unread, "old", false));
        let mut panes = vec![pane("new", false)];

        reconciler.reconcile(&mut panes);

        assert_eq!(panes[0].status, PaneStatus::Busy);
    }

    #[test]
    fn focus_change_redraw_preserves_unread_when_focus_moves_away() {
        let mut reconciler = Reconciler::new();
        reconciler.seed_from_snapshot(&snapshot(PaneStatus::Unread, "old", true));
        let mut panes = vec![pane("new", false)];

        reconciler.reconcile(&mut panes);

        assert_eq!(panes[0].status, PaneStatus::Unread);
    }

    #[test]
    fn busy_settles_after_three_unchanged_polls() {
        let mut reconciler = Reconciler::new();
        reconciler.seed_from_snapshot(&snapshot(PaneStatus::Busy, "same", false));

        for _ in 0..2 {
            let mut panes = vec![pane("same", false)];

            reconciler.reconcile(&mut panes);

            assert_eq!(panes[0].status, PaneStatus::Busy);
        }

        let mut panes = vec![pane("same", false)];

        reconciler.reconcile(&mut panes);

        assert_eq!(panes[0].status, PaneStatus::Unread);
    }

    #[test]
    fn resize_content_reflow_does_not_mark_busy() {
        let mut reconciler = Reconciler::new();
        reconciler.seed_from_snapshot(&snapshot(PaneStatus::Idle, "old", false));
        // A stable poll establishes the baseline dimensions.
        let mut panes = vec![pane_with_dims("old", 80, 24)];
        reconciler.reconcile(&mut panes);
        assert_eq!(panes[0].status, PaneStatus::Idle);

        // Resize reflows the captured content, changing the hash. This must
        // not be treated as new activity.
        let mut panes = vec![pane_with_dims("new", 120, 24)];
        reconciler.reconcile(&mut panes);
        assert_eq!(panes[0].status, PaneStatus::Idle);
    }

    #[test]
    fn resize_repaint_burst_across_polls_stays_read() {
        let mut reconciler = Reconciler::new();
        reconciler.seed_from_snapshot(&snapshot(PaneStatus::Idle, "old", false));
        // Baseline poll establishes the dimensions.
        let mut panes = vec![pane_with_dims("old", 80, 24)];
        reconciler.reconcile(&mut panes);

        // Poll where the resize is first observed: tmux reflow changes the hash.
        let mut panes = vec![pane_with_dims("reflow", 120, 24)];
        reconciler.reconcile(&mut panes);
        assert_eq!(panes[0].status, PaneStatus::Idle);

        // Next poll: the program repaints itself, changing the hash again while
        // the dimensions are already stable. This must still be suppressed.
        let mut panes = vec![pane_with_dims("repaint", 120, 24)];
        reconciler.reconcile(&mut panes);
        assert_eq!(panes[0].status, PaneStatus::Idle);

        // Content settles.
        let mut panes = vec![pane_with_dims("repaint", 120, 24)];
        reconciler.reconcile(&mut panes);
        assert_eq!(panes[0].status, PaneStatus::Idle);
    }

    #[test]
    fn content_change_after_resize_settles_marks_busy() {
        let mut reconciler = Reconciler::new();
        reconciler.seed_from_snapshot(&snapshot(PaneStatus::Idle, "old", false));
        // Baseline, resize, then let the content settle over the cap.
        for (hash, w) in [("old", 80), ("reflow", 120), ("done", 120), ("done", 120)] {
            let mut panes = vec![pane_with_dims(hash, w, 24)];
            reconciler.reconcile(&mut panes);
        }

        // Genuine new output after the resize settled must register as activity.
        let mut panes = vec![pane_with_dims("fresh", 120, 24)];
        reconciler.reconcile(&mut panes);
        assert_eq!(panes[0].status, PaneStatus::Busy);
    }

    #[test]
    fn content_change_at_stable_dimensions_marks_busy() {
        let mut reconciler = Reconciler::new();
        reconciler.seed_from_snapshot(&snapshot(PaneStatus::Idle, "old", false));
        let mut panes = vec![pane_with_dims("old", 80, 24)];
        reconciler.reconcile(&mut panes);

        let mut panes = vec![pane_with_dims("new", 80, 24)];
        reconciler.reconcile(&mut panes);
        assert_eq!(panes[0].status, PaneStatus::Busy);
    }

    #[test]
    fn content_change_starts_busy_in_focused_pane() {
        let mut reconciler = Reconciler::new();
        reconciler.seed_from_snapshot(&snapshot(PaneStatus::Idle, "old", true));
        let mut panes = vec![pane("new", true)];

        reconciler.reconcile(&mut panes);

        assert_eq!(panes[0].status, PaneStatus::Busy);
    }

    #[test]
    fn waiting_leaves_working_period_but_resuming_keeps_it() {
        let mut reconciler = Reconciler::new();
        reconciler.seed_from_snapshot(&snapshot(PaneStatus::Busy, "same", false));
        let mut panes = vec![pane_with_observed("same", false, true)];
        reconciler.reconcile(&mut panes);
        let busy_epoch = panes[0].park_epoch;

        let mut panes = vec![Pane {
            observed: Some(Activity::Waiting),
            ..pane_with_observed("same", false, false)
        }];
        reconciler.reconcile(&mut panes);
        assert_eq!(panes[0].status, PaneStatus::Waiting);
        let waiting_epoch = panes[0].park_epoch;
        assert_ne!(waiting_epoch, busy_epoch);

        let mut panes = vec![pane_with_observed("same", false, true)];
        reconciler.reconcile(&mut panes);
        assert_eq!(panes[0].status, PaneStatus::Busy);
        assert_eq!(panes[0].park_epoch, waiting_epoch);
    }

    #[test]
    fn waiting_becoming_inactive_marks_unfocused_pane_unread() {
        let mut reconciler = Reconciler::new();
        reconciler.seed_from_snapshot(&snapshot(PaneStatus::Waiting, "same", false));
        let mut panes = vec![pane_with_observed("same", false, false)];

        reconciler.reconcile(&mut panes);

        assert_eq!(panes[0].status, PaneStatus::Unread);
    }
}

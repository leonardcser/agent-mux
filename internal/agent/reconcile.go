package agent

// StatusOverride captures a user-toggled status that persists until the pane
// produces new output.
type StatusOverride struct {
	Status         PaneStatus
	WindowActivity int64
}

// Reconciler tracks per-pane activity and drives the status state machine:
//
//	Idle → Busy (activity detected)
//	Busy → Unread (activity stopped)
//	Busy → NeedsAttention (heuristic match)
//	* → NeedsAttention (heuristic match, when not busy)
//
// Both the TUI and the background watch daemon use this.
type Reconciler struct {
	prevActivity map[string]int64
	prevStatuses map[string]PaneStatus
	overrides    map[string]StatusOverride
}

func NewReconciler() *Reconciler {
	return &Reconciler{
		prevActivity: make(map[string]int64),
		prevStatuses: make(map[string]PaneStatus),
		overrides:    make(map[string]StatusOverride),
	}
}

// SeedFromState restores tracking state from a persisted State.
func (r *Reconciler) SeedFromState(state State) {
	for _, cp := range state.Panes {
		if cp.WindowActivity != 0 {
			r.prevActivity[cp.Target] = cp.WindowActivity
		}
		if cp.LastStatus != nil {
			r.prevStatuses[cp.Target] = PaneStatus(*cp.LastStatus)
		}
		if cp.StatusOverride != nil {
			r.overrides[cp.Target] = StatusOverride{
				Status:         PaneStatus(*cp.StatusOverride),
				WindowActivity: cp.WindowActivity,
			}
		}
	}
}

// Status returns the reconciler's tracked status for target, or StatusIdle.
func (r *Reconciler) Status(target string) PaneStatus {
	if s, ok := r.prevStatuses[target]; ok {
		return s
	}
	return StatusIdle
}

// SetOverride records a user-toggled status that sticks until the pane
// produces new output.
func (r *Reconciler) SetOverride(target string, status PaneStatus, activity int64) {
	r.overrides[target] = StatusOverride{
		Status:         status,
		WindowActivity: activity,
	}
	r.prevStatuses[target] = status
}

// ClearTarget removes all tracking state for a pane (used when marking as read).
func (r *Reconciler) ClearTarget(target string) {
	delete(r.overrides, target)
	delete(r.prevStatuses, target)
	delete(r.prevActivity, target)
}

// Seed populates activity baselines from fresh panes without running the
// state machine. Preserves cached statuses so the first real Reconcile has
// an accurate baseline. Called during the TUI's fast startup window.
func (r *Reconciler) Seed(panes []Pane) {
	alive := make(map[string]bool, len(panes))
	for i := range panes {
		p := &panes[i]
		alive[p.Target] = true
		r.prevActivity[p.Target] = p.WindowActivity
		if prev, ok := r.prevStatuses[p.Target]; ok {
			p.Status = prev
		}
	}
	r.cleanup(alive)
}

// Reconcile runs the status state machine on a fresh set of panes.
// Pane statuses are updated in place.
func (r *Reconciler) Reconcile(panes []Pane) {
	alive := make(map[string]bool, len(panes))
	for i := range panes {
		p := &panes[i]
		alive[p.Target] = true

		if ov, ok := r.overrides[p.Target]; ok {
			if p.WindowActivity > ov.WindowActivity {
				delete(r.overrides, p.Target)
			} else {
				p.Status = ov.Status
				r.prevActivity[p.Target] = p.WindowActivity
				r.prevStatuses[p.Target] = p.Status
				continue
			}
		}

		prev, seen := r.prevActivity[p.Target]
		if seen && p.WindowActivity > prev {
			p.Status = StatusBusy
		} else if p.HeuristicAttention {
			p.Status = StatusNeedsAttention
		} else if r.prevStatuses[p.Target] == StatusBusy {
			p.Status = StatusUnread
		} else if r.prevStatuses[p.Target] == StatusNeedsAttention ||
			r.prevStatuses[p.Target] == StatusUnread {
			p.Status = r.prevStatuses[p.Target]
		}

		r.prevActivity[p.Target] = p.WindowActivity
		r.prevStatuses[p.Target] = p.Status
	}
	r.cleanup(alive)
}

// MergeOverrides picks up overrides written by another process (e.g., the TUI
// writing an override that the watch daemon should respect). Only new overrides
// are absorbed — existing ones are left untouched.
func (r *Reconciler) MergeOverrides(state State) {
	for _, cp := range state.Panes {
		if cp.StatusOverride == nil {
			continue
		}
		if _, exists := r.overrides[cp.Target]; exists {
			continue
		}
		r.overrides[cp.Target] = StatusOverride{
			Status:         PaneStatus(*cp.StatusOverride),
			WindowActivity: cp.WindowActivity,
		}
	}
}

// ApplyToCache writes reconciler state (activity, statuses, overrides) onto
// a slice of CachedPanes for persistence.
func (r *Reconciler) ApplyToCache(panes []CachedPane) {
	for i := range panes {
		cp := &panes[i]
		if ov, ok := r.overrides[cp.Target]; ok {
			s := int(ov.Status)
			cp.StatusOverride = &s
			cp.WindowActivity = ov.WindowActivity
		}
		if a, ok := r.prevActivity[cp.Target]; ok {
			cp.WindowActivity = a
		}
		if s, ok := r.prevStatuses[cp.Target]; ok {
			v := int(s)
			cp.LastStatus = &v
		}
	}
}

// cleanup removes tracking for panes that no longer exist.
func (r *Reconciler) cleanup(alive map[string]bool) {
	for target := range r.prevActivity {
		if !alive[target] {
			delete(r.prevActivity, target)
			delete(r.prevStatuses, target)
			delete(r.overrides, target)
		}
	}
	// Catch orphaned overrides (e.g., merged from disk before the pane appeared).
	for target := range r.overrides {
		if !alive[target] {
			delete(r.overrides, target)
		}
	}
}

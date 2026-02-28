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
	PrevActivity map[string]int64
	PrevStatuses map[string]PaneStatus
	Overrides    map[string]StatusOverride
}

func NewReconciler() *Reconciler {
	return &Reconciler{
		PrevActivity: make(map[string]int64),
		PrevStatuses: make(map[string]PaneStatus),
		Overrides:    make(map[string]StatusOverride),
	}
}

// SeedFromState restores tracking state from a persisted State.
func (r *Reconciler) SeedFromState(state State) {
	for _, cp := range state.Panes {
		if cp.WindowActivity != 0 {
			r.PrevActivity[cp.Target] = cp.WindowActivity
		}
		if cp.LastStatus != nil {
			r.PrevStatuses[cp.Target] = PaneStatus(*cp.LastStatus)
		}
		if cp.StatusOverride != nil {
			r.Overrides[cp.Target] = StatusOverride{
				Status:         PaneStatus(*cp.StatusOverride),
				WindowActivity: cp.WindowActivity,
			}
		}
	}
}

// Seed populates activity baselines from fresh panes without running the
// state machine. Preserves cached statuses so the first real Reconcile has
// an accurate baseline. Called on the TUI's first load.
func (r *Reconciler) Seed(panes []Pane) {
	alive := make(map[string]bool, len(panes))
	for i := range panes {
		p := &panes[i]
		alive[p.Target] = true
		r.PrevActivity[p.Target] = p.WindowActivity
		if prev, ok := r.PrevStatuses[p.Target]; ok {
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

		if ov, ok := r.Overrides[p.Target]; ok {
			if p.WindowActivity > ov.WindowActivity {
				delete(r.Overrides, p.Target)
			} else {
				p.Status = ov.Status
				r.PrevActivity[p.Target] = p.WindowActivity
				r.PrevStatuses[p.Target] = p.Status
				continue
			}
		}

		prev, seen := r.PrevActivity[p.Target]
		if seen && p.WindowActivity > prev {
			p.Status = StatusBusy
		} else if p.HeuristicAttention {
			p.Status = StatusNeedsAttention
		} else if r.PrevStatuses[p.Target] == StatusBusy {
			p.Status = StatusUnread
		} else if r.PrevStatuses[p.Target] == StatusNeedsAttention ||
			r.PrevStatuses[p.Target] == StatusUnread {
			p.Status = r.PrevStatuses[p.Target]
		}

		r.PrevActivity[p.Target] = p.WindowActivity
		r.PrevStatuses[p.Target] = p.Status
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
		if _, exists := r.Overrides[cp.Target]; exists {
			continue
		}
		r.Overrides[cp.Target] = StatusOverride{
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
		if ov, ok := r.Overrides[cp.Target]; ok {
			s := int(ov.Status)
			cp.StatusOverride = &s
			cp.WindowActivity = ov.WindowActivity
		}
		if a, ok := r.PrevActivity[cp.Target]; ok {
			cp.WindowActivity = a
		}
		if s, ok := r.PrevStatuses[cp.Target]; ok {
			v := int(s)
			cp.LastStatus = &v
		}
	}
}

func (r *Reconciler) cleanup(alive map[string]bool) {
	for target := range r.PrevActivity {
		if !alive[target] {
			delete(r.PrevActivity, target)
			delete(r.PrevStatuses, target)
			delete(r.Overrides, target)
		}
	}
}

package agent

// StatusOverride captures a user-toggled status that persists until the pane
// produces new output.
type StatusOverride struct {
	Status      PaneStatus
	ContentHash string
}

// Reconciler tracks per-pane activity and drives the status state machine:
//
//	Idle → Busy (content changed)
//	Busy → Unread (content unchanged for 2 consecutive polls)
//	Busy → NeedsAttention (heuristic match)
//	* → NeedsAttention (heuristic match, when not busy)
//
// Both the TUI and the background watch daemon use this.
type Reconciler struct {
	prevContent    map[string]string
	unchangedCount map[string]int
	prevStatuses   map[string]PaneStatus
	overrides      map[string]StatusOverride
}

func NewReconciler() *Reconciler {
	return &Reconciler{
		prevContent:    make(map[string]string),
		unchangedCount: make(map[string]int),
		prevStatuses:   make(map[string]PaneStatus),
		overrides:      make(map[string]StatusOverride),
	}
}

// SeedFromState restores tracking state from a persisted State.
func (r *Reconciler) SeedFromState(state State) {
	for _, cp := range state.Panes {
		if cp.ContentHash != "" {
			r.prevContent[cp.Target] = cp.ContentHash
		}
		if cp.LastStatus != nil {
			r.prevStatuses[cp.Target] = PaneStatus(*cp.LastStatus)
		}
		if cp.StatusOverride != nil {
			r.overrides[cp.Target] = StatusOverride{
				Status:      PaneStatus(*cp.StatusOverride),
				ContentHash: cp.ContentHash,
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
func (r *Reconciler) SetOverride(target string, status PaneStatus, contentHash string) {
	r.overrides[target] = StatusOverride{
		Status:      status,
		ContentHash: contentHash,
	}
	r.prevStatuses[target] = status
}

// ClearTarget removes all tracking state for a pane (used when marking as read).
func (r *Reconciler) ClearTarget(target string) {
	delete(r.overrides, target)
	delete(r.prevStatuses, target)
	delete(r.prevContent, target)
	delete(r.unchangedCount, target)
}

// Reconcile runs the status state machine on a fresh set of panes.
// Pane statuses are updated in place.
func (r *Reconciler) Reconcile(panes []Pane) {
	alive := make(map[string]bool, len(panes))
	for i := range panes {
		p := &panes[i]
		alive[p.Target] = true

		contentChanged := p.ContentHash != "" && p.ContentHash != r.prevContent[p.Target]

		if ov, ok := r.overrides[p.Target]; ok {
			if contentChanged {
				delete(r.overrides, p.Target)
			} else {
				p.Status = ov.Status
				r.prevContent[p.Target] = p.ContentHash
				r.prevStatuses[p.Target] = p.Status
				continue
			}
		}

		if contentChanged {
			p.Status = StatusBusy
			r.unchangedCount[p.Target] = 0
		} else if r.prevStatuses[p.Target] == StatusBusy {
			r.unchangedCount[p.Target]++
			if r.unchangedCount[p.Target] >= 2 {
				if p.HeuristicAttention {
					p.Status = StatusNeedsAttention
				} else {
					p.Status = StatusUnread
				}
			} else {
				p.Status = StatusBusy
			}
		} else if p.HeuristicAttention {
			p.Status = StatusNeedsAttention
		} else if r.prevStatuses[p.Target] == StatusNeedsAttention ||
			r.prevStatuses[p.Target] == StatusUnread {
			p.Status = r.prevStatuses[p.Target]
		}

		if p.ContentHash != "" {
			r.prevContent[p.Target] = p.ContentHash
		}
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
		ov := StatusOverride{
			Status:      PaneStatus(*cp.StatusOverride),
			ContentHash: cp.ContentHash,
		}
		r.overrides[cp.Target] = ov
		r.prevStatuses[cp.Target] = ov.Status
	}
}

// ApplyToCache writes reconciler state (content hash, statuses, overrides) onto
// a slice of CachedPanes for persistence.
func (r *Reconciler) ApplyToCache(panes []CachedPane) {
	for i := range panes {
		cp := &panes[i]
		if ov, ok := r.overrides[cp.Target]; ok {
			s := int(ov.Status)
			cp.StatusOverride = &s
			cp.ContentHash = ov.ContentHash
		}
		if h, ok := r.prevContent[cp.Target]; ok {
			cp.ContentHash = h
		}
		if s, ok := r.prevStatuses[cp.Target]; ok {
			v := int(s)
			cp.LastStatus = &v
		}
	}
}

// cleanup removes tracking for panes that no longer exist.
func (r *Reconciler) cleanup(alive map[string]bool) {
	for target := range r.prevContent {
		if !alive[target] {
			delete(r.prevContent, target)
			delete(r.unchangedCount, target)
			delete(r.prevStatuses, target)
			delete(r.overrides, target)
		}
	}
	for target := range r.overrides {
		if !alive[target] {
			delete(r.overrides, target)
		}
	}
}

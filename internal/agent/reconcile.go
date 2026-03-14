package agent

import "time"

// StatusOverride captures a user-toggled status. Unread overrides persist
// through content changes (manual bookmark); others clear on new output.
type StatusOverride struct {
	Status      PaneStatus
	ContentHash string
}

// Reconciler tracks per-pane activity and drives the status state machine:
//
//	Idle → Busy (content changed)
//	Busy → Idle (content settled + user viewing window)
//	Busy → NeedsAttention (content settled + user not viewing, or heuristic match)
//	* → NeedsAttention (heuristic match, when not busy)
//
// All maps are keyed by PaneID (tmux's stable pane identifier).
// Both the TUI and the background watch daemon use this.
type Reconciler struct {
	prevContent    map[string]string
	unchangedCount map[string]int
	prevStatuses   map[string]PaneStatus
	overrides      map[string]StatusOverride
	lastActive     map[string]time.Time
}

func NewReconciler() *Reconciler {
	return &Reconciler{
		prevContent:    make(map[string]string),
		unchangedCount: make(map[string]int),
		prevStatuses:   make(map[string]PaneStatus),
		overrides:      make(map[string]StatusOverride),
		lastActive:     make(map[string]time.Time),
	}
}

// SeedFromState restores tracking state from a persisted State.
func (r *Reconciler) SeedFromState(state State) {
	for _, cp := range state.Panes {
		id := cp.paneKey()
		if cp.ContentHash != "" {
			r.prevContent[id] = cp.ContentHash
		}
		if cp.LastStatus != nil {
			r.prevStatuses[id] = PaneStatus(*cp.LastStatus)
		}
		if cp.StatusOverride != nil {
			r.overrides[id] = StatusOverride{
				Status:      PaneStatus(*cp.StatusOverride),
				ContentHash: cp.ContentHash,
			}
		}
		if cp.LastActive != nil {
			r.lastActive[id] = *cp.LastActive
		}
	}
}

// Status returns the reconciler's tracked status for a pane, or StatusIdle.
func (r *Reconciler) Status(paneID string) PaneStatus {
	if s, ok := r.prevStatuses[paneID]; ok {
		return s
	}
	return StatusIdle
}

// SetOverride records a user-toggled status. Unread overrides persist through
// content changes; others clear on new output.
func (r *Reconciler) SetOverride(paneID string, status PaneStatus, contentHash string) {
	r.overrides[paneID] = StatusOverride{
		Status:      status,
		ContentHash: contentHash,
	}
	r.prevStatuses[paneID] = status
}

// HasOverride reports whether target has an active user-set override.
func (r *Reconciler) HasOverride(paneID string) bool {
	_, ok := r.overrides[paneID]
	return ok
}

// ClearPane removes all tracking state for a pane (used when marking as read).
func (r *Reconciler) ClearPane(paneID string) {
	delete(r.overrides, paneID)
	delete(r.prevStatuses, paneID)
	delete(r.prevContent, paneID)
	delete(r.unchangedCount, paneID)
}

// Reconcile runs the status state machine on a fresh set of panes.
// Pane statuses are updated in place.
func (r *Reconciler) Reconcile(panes []Pane) {
	now := time.Now()
	alive := make(map[string]bool, len(panes))
	for i := range panes {
		p := &panes[i]
		id := p.PaneID
		alive[id] = true

		contentChanged := p.ContentHash != "" && p.ContentHash != r.prevContent[id]

		// Track per-pane activity: update on content change, apply if tracked.
		if contentChanged {
			r.lastActive[id] = now
		}
		if t, ok := r.lastActive[id]; ok {
			p.LastActive = t
		}

		if ov, ok := r.overrides[id]; ok {
			if contentChanged {
				delete(r.overrides, id)
			} else {
				p.Status = ov.Status
				r.prevContent[id] = p.ContentHash
				r.prevStatuses[id] = p.Status
				continue
			}
		}

		if contentChanged && !p.WindowActive {
			p.Status = StatusBusy
			r.unchangedCount[id] = 0
		} else if contentChanged {
			// Active pane: content changed but user is viewing it, stay idle.
			r.unchangedCount[id] = 0
		} else if r.prevStatuses[id] == StatusBusy {
			r.unchangedCount[id]++
			if r.unchangedCount[id] >= 2 {
				if p.HeuristicAttention {
					p.Status = StatusNeedsAttention
				} else if p.WindowActive {
					p.Status = StatusIdle
				} else {
					p.Status = StatusUnread
				}
			} else {
				p.Status = StatusBusy
			}
		} else if p.HeuristicAttention {
			p.Status = StatusNeedsAttention
		} else if r.prevStatuses[id] == StatusNeedsAttention {
			p.Status = StatusNeedsAttention
		} else if r.prevStatuses[id] == StatusUnread {
			if p.WindowActive {
				p.Status = StatusIdle
			} else {
				p.Status = StatusUnread
			}
		}

		if p.ContentHash != "" {
			r.prevContent[id] = p.ContentHash
		}
		r.prevStatuses[id] = p.Status
	}
	r.cleanup(alive)
}

// MergeOverrides picks up overrides written by another process (e.g., the TUI
// writing an override that the watch daemon should respect).
func (r *Reconciler) MergeOverrides(state State) {
	for _, cp := range state.Panes {
		if cp.StatusOverride == nil {
			continue
		}
		id := cp.paneKey()
		ov := StatusOverride{
			Status:      PaneStatus(*cp.StatusOverride),
			ContentHash: cp.ContentHash,
		}
		r.overrides[id] = ov
		r.prevStatuses[id] = ov.Status
	}
}

// MergeNewOverrides merges only overrides that appeared between two state reads.
// Overrides already present in prev were processed by Reconcile; re-applying
// them would undo cleared overrides (e.g. an Idle override cleared by content
// change would be re-applied from the stale state file).
func (r *Reconciler) MergeNewOverrides(prev, fresh State) {
	existing := make(map[string]int, len(prev.Panes))
	for _, cp := range prev.Panes {
		if cp.StatusOverride != nil {
			existing[cp.paneKey()] = *cp.StatusOverride
		}
	}
	for _, cp := range fresh.Panes {
		if cp.StatusOverride == nil {
			continue
		}
		id := cp.paneKey()
		if old, had := existing[id]; had && old == *cp.StatusOverride {
			continue
		}
		ov := StatusOverride{
			Status:      PaneStatus(*cp.StatusOverride),
			ContentHash: cp.ContentHash,
		}
		r.overrides[id] = ov
		r.prevStatuses[id] = ov.Status
	}
}

// ApplyToCache writes reconciler state (content hash, statuses, overrides) onto
// a slice of CachedPanes for persistence.
func (r *Reconciler) ApplyToCache(panes []CachedPane) {
	for i := range panes {
		cp := &panes[i]
		id := cp.paneKey()
		if ov, ok := r.overrides[id]; ok {
			s := int(ov.Status)
			cp.StatusOverride = &s
			cp.ContentHash = ov.ContentHash
		}
		if h, ok := r.prevContent[id]; ok {
			cp.ContentHash = h
		}
		if s, ok := r.prevStatuses[id]; ok {
			v := int(s)
			cp.LastStatus = &v
		}
		if t, ok := r.lastActive[id]; ok {
			cp.LastActive = &t
		}
	}
}

// cleanup removes tracking for panes that no longer exist.
func (r *Reconciler) cleanup(alive map[string]bool) {
	for id := range r.prevContent {
		if !alive[id] {
			delete(r.prevContent, id)
			delete(r.unchangedCount, id)
			delete(r.prevStatuses, id)
			delete(r.overrides, id)
			delete(r.lastActive, id)
		}
	}
	for id := range r.overrides {
		if !alive[id] {
			delete(r.overrides, id)
		}
	}
}

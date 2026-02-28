package agent

import (
	"context"
	"time"
)

// Watch runs a background poll loop that keeps the state file up to date.
// Designed to be started via `run-shell -b` in tmux.conf so the TUI always
// opens with accurate statuses.
func Watch(ctx context.Context) error {
	r := NewReconciler()
	if state, ok := LoadState(); ok {
		r.SeedFromState(state)
	}

	ticker := time.NewTicker(2 * time.Second)
	defer ticker.Stop()

	for {
		// Pick up overrides the TUI may have written while we were sleeping.
		if state, ok := LoadState(); ok {
			r.MergeOverrides(state)
		}

		if panes, err := ListPanes(); err == nil {
			r.Reconcile(panes)

			// Read existing state to preserve TUI-owned fields (LastPosition, SidebarWidth).
			state, _ := LoadState()
			paneRefs := make([]*Pane, len(panes))
			for i := range panes {
				paneRefs[i] = &panes[i]
			}
			state.Panes = CachePanes(paneRefs)
			r.ApplyToCache(state.Panes)
			_ = SaveState(state)
		}

		select {
		case <-ctx.Done():
			return nil
		case <-ticker.C:
		}
	}
}

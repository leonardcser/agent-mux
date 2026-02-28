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
		// Read state once per cycle: merge TUI overrides and preserve
		// TUI-owned fields (LastPosition, SidebarWidth) for the save.
		state, _ := LoadState()
		r.MergeOverrides(state)

		if panes, err := ListPanes(); err == nil {
			r.Reconcile(panes)

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

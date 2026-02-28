package agent

import (
	"context"
	"fmt"
	"os"
	"path/filepath"
	"syscall"
	"time"
)

// Watch runs a background poll loop that keeps the state file up to date.
// Designed to be started via `run-shell -b` in tmux.conf so the TUI always
// opens with accurate statuses.
func Watch(ctx context.Context) error {
	// Acquire an exclusive lock so only one watcher runs at a time.
	home, _ := os.UserHomeDir()
	dir := filepath.Join(home, ".local", "state", "agent-mux")
	_ = os.MkdirAll(dir, 0755)
	lockFile, err := os.OpenFile(filepath.Join(dir, "watch.lock"), os.O_CREATE|os.O_RDWR, 0644)
	if err != nil {
		return fmt.Errorf("watch: open lock: %w", err)
	}
	defer lockFile.Close()
	if err := syscall.Flock(int(lockFile.Fd()), syscall.LOCK_EX|syscall.LOCK_NB); err != nil {
		return nil // another watcher is already running
	}
	defer syscall.Flock(int(lockFile.Fd()), syscall.LOCK_UN)

	r := NewReconciler()
	if state, ok := LoadState(); ok {
		r.SeedFromState(state)
	}

	ticker := time.NewTicker(1 * time.Second)
	defer ticker.Stop()

	for {
		// Read state once per cycle: merge TUI overrides and preserve
		// TUI-owned fields (LastPosition, SidebarWidth) for the save.
		state, _ := LoadState()
		r.MergeOverrides(state)

		if panes, err := ListPanes(); err == nil {
			r.Reconcile(panes)

			// Preserve stashed state from the previous save.
			stashed := make(map[string]bool, len(state.Panes))
			for _, cp := range state.Panes {
				if cp.Stashed {
					stashed[cp.Target] = true
				}
			}

			paneRefs := make([]*Pane, len(panes))
			for i := range panes {
				panes[i].Stashed = stashed[panes[i].Target]
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

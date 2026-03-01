package agent

import (
	"context"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"strconv"
	"strings"
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

	// Write PID so RestartWatch can find us.
	lockFile.Truncate(0)
	lockFile.Seek(0, 0)
	fmt.Fprintf(lockFile, "%d", os.Getpid())

	r := NewReconciler()
	if state, ok := LoadState(); ok {
		r.SeedFromState(state)
	}

	const interval = 500 * time.Millisecond

	for {
		start := time.Now()

		// Read state once per cycle: merge TUI overrides and preserve
		// TUI-owned fields (LastPosition, SidebarWidth) for the save.
		state, _ := LoadState()
		r.MergeOverrides(state)

		if panes, err := ListPanes(); err == nil {
			r.Reconcile(panes)

			// Re-read state right before saving to pick up any stashed
			// or override changes the TUI wrote while ListPanes was running.
			fresh, _ := LoadState()
			r.MergeOverrides(fresh)
			stashed := make(map[string]bool, len(fresh.Panes))
			for _, cp := range fresh.Panes {
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

		// Sleep for the remainder of the interval, accounting for work time.
		elapsed := time.Since(start)
		if remaining := interval - elapsed; remaining > 0 {
			select {
			case <-ctx.Done():
				return nil
			case <-time.After(remaining):
			}
		} else {
			// Work took longer than interval; check cancellation without blocking.
			select {
			case <-ctx.Done():
				return nil
			default:
			}
		}
	}
}

// watchLockPath returns the path to the watch lock file.
func watchLockPath() string {
	home, _ := os.UserHomeDir()
	return filepath.Join(home, ".local", "state", "agent-mux", "watch.lock")
}

// RestartWatch kills the running watch process (if any) and spawns a new one.
func RestartWatch() error {
	lockPath := watchLockPath()

	// Kill existing watcher by reading its PID from the lock file.
	if data, err := os.ReadFile(lockPath); err == nil {
		if pid, err := strconv.Atoi(strings.TrimSpace(string(data))); err == nil && pid > 0 {
			if proc, err := os.FindProcess(pid); err == nil {
				_ = proc.Signal(syscall.SIGTERM)
			}
		}
	}

	// Spawn a new watcher. Use our own executable.
	exe, err := os.Executable()
	if err != nil {
		return fmt.Errorf("restart watch: %w", err)
	}
	cmd := exec.Command(exe, "watch")
	cmd.SysProcAttr = &syscall.SysProcAttr{Setpgid: true}
	return cmd.Start()
}

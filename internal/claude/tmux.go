package claude

import (
	"fmt"
	"os/exec"
	"strconv"
	"strings"
	"sync"
	"time"
)

// processTable holds a snapshot of the system process tree built from a single
// ps call, replacing per-pane pgrep invocations with in-memory lookups.
type processTable struct {
	children map[int][]int  // ppid -> child pids
	comm     map[int]string // pid -> command basename
}

// loadProcessTable snapshots the process tree via a single ps call.
func loadProcessTable() processTable {
	pt := processTable{
		children: make(map[int][]int),
		comm:     make(map[int]string),
	}
	out, err := exec.Command("ps", "-eo", "pid,ppid,comm").Output()
	if err != nil {
		return pt
	}
	for line := range strings.SplitSeq(strings.TrimSpace(string(out)), "\n") {
		fields := strings.Fields(line)
		if len(fields) < 3 {
			continue
		}
		pid, err1 := strconv.Atoi(fields[0])
		ppid, err2 := strconv.Atoi(fields[1])
		if err1 != nil || err2 != nil {
			continue
		}
		pt.children[ppid] = append(pt.children[ppid], pid)
		pt.comm[pid] = fields[2]
	}
	return pt
}

// rawPane holds parsed tmux pane info before status detection.
type rawPane struct {
	target, session, window, pane, path string
	pid                                 int
}

// parseTmuxPanes parses tmux list-panes output into rawPane structs.
func parseTmuxPanes(out []byte) []rawPane {
	var raw []rawPane
	for line := range strings.SplitSeq(strings.TrimSpace(string(out)), "\n") {
		if line == "" {
			continue
		}
		fields := strings.SplitN(line, "\t", 4)
		if len(fields) < 4 {
			continue
		}
		target, cmd, path, pidStr := fields[0], fields[1], fields[2], fields[3]
		if cmd != "claude" {
			continue
		}
		pid, _ := strconv.Atoi(pidStr)
		session, window, pane := parseTarget(target)
		raw = append(raw, rawPane{target, session, window, pane, path, pid})
	}
	return raw
}

// listTmuxPanes runs tmux list-panes and returns raw output.
func listTmuxPanes() ([]byte, error) {
	return exec.Command("tmux", "list-panes", "-a", "-F",
		"#{session_name}:#{window_index}.#{pane_index}\t#{pane_current_command}\t#{pane_current_path}\t#{pane_pid}").Output()
}

// ListClaudePanesBasic returns panes with StatusIdle (no status detection).
// Used for instant initial display before async status detection kicks in.
func ListClaudePanesBasic() ([]ClaudePane, error) {
	// Run tmux list-panes and history read in parallel.
	var (
		tmuxOut []byte
		tmuxErr error
		history map[string]time.Time
	)
	var wg sync.WaitGroup
	wg.Add(2)
	go func() {
		defer wg.Done()
		tmuxOut, tmuxErr = listTmuxPanes()
	}()
	go func() {
		defer wg.Done()
		history = LastActiveByProject()
	}()
	wg.Wait()

	if tmuxErr != nil {
		return nil, fmt.Errorf("tmux list-panes: %w", tmuxErr)
	}

	raw := parseTmuxPanes(tmuxOut)
	panes := make([]ClaudePane, len(raw))
	for i, r := range raw {
		panes[i] = ClaudePane{
			Target:     r.target,
			Session:    r.session,
			Window:     r.window,
			Pane:       r.pane,
			Path:       r.path,
			PID:        r.pid,
			Status:     StatusIdle,
			LastActive: history[r.path],
		}
	}
	return panes, nil
}

// ListClaudePanes returns all tmux panes currently running claude with full
// status detection. Runs tmux list-panes, history read, and process table
// snapshot in parallel, then detects per-pane attention status concurrently.
func ListClaudePanes() ([]ClaudePane, error) {
	// Run tmux list-panes, history read, and process table snapshot in parallel.
	var (
		tmuxOut []byte
		tmuxErr error
		history map[string]time.Time
		pt      processTable
	)
	var wg sync.WaitGroup
	wg.Add(3)
	go func() {
		defer wg.Done()
		tmuxOut, tmuxErr = listTmuxPanes()
	}()
	go func() {
		defer wg.Done()
		history = LastActiveByProject()
	}()
	go func() {
		defer wg.Done()
		pt = loadProcessTable()
	}()
	wg.Wait()

	if tmuxErr != nil {
		return nil, fmt.Errorf("tmux list-panes: %w", tmuxErr)
	}

	raw := parseTmuxPanes(tmuxOut)

	// Detect status sequentially. needsAttention calls tmux capture-pane,
	// and tmux serializes these internally via a server lock — concurrent
	// calls just contend on that lock. Sequential avoids the overhead.
	// isClaudeBusy is a pure in-memory lookup via the process table.
	panes := make([]ClaudePane, len(raw))
	for i, r := range raw {
		panes[i] = ClaudePane{
			Target:     r.target,
			Session:    r.session,
			Window:     r.window,
			Pane:       r.pane,
			Path:       r.path,
			PID:        r.pid,
			Status:     detectStatus(r.pid, r.target, &pt),
			LastActive: history[r.path],
		}
	}
	return panes, nil
}

// detectStatus determines whether Claude needs attention, is busy, or is idle.
func detectStatus(shellPID int, target string, pt *processTable) PaneStatus {
	if needsAttention(target) {
		return StatusNeedsAttention
	}
	if isClaudeBusy(shellPID, pt) {
		return StatusBusy
	}
	return StatusIdle
}

// isClaudeBusy checks if Claude is actively working by looking for caffeinate
// in the process tree. Uses the pre-loaded process table for pure in-memory
// lookups instead of spawning pgrep processes.
func isClaudeBusy(shellPID int, pt *processTable) bool {
	for _, childPID := range pt.children[shellPID] {
		for _, grandchildPID := range pt.children[childPID] {
			comm := pt.comm[grandchildPID]
			// Match basename — comm may be a full path like /usr/bin/caffeinate.
			if comm == "caffeinate" || strings.HasSuffix(comm, "/caffeinate") {
				return true
			}
		}
	}
	return false
}

// needsAttention checks if Claude is waiting for user interaction.
func needsAttention(target string) bool {
	out, err := exec.Command("tmux", "capture-pane", "-t", target, "-p", "-S", "-15").Output()
	if err != nil {
		return false
	}
	content := string(out)
	for _, pattern := range []string{
		// Tool permission prompts
		"Do you want to proceed?",
		"Do you want to allow",
		"Allow once",
		"press Enter to approve",
		// Question / selection prompts
		"Enter to select",
		"Type something",
		"Esc to cancel",
		// Waiting for user response
		"I'll wait for your",
		"waiting for your response",
		"Let me know when",
		"Please let me know",
		"What would you like",
		"How would you like",
		"Should I proceed",
		"Would you like me to",
		"please provide",
		"please specify",
		"I need more information",
		"Could you clarify",
		"awaiting your",
		"ready when you are",
		"let me know if you'd like",
		"Feel free to ask",
		"Is there anything else",
		"What else can I help",
		"Want me to go ahead",
		"Shall I",
		"Do you want me to",
		"Ready to proceed",
	} {
		if strings.Contains(content, pattern) {
			return true
		}
	}
	// Check if any of the last non-empty lines ends with a question mark.
	// This catches ad-hoc questions Claude asks that don't match explicit patterns.
	// We scan 15 lines back because the Claude CLI renders several UI elements
	// (separator, prompt, cost info) between the question and the bottom of the pane.
	lines := strings.Split(strings.TrimSpace(content), "\n")
	checked := 0
	for i := len(lines) - 1; i >= 0 && checked < 15; i-- {
		line := strings.TrimSpace(lines[i])
		if line == "" {
			continue
		}
		checked++
		if strings.HasSuffix(line, "?") {
			return true
		}
	}
	return false
}

// CapturePane captures the visible content of a tmux pane.
func CapturePane(target string, lines int) (string, error) {
	out, err := exec.Command("tmux", "capture-pane", "-t", target, "-e", "-p", "-S",
		fmt.Sprintf("-%d", lines)).Output()
	if err != nil {
		return "", fmt.Errorf("capture-pane %s: %w", target, err)
	}
	return string(out), nil
}

// SwitchToPane switches the tmux client to the given pane.
func SwitchToPane(target string) error {
	session, window, _ := parseTarget(target)
	sessionWindow := session + ":" + window
	if err := exec.Command("tmux", "switch-client", "-t", sessionWindow).Run(); err != nil {
		return fmt.Errorf("switch-client: %w", err)
	}
	if err := exec.Command("tmux", "select-pane", "-t", target).Run(); err != nil {
		return fmt.Errorf("select-pane: %w", err)
	}
	return nil
}

// KillPane kills a tmux pane. If it's the only pane in the window, kills the window instead.
func KillPane(target string) error {
	session, window, _ := parseTarget(target)
	sessionWindow := session + ":" + window

	out, err := exec.Command("tmux", "list-panes", "-t", sessionWindow).Output()
	if err != nil {
		return fmt.Errorf("list-panes: %w", err)
	}
	paneCount := len(strings.Split(strings.TrimSpace(string(out)), "\n"))

	if paneCount <= 1 {
		return exec.Command("tmux", "kill-window", "-t", sessionWindow).Run()
	}
	return exec.Command("tmux", "kill-pane", "-t", target).Run()
}

// parseTarget splits "foo:2.1" into session="foo", window="2", pane="1".
func parseTarget(s string) (session, window, pane string) {
	colonIdx := strings.LastIndex(s, ":")
	if colonIdx < 0 {
		return s, "", ""
	}
	session = s[:colonIdx]
	rest := s[colonIdx+1:]
	dotIdx := strings.LastIndex(rest, ".")
	if dotIdx < 0 {
		return session, rest, ""
	}
	return session, rest[:dotIdx], rest[dotIdx+1:]
}

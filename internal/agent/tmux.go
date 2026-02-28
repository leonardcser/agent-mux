package agent

import (
	"fmt"
	"os/exec"
	"strconv"
	"strings"
	"sync"
	"time"

	"github.com/leo/agent-mux/internal/provider"
)

// rawPane holds parsed tmux pane info before status detection.
type rawPane struct {
	target, session, window, windowName, pane, path, cmd string
	pid                                                  int
	windowActivity                                       int64
	heuristicAttention                                   bool
}

// parseTmuxPanes parses tmux list-panes output into rawPane structs.
func parseTmuxPanes(out []byte) []rawPane {
	var raw []rawPane
	for line := range strings.SplitSeq(strings.TrimSpace(string(out)), "\n") {
		if line == "" {
			continue
		}
		fields := strings.SplitN(line, "\t", 7)
		if len(fields) < 7 {
			continue
		}
		target, cmd, path, pidStr, windowName, actStr, attnStr := fields[0], fields[1], fields[2], fields[3], fields[4], fields[5], fields[6]
		pid, _ := strconv.Atoi(pidStr)
		activity, _ := strconv.ParseInt(actStr, 10, 64)
		// #{C/r:...} returns "0" when not found, or a line number when found.
		attention := attnStr != "" && attnStr != "0"
		session, window, pane := ParseTarget(target)
		raw = append(raw, rawPane{target, session, window, windowName, pane, path, cmd, pid, activity, attention})
	}
	return raw
}

// resolveAgentPanes filters raw panes to only those running a registered agent.
// Uses the process table to resolve agents that run under a generic command
// (e.g. gemini runs as "node").
func resolveAgentPanes(raw []rawPane, pt *provider.ProcessTable) []rawPane {
	var agents []rawPane
	for _, r := range raw {
		cmd := provider.Resolve(r.cmd, r.pid, pt)
		if cmd == "" {
			continue
		}
		r.cmd = cmd
		agents = append(agents, r)
	}
	return agents
}

// attentionPattern is a combined regex for all attention heuristics, used
// inline in the tmux format string via #{C/r:...} to avoid per-pane
// capture-pane calls.
// attentionPattern includes all fixed phrases plus a catch-all for lines
// ending in "?" (the trailing-question heuristic). The original code excluded
// lines starting with "❯" but #{C/r:...} searches the full history; this is
// an acceptable trade-off since false positives from prompt lines are rare.
const attentionPattern = `Do you want to proceed\?|Do you want to allow|Allow once|press Enter to approve|Enter to select|Type something|Esc to cancel|I'll wait for your|waiting for your response|Let me know when|Please let me know|What would you like|How would you like|Should I proceed|Would you like me to|please provide|please specify|I need more information|Could you clarify|awaiting your|ready when you are|let me know if you'd like|Feel free to ask|Is there anything else|What else can I help|Want me to|Shall I|Do you want me to|Ready to proceed`

// listTmuxPanes runs tmux list-panes and returns raw output.
// The format includes an inline content search (#{C/r:...}) for attention
// heuristics, eliminating the need for per-pane capture-pane calls.
func listTmuxPanes() ([]byte, error) {
	return exec.Command("tmux", "list-panes", "-a", "-F",
		"#{session_name}:#{window_index}.#{pane_index}\t#{pane_current_command}\t#{pane_current_path}\t#{pane_pid}\t#{window_name}\t#{window_activity}\t#{C/r:"+attentionPattern+"}").Output()
}

// loadProcessTable snapshots the process tree via a single ps call.
func loadProcessTable() provider.ProcessTable {
	out, err := exec.Command("ps", "-eo", "pid,ppid,comm,args").Output()
	if err != nil {
		return provider.ProcessTable{
			Children: make(map[int][]int),
			Comm:     make(map[int]string),
			Args:     make(map[int]string),
		}
	}
	return provider.ParseProcessTable(string(out))
}

// fetchPanes runs the tmux query, process table snapshot, and history read
// in parallel, then resolves agent panes and builds the Pane slice.
func fetchPanes() ([]Pane, error) {
	var (
		tmuxOut []byte
		tmuxErr error
		history map[string]time.Time
		pt      provider.ProcessTable
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

	raw := resolveAgentPanes(parseTmuxPanes(tmuxOut), &pt)
	panes := make([]Pane, len(raw))
	for i, r := range raw {
		panes[i] = Pane{
			Target:             r.target,
			Session:            r.session,
			Window:             r.window,
			WindowName:         r.windowName,
			Pane:               r.pane,
			Path:               r.path,
			PID:                r.pid,
			Status:             StatusIdle,
			WindowActivity:     r.windowActivity,
			LastActive:         history[r.path],
			HeuristicAttention: r.heuristicAttention,
		}
	}
	return panes, nil
}

// ListPanesBasic returns panes with StatusIdle (no enrichment or heuristics).
// Used for instant initial display before async status detection kicks in.
func ListPanesBasic() ([]Pane, error) {
	return fetchPanes()
}

// ListPanes returns all tmux panes running a registered agent with full
// attention heuristics and git enrichment.
func ListPanes() ([]Pane, error) {
	panes, err := fetchPanes()
	if err != nil {
		return nil, err
	}

	// Attention heuristics are now resolved inline via #{C/r:...} in the
	// tmux format string (see listTmuxPanes), so only git enrichment remains.
	EnrichPanes(panes)
	return panes, nil
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
	session, window, _ := ParseTarget(target)
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
	session, window, _ := ParseTarget(target)
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
func ParseTarget(s string) (session, window, pane string) {
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

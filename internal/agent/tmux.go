package agent

import (
	"crypto/sha256"
	"fmt"
	"os/exec"
	"regexp"
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
}

// parseTmuxPanes parses tmux list-panes output into rawPane structs.
func parseTmuxPanes(out []byte) []rawPane {
	var raw []rawPane
	for line := range strings.SplitSeq(strings.TrimSpace(string(out)), "\n") {
		if line == "" {
			continue
		}
		fields := strings.SplitN(line, "\t", 5)
		if len(fields) < 5 {
			continue
		}
		target, cmd, path, pidStr, windowName := fields[0], fields[1], fields[2], fields[3], fields[4]
		pid, _ := strconv.Atoi(pidStr)
		session, window, pane := ParseTarget(target)
		raw = append(raw, rawPane{target, session, window, windowName, pane, path, cmd, pid})
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

// attentionRe matches attention heuristic phrases in captured pane content.
var attentionRe = regexp.MustCompile(`Do you want to proceed\?|Do you want to allow|Allow once|press Enter to approve|Enter to select|Type something|Esc to cancel|I'll wait for your|waiting for your response|Let me know when|Please let me know|What would you like|How would you like|Should I proceed|Would you like me to|please provide|please specify|I need more information|Could you clarify|awaiting your|ready when you are|let me know if you'd like|Feel free to ask|Is there anything else|What else can I help|Want me to|Shall I|Do you want me to|Ready to proceed`)

// listTmuxPanes runs tmux list-panes and returns raw output.
func listTmuxPanes() ([]byte, error) {
	return exec.Command("tmux", "list-panes", "-a", "-F",
		"#{session_name}:#{window_index}.#{pane_index}\t#{pane_current_command}\t#{pane_current_path}\t#{pane_pid}\t#{window_name}").Output()
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
			Target:     r.target,
			Session:    r.session,
			Window:     r.window,
			WindowName: r.windowName,
			Pane:       r.pane,
			Path:       r.path,
			PID:        r.pid,
			Status:     StatusIdle,
			LastActive: history[r.path],
		}
	}
	return panes, nil
}

// capturePaneContent captures the last 10 lines of a tmux pane and returns
// a content hash and whether the content matches attention heuristics.
func capturePaneContent(target string) (hash string, attention bool) {
	out, err := exec.Command("tmux", "capture-pane", "-t", target, "-p", "-S", "-10").Output()
	if err != nil {
		return "", false
	}
	h := sha256.Sum256(out)
	return fmt.Sprintf("%x", h[:8]), attentionRe.Match(out)
}

// CaptureContent populates ContentHash and HeuristicAttention on each pane
// by capturing the last 10 lines in parallel.
func CaptureContent(panes []Pane) {
	var wg sync.WaitGroup
	for i := range panes {
		wg.Add(1)
		go func(idx int) {
			defer wg.Done()
			panes[idx].ContentHash, panes[idx].HeuristicAttention = capturePaneContent(panes[idx].Target)
		}(i)
	}
	wg.Wait()
}

// ListPanesBasic returns panes with StatusIdle (no content capture or enrichment).
// Used for instant initial display before async status detection kicks in.
func ListPanesBasic() ([]Pane, error) {
	return fetchPanes()
}

// ListPanes returns all tmux panes running a registered agent with content
// capture, attention heuristics, and git enrichment.
func ListPanes() ([]Pane, error) {
	panes, err := fetchPanes()
	if err != nil {
		return nil, err
	}
	CaptureContent(panes)
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

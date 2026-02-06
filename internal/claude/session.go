package claude

import (
	"os"
	"path/filepath"
	"sort"
	"strings"
	"time"
)

// PaneStatus represents the state of a Claude pane.
type PaneStatus int

const (
	StatusIdle           PaneStatus = iota // waiting for user input
	StatusBusy                             // Claude is working
	StatusNeedsAttention                   // Claude needs user attention
)

// ClaudePane represents a tmux pane running Claude.
type ClaudePane struct {
	Target     string // e.g. "main:2.1"
	Session    string
	Window     string
	Pane       string
	Path       string
	PID        int
	Status     PaneStatus
	LastActive time.Time
}

// Workspace groups panes by working directory.
type Workspace struct {
	Path      string
	ShortPath string
	GitBranch string
	Panes     []ClaudePane
}

// GroupByWorkspace groups panes by their working directory.
func GroupByWorkspace(panes []ClaudePane) []Workspace {
	home, _ := os.UserHomeDir()
	groups := make(map[string][]ClaudePane)
	for _, p := range panes {
		groups[p.Path] = append(groups[p.Path], p)
	}

	var workspaces []Workspace
	for path, ps := range groups {
		short := filepath.Base(path)
		if short == "." || short == "/" {
			short = path
			if home != "" && strings.HasPrefix(short, home) {
				short = "~" + strings.TrimPrefix(short, home)
			}
		}
		workspaces = append(workspaces, Workspace{
			Path:      path,
			ShortPath: short,
			GitBranch: gitBranch(path),
			Panes:     ps,
		})
	}

	sort.Slice(workspaces, func(i, j int) bool {
		return workspaces[i].Path < workspaces[j].Path
	})
	return workspaces
}

// gitBranch returns the current git branch by reading .git/HEAD directly,
// avoiding a process spawn. Returns "" if not a git repo or on any error.
func gitBranch(dir string) string {
	data, err := os.ReadFile(filepath.Join(dir, ".git", "HEAD"))
	if err != nil {
		return ""
	}
	ref := strings.TrimSpace(string(data))
	// Normal branch: "ref: refs/heads/main"
	if branch, ok := strings.CutPrefix(ref, "ref: refs/heads/"); ok {
		return branch
	}
	// Detached HEAD — return short sha
	if len(ref) >= 8 {
		return ref[:8]
	}
	return ref
}

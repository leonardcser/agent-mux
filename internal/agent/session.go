package agent

import (
	"os"
	"os/exec"
	"path/filepath"
	"sort"
	"strings"
	"time"
)

// PaneStatus represents the state of an agent pane.
type PaneStatus int

const (
	StatusIdle           PaneStatus = iota // waiting for user input
	StatusBusy                             // agent is working
	StatusNeedsAttention                   // agent needs user attention
)

// Pane represents a tmux pane running an AI coding agent.
type Pane struct {
	Target     string // e.g. "main:2.1"
	Session    string
	Window     string
	Pane       string
	Path       string
	PID        int
	Status      PaneStatus
	ContentHash uint64
	LastActive  time.Time
	Stashed     bool
}

// Workspace groups panes by working directory.
type Workspace struct {
	Path      string
	ShortPath string
	GitBranch string
	GitDirty  bool
	Panes     []Pane
}

// GroupByWorkspace groups panes by their working directory.
func GroupByWorkspace(panes []Pane) []Workspace {
	home, _ := os.UserHomeDir()
	groups := make(map[string][]Pane)
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
			GitDirty:  gitDirty(path),
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
	if branch, ok := strings.CutPrefix(ref, "ref: refs/heads/"); ok {
		return branch
	}
	if len(ref) >= 8 {
		return ref[:8]
	}
	return ref
}

// gitDirty returns true if the git working tree has uncommitted changes.
func gitDirty(dir string) bool {
	cmd := exec.Command("git", "status", "--porcelain")
	cmd.Dir = dir
	out, err := cmd.Output()
	if err != nil {
		return false
	}
	return len(strings.TrimSpace(string(out))) > 0
}

package agent

import (
	"os"
	"os/exec"
	"path/filepath"
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
	Target      string // e.g. "main:2.1"
	Session     string
	Window      string
	Pane        string
	Path        string
	ShortPath   string
	GitBranch   string
	GitDirty    bool
	PID         int
	Status             PaneStatus
	ContentHash        uint64
	HeuristicAttention bool
	LastActive         time.Time
	Stashed            bool
}

// EnrichPanes populates workspace metadata (ShortPath, GitBranch, GitDirty) on each pane.
// Metadata is computed once per unique path.
func EnrichPanes(panes []Pane) {
	home, _ := os.UserHomeDir()
	type wsInfo struct {
		ShortPath string
		GitBranch string
		GitDirty  bool
	}
	cache := make(map[string]wsInfo)
	for i := range panes {
		p := &panes[i]
		info, ok := cache[p.Path]
		if !ok {
			short := filepath.Base(p.Path)
			if short == "." || short == "/" {
				short = p.Path
				if home != "" && strings.HasPrefix(short, home) {
					short = "~" + strings.TrimPrefix(short, home)
				}
			}
			info = wsInfo{
				ShortPath: short,
				GitBranch: gitBranch(p.Path),
				GitDirty:  gitDirty(p.Path),
			}
			cache[p.Path] = info
		}
		p.ShortPath = info.ShortPath
		p.GitBranch = info.GitBranch
		p.GitDirty = info.GitDirty
	}
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

package agent

import (
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"sync"
	"time"
)

// PaneStatus represents the state of an agent pane.
type PaneStatus int

const (
	StatusIdle           PaneStatus = iota // waiting for user input
	StatusBusy                             // agent is working
	StatusNeedsAttention                   // agent needs user attention
	StatusUnread                           // finished but not yet viewed
)

// Pane represents a tmux pane running an AI coding agent.
type Pane struct {
	Target             string // e.g. "main:2.1"
	Session            string
	Window             string
	WindowName         string
	Pane               string
	Path               string
	ShortPath          string
	GitBranch          string
	GitDirty           bool
	PID                int
	Status             PaneStatus
	ContentHash        string
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

	unique := make(map[string]*wsInfo)
	for i := range panes {
		if _, ok := unique[panes[i].Path]; !ok {
			short := filepath.Base(panes[i].Path)
			if short == "." || short == "/" {
				short = panes[i].Path
				if home != "" && strings.HasPrefix(short, home) {
					short = "~" + strings.TrimPrefix(short, home)
				}
			}
			unique[panes[i].Path] = &wsInfo{ShortPath: short}
		}
	}

	var wg sync.WaitGroup
	for path, info := range unique {
		wg.Add(1)
		go func(path string, info *wsInfo) {
			defer wg.Done()
			info.GitBranch = gitBranch(path)
			info.GitDirty = gitDirty(path)
		}(path, info)
	}
	wg.Wait()

	for i := range panes {
		info := unique[panes[i].Path]
		panes[i].ShortPath = info.ShortPath
		panes[i].GitBranch = info.GitBranch
		panes[i].GitDirty = info.GitDirty
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

// gitDirtyCache caches git dirty state per directory, keyed by .git/index mtime.
var gitDirtyCache struct {
	mu      sync.Mutex
	entries map[string]gitDirtyCacheEntry
}

type gitDirtyCacheEntry struct {
	indexMtime time.Time
	dirty      bool
}

func init() {
	gitDirtyCache.entries = make(map[string]gitDirtyCacheEntry)
}

// gitDirty returns true if the git working tree has uncommitted changes.
// Results are cached and only recomputed when .git/index mtime changes.
func gitDirty(dir string) bool {
	indexPath := filepath.Join(dir, ".git", "index")
	info, err := os.Stat(indexPath)
	if err != nil {
		return false
	}
	mtime := info.ModTime()

	gitDirtyCache.mu.Lock()
	if cached, ok := gitDirtyCache.entries[dir]; ok && cached.indexMtime.Equal(mtime) {
		gitDirtyCache.mu.Unlock()
		return cached.dirty
	}
	gitDirtyCache.mu.Unlock()

	cmd := exec.Command("git", "status", "--porcelain")
	cmd.Dir = dir
	out, err := cmd.Output()
	if err != nil {
		return false
	}
	dirty := len(strings.TrimSpace(string(out))) > 0

	gitDirtyCache.mu.Lock()
	gitDirtyCache.entries[dir] = gitDirtyCacheEntry{indexMtime: mtime, dirty: dirty}
	gitDirtyCache.mu.Unlock()
	return dirty
}

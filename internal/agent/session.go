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
	StatusNeedsAttention                   // heuristic-detected attention
	StatusUnread                           // finished but not viewed, or manually bookmarked
)

// Pane represents a tmux pane running an AI coding agent.
type Pane struct {
	PaneID             string // stable tmux pane id, e.g. "%42"
	Target             string // e.g. "main:2.1"
	Session            string
	Window             string
	WindowName         string
	Pane               string
	Path               string
	ShortPath          string
	ProjectRoot        string // main repo path; equals Path when not a worktree
	ProjectShort       string // basename of ProjectRoot
	ProjectBranch      string // branch of ProjectRoot
	ProjectDirty       bool   // dirty state of ProjectRoot
	GitBranch          string
	GitDirty           bool
	PID                int
	Status             PaneStatus
	ContentHash        string
	ContentMoving      bool
	HeuristicAttention bool
	WindowActive       bool
	LastActive         time.Time
	Stashed            bool
	Order              int    // position in tmux list-panes output
	Provider           string // resolved agent provider name (claude, codex, kimi, etc.)
}

// EnrichPanes populates workspace metadata (ShortPath, GitBranch, GitDirty,
// ProjectRoot, ProjectShort) on each pane. Metadata is computed once per
// unique path.
func EnrichPanes(panes []Pane) {
	home, _ := os.UserHomeDir()
	shorten := func(p string) string {
		short := filepath.Base(p)
		if short == "." || short == "/" {
			short = p
			if home != "" && strings.HasPrefix(short, home) {
				short = "~" + strings.TrimPrefix(short, home)
			}
		}
		return short
	}

	type wsInfo struct {
		ShortPath    string
		ProjectRoot  string
		ProjectShort string
		GitBranch    string
		GitDirty     bool
	}

	unique := make(map[string]*wsInfo)
	for i := range panes {
		if _, ok := unique[panes[i].Path]; !ok {
			unique[panes[i].Path] = &wsInfo{ShortPath: shorten(panes[i].Path)}
		}
	}

	var wg sync.WaitGroup
	for path, info := range unique {
		wg.Add(1)
		go func(path string, info *wsInfo) {
			defer wg.Done()
			info.GitBranch = gitBranch(path)
			info.GitDirty = gitDirty(path)
			root := projectRoot(path)
			info.ProjectRoot = root
			info.ProjectShort = shorten(root)
		}(path, info)
	}
	wg.Wait()

	// Resolve branch/dirty for each unique project root (parallel) so the
	// project header can show the main-repo branch even when no pane lives
	// at the root path.
	type projInfo struct {
		Branch string
		Dirty  bool
	}
	projects := make(map[string]*projInfo)
	for _, info := range unique {
		if _, ok := projects[info.ProjectRoot]; !ok {
			projects[info.ProjectRoot] = &projInfo{}
		}
	}
	var pwg sync.WaitGroup
	for root, pi := range projects {
		pwg.Add(1)
		go func(root string, pi *projInfo) {
			defer pwg.Done()
			pi.Branch = gitBranch(root)
			pi.Dirty = gitDirty(root)
		}(root, pi)
	}
	pwg.Wait()

	for i := range panes {
		info := unique[panes[i].Path]
		panes[i].ShortPath = info.ShortPath
		panes[i].ProjectRoot = info.ProjectRoot
		panes[i].ProjectShort = info.ProjectShort
		panes[i].GitBranch = info.GitBranch
		panes[i].GitDirty = info.GitDirty
		if pi := projects[info.ProjectRoot]; pi != nil {
			panes[i].ProjectBranch = pi.Branch
			panes[i].ProjectDirty = pi.Dirty
		}
	}
}

// projectRoot returns the main repo path for dir. If dir is a git worktree
// (i.e. <dir>/.git is a file), the main repo is parsed from its gitdir
// pointer. Otherwise dir itself is returned.
func projectRoot(dir string) string {
	gitPath := filepath.Join(dir, ".git")
	info, err := os.Lstat(gitPath)
	if err != nil {
		return dir
	}
	if info.IsDir() {
		return dir
	}
	data, err := os.ReadFile(gitPath)
	if err != nil {
		return dir
	}
	line := strings.TrimSpace(string(data))
	gitdir, ok := strings.CutPrefix(line, "gitdir:")
	if !ok {
		return dir
	}
	gitdir = strings.TrimSpace(gitdir)
	if !filepath.IsAbs(gitdir) {
		gitdir = filepath.Join(dir, gitdir)
	}
	gitdir = filepath.Clean(gitdir)
	// Expect <main>/.git/worktrees/<name>; strip the trailing two segments
	// plus the .git component to recover <main>.
	parent := filepath.Dir(filepath.Dir(gitdir))
	if filepath.Base(parent) != ".git" {
		return dir
	}
	return filepath.Dir(parent)
}

// resolveGitDir returns the directory containing HEAD/index for dir. For a
// regular repo this is <dir>/.git; for a worktree this is the path the
// worktree's .git file points to. Returns "" if dir is not a git repo.
func resolveGitDir(dir string) string {
	gitPath := filepath.Join(dir, ".git")
	info, err := os.Lstat(gitPath)
	if err != nil {
		return ""
	}
	if info.IsDir() {
		return gitPath
	}
	data, err := os.ReadFile(gitPath)
	if err != nil {
		return ""
	}
	gitdir, ok := strings.CutPrefix(strings.TrimSpace(string(data)), "gitdir:")
	if !ok {
		return ""
	}
	gitdir = strings.TrimSpace(gitdir)
	if !filepath.IsAbs(gitdir) {
		gitdir = filepath.Join(dir, gitdir)
	}
	return filepath.Clean(gitdir)
}

// gitBranch returns the current git branch by reading HEAD directly,
// avoiding a process spawn. Returns "" if not a git repo or on any error.
func gitBranch(dir string) string {
	gitdir := resolveGitDir(dir)
	if gitdir == "" {
		return ""
	}
	data, err := os.ReadFile(filepath.Join(gitdir, "HEAD"))
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
// Results are cached and only recomputed when index mtime changes.
func gitDirty(dir string) bool {
	gitdir := resolveGitDir(dir)
	if gitdir == "" {
		return false
	}
	info, err := os.Stat(filepath.Join(gitdir, "index"))
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

package agent

import (
	"encoding/json"
	"os"
	"path/filepath"
	"sync"
	"syscall"
	"time"
)

type CachedPane struct {
	PaneID         string     `json:"paneID,omitempty"`
	Target         string     `json:"target"`
	WindowName     string     `json:"windowName,omitempty"`
	Path           string     `json:"path"`
	ShortPath      string     `json:"shortPath"`
	ProjectRoot    string     `json:"projectRoot,omitempty"`
	ProjectShort   string     `json:"projectShort,omitempty"`
	ProjectBranch  string     `json:"projectBranch,omitempty"`
	ProjectDirty   bool       `json:"projectDirty,omitempty"`
	GitBranch      string     `json:"gitBranch,omitempty"`
	GitDirty       bool       `json:"gitDirty,omitempty"`
	Stashed        bool       `json:"stashed"`
	Order          int        `json:"order,omitempty"`
	Provider       string     `json:"provider,omitempty"`
	StatusOverride *int       `json:"statusOverride,omitempty"`
	ContentHash    string     `json:"contentHash,omitempty"`
	LastStatus     *int       `json:"lastStatus,omitempty"`
	LastActive     *time.Time `json:"lastActive,omitempty"`
}

type State struct {
	Version      int          `json:"version"`
	Panes        []CachedPane `json:"panes"`
	LastPosition LastPosition `json:"lastPosition"`
	SidebarWidth int          `json:"sidebarWidth,omitempty"`
	UpdatedAt    *time.Time   `json:"updatedAt,omitempty"`
}

type LastPosition struct {
	PaneID      string `json:"pane_id,omitempty"`
	PaneTarget  string `json:"pane_target"`
	Cursor      int    `json:"cursor"`
	ScrollStart int    `json:"scroll_start"`
}

// paneKey returns the stable identity key for a cached pane.
// Uses PaneID when available, falling back to Target for old state files.
func (cp CachedPane) paneKey() string {
	if cp.PaneID != "" {
		return cp.PaneID
	}
	return cp.Target
}

var stateDir sync.Once

func stateDirPath() string {
	home, _ := os.UserHomeDir()
	dir := filepath.Join(home, ".local", "state", "agent-mux")
	stateDir.Do(func() { os.MkdirAll(dir, 0755) })
	return dir
}

func statePath() string {
	return filepath.Join(stateDirPath(), "state.json")
}

func stateWriteLockPath() string {
	return filepath.Join(stateDirPath(), "state.lock")
}

func LoadState() (State, bool) {
	return loadStateFile(statePath())
}

func loadStateFile(path string) (State, bool) {
	data, err := os.ReadFile(path)
	if err != nil {
		return State{}, false
	}

	var state State
	if err := json.Unmarshal(data, &state); err != nil {
		return State{}, false
	}
	if state.Version != 1 {
		return State{}, false
	}

	return state, true
}

func SaveState(state State) error {
	unlock, err := lockStateFile()
	if err != nil {
		return err
	}
	defer unlock()
	return writeStateFile(state)
}

func UpdateState(fn func(*State)) error {
	unlock, err := lockStateFile()
	if err != nil {
		return err
	}
	defer unlock()

	state, _ := loadStateFile(statePath())
	fn(&state)
	return writeStateFile(state)
}

func lockStateFile() (func(), error) {
	lockFile, err := os.OpenFile(stateWriteLockPath(), os.O_CREATE|os.O_RDWR, 0644)
	if err != nil {
		return nil, err
	}
	if err := syscall.Flock(int(lockFile.Fd()), syscall.LOCK_EX); err != nil {
		lockFile.Close()
		return nil, err
	}
	return func() {
		_ = syscall.Flock(int(lockFile.Fd()), syscall.LOCK_UN)
		_ = lockFile.Close()
	}, nil
}

func writeStateFile(state State) error {
	path := statePath()
	state.Version = 1
	now := time.Now()
	state.UpdatedAt = &now
	data, err := json.MarshalIndent(state, "", "  ")
	if err != nil {
		return err
	}
	tmp, err := os.CreateTemp(stateDirPath(), ".state-*.tmp")
	if err != nil {
		return err
	}
	tmpName := tmp.Name()
	defer os.Remove(tmpName)
	if _, err := tmp.Write(data); err != nil {
		_ = tmp.Close()
		return err
	}
	if err := tmp.Close(); err != nil {
		return err
	}
	return os.Rename(tmpName, path)
}

// CachePanes converts live Pane structs into the cached format.
func CachePanes(panes []*Pane) []CachedPane {
	cached := make([]CachedPane, len(panes))
	for i, p := range panes {
		cp := CachedPane{
			PaneID:        p.PaneID,
			Target:        p.Target,
			WindowName:    p.WindowName,
			Path:          p.Path,
			ShortPath:     p.ShortPath,
			ProjectRoot:   p.ProjectRoot,
			ProjectShort:  p.ProjectShort,
			ProjectBranch: p.ProjectBranch,
			ProjectDirty:  p.ProjectDirty,
			GitBranch:     p.GitBranch,
			GitDirty:      p.GitDirty,
			Stashed:       p.Stashed,
			Order:         p.Order,
			Provider:      p.Provider,
		}
		if !p.LastActive.IsZero() {
			t := p.LastActive
			cp.LastActive = &t
		}
		cached[i] = cp
	}
	return cached
}

// PanesFromState converts cached panes into live panes for display.
func PanesFromState(state State) []Pane {
	panes := make([]Pane, 0, len(state.Panes))
	for _, cp := range state.Panes {
		id := cp.PaneID
		if id == "" {
			id = cp.Target
		}
		session, window, pane := ParseTarget(cp.Target)
		p := Pane{
			PaneID:        id,
			Target:        cp.Target,
			Session:       session,
			Window:        window,
			WindowName:    cp.WindowName,
			Pane:          pane,
			Path:          cp.Path,
			ShortPath:     cp.ShortPath,
			ProjectRoot:   cp.ProjectRoot,
			ProjectShort:  cp.ProjectShort,
			ProjectBranch: cp.ProjectBranch,
			ProjectDirty:  cp.ProjectDirty,
			GitBranch:     cp.GitBranch,
			GitDirty:      cp.GitDirty,
			Stashed:       cp.Stashed,
			Order:         cp.Order,
			Provider:      cp.Provider,
			ContentHash:   cp.ContentHash,
		}
		if cp.LastStatus != nil {
			p.Status = PaneStatus(*cp.LastStatus)
		}
		if cp.LastActive != nil {
			p.LastActive = *cp.LastActive
		}
		panes = append(panes, p)
	}
	return panes
}

func HasStatusOverride(state State, paneID string) bool {
	for _, cp := range state.Panes {
		if cp.paneKey() == paneID {
			return cp.StatusOverride != nil
		}
	}
	return false
}

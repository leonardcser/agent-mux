package agent

import (
	"encoding/json"
	"os"
	"path/filepath"
	"time"
)

type CachedPane struct {
	Target         string     `json:"target"`
	Path           string     `json:"path"`
	ShortPath      string     `json:"shortPath"`
	GitBranch      string     `json:"gitBranch,omitempty"`
	GitDirty       bool       `json:"gitDirty,omitempty"`
	Stashed        bool       `json:"stashed"`
	StatusOverride *int       `json:"statusOverride,omitempty"`
	ContentHash    uint64     `json:"contentHash,omitempty"`
	LastStatus     *int       `json:"lastStatus,omitempty"`
	LastActive     *time.Time `json:"lastActive,omitempty"`
}

type State struct {
	Version      int          `json:"version"`
	Panes        []CachedPane `json:"panes"`
	LastPosition LastPosition `json:"lastPosition"`
	SidebarWidth int          `json:"sidebarWidth,omitempty"`
}

type LastPosition struct {
	PaneTarget  string `json:"pane_target"`
	Cursor      int    `json:"cursor"`
	ScrollStart int    `json:"scroll_start"`
}

func statePath() string {
	home, _ := os.UserHomeDir()
	dir := filepath.Join(home, ".local", "state", "agent-mux")
	os.MkdirAll(dir, 0755)
	return filepath.Join(dir, "state.json")
}

func LoadState() (State, bool) {
	path := statePath()
	data, err := os.ReadFile(path)
	if err != nil {
		return State{}, false
	}

	var state State
	if err := json.Unmarshal(data, &state); err != nil {
		return State{}, false
	}
	if state.Version != 4 {
		return State{}, false
	}

	return state, true
}

func SaveState(state State) error {
	path := statePath()
	state.Version = 4
	data, err := json.MarshalIndent(state, "", "  ")
	if err != nil {
		return err
	}
	return os.WriteFile(path, data, 0644)
}

// PanesFromState rebuilds Pane structs from cached state.
func PanesFromState(cached []CachedPane) []Pane {
	panes := make([]Pane, len(cached))
	for i, cp := range cached {
		panes[i] = Pane{
			Target:    cp.Target,
			Path:      cp.Path,
			ShortPath: cp.ShortPath,
			GitBranch: cp.GitBranch,
			GitDirty:  cp.GitDirty,
			Stashed:   cp.Stashed,
		}
	}
	return panes
}

// CachePanes converts live Pane structs into the cached format.
func CachePanes(panes []*Pane) []CachedPane {
	cached := make([]CachedPane, len(panes))
	for i, p := range panes {
		cp := CachedPane{
			Target:    p.Target,
			Path:      p.Path,
			ShortPath: p.ShortPath,
			GitBranch: p.GitBranch,
			GitDirty:  p.GitDirty,
			Stashed:   p.Stashed,
		}
		if !p.LastActive.IsZero() {
			t := p.LastActive
			cp.LastActive = &t
		}
		cached[i] = cp
	}
	return cached
}

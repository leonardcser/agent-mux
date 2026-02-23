package agent

import (
	"encoding/json"
	"os"
	"path/filepath"
)

type CachedPane struct {
	Target         string `json:"target"`
	Stashed        bool   `json:"stashed"`
	StatusOverride *int   `json:"statusOverride,omitempty"`
	ContentHash    uint64 `json:"contentHash,omitempty"`
	LastStatus     *int   `json:"lastStatus,omitempty"`
}

type CachedWorkspace struct {
	Path      string       `json:"path"`
	ShortPath string       `json:"shortPath"`
	GitBranch string       `json:"gitBranch"`
	GitDirty  bool         `json:"gitDirty"`
	Panes     []CachedPane `json:"panes"`
}

type State struct {
	Version      int               `json:"version"`
	Workspaces   []CachedWorkspace `json:"workspaces"`
	LastPosition LastPosition      `json:"lastPosition"`
	LastUpdated  string            `json:"lastUpdated"`
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
	if state.Version != 2 {
		return State{}, false
	}

	return state, true
}

func SaveState(state State) error {
	path := statePath()
	state.Version = 2
	state.LastUpdated = ""
	data, err := json.MarshalIndent(state, "", "  ")
	if err != nil {
		return err
	}
	return os.WriteFile(path, data, 0644)
}

// WorkspacesFromState rebuilds Workspace structs from cached state.
// Panes are populated with only the Target and Stashed fields.
func WorkspacesFromState(cached []CachedWorkspace) []Workspace {
	var workspaces []Workspace
	for _, cw := range cached {
		var panes []Pane
		for _, cp := range cw.Panes {
			panes = append(panes, Pane{
				Target:  cp.Target,
				Stashed: cp.Stashed,
			})
		}
		workspaces = append(workspaces, Workspace{
			Path:      cw.Path,
			ShortPath: cw.ShortPath,
			GitBranch: cw.GitBranch,
			GitDirty:  cw.GitDirty,
			Panes:     panes,
		})
	}
	return workspaces
}

// CacheWorkspaces converts live Workspace structs into the cached format.
func CacheWorkspaces(workspaces []Workspace) []CachedWorkspace {
	var cached []CachedWorkspace
	for _, ws := range workspaces {
		cw := CachedWorkspace{
			Path:      ws.Path,
			ShortPath: ws.ShortPath,
			GitBranch: ws.GitBranch,
			GitDirty:  ws.GitDirty,
		}
		for _, p := range ws.Panes {
			cw.Panes = append(cw.Panes, CachedPane{
				Target:  p.Target,
				Stashed: p.Stashed,
			})
		}
		cached = append(cached, cw)
	}
	return cached
}

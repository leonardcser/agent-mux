package agent

import (
	"encoding/json"
	"os"
	"path/filepath"
)

type State struct {
	Version      int             `json:"version"`
	CachedPanes  []Pane          `json:"cachedPanes"`
	StashState   map[string]bool `json:"stashState"`
	LastPosition LastPosition    `json:"lastPosition"`
	LastUpdated  string          `json:"lastUpdated"`
}

type LastPosition struct {
	PaneTarget  string `json:"pane_target"`
	Cursor      int    `json:"cursor"`
	ScrollStart int    `json:"scroll_start"`
	Timestamp   string `json:"timestamp"`
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

	return state, true
}

func SaveState(state State) error {
	path := statePath()
	state.LastUpdated = ""
	data, err := json.MarshalIndent(state, "", "  ")
	if err != nil {
		return err
	}
	return os.WriteFile(path, data, 0644)
}

package agent

import (
	"encoding/json"
	"os"
	"path/filepath"
	"time"
)

func stateFilePath(sessionID string) string {
	return filepath.Join(stateDir(), sessionID+".json")
}

func cacheFilePath() string {
	return filepath.Join(stateDir(), "cache.json")
}

func stateDir() string {
	home, _ := os.UserHomeDir()
	dir := filepath.Join(home, ".local", "state", "agent-mux")
	os.MkdirAll(dir, 0755)
	return dir
}

type paneState struct {
	PaneTarget    string `json:"pane_target"`
	WorkspacePath string `json:"workspace_path"`
	Cursor        int    `json:"cursor"`
	ScrollStart   int    `json:"scroll_start"`
	Timestamp     string `json:"timestamp"`
}

type cacheData struct {
	Panes     []Pane `json:"panes"`
	Timestamp string `json:"timestamp"`
	ExpiresAt string `json:"expires_at"`
}

func LoadLastPosition(sessionID string) (paneTarget string, cursor, scrollStart int, ok bool) {
	path := stateFilePath(sessionID)
	data, err := os.ReadFile(path)
	if err != nil {
		return "", 0, 0, false
	}

	var state paneState
	if err := json.Unmarshal(data, &state); err != nil {
		return "", 0, 0, false
	}

	return state.PaneTarget, state.Cursor, state.ScrollStart, true
}

func SaveLastPosition(sessionID, paneTarget string, cursor, scrollStart int) error {
	path := stateFilePath(sessionID)
	state := paneState{
		PaneTarget:    paneTarget,
		WorkspacePath: "",
		Cursor:        cursor,
		ScrollStart:   scrollStart,
		Timestamp:     time.Now().Format(time.RFC3339),
	}
	data, err := json.MarshalIndent(state, "", "  ")
	if err != nil {
		return err
	}
	return os.WriteFile(path, data, 0644)
}

func LoadCachedPanes() (panes []Pane, ok bool) {
	path := cacheFilePath()
	data, err := os.ReadFile(path)
	if err != nil {
		return nil, false
	}

	var cache cacheData
	if err := json.Unmarshal(data, &cache); err != nil {
		return nil, false
	}

	return cache.Panes, true
}

func SaveCachedPanes(panes []Pane) error {
	path := cacheFilePath()
	cache := cacheData{
		Panes:     panes,
		Timestamp: time.Now().Format(time.RFC3339),
		ExpiresAt: time.Now().Add(5 * time.Hour).Format(time.RFC3339),
	}
	data, err := json.MarshalIndent(cache, "", "  ")
	if err != nil {
		return err
	}
	return os.WriteFile(path, data, 0644)
}

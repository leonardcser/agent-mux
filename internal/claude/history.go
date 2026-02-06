package claude

import (
	"bufio"
	"encoding/json"
	"os"
	"path/filepath"
	"sync"
	"time"
)

type historyEntry struct {
	Timestamp int64  `json:"timestamp"`
	Project   string `json:"project"`
}

var historyCache struct {
	sync.Mutex
	modTime time.Time
	data    map[string]time.Time
}

// LastActiveByProject reads ~/.claude/history.jsonl and returns
// a map of project path -> last activity time.
// Results are cached and only re-read when the file's mtime changes.
func LastActiveByProject() map[string]time.Time {
	home, err := os.UserHomeDir()
	if err != nil {
		return nil
	}
	path := filepath.Join(home, ".claude", "history.jsonl")

	info, err := os.Stat(path)
	if err != nil {
		return nil
	}

	historyCache.Lock()
	defer historyCache.Unlock()

	if historyCache.data != nil && info.ModTime().Equal(historyCache.modTime) {
		return historyCache.data
	}

	f, err := os.Open(path)
	if err != nil {
		return nil
	}
	defer f.Close()

	result := make(map[string]time.Time)
	scanner := bufio.NewScanner(f)
	scanner.Buffer(make([]byte, 0, 256*1024), 1024*1024)
	for scanner.Scan() {
		var entry historyEntry
		if json.Unmarshal(scanner.Bytes(), &entry) != nil {
			continue
		}
		if entry.Project == "" || entry.Timestamp == 0 {
			continue
		}
		t := time.UnixMilli(entry.Timestamp)
		if existing, ok := result[entry.Project]; !ok || t.After(existing) {
			result[entry.Project] = t
		}
	}

	historyCache.modTime = info.ModTime()
	historyCache.data = result
	return result
}

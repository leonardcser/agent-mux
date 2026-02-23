package main

import (
	"fmt"
	"os"
	"path/filepath"

	tea "github.com/charmbracelet/bubbletea"
	_ "github.com/leo/agent-mux/internal/provider" // register all providers
	"github.com/leo/agent-mux/internal/tui"
)

func main() {
	if os.Getenv("TMUX") == "" {
		fmt.Fprintln(os.Stderr, "error: agent-mux must be run inside tmux")
		os.Exit(1)
	}

	tmux := os.Getenv("TMUX")
	sessionID := filepath.Base(tmux)

	p := tea.NewProgram(tui.NewModel(sessionID), tea.WithAltScreen())
	if _, err := p.Run(); err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
}

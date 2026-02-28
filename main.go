package main

import (
	"context"
	"fmt"
	"os"
	"os/signal"
	"path/filepath"
	"slices"
	"time"

	tea "github.com/charmbracelet/bubbletea"
	"github.com/leo/agent-mux/internal/agent"
	_ "github.com/leo/agent-mux/internal/provider" // register all providers
	"github.com/leo/agent-mux/internal/tui"
)

func main() {
	if os.Getenv("TMUX") == "" {
		fmt.Fprintln(os.Stderr, "error: agent-mux must be run inside tmux")
		os.Exit(1)
	}

	if slices.Contains(os.Args[1:], "watch") {
		ctx, stop := signal.NotifyContext(context.Background(), os.Interrupt)
		defer stop()
		if err := agent.Watch(ctx); err != nil {
			fmt.Fprintln(os.Stderr, err)
			os.Exit(1)
		}
		return
	}

	if slices.Contains(os.Args[1:], "--bench") || slices.Contains(os.Args[1:], "--bench-cold") {
		runBench(slices.Contains(os.Args[1:], "--bench-cold"))
		return
	}
	if slices.Contains(os.Args[1:], "--bench-loop") {
		runBenchLoop()
		return
	}

	tmux := os.Getenv("TMUX")
	sessionID := filepath.Base(tmux)

	p := tea.NewProgram(tui.NewModel(sessionID), tea.WithAltScreen(), tea.WithMouseCellMotion())
	if _, err := p.Run(); err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
}

func runBenchLoop() {
	// Simulate one full refresh cycle (what runs every 2s in the runtime loop).
	// 1. ListPanes (tmux + ps + history + attention heuristics, parallel)
	// 2. CapturePane (preview load)

	t0 := time.Now()
	panes, err := agent.ListPanes()
	fmt.Fprintf(os.Stderr, "ListPanes:      %v (panes=%d, err=%v)\n", time.Since(t0), len(panes), err)
	if err != nil || len(panes) == 0 {
		return
	}

	t1 := time.Now()
	_, _ = agent.CapturePane(panes[0].Target, 50)
	fmt.Fprintf(os.Stderr, "CapturePane:    %v\n", time.Since(t1))

	fmt.Fprintf(os.Stderr, "Total:          %v\n", time.Since(t0))
}

func runBench(cold bool) {
	start := time.Now()

	if !cold {
		t0 := time.Now()
		_, ok := agent.LoadState()
		fmt.Fprintf(os.Stderr, "LoadState:      %v (hit=%v)\n", time.Since(t0), ok)
	}

	t1 := time.Now()
	panes, err := agent.ListPanesBasic()
	fmt.Fprintf(os.Stderr, "ListPanesBasic: %v (panes=%d, err=%v)\n", time.Since(t1), len(panes), err)

	t2 := time.Now()
	agent.EnrichPanes(panes)
	fmt.Fprintf(os.Stderr, "EnrichPanes:    %v\n", time.Since(t2))

	t3 := time.Now()
	full, err := agent.ListPanes()
	fmt.Fprintf(os.Stderr, "ListPanes:      %v (panes=%d, err=%v)\n", time.Since(t3), len(full), err)

	fmt.Fprintf(os.Stderr, "Total:          %v\n", time.Since(start))
}

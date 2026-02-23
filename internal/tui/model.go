package tui

import (
	"strings"
	"time"

	"github.com/charmbracelet/bubbles/viewport"
	tea "github.com/charmbracelet/bubbletea"
	"github.com/charmbracelet/lipgloss"
	"github.com/leo/agent-mux/internal/agent"
)

// Messages
type panesLoadedMsg struct {
	panes []agent.Pane
	err   error
}

type previewLoadedMsg struct {
	target  string
	content string
}

type paneKilledMsg struct{ err error }
type previewTickMsg time.Time
type panesTickMsg time.Time

func previewTickCmd() tea.Cmd {
	return tea.Tick(200*time.Millisecond, func(t time.Time) tea.Msg {
		return previewTickMsg(t)
	})
}

func panesTickCmd() tea.Cmd {
	return tea.Tick(2*time.Second, func(t time.Time) tea.Msg {
		return panesTickMsg(t)
	})
}

// Commands
func loadPanes() tea.Msg {
	panes, err := agent.ListPanes()
	return panesLoadedMsg{panes: panes, err: err}
}

func loadPreview(target string) tea.Cmd {
	return func() tea.Msg {
		content, err := agent.CapturePane(target, 50)
		if err != nil {
			content = "error: " + err.Error()
		}
		return previewLoadedMsg{target: target, content: content}
	}
}

// Model is the top-level Bubble Tea model.
type Model struct {
	workspaces         []agent.Workspace
	items              []TreeItem
	cursor             int
	scrollStart        int
	preview            viewport.Model
	previewFor         string
	lastPreviewContent string // raw content for dedup
	width              int
	height             int
	err                error
	loaded             bool
	firstLoad          bool
	pendingD           bool
	pendingG           bool
	count              int // vim-style numeric prefix
	tmuxSession        string
}

// NewModel creates the initial model.
// Uses the fast path (no status detection) so the UI is ready on the first frame.
// Full status detection happens on the first async tick.
func NewModel(tmuxSession string) Model {
	m := Model{
		preview:     viewport.New(40, 20),
		firstLoad:   true,
		tmuxSession: tmuxSession,
	}

	cachedPanes, cachedOK := agent.LoadCachedPanes()
	paneTarget, _, lastScrollStart, stateOK := agent.LoadLastPosition(tmuxSession)

	if cachedOK {
		m.workspaces = agent.GroupByWorkspace(cachedPanes)
		m.items = FlattenTree(m.workspaces)
		if stateOK {
			m.cursor = FindPaneByTarget(m.items, m.workspaces, paneTarget)
			m.scrollStart = lastScrollStart
		} else {
			m.cursor = FirstAttentionPane(m.items, m.workspaces)
		}
	} else {
		panes, err := agent.ListPanesBasic()
		if err != nil {
			m.err = err
			m.loaded = true
			return m
		}
		m.workspaces = agent.GroupByWorkspace(panes)
		m.items = FlattenTree(m.workspaces)
		m.cursor = FirstPane(m.items)
	}
	return m
}

func (m Model) Init() tea.Cmd {
	// Don't schedule ticks here — completion handlers start the tick chains,
	// ensuring the next tick only fires after the previous work completes.
	return tea.Batch(loadPanes, m.previewCmd())
}

func (m Model) Update(msg tea.Msg) (tea.Model, tea.Cmd) {
	switch msg := msg.(type) {
	case tea.WindowSizeMsg:
		m.width = msg.Width
		m.height = msg.Height
		m.preview.Width = m.previewWidth()
		m.preview.Height = m.height
		return m, nil

	case panesLoadedMsg:
		m.loaded = true
		if msg.err != nil {
			m.err = msg.err
			return m, panesTickCmd() // keep ticking even on error
		}
		m.err = nil
		agent.SaveCachedPanes(msg.panes)

		m.workspaces = agent.GroupByWorkspace(msg.panes)
		m.items = FlattenTree(m.workspaces)

		if m.firstLoad {
			m.firstLoad = false
			paneTarget, _, scrollStart, stateOK := agent.LoadLastPosition(m.tmuxSession)
			attentionIndex := FirstAttentionPane(m.items, m.workspaces)
			if attentionIndex != FirstPane(m.items) {
				m.cursor = attentionIndex
			} else if stateOK {
				m.cursor = FindPaneByTarget(m.items, m.workspaces, paneTarget)
				m.scrollStart = scrollStart
			} else {
				m.cursor = FirstPane(m.items)
			}
		}
		// Schedule next panes tick after completion (backpressure).
		cmds := []tea.Cmd{panesTickCmd()}
		if cmd := m.previewCmd(); cmd != nil {
			cmds = append(cmds, cmd)
		}
		return m, tea.Batch(cmds...)

	case previewLoadedMsg:
		m.previewFor = msg.target
		content := strings.TrimRight(msg.content, "\n")
		// Skip re-render when content hasn't changed.
		if content != m.lastPreviewContent {
			m.lastPreviewContent = content
			m.preview.SetContent(content)
			m.preview.GotoBottom()
		}
		// Schedule next preview tick after completion (backpressure).
		return m, previewTickCmd()

	case previewTickMsg:
		// Fire preview load. Next tick scheduled from previewLoadedMsg.
		m.previewFor = "" // force refresh
		if cmd := m.previewCmd(); cmd != nil {
			return m, cmd
		}
		// No active pane to preview — keep ticking.
		return m, previewTickCmd()

	case panesTickMsg:
		// Fire pane load. Next tick scheduled from panesLoadedMsg.
		return m, loadPanes

	case paneKilledMsg:
		if msg.err != nil {
			m.err = msg.err
			return m, nil
		}
		return m, loadPanes

	case tea.KeyMsg:
		key := msg.String()

		// Accumulate numeric prefix (vim count).
		if len(key) == 1 && key[0] >= '0' && key[0] <= '9' {
			// Don't allow leading zeros as a count (0 is not a motion here).
			if m.count > 0 || key[0] != '0' {
				m.count = m.count*10 + int(key[0]-'0')
				return m, nil
			}
		}
		count := max(m.count, 1)
		m.count = 0

		// Handle dd sequence
		if key == "d" {
			if m.pendingD {
				m.pendingD = false
				m.pendingG = false
				return m, m.killCurrentPane()
			}
			m.pendingD = true
			m.pendingG = false
			return m, nil
		}
		m.pendingD = false

		// Handle gg sequence
		if key == "g" {
			if m.pendingG {
				m.pendingG = false
				m.cursor = FirstPane(m.items)
				return m, m.previewCmd()
			}
			m.pendingG = true
			return m, nil
		}
		m.pendingG = false

		switch key {
		case "G":
			m.cursor = LastPane(m.items)
			return m, m.previewCmd()

		case "j", "down":
			for range count {
				next := NextPane(m.items, m.cursor)
				if next == m.cursor {
					break
				}
				m.cursor = next
			}
			return m, m.previewCmd()

		case "k", "up":
			for range count {
				prev := PrevPane(m.items, m.cursor)
				if prev == m.cursor {
					break
				}
				m.cursor = prev
			}
			return m, m.previewCmd()

		case "enter":
			if m.cursor >= 0 && m.cursor < len(m.items) && m.items[m.cursor].Kind == KindPane {
				pane := m.workspaces[m.items[m.cursor].WorkspaceIndex].Panes[m.items[m.cursor].PaneIndex]
				_ = agent.SwitchToPane(pane.Target)
				agent.SaveLastPosition(m.tmuxSession, pane.Target, m.cursor, m.scrollStart)
				return m, tea.Quit
			}

		case "q", "esc", "ctrl+c":
			if m.cursor >= 0 && m.cursor < len(m.items) && m.items[m.cursor].Kind == KindPane {
				pane := m.workspaces[m.items[m.cursor].WorkspaceIndex].Panes[m.items[m.cursor].PaneIndex]
				agent.SaveLastPosition(m.tmuxSession, pane.Target, m.cursor, m.scrollStart)
			}
			return m, tea.Quit

		}
	}
	return m, nil
}

func (m Model) View() string {
	if m.width == 0 || !m.loaded {
		return ""
	}

	if m.err != nil {
		return errStyle.Render("Error: " + m.err.Error())
	}

	if m.loaded && len(m.items) == 0 {
		return helpStyle.Render("No active sessions found.\nPress q to quit.")
	}

	listWidth := m.listWidth()
	h := m.height

	// Render tree
	treeLines := m.renderTree(listWidth, h)
	listContent := strings.Join(treeLines, "\n")
	// Pad list to full height
	listRendered := lipgloss.NewStyle().Width(listWidth).Height(h).Render(listContent)

	// Vertical separator: one column of "│" repeated for each row
	sep := separatorStyle.Render(strings.Repeat("│\n", h-1) + "│")

	// Render preview
	pw := m.previewWidth()
	m.preview.Width = pw
	m.preview.Height = h
	previewRendered := lipgloss.NewStyle().Width(pw).Height(h).Render(m.preview.View())

	return lipgloss.JoinHorizontal(lipgloss.Top, listRendered, sep, previewRendered)
}

func (m Model) listWidth() int {
	return max(m.width*25/100, 20)
}

func (m Model) previewWidth() int {
	return m.width - m.listWidth() - 1 // 1 for separator
}

func (m Model) renderTree(width, height int) []string {
	if len(m.items) == 0 {
		return []string{"  No sessions"}
	}

	start := VisibleSlice(len(m.items), m.cursor, height)
	m.scrollStart = start
	end := min(start+height, len(m.items))

	lines := make([]string, 0, end-start)
	for i := start; i < end; i++ {
		lines = append(lines, RenderTreeItem(m.items[i], m.workspaces, i == m.cursor, width))
	}
	return lines
}

func (m Model) killCurrentPane() tea.Cmd {
	if m.cursor < 0 || m.cursor >= len(m.items) {
		return nil
	}
	item := m.items[m.cursor]
	if item.Kind != KindPane {
		return nil
	}
	target := m.workspaces[item.WorkspaceIndex].Panes[item.PaneIndex].Target
	return func() tea.Msg {
		return paneKilledMsg{err: agent.KillPane(target)}
	}
}

func (m Model) previewCmd() tea.Cmd {
	if m.cursor < 0 || m.cursor >= len(m.items) {
		return nil
	}
	item := m.items[m.cursor]
	if item.Kind != KindPane {
		return nil
	}
	target := m.workspaces[item.WorkspaceIndex].Panes[item.PaneIndex].Target
	if target == m.previewFor {
		return nil
	}
	return loadPreview(target)
}

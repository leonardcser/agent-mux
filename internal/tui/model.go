package tui

import (
	"strings"
	"time"

	"github.com/charmbracelet/bubbles/viewport"
	tea "github.com/charmbracelet/bubbletea"
	"github.com/charmbracelet/lipgloss"
	"github.com/leo/claude-mux/internal/claude"
)

// Messages
type panesLoadedMsg struct {
	panes []claude.ClaudePane
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
	panes, err := claude.ListClaudePanes()
	return panesLoadedMsg{panes: panes, err: err}
}

func loadPreview(target string) tea.Cmd {
	return func() tea.Msg {
		content, err := claude.CapturePane(target, 50)
		if err != nil {
			content = "error: " + err.Error()
		}
		return previewLoadedMsg{target: target, content: content}
	}
}

// Model is the top-level Bubble Tea model.
type Model struct {
	workspaces         []claude.Workspace
	items              []TreeItem
	cursor             int
	preview            viewport.Model
	previewFor         string
	lastPreviewContent string // raw content for dedup
	width              int
	height             int
	err                error
	loaded             bool
	statusLoaded       bool // true once first full status detection completes
	pendingD           bool
}

// NewModel creates the initial model.
// Uses the fast path (no status detection) so the UI is ready on the first frame.
// Full status detection happens on the first async tick.
func NewModel() Model {
	m := Model{
		preview: viewport.New(40, 20),
	}
	panes, err := claude.ListClaudePanesBasic()
	m.loaded = true
	if err != nil {
		m.err = err
	} else {
		m.workspaces = claude.GroupByWorkspace(panes)
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
		firstStatus := !m.statusLoaded
		m.statusLoaded = true
		m.workspaces = claude.GroupByWorkspace(msg.panes)
		m.items = FlattenTree(m.workspaces)
		if firstStatus {
			m.cursor = FirstAttentionPane(m.items, m.workspaces)
		} else {
			m.cursor = NearestPane(m.items, m.cursor)
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

		// Handle dd sequence
		if key == "d" {
			if m.pendingD {
				m.pendingD = false
				return m, m.killCurrentPane()
			}
			m.pendingD = true
			return m, nil
		}
		m.pendingD = false

		switch key {
		case "q", "esc", "ctrl+c":
			return m, tea.Quit

		case "j", "down":
			next := NextPane(m.items, m.cursor)
			if next != m.cursor {
				m.cursor = next
				return m, m.previewCmd()
			}

		case "k", "up":
			prev := PrevPane(m.items, m.cursor)
			if prev != m.cursor {
				m.cursor = prev
				return m, m.previewCmd()
			}

		case "enter":
			if m.cursor >= 0 && m.cursor < len(m.items) && m.items[m.cursor].Kind == KindPane {
				pane := m.workspaces[m.items[m.cursor].WorkspaceIndex].Panes[m.items[m.cursor].PaneIndex]
				_ = claude.SwitchToPane(pane.Target)
				return m, tea.Quit
			}

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
		return helpStyle.Render("No active Claude sessions found.\nPress q to quit.")
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
		return paneKilledMsg{err: claude.KillPane(target)}
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

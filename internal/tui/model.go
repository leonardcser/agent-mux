package tui

import (
	"strings"
	"time"

	"github.com/charmbracelet/bubbles/viewport"
	tea "github.com/charmbracelet/bubbletea"
	"github.com/charmbracelet/lipgloss"
	"github.com/leo/agent-mux/internal/agent"
)

type panesLoadedMsg struct {
	panes []agent.Pane
	err   error
}

type previewLoadedMsg struct {
	target  string
	content string
	gen     int
}

type paneKilledMsg struct{ err error }
type previewTickMsg struct{ gen int }
type previewDebounceMsg struct{ gen int }
type panesTickMsg time.Time

func previewTickCmd(gen int) tea.Cmd {
	return tea.Tick(200*time.Millisecond, func(t time.Time) tea.Msg {
		return previewTickMsg{gen: gen}
	})
}

func panesTickCmd() tea.Cmd {
	return tea.Tick(2*time.Second, func(t time.Time) tea.Msg {
		return panesTickMsg(t)
	})
}

func loadPanes() tea.Msg {
	panes, err := agent.ListPanes()
	return panesLoadedMsg{panes: panes, err: err}
}

func loadPreview(target string, lines int, gen int) tea.Cmd {
	return func() tea.Msg {
		content, err := agent.CapturePane(target, lines)
		if err != nil {
			content = "error: " + err.Error()
		}
		return previewLoadedMsg{target: target, content: content, gen: gen}
	}
}

type statusOverride struct {
	Status      agent.PaneStatus
	ContentHash uint64
}

// Model is the top-level Bubble Tea model.
type Model struct {
	workspaces         []agent.Workspace
	stashed            []agent.Workspace
	items              []TreeItem
	cursor             int
	scrollStart        int
	preview            viewport.Model
	previewFor         string
	lastPreviewContent string
	previewGen         int
	width              int
	height             int
	err                error
	loaded             bool
	showHelp           bool
	pendingD           bool
	pendingG           bool
	count              int
	tmuxSession        string
	state              agent.State
	overrides          map[string]statusOverride
}

func NewModel(tmuxSession string) Model {
	m := Model{
		preview:     viewport.New(40, 20),
		tmuxSession: tmuxSession,
		overrides:   make(map[string]statusOverride),
	}

	state, stateOK := agent.LoadState()
	m.state = state
	if stateOK {
		for _, cw := range state.Workspaces {
			for _, cp := range cw.Panes {
				if cp.StatusOverride != nil {
					m.overrides[cp.Target] = statusOverride{
						Status:      agent.PaneStatus(*cp.StatusOverride),
						ContentHash: cp.ContentHash,
					}
				}
			}
		}
		all := agent.WorkspacesFromState(state.Workspaces)
		m.workspaces, m.stashed = splitByStash(all)
	} else {
		panes, err := agent.ListPanesBasic()
		if err != nil {
			m.err = err
			m.loaded = true
			return m
		}
		m.workspaces = agent.GroupByWorkspace(panes)
	}
	m.items = FlattenTree(m.workspaces, m.stashed)

	if stateOK && state.LastPosition.PaneTarget != "" {
		if pos := FindPaneByTarget(m.items, m.workspaces, m.stashed, state.LastPosition.PaneTarget); pos >= 0 {
			m.cursor = pos
			m.scrollStart = state.LastPosition.ScrollStart
		} else {
			m.cursor = FirstPane(m.items)
		}
	} else {
		m.cursor = FirstPane(m.items)
	}
	return m
}

// splitByStash separates workspaces into working and stashed based on each pane's Stashed field.
func splitByStash(workspaces []agent.Workspace) (working, stashed []agent.Workspace) {
	for _, ws := range workspaces {
		var stashedPanes, workingPanes []agent.Pane
		for _, p := range ws.Panes {
			if p.Stashed {
				stashedPanes = append(stashedPanes, p)
			} else {
				workingPanes = append(workingPanes, p)
			}
		}
		base := agent.Workspace{Path: ws.Path, ShortPath: ws.ShortPath, GitBranch: ws.GitBranch, GitDirty: ws.GitDirty}
		if len(stashedPanes) > 0 {
			s := base
			s.Panes = stashedPanes
			stashed = append(stashed, s)
		}
		if len(workingPanes) > 0 {
			w := base
			w.Panes = workingPanes
			working = append(working, w)
		}
	}
	return working, stashed
}

// splitByStashSet separates workspaces using an external set of stashed targets.
func splitByStashSet(workspaces []agent.Workspace, stashSet map[string]bool) (working, stashed []agent.Workspace) {
	for _, ws := range workspaces {
		var stashedPanes, workingPanes []agent.Pane
		for _, p := range ws.Panes {
			if stashSet[p.Target] {
				p.Stashed = true
				stashedPanes = append(stashedPanes, p)
			} else {
				p.Stashed = false
				workingPanes = append(workingPanes, p)
			}
		}
		base := agent.Workspace{Path: ws.Path, ShortPath: ws.ShortPath, GitBranch: ws.GitBranch, GitDirty: ws.GitDirty}
		if len(stashedPanes) > 0 {
			s := base
			s.Panes = stashedPanes
			stashed = append(stashed, s)
		}
		if len(workingPanes) > 0 {
			w := base
			w.Panes = workingPanes
			working = append(working, w)
		}
	}
	return working, stashed
}

// resolvePane returns the pane referenced by the tree item at the given index,
// or nil if the index is out of bounds or not a pane item.
func (m Model) resolvePane(idx int) *agent.Pane {
	if idx < 0 || idx >= len(m.items) {
		return nil
	}
	item := m.items[idx]
	if item.Kind != KindPane {
		return nil
	}
	ws := m.workspaces
	wsIdx := item.WorkspaceIndex
	if wsIdx >= len(m.workspaces) {
		ws = m.stashed
		wsIdx -= len(m.workspaces)
	}
	return &ws[wsIdx].Panes[item.PaneIndex]
}

func (m Model) Init() tea.Cmd {
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
		firstLoad := !m.loaded
		m.loaded = true
		if msg.err != nil {
			m.err = msg.err
			return m, panesTickCmd()
		}
		m.err = nil
		for i := range msg.panes {
			p := &msg.panes[i]
			if ov, ok := m.overrides[p.Target]; ok {
				if p.ContentHash != ov.ContentHash {
					delete(m.overrides, p.Target)
				} else {
					p.Status = ov.Status
				}
			}
		}
		m.workspaces = agent.GroupByWorkspace(msg.panes)
		stashSet := m.stashSet()
		m.workspaces, m.stashed = splitByStashSet(m.workspaces, stashSet)
		m.items = FlattenTree(m.workspaces, m.stashed)
		if firstLoad {
			if att := FirstAttentionPane(m.items, m.workspaces); att >= 0 {
				m.cursor = att
			} else {
				m.cursor = NearestPane(m.items, m.cursor)
			}
		} else {
			m.cursor = NearestPane(m.items, m.cursor)
		}
		return m, panesTickCmd()

	case previewLoadedMsg:
		if msg.gen != m.previewGen {
			return m, nil
		}
		m.previewFor = msg.target
		content := strings.TrimRight(msg.content, "\n")
		if content != m.lastPreviewContent {
			m.lastPreviewContent = content
			m.preview.SetContent(content)
			m.preview.GotoBottom()
		}
		return m, previewTickCmd(m.previewGen)

	case previewDebounceMsg:
		if msg.gen != m.previewGen {
			return m, nil
		}
		m.previewFor = ""
		if cmd := m.previewCmd(); cmd != nil {
			return m, cmd
		}
		return m, previewTickCmd(m.previewGen)

	case previewTickMsg:
		if msg.gen != m.previewGen {
			return m, nil
		}
		m.previewFor = ""
		if cmd := m.previewCmd(); cmd != nil {
			return m, cmd
		}
		return m, previewTickCmd(m.previewGen)

	case panesTickMsg:
		return m, loadPanes

	case paneKilledMsg:
		if msg.err != nil {
			m.err = msg.err
			return m, nil
		}
		return m, loadPanes

	case tea.KeyMsg:
		return m.handleKey(msg)
	}
	return m, nil
}

func (m Model) handleKey(msg tea.KeyMsg) (tea.Model, tea.Cmd) {
	key := msg.String()

	if len(key) == 1 && key[0] >= '0' && key[0] <= '9' {
		if m.count > 0 || key[0] != '0' {
			m.count = m.count*10 + int(key[0]-'0')
			return m, nil
		}
	}
	count := max(m.count, 1)
	m.count = 0

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

	if key == "g" {
		if m.pendingG {
			m.pendingG = false
			m.cursor = FirstPane(m.items)
			return m, m.newPreviewCmd()
		}
		m.pendingG = true
		return m, nil
	}
	m.pendingG = false

	switch key {
	case "?":
		m.showHelp = !m.showHelp
		return m, nil

	case "G":
		m.cursor = LastPane(m.items)
		return m, m.newPreviewCmd()

	case " ":
		if p := m.resolvePane(m.cursor); p != nil {
			switch p.Status {
			case agent.StatusIdle:
				p.Status = agent.StatusBusy
			case agent.StatusBusy:
				p.Status = agent.StatusNeedsAttention
			case agent.StatusNeedsAttention:
				p.Status = agent.StatusIdle
			}
			m.overrides[p.Target] = statusOverride{
				Status:      p.Status,
				ContentHash: p.ContentHash,
			}
		}
		return m, nil

	case "s":
		if m.toggleStash() {
			m.items = FlattenTree(m.workspaces, m.stashed)
			m.cursor = NearestPane(m.items, m.cursor)
			m.scrollStart = VisibleSlice(len(m.items), m.cursor, m.height)
		}
		return m, nil

	case "u":
		if p := m.resolvePane(m.cursor); p != nil && p.Stashed {
			if m.toggleStash() {
				m.items = FlattenTree(m.workspaces, m.stashed)
				m.cursor = NearestPane(m.items, m.cursor)
				m.scrollStart = VisibleSlice(len(m.items), m.cursor, m.height)
			}
		}
		return m, nil

	case "j", "down":
		for range count {
			next := NextPane(m.items, m.cursor)
			if next == m.cursor {
				break
			}
			m.cursor = next
		}
		return m, m.newPreviewCmd()

	case "k", "up":
		for range count {
			prev := PrevPane(m.items, m.cursor)
			if prev == m.cursor {
				break
			}
			m.cursor = prev
		}
		return m, m.newPreviewCmd()

	case "enter", "q", "esc", "ctrl+c":
		if key == "enter" {
			if p := m.resolvePane(m.cursor); p != nil {
				_ = agent.SwitchToPane(p.Target)
			}
		}
		m.saveState()
		return m, tea.Quit
	}
	return m, nil
}

func (m *Model) saveState() {
	all := append(m.workspaces, m.stashed...)
	m.state.Workspaces = agent.CacheWorkspaces(all)
	for i := range m.state.Workspaces {
		for j := range m.state.Workspaces[i].Panes {
			cp := &m.state.Workspaces[i].Panes[j]
			if ov, ok := m.overrides[cp.Target]; ok {
				s := int(ov.Status)
				cp.StatusOverride = &s
				cp.ContentHash = ov.ContentHash
			}
		}
	}
	var paneTarget string
	if p := m.resolvePane(m.cursor); p != nil {
		paneTarget = p.Target
	}
	m.state.LastPosition = agent.LastPosition{
		PaneTarget:  paneTarget,
		Cursor:      m.cursor,
		ScrollStart: m.scrollStart,
	}
	_ = agent.SaveState(m.state)
}

// stashSet builds a set of stashed pane targets from current model state.
func (m Model) stashSet() map[string]bool {
	set := make(map[string]bool)
	for _, ws := range m.stashed {
		for _, p := range ws.Panes {
			set[p.Target] = true
		}
	}
	return set
}

func (m Model) View() string {
	if m.width == 0 || !m.loaded {
		return ""
	}
	if m.err != nil {
		return errStyle.Render("Error: " + m.err.Error())
	}
	if len(m.items) == 0 {
		return helpStyle.Render("No active sessions found.\nPress q to quit.")
	}

	listWidth := m.listWidth()
	h := m.height

	treeLines := m.renderTree(listWidth, h)
	listContent := strings.Join(treeLines, "\n")
	listRendered := lipgloss.NewStyle().Width(listWidth).Height(h).Render(listContent)

	sep := separatorStyle.Render(strings.Repeat("│\n", h-1) + "│")

	pw := m.previewWidth()
	var previewRendered string
	if m.showHelp {
		previewRendered = lipgloss.NewStyle().Width(pw).Height(h).Render(m.renderHelp())
	} else {
		m.preview.Width = pw
		m.preview.Height = h
		previewRendered = lipgloss.NewStyle().Width(pw).Height(h).Render(m.preview.View())
	}

	return lipgloss.JoinHorizontal(lipgloss.Top, listRendered, sep, previewRendered)
}

func (m Model) renderHelp() string {
	keys := []struct{ key, desc string }{
		{"j/k", "move down/up"},
		{"[n]j/k", "move down/up n times"},
		{"enter", "switch to pane"},
		{"space", "cycle status"},
		{"s/u", "stash/unstash"},
		{"dd", "kill pane"},
		{"gg", "go to first"},
		{"G", "go to last"},
		{"?", "toggle help"},
		{"q/esc", "quit"},
	}
	var b strings.Builder
	b.WriteString(helpTitleStyle.Render(" Keybindings"))
	b.WriteString("\n\n")
	for _, k := range keys {
		b.WriteString("  ")
		b.WriteString(helpKeyStyle.Render(k.key))
		b.WriteString("  ")
		b.WriteString(helpDescStyle.Render(k.desc))
		b.WriteString("\n")
	}
	return b.String()
}

func (m Model) listWidth() int {
	return max(m.width*25/100, 20)
}

func (m Model) previewWidth() int {
	return m.width - m.listWidth() - 1
}

func (m Model) renderTree(width, height int) []string {
	if len(m.items) == 0 {
		return []string{"  No sessions"}
	}

	cursor := max(m.cursor, 0)
	start := VisibleSlice(len(m.items), cursor, height)
	end := min(start+height, len(m.items))

	lines := make([]string, 0, end-start)
	for i := start; i < end; i++ {
		lines = append(lines, RenderTreeItem(m.items[i], m.workspaces, m.stashed, i == m.cursor, width))
	}
	return lines
}

func (m Model) killCurrentPane() tea.Cmd {
	p := m.resolvePane(m.cursor)
	if p == nil {
		return nil
	}
	target := p.Target
	return func() tea.Msg {
		return paneKilledMsg{err: agent.KillPane(target)}
	}
}

// toggleStash moves the pane under the cursor between working and stashed lists.
// Returns true if a pane was toggled.
func (m *Model) toggleStash() bool {
	p := m.resolvePane(m.cursor)
	if p == nil {
		return false
	}
	target := p.Target

	if !p.Stashed {
		m.movePaneBetween(&m.workspaces, &m.stashed, target, true)
	} else {
		m.movePaneBetween(&m.stashed, &m.workspaces, target, false)
	}
	return true
}

// movePaneBetween removes the pane with the given target from src and adds it to dst,
// merging into an existing workspace or creating a new one.
func (m *Model) movePaneBetween(src, dst *[]agent.Workspace, target string, stashed bool) {
	for i := range *src {
		for j := range (*src)[i].Panes {
			if (*src)[i].Panes[j].Target != target {
				continue
			}
			pane := (*src)[i].Panes[j]
			pane.Stashed = stashed
			path := (*src)[i].Path

			(*src)[i].Panes = append((*src)[i].Panes[:j], (*src)[i].Panes[j+1:]...)

			added := false
			for di := range *dst {
				if (*dst)[di].Path == path {
					insertAt := len((*dst)[di].Panes)
					for pi := range (*dst)[di].Panes {
						if (*dst)[di].Panes[pi].Target > pane.Target {
							insertAt = pi
							break
						}
					}
					(*dst)[di].Panes = append((*dst)[di].Panes, agent.Pane{})
					copy((*dst)[di].Panes[insertAt+1:], (*dst)[di].Panes[insertAt:])
					(*dst)[di].Panes[insertAt] = pane
					added = true
					break
				}
			}
			if !added {
				newWS := agent.Workspace{
					Path:      (*src)[i].Path,
					ShortPath: (*src)[i].ShortPath,
					GitBranch: (*src)[i].GitBranch,
					GitDirty:  (*src)[i].GitDirty,
					Panes:     []agent.Pane{pane},
				}
				insertAt := len(*dst)
				for di := range *dst {
					if (*dst)[di].Path > path {
						insertAt = di
						break
					}
				}
				*dst = append(*dst, agent.Workspace{})
				copy((*dst)[insertAt+1:], (*dst)[insertAt:])
				(*dst)[insertAt] = newWS
			}

			if len((*src)[i].Panes) == 0 {
				*src = append((*src)[:i], (*src)[i+1:]...)
			}
			return
		}
	}
}

func (m *Model) newPreviewCmd() tea.Cmd {
	m.previewGen++
	gen := m.previewGen
	return tea.Tick(50*time.Millisecond, func(t time.Time) tea.Msg {
		return previewDebounceMsg{gen: gen}
	})
}

func (m Model) previewCmd() tea.Cmd {
	p := m.resolvePane(m.cursor)
	if p == nil {
		return nil
	}
	if p.Target == m.previewFor {
		return nil
	}
	lines := m.height
	if lines <= 0 {
		lines = 50
	}
	return loadPreview(p.Target, lines, m.previewGen)
}

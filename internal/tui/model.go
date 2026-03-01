package tui

import (
	"sort"
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

func (m Model) pollInterval() time.Duration {
	if m.refreshCount <= 2 {
		return 500 * time.Millisecond
	}
	return 2 * time.Second
}

func panesTickCmd(d time.Duration) tea.Cmd {
	return tea.Tick(d, func(t time.Time) tea.Msg {
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

// Model is the top-level Bubble Tea model.
type Model struct {
	panes              map[string]*agent.Pane
	reconciler         *agent.Reconciler
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
	firstRefreshDone   bool
	showHelp           bool
	pendingD           bool
	pendingG           bool
	count              int
	sidebarWidth       int
	dragging           bool
	tmuxSession        string
	state              agent.State
	refreshCount       int
}

func NewModel(tmuxSession string) Model {
	m := Model{
		preview:    viewport.New(40, 20),
		tmuxSession: tmuxSession,
		panes:      make(map[string]*agent.Pane),
		reconciler: agent.NewReconciler(),
	}

	state, stateOK := agent.LoadState()
	m.state = state
	m.sidebarWidth = state.SidebarWidth
	if stateOK {
		m.reconciler.SeedFromState(state)
		for _, cp := range state.Panes {
			session, window, pane := agent.ParseTarget(cp.Target)
			p := &agent.Pane{
				Target:     cp.Target,
				Session:    session,
				Window:     window,
				WindowName: cp.WindowName,
				Pane:       pane,
				Path:       cp.Path,
				ShortPath:  cp.ShortPath,
				GitBranch:  cp.GitBranch,
				GitDirty:   cp.GitDirty,
				Stashed:    cp.Stashed,
			}
			if cp.LastActive != nil {
				p.LastActive = *cp.LastActive
			}
			p.Status = m.reconciler.Status(cp.Target)
			m.panes[cp.Target] = p
		}
		m.loaded = true
	} else {
		panes, err := agent.ListPanesBasic()
		if err != nil {
			m.err = err
			m.loaded = true
			return m
		}
		agent.EnrichPanes(panes)
		for i := range panes {
			m.panes[panes[i].Target] = &panes[i]
		}
		m.loaded = true
	}
	m.rebuildItems()

	if att := m.firstAttentionPane(); att >= 0 {
		m.cursor = att
	} else if stateOK && state.LastPosition.PaneTarget != "" {
		if pos := m.findPaneByTarget(state.LastPosition.PaneTarget); pos >= 0 {
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

// rebuildItems builds the flat display list from the pane map.
// Sorts by (stashed, path, target) and inserts workspace headers.
func (m *Model) rebuildItems() {
	sorted := make([]*agent.Pane, 0, len(m.panes))
	for _, p := range m.panes {
		sorted = append(sorted, p)
	}
	sort.Slice(sorted, func(i, j int) bool {
		if sorted[i].Stashed != sorted[j].Stashed {
			return !sorted[i].Stashed
		}
		if sorted[i].Path != sorted[j].Path {
			return sorted[i].Path < sorted[j].Path
		}
		return sorted[i].Target < sorted[j].Target
	})

	var items []TreeItem
	prevPath := ""
	inStashed := false
	for _, p := range sorted {
		if p.Stashed && !inStashed {
			inStashed = true
			prevPath = ""
			items = append(items,
				TreeItem{Kind: KindSectionHeader},
				TreeItem{Kind: KindSectionHeader, HeaderTitle: "stashed"},
			)
		}
		if p.Path != prevPath {
			prevPath = p.Path
			items = append(items, TreeItem{Kind: KindWorkspace, Target: p.Target})
		}
		items = append(items, TreeItem{Kind: KindPane, Target: p.Target})
	}
	m.items = items
}

// resolvePane returns the pane for the tree item at idx, or nil.
func (m Model) resolvePane(idx int) *agent.Pane {
	if idx < 0 || idx >= len(m.items) || m.items[idx].Kind != KindPane {
		return nil
	}
	return m.panes[m.items[idx].Target]
}

// findPaneByTarget returns the item index for the given target, or -1.
func (m Model) findPaneByTarget(target string) int {
	for i, item := range m.items {
		if item.Kind == KindPane && item.Target == target {
			return i
		}
	}
	return -1
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
		firstLoad := !m.firstRefreshDone
		m.firstRefreshDone = true
		m.loaded = true
		m.refreshCount++
		if msg.err != nil {
			m.err = msg.err
			return m, panesTickCmd(m.pollInterval())
		}
		m.err = nil

		// Preserve stashed state before reconciliation.
		stashed := make(map[string]bool, len(m.panes))
		for target, p := range m.panes {
			if p.Stashed {
				stashed[target] = true
			}
		}

		m.reconciler.Reconcile(msg.panes)

		// Rebuild pane map from fresh data.
		newPanes := make(map[string]*agent.Pane, len(msg.panes))
		for i := range msg.panes {
			p := &msg.panes[i]
			p.Stashed = stashed[p.Target]
			newPanes[p.Target] = p
		}
		m.panes = newPanes

		m.rebuildItems()
		if firstLoad {
			if att := m.firstAttentionPane(); att >= 0 {
				m.cursor = att
			} else {
				m.cursor = NearestPane(m.items, m.cursor)
			}
		} else {
			m.cursor = NearestPane(m.items, m.cursor)
		}
		return m, panesTickCmd(m.pollInterval())

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

	case tea.MouseMsg:
		return m.handleMouse(msg)

	case tea.KeyMsg:
		return m.handleKey(msg)
	}
	return m, nil
}

func (m Model) handleMouse(msg tea.MouseMsg) (tea.Model, tea.Cmd) {
	sep := m.listWidth()
	switch msg.Action {
	case tea.MouseActionPress:
		if msg.Button == tea.MouseButtonLeft && msg.X >= sep-1 && msg.X <= sep+1 {
			m.dragging = true
		}
	case tea.MouseActionMotion:
		if m.dragging {
			w := max(min(msg.X, m.width-20), 20)
			m.sidebarWidth = w
			m.preview.Width = m.previewWidth()
		}
	case tea.MouseActionRelease:
		m.dragging = false
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
				p.Status = agent.StatusNeedsAttention
			case agent.StatusNeedsAttention, agent.StatusUnread:
				p.Status = agent.StatusIdle
			default:
				return m, nil
			}
			m.reconciler.SetOverride(p.Target, p.Status, p.ContentHash)
		}
		return m, nil

	case "s":
		if p := m.resolvePane(m.cursor); p != nil {
			wasStashed := p.Stashed
			p.Stashed = !p.Stashed
			m.rebuildItems()
			m.clampCursorInSection(m.cursor, wasStashed)
		}
		return m, nil

	case "u":
		if p := m.resolvePane(m.cursor); p != nil && p.Stashed {
			p.Stashed = false
			m.rebuildItems()
			m.clampCursorInSection(m.cursor, true)
		}
		return m, nil

	case "R":
		agent.RestartWatch()
		return m, loadPanes

	case "H":
		w := max(m.listWidth()-2*count, 20)
		m.sidebarWidth = w
		m.preview.Width = m.previewWidth()
		return m, nil

	case "L":
		w := min(m.listWidth()+2*count, m.width-20)
		m.sidebarWidth = w
		m.preview.Width = m.previewWidth()
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
				if p.Status == agent.StatusUnread {
					p.Status = agent.StatusIdle
					m.reconciler.SetOverride(p.Target, agent.StatusIdle, p.ContentHash)
				}
				_ = agent.SwitchToPane(p.Target)
			}
		}
		m.saveState()
		return m, tea.Quit
	}
	return m, nil
}

// clampCursorInSection keeps the cursor at the same index but ensures it stays
// within the section the pane was originally in (wasStashed). Falls back to
// other sections only if the original section has no panes left.
func (m *Model) clampCursorInSection(idx int, wasStashed bool) {
	// Find the bounds of the original section in the new item list.
	sectionStart, sectionEnd := m.stashedSectionBounds()

	var start, end int
	if wasStashed {
		start, end = sectionStart, sectionEnd
	} else {
		start, end = 0, sectionStart
	}

	// Section is empty, fall back to any pane.
	if start >= end {
		m.cursor = NearestPane(m.items, idx)
		m.scrollStart = VisibleSlice(len(m.items), m.cursor, m.height)
		return
	}

	// Clamp idx within section, then find nearest pane.
	if idx >= end {
		idx = end - 1
	}
	if idx < start {
		idx = start
	}

	// Search for a pane within the section bounds.
	for i := idx; i >= start; i-- {
		if m.items[i].Kind == KindPane {
			m.cursor = i
			m.scrollStart = VisibleSlice(len(m.items), m.cursor, m.height)
			return
		}
	}
	for i := idx + 1; i < end; i++ {
		if m.items[i].Kind == KindPane {
			m.cursor = i
			m.scrollStart = VisibleSlice(len(m.items), m.cursor, m.height)
			return
		}
	}

	// Section is empty, fall back to any pane.
	m.cursor = NearestPane(m.items, idx)
	m.scrollStart = VisibleSlice(len(m.items), m.cursor, m.height)
}

// stashedSectionBounds returns the start and end indices of the stashed section.
// If there's no stashed section, returns (len(items), len(items)).
func (m Model) stashedSectionBounds() (int, int) {
	for i, item := range m.items {
		if item.Kind == KindSectionHeader && item.HeaderTitle == "stashed" {
			return i, len(m.items)
		}
	}
	return len(m.items), len(m.items)
}

func (m *Model) saveState() {
	paneList := make([]*agent.Pane, 0, len(m.panes))
	for _, p := range m.panes {
		paneList = append(paneList, p)
	}
	m.state.Panes = agent.CachePanes(paneList)
	m.reconciler.ApplyToCache(m.state.Panes)
	cursor := m.cursor
	scrollStart := m.scrollStart
	if att := m.firstAttentionPane(); att >= 0 {
		cursor = att
		scrollStart = 0
	}
	var paneTarget string
	if p := m.resolvePane(cursor); p != nil {
		paneTarget = p.Target
	}
	m.state.LastPosition = agent.LastPosition{
		PaneTarget:  paneTarget,
		Cursor:      cursor,
		ScrollStart: scrollStart,
	}
	m.state.SidebarWidth = m.sidebarWidth
	_ = agent.SaveState(m.state)
}

// firstAttentionPane returns the index of the first non-stashed pane needing attention, or -1.
func (m Model) firstAttentionPane() int {
	for i, item := range m.items {
		if item.Kind != KindPane {
			continue
		}
		p := m.panes[item.Target]
		if p != nil && !p.Stashed && (p.Status == agent.StatusNeedsAttention || p.Status == agent.StatusUnread) {
			return i
		}
	}
	return -1
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
		{"space", "toggle attention"},
		{"s/u", "stash/unstash"},
		{"dd", "kill pane"},
		{"gg", "go to first"},
		{"G", "go to last"},
		{"R", "reload watch"},
		{"H/L", "resize sidebar"},
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
	if m.sidebarWidth > 0 {
		return m.sidebarWidth
	}
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
		lines = append(lines, m.renderTreeItem(m.items[i], i == cursor, width))
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

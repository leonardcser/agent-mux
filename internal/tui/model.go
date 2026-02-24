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
	panes              map[string]*agent.Pane
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
	tmuxSession        string
	state              agent.State
	overrides          map[string]statusOverride
	prevHashes         map[string]uint64
	prevStatuses       map[string]agent.PaneStatus
}

func NewModel(tmuxSession string) Model {
	m := Model{
		preview:      viewport.New(40, 20),
		tmuxSession:  tmuxSession,
		panes:        make(map[string]*agent.Pane),
		overrides:     make(map[string]statusOverride),
		prevHashes:    make(map[string]uint64),
		prevStatuses:  make(map[string]agent.PaneStatus),
	}

	state, stateOK := agent.LoadState()
	m.state = state
	if stateOK {
		for _, cp := range state.Panes {
			session, window, pane := agent.ParseTarget(cp.Target)
			p := &agent.Pane{
				Target:    cp.Target,
				Session:   session,
				Window:    window,
				Pane:      pane,
				Path:      cp.Path,
				ShortPath: cp.ShortPath,
				GitBranch: cp.GitBranch,
				GitDirty:  cp.GitDirty,
				Stashed:   cp.Stashed,
			}
			if cp.LastActive != nil {
				p.LastActive = *cp.LastActive
			}
			if cp.StatusOverride != nil {
				p.Status = agent.PaneStatus(*cp.StatusOverride)
				m.overrides[cp.Target] = statusOverride{
					Status:      agent.PaneStatus(*cp.StatusOverride),
					ContentHash: cp.ContentHash,
				}
			} else if cp.LastStatus != nil {
				p.Status = agent.PaneStatus(*cp.LastStatus)
			}
			m.panes[cp.Target] = p
			if cp.ContentHash != 0 {
				m.prevHashes[cp.Target] = cp.ContentHash
			}
			if cp.LastStatus != nil {
				m.prevStatuses[cp.Target] = agent.PaneStatus(*cp.LastStatus)
			}
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

	if stateOK && state.LastPosition.PaneTarget != "" {
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
		if msg.err != nil {
			m.err = msg.err
			return m, panesTickCmd()
		}
		m.err = nil

		// Build new pane set, preserving stashed state from existing panes.
		alive := make(map[string]bool, len(msg.panes))
		for i := range msg.panes {
			p := &msg.panes[i]
			alive[p.Target] = true

			if existing, ok := m.panes[p.Target]; ok {
				p.Stashed = existing.Stashed
			}

			if ov, ok := m.overrides[p.Target]; ok {
				if p.ContentHash != ov.ContentHash {
					delete(m.overrides, p.Target)
				} else {
					p.Status = ov.Status
					m.prevHashes[p.Target] = p.ContentHash
					m.prevStatuses[p.Target] = p.Status
					m.panes[p.Target] = p
					continue
				}
			}
			{
				prev, seen := m.prevHashes[p.Target]
				if seen && p.ContentHash != prev {
					p.Status = agent.StatusBusy
				} else if p.HeuristicAttention {
					p.Status = agent.StatusNeedsAttention
				} else if m.prevStatuses[p.Target] == agent.StatusBusy {
					p.Status = agent.StatusUnread
				} else if m.prevStatuses[p.Target] == agent.StatusNeedsAttention ||
					m.prevStatuses[p.Target] == agent.StatusUnread {
					p.Status = m.prevStatuses[p.Target]
				}
			}
			m.prevHashes[p.Target] = p.ContentHash
			m.prevStatuses[p.Target] = p.Status
			m.panes[p.Target] = p
		}

		// Remove panes that no longer exist.
		for target := range m.panes {
			if !alive[target] {
				delete(m.panes, target)
				delete(m.overrides, target)
				delete(m.prevHashes, target)
				delete(m.prevStatuses, target)
			}
		}

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
				p.Status = agent.StatusNeedsAttention
			case agent.StatusNeedsAttention, agent.StatusUnread:
				p.Status = agent.StatusIdle
			default:
				return m, nil
			}
			m.overrides[p.Target] = statusOverride{
				Status:      p.Status,
				ContentHash: p.ContentHash,
			}
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
					delete(m.overrides, p.Target)
					delete(m.prevStatuses, p.Target)
					delete(m.prevHashes, p.Target)
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
	for i := range m.state.Panes {
		cp := &m.state.Panes[i]
		if ov, ok := m.overrides[cp.Target]; ok {
			s := int(ov.Status)
			cp.StatusOverride = &s
			cp.ContentHash = ov.ContentHash
		}
		if h, ok := m.prevHashes[cp.Target]; ok {
			cp.ContentHash = h
		}
		if s, ok := m.prevStatuses[cp.Target]; ok {
			v := int(s)
			cp.LastStatus = &v
		}
	}
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
	_ = agent.SaveState(m.state)
}

// firstAttentionPane returns the index of the first non-stashed pane needing attention, or -1.
func (m Model) firstAttentionPane() int {
	for i, item := range m.items {
		if item.Kind != KindPane {
			continue
		}
		p := m.panes[item.Target]
		if p != nil && !p.Stashed && p.Status == agent.StatusNeedsAttention || p.Status == agent.StatusUnread {
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

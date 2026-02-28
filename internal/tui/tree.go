package tui

import (
	"fmt"
	"strings"
	"time"

	"github.com/leo/agent-mux/internal/agent"
)

// ItemKind distinguishes workspace headers from pane entries.
type ItemKind int

const (
	KindWorkspace ItemKind = iota
	KindPane
	KindSectionHeader
)

// TreeItem is one visible row in the flattened tree.
type TreeItem struct {
	Kind        ItemKind
	Target      string // pane target (KindPane) or first pane target in workspace (KindWorkspace)
	HeaderTitle string // for KindSectionHeader
}

// NextPane returns the index of the next KindPane item after from, wrapping around if none.
func NextPane(items []TreeItem, from int) int {
	for i := from + 1; i < len(items); i++ {
		if items[i].Kind == KindPane {
			return i
		}
	}
	if len(items) > 0 {
		for i := range from {
			if items[i].Kind == KindPane {
				return i
			}
		}
	}
	return from
}

// PrevPane returns the index of the previous KindPane item before from, wrapping around if none.
func PrevPane(items []TreeItem, from int) int {
	for i := from - 1; i >= 0; i-- {
		if items[i].Kind == KindPane {
			return i
		}
	}
	if len(items) > 0 {
		for i := len(items) - 1; i > from; i-- {
			if items[i].Kind == KindPane {
				return i
			}
		}
	}
	return from
}

// NearestPane returns the closest KindPane to the given index without wrapping.
func NearestPane(items []TreeItem, from int) int {
	if len(items) == 0 {
		return 0
	}
	if from >= len(items) {
		from = len(items) - 1
	}
	if from < 0 {
		from = 0
	}
	if items[from].Kind == KindPane {
		return from
	}

	sectionStart, sectionEnd := sectionBounds(items, from)

	// Find closest pane within the current section (by distance).
	best := -1
	for i := sectionStart; i < sectionEnd; i++ {
		if items[i].Kind != KindPane {
			continue
		}
		dist := i - from
		bestDist := best - from
		if dist < 0 {
			dist = -dist
		}
		if bestDist < 0 {
			bestDist = -bestDist
		}
		if best < 0 || dist < bestDist {
			best = i
		}
	}
	if best >= 0 {
		return best
	}

	// Fall back to other sections.
	for i := from - 1; i >= 0; i-- {
		if items[i].Kind == KindPane {
			return i
		}
	}
	for i := from + 1; i < len(items); i++ {
		if items[i].Kind == KindPane {
			return i
		}
	}
	return 0
}

// sectionBounds returns the start (inclusive) and end (exclusive) indices of
// the section containing the given index.
func sectionBounds(items []TreeItem, idx int) (int, int) {
	start := 0
	for i := idx - 1; i >= 0; i-- {
		if items[i].Kind == KindSectionHeader && items[i].HeaderTitle != "" {
			start = i
			break
		}
	}
	end := len(items)
	for i := idx + 1; i < len(items); i++ {
		if items[i].Kind == KindSectionHeader && items[i].HeaderTitle != "" {
			end = i
			break
		}
	}
	return start, end
}

// LastPane returns the index of the last KindPane item, or 0 if none.
func LastPane(items []TreeItem) int {
	for i := len(items) - 1; i >= 0; i-- {
		if items[i].Kind == KindPane {
			return i
		}
	}
	return 0
}

// FirstPane returns the index of the first KindPane item, or -1 if none.
func FirstPane(items []TreeItem) int {
	for i, it := range items {
		if it.Kind == KindPane {
			return i
		}
	}
	return -1
}

// renderTreeItem renders a single row.
func (m Model) renderTreeItem(item TreeItem, selected bool, width int) string {
	if item.Kind == KindSectionHeader {
		if item.HeaderTitle == "" {
			return ""
		}
		label := " " + item.HeaderTitle + " "
		lineLen := max(width-len(label)-1, 0)
		return stashedSectionStyle.Render("─" + label + strings.Repeat("─", lineLen))
	}

	p := m.panes[item.Target]
	if p == nil {
		return ""
	}

	switch item.Kind {
	case KindWorkspace:
		return renderWorkspaceHeader(p, width)
	case KindPane:
		return renderPaneRow(p, selected, width)
	}
	return ""
}

func renderWorkspaceHeader(p *agent.Pane, width int) string {
	avail := width - 2
	name := p.ShortPath
	branch := p.GitBranch
	if branch != "" && p.GitDirty {
		branch += "*"
	}

	if branch != "" {
		needed := len(name) + 1 + len(branch)
		if needed > avail {
			branchAvail := avail - len(name) - 1
			if branchAvail >= 4 {
				branch = truncate(branch, branchAvail)
			} else {
				branch = ""
			}
		}
		if branch == "" {
			name = truncate(name, avail)
		}
	} else {
		name = truncate(name, avail)
	}

	text := " " + name
	if branch != "" {
		pad := max(width-len(text)-len(branch)-1, 0)
		text += strings.Repeat(" ", pad)
		return workspaceStyle.Render(text) + branchStyle.Render(branch) + branchStyle.Render(" ")
	}
	text += strings.Repeat(" ", max(width-len(text), 0))
	return workspaceStyle.Render(text)
}

func renderPaneRow(p *agent.Pane, selected bool, width int) string {
	var label string
	if p.WindowName != "" {
		label = fmt.Sprintf("%s:%s", p.Window, p.WindowName)
	} else {
		label = fmt.Sprintf("%s:%s", p.Session, p.Window)
	}

	prefix := "   "
	right := ""
	if !p.LastActive.IsZero() {
		right = " " + formatElapsed(time.Since(p.LastActive)) + " "
	}
	middle := label
	avail := width - len(prefix) - 2 - len(right)
	if len(middle) > avail {
		middle = truncate(middle, avail)
	}
	gap := max(avail-len(middle), 0)

	icons := normalIcons
	if selected {
		icons = selectedIcons
	} else if p.Stashed {
		icons = stashedIcons
	}

	var icon string
	switch p.Status {
	case agent.StatusBusy:
		icon = icons.busy
	case agent.StatusNeedsAttention, agent.StatusUnread:
		icon = icons.attention
	default:
		icon = icons.idle
	}

	if selected {
		return selectedStyle.Render(prefix) + icon + selectedStyle.Render(" "+middle+strings.Repeat(" ", gap)+right)
	}
	if p.Stashed {
		return icons.text.Render(prefix) + icon + icons.text.Render(" "+middle) + icons.dim.Render(strings.Repeat(" ", gap)+right)
	}
	return icons.text.Render(prefix) + icon + icons.text.Render(" "+middle) + icons.dim.Render(strings.Repeat(" ", gap)+right)
}

// truncate shortens s to maxLen, adding ellipsis if needed.
func truncate(s string, maxLen int) string {
	if maxLen <= 0 {
		return ""
	}
	if len(s) <= maxLen {
		return s
	}
	if maxLen <= 3 {
		return s[:maxLen]
	}
	return s[:maxLen-3] + "..."
}

// formatElapsed returns a human-readable short duration string.
func formatElapsed(d time.Duration) string {
	switch {
	case d < time.Minute:
		return fmt.Sprintf("%ds", int(d.Seconds()))
	case d < time.Hour:
		return fmt.Sprintf("%dm", int(d.Minutes()))
	case d < 24*time.Hour:
		h := int(d.Hours())
		m := int(d.Minutes()) % 60
		if m == 0 {
			return fmt.Sprintf("%dh", h)
		}
		return fmt.Sprintf("%dh%dm", h, m)
	default:
		return fmt.Sprintf("%dd", int(d.Hours())/24)
	}
}

// VisibleSlice returns the start index for scrolling the tree view.
func VisibleSlice(total, cursor, height int) int {
	if total <= height {
		return 0
	}
	start := 0
	if cursor >= height {
		start = cursor - height + 1
	}
	if start+height > total {
		start = total - height
	}
	return start
}

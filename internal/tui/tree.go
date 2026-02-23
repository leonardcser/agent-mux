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
	Kind           ItemKind
	WorkspaceIndex int
	PaneIndex      int
	HeaderTitle    string
}

// FlattenTree builds the visible flat list from workspaces.
// Workspaces are always expanded; headers are non-selectable.
func FlattenTree(workspaces []agent.Workspace, stashed []agent.Workspace) []TreeItem {
	var items []TreeItem

	for wi, ws := range workspaces {
		items = append(items, TreeItem{Kind: KindWorkspace, WorkspaceIndex: wi})
		for pi := range ws.Panes {
			if !ws.Panes[pi].Stashed {
				items = append(items, TreeItem{Kind: KindPane, WorkspaceIndex: wi, PaneIndex: pi})
			}
		}
	}

	if len(stashed) > 0 {
		items = append(items, TreeItem{Kind: KindSectionHeader, HeaderTitle: ""})
		items = append(items, TreeItem{Kind: KindSectionHeader, HeaderTitle: "stashed"})
	}

	for wi, ws := range stashed {
		items = append(items, TreeItem{Kind: KindWorkspace, WorkspaceIndex: wi + len(workspaces)})
		for pi := range ws.Panes {
			if ws.Panes[pi].Stashed {
				items = append(items, TreeItem{Kind: KindPane, WorkspaceIndex: wi + len(workspaces), PaneIndex: pi})
			}
		}
	}
	return items
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
// It clamps out-of-bounds indices, keeps the position if it's already a pane,
// otherwise tries the next pane forward first, then previous.
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
	for i := from + 1; i < len(items); i++ {
		if items[i].Kind == KindPane {
			return i
		}
	}
	for i := from - 1; i >= 0; i-- {
		if items[i].Kind == KindPane {
			return i
		}
	}
	return 0
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

// FirstAttentionPane returns the index of the first pane that needs attention, or -1 if none.
func FirstAttentionPane(items []TreeItem, workspaces []agent.Workspace, stashed []agent.Workspace) int {
	for i, it := range items {
		if it.Kind == KindPane {
			ws := workspaces
			wsIndex := it.WorkspaceIndex
			if wsIndex >= len(workspaces) {
				ws = stashed
				wsIndex -= len(workspaces)
			}
			if ws[wsIndex].Panes[it.PaneIndex].Status == agent.StatusNeedsAttention {
				return i
			}
		}
	}
	return -1
}

// FindPaneByTarget returns the index of the pane with the given target.
func FindPaneByTarget(items []TreeItem, workspaces []agent.Workspace, stashed []agent.Workspace, target string) int {
	for i, item := range items {
		if item.Kind == KindPane {
			ws := workspaces
			wsIndex := item.WorkspaceIndex
			if wsIndex >= len(workspaces) {
				ws = stashed
				wsIndex -= len(workspaces)
			}
			pane := ws[wsIndex].Panes[item.PaneIndex]
			if pane.Target == target {
				return i
			}
		}
	}
	return 0
}

// RenderTreeItem renders a single row.
func RenderTreeItem(item TreeItem, workspaces []agent.Workspace, stashed []agent.Workspace, selected bool, width int) string {
	if item.Kind == KindSectionHeader {
		if item.HeaderTitle == "" {
			return ""
		}
		label := " " + item.HeaderTitle + " "
		lineLen := max(width-len(label)-1, 0)
		return stashedSectionStyle.Render("─" + label + strings.Repeat("─", lineLen))
	}

	var ws agent.Workspace
	if item.WorkspaceIndex < len(workspaces) {
		ws = workspaces[item.WorkspaceIndex]
	} else {
		ws = stashed[item.WorkspaceIndex-len(workspaces)]
	}

	switch item.Kind {
	case KindWorkspace:
		avail := width - 2 // 1 leading space + 1 trailing minimum
		name := ws.ShortPath
		branch := ws.GitBranch

		if branch != "" {
			// " name branch " — space between name and branch = 1
			needed := len(name) + 1 + len(branch)
			if needed > avail {
				// Step 1: truncate the branch name
				branchAvail := avail - len(name) - 1
				if branchAvail >= 4 { // room for at least "x..."
					branch = truncate(branch, branchAvail)
				} else {
					// Step 2: drop branch entirely, show only name
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

	case KindPane:
		p := ws.Panes[item.PaneIndex]
		label := fmt.Sprintf("%s:%s", p.Session, p.Window)
		elapsed := formatElapsed(time.Since(p.LastActive))

		prefix := "   "
		right := " " + elapsed + " "
		middle := label
		avail := width - len(prefix) - 2 - len(right) // 2 for icon+space
		if len(middle) > avail {
			middle = truncate(middle, avail)
		}
		gap := max(avail-len(middle), 0)

		var icon string
		switch p.Status {
		case agent.StatusBusy:
			if selected {
				icon = busyIconSelectedStyle.Render("●")
			} else if p.Stashed {
				icon = stashedBusyIconStyle.Render("●")
			} else {
				icon = busyIconStyle.Render("●")
			}
		case agent.StatusNeedsAttention:
			if selected {
				icon = attentionIconSelectedStyle.Render("●")
			} else if p.Stashed {
				icon = stashedAttentionIconStyle.Render("●")
			} else {
				icon = attentionIconStyle.Render("●")
			}
		default:
			if selected {
				icon = idleIconSelectedStyle.Render("○")
			} else if p.Stashed {
				icon = stashedIdleIconStyle.Render("○")
			} else {
				icon = stashedPaneItemStyle.Render("○")
			}
		}

		if selected {
			return selectedStyle.Render(prefix) + icon + selectedStyle.Render(" "+middle+strings.Repeat(" ", gap)+right)
		}

		if p.Stashed {
			return stashedPaneItemStyle.Render(prefix) + icon + stashedPaneItemStyle.Render(" "+middle) + stashedDimStyle.Render(strings.Repeat(" ", gap)+right)
		}
		return paneItemStyle.Render(prefix) + icon + paneItemStyle.Render(" "+middle) + dimStyle.Render(strings.Repeat(" ", gap)+right)
	}
	return ""
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

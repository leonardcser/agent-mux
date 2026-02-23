package tui

import "github.com/charmbracelet/lipgloss"

var (
	// Tree items
	selectedStyle = lipgloss.NewStyle().
			Background(lipgloss.Color("8")).
			Foreground(lipgloss.Color("15")).
			Bold(true)

	workspaceStyle = lipgloss.NewStyle().
			Foreground(lipgloss.Color("15")).
			Bold(true)

	branchStyle = lipgloss.NewStyle().
			Foreground(lipgloss.Color("2"))

	paneItemStyle = lipgloss.NewStyle().
			Foreground(lipgloss.Color("8"))

	busyIconStyle = lipgloss.NewStyle().
			Foreground(lipgloss.Color("#D97706"))

	attentionIconStyle = lipgloss.NewStyle().
				Foreground(lipgloss.Color("#9B9BF5"))

	busyIconSelectedStyle = lipgloss.NewStyle().
				Foreground(lipgloss.Color("#D97706")).
				Background(lipgloss.Color("8"))

	attentionIconSelectedStyle = lipgloss.NewStyle().
					Foreground(lipgloss.Color("#9B9BF5")).
					Background(lipgloss.Color("8"))

	idleIconSelectedStyle = lipgloss.NewStyle().
				Foreground(lipgloss.Color("15")).
				Background(lipgloss.Color("8"))

	dimStyle = lipgloss.NewStyle().
			Foreground(lipgloss.Color("8"))

	// Separator
	separatorStyle = lipgloss.NewStyle().
			Foreground(lipgloss.Color("8"))

	// Stashed items
	stashedPaneItemStyle = lipgloss.NewStyle().
				Foreground(lipgloss.Color("8"))
	stashedBusyIconStyle = lipgloss.NewStyle().
				Foreground(lipgloss.Color("242"))
	stashedAttentionIconStyle = lipgloss.NewStyle().
					Foreground(lipgloss.Color("242"))
	stashedIdleIconStyle = lipgloss.NewStyle().
				Foreground(lipgloss.Color("242"))
	stashedDimStyle = lipgloss.NewStyle().
			Foreground(lipgloss.Color("242"))
	stashedSectionStyle = lipgloss.NewStyle().
				Foreground(lipgloss.Color("242"))

	// Help / status
	helpStyle = lipgloss.NewStyle().
			Foreground(lipgloss.Color("8"))

	// Error
	errStyle = lipgloss.NewStyle().
			Foreground(lipgloss.Color("1"))
)

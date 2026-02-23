package tui

import "github.com/charmbracelet/lipgloss"

type iconSet struct {
	busy      string
	attention string
	idle      string
	text      lipgloss.Style
	dim       lipgloss.Style
}

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

	dimStyle = lipgloss.NewStyle().
			Foreground(lipgloss.Color("8"))

	// Separator
	separatorStyle = lipgloss.NewStyle().
			Foreground(lipgloss.Color("8"))

	// Stashed items
	stashedSectionStyle = lipgloss.NewStyle().
				Foreground(lipgloss.Color("242"))

	// Help
	helpStyle = lipgloss.NewStyle().
			Foreground(lipgloss.Color("8"))
	helpTitleStyle = lipgloss.NewStyle().
			Foreground(lipgloss.Color("15")).
			Bold(true)
	helpKeyStyle = lipgloss.NewStyle().
			Foreground(lipgloss.Color("15")).
			Bold(true).
			Width(8)
	helpDescStyle = lipgloss.NewStyle().
			Foreground(lipgloss.Color("8"))

	// Error
	errStyle = lipgloss.NewStyle().
			Foreground(lipgloss.Color("1"))

	// Icon sets for status × context
	normalIcons = iconSet{
		busy:      lipgloss.NewStyle().Foreground(lipgloss.Color("#D97706")).Render("●"),
		attention: lipgloss.NewStyle().Foreground(lipgloss.Color("#9B9BF5")).Render("●"),
		idle:      lipgloss.NewStyle().Foreground(lipgloss.Color("8")).Render("○"),
		text:      paneItemStyle,
		dim:       dimStyle,
	}
	selectedIcons = iconSet{
		busy:      lipgloss.NewStyle().Foreground(lipgloss.Color("#D97706")).Background(lipgloss.Color("8")).Render("●"),
		attention: lipgloss.NewStyle().Foreground(lipgloss.Color("#9B9BF5")).Background(lipgloss.Color("8")).Render("●"),
		idle:      lipgloss.NewStyle().Foreground(lipgloss.Color("15")).Background(lipgloss.Color("8")).Render("○"),
		text:      selectedStyle,
		dim:       selectedStyle,
	}
	stashedIcons = iconSet{
		busy:      lipgloss.NewStyle().Foreground(lipgloss.Color("242")).Render("●"),
		attention: lipgloss.NewStyle().Foreground(lipgloss.Color("242")).Render("●"),
		idle:      lipgloss.NewStyle().Foreground(lipgloss.Color("242")).Render("○"),
		text:      lipgloss.NewStyle().Foreground(lipgloss.Color("8")),
		dim:       lipgloss.NewStyle().Foreground(lipgloss.Color("242")),
	}
)

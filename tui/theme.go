// Package tui holds wally's shared Bubble Tea theme and widgets.
package tui

import "github.com/charmbracelet/lipgloss"

var (
	colorAccent    = lipgloss.AdaptiveColor{Light: "#2F6D3C", Dark: "#7FD98B"}
	colorSecondary = lipgloss.AdaptiveColor{Light: "#1B5FA6", Dark: "#7EC1F5"}
	colorMuted     = lipgloss.AdaptiveColor{Light: "#6B6B6B", Dark: "#9B9B9B"}
	colorFaint     = lipgloss.AdaptiveColor{Light: "#9B9B9B", Dark: "#6B6B6B"}
	colorText      = lipgloss.AdaptiveColor{Light: "#1A1A1A", Dark: "#E8E8E8"}
	colorOnSel     = lipgloss.AdaptiveColor{Light: "#EAF3EA", Dark: "#1E2B20"}
	colorDanger    = lipgloss.AdaptiveColor{Light: "#B3261E", Dark: "#F2938C"}

	titleStyle = lipgloss.NewStyle().Bold(true).Foreground(colorAccent)

	itemStyle = lipgloss.NewStyle().PaddingLeft(2).Foreground(colorText)

	selectedItemStyle = lipgloss.NewStyle().
				PaddingLeft(2).
				Bold(true).
				Foreground(colorAccent).
				Background(colorOnSel)

	disabledItemStyle = lipgloss.NewStyle().PaddingLeft(2).Foreground(colorFaint).Italic(true)

	// headerPrimaryStyle and headerSecondaryStyle mark a list's section
	// headers (e.g. ONLINE vs OFFLINE) with distinct, non-faint colors so the
	// sections read apart at a glance instead of both fading into
	// disabledItemStyle.
	headerPrimaryStyle   = lipgloss.NewStyle().PaddingLeft(2).Bold(true).Foreground(colorAccent)
	headerSecondaryStyle = lipgloss.NewStyle().PaddingLeft(2).Bold(true).Foreground(colorSecondary)

	detailStyle = lipgloss.NewStyle().PaddingLeft(4).Foreground(colorMuted)

	helpStyle = lipgloss.NewStyle().Foreground(colorFaint)

	checkedGlyphStyle = lipgloss.NewStyle().Foreground(colorAccent).Bold(true)

	dangerStyle = lipgloss.NewStyle().Bold(true).Foreground(colorDanger)

	activeButtonStyle = lipgloss.NewStyle().Bold(true).Foreground(colorAccent).Background(colorOnSel).Padding(0, 2)

	inactiveButtonStyle = lipgloss.NewStyle().Foreground(colorFaint).Padding(0, 2)
)

// bloom is the one decorative accent the theme allows, used only in Header.
// A single glyph, not a block of art, so it reads as a mark rather than a
// banner.
const bloom = "✿"

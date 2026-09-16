package tui

import (
	"errors"
	"strings"

	tea "github.com/charmbracelet/bubbletea"
	"github.com/charmbracelet/lipgloss"
)

// ErrCancelled is returned by every Run* function when the person backs out
// with esc or ctrl+c instead of completing the prompt.
var ErrCancelled = errors.New("cancelled")

// Item is one row in a list or checklist.
type Item struct {
	Title    string
	Detail   string
	Disabled bool   // shown greyed out and unselectable
	Accent   Accent // header color for a Disabled row; ignored otherwise
}

// Accent distinguishes a disabled row that is a section header (colored)
// from one that is a plain note or hint (faint). AccentNone, the zero value,
// keeps the plain faint styling.
type Accent int

const (
	AccentNone Accent = iota
	AccentPrimary
	AccentSecondary
)

// ListModel is a single-select list: up/down to move, enter to choose, esc
// or ctrl+c to cancel.
type ListModel struct {
	title     string
	items     []Item
	cursor    int
	help      string
	selected  int
	confirmed bool
	cancelled bool
}

// NewList builds a single-select list over items. help overrides the default
// footer text when non-empty.
func NewList(title string, items []Item, help string) ListModel {
	m := ListModel{title: title, items: items, help: help, selected: -1}
	m.cursor = firstSelectable(items, 0)
	return m
}

func firstSelectable(items []Item, from int) int {
	for i := from; i < len(items); i++ {
		if !items[i].Disabled {
			return i
		}
	}
	for i := from - 1; i >= 0; i-- {
		if !items[i].Disabled {
			return i
		}
	}
	return 0
}

func (m ListModel) Init() tea.Cmd { return nil }

func (m ListModel) Update(msg tea.Msg) (tea.Model, tea.Cmd) {
	keyMsg, ok := msg.(tea.KeyMsg)
	if !ok {
		return m, nil
	}
	switch keyMsg.String() {
	case "ctrl+c", "esc":
		m.cancelled = true
		return m, tea.Quit
	case "up", "k":
		m.cursor = prevSelectable(m.items, m.cursor)
		return m, nil
	case "down", "j":
		m.cursor = nextSelectable(m.items, m.cursor)
		return m, nil
	case "enter", " ":
		if len(m.items) == 0 || m.items[m.cursor].Disabled {
			return m, nil
		}
		m.selected = m.cursor
		m.confirmed = true
		return m, tea.Quit
	}
	return m, nil
}

func prevSelectable(items []Item, cursor int) int {
	for i := cursor - 1; i >= 0; i-- {
		if !items[i].Disabled {
			return i
		}
	}
	return cursor
}

func nextSelectable(items []Item, cursor int) int {
	for i := cursor + 1; i < len(items); i++ {
		if !items[i].Disabled {
			return i
		}
	}
	return cursor
}

func (m ListModel) View() string {
	if m.confirmed || m.cancelled {
		return ""
	}
	var b strings.Builder
	b.WriteString(Header(m.title))
	b.WriteString("\n")
	for i, item := range m.items {
		writeListItem(&b, item, i == m.cursor)
	}
	b.WriteString("\n")
	help := m.help
	if help == "" {
		help = "↑/↓ move • enter select • esc cancel"
	}
	b.WriteString(helpStyle.Render(help))
	return b.String()
}

// itemStyleFor picks a row's style: an accented header color for a Disabled
// row carrying one, the plain faint style for any other Disabled row, the
// highlighted style for the active row, or the default otherwise.
func itemStyleFor(item Item, active bool) lipgloss.Style {
	switch {
	case item.Disabled && item.Accent == AccentPrimary:
		return headerPrimaryStyle
	case item.Disabled && item.Accent == AccentSecondary:
		return headerSecondaryStyle
	case item.Disabled:
		return disabledItemStyle
	case active:
		return selectedItemStyle
	default:
		return itemStyle
	}
}

func writeListItem(b *strings.Builder, item Item, active bool) {
	style := itemStyleFor(item, active)
	cursor := "  "
	if active && !item.Disabled {
		cursor = "▸ "
	}
	b.WriteString(style.Render(cursor + item.Title))
	b.WriteString("\n")
	if item.Detail != "" {
		b.WriteString(detailStyle.Render(item.Detail))
		b.WriteString("\n")
	}
}

// RunList runs a full-screen single-select list and returns the chosen
// item's index, or ErrCancelled.
func RunList(title string, items []Item, help string) (int, error) {
	m := NewList(title, items, help)
	final, err := tea.NewProgram(m).Run()
	if err != nil {
		return 0, err
	}
	fm := final.(ListModel)
	if fm.cancelled {
		return 0, ErrCancelled
	}
	return fm.selected, nil
}

// MultiListModel is a checklist: space toggles the current row, "a" toggles
// select-all, enter confirms the current selection (once at least one row is
// checked), esc or ctrl+c cancels.
type MultiListModel struct {
	title     string
	items     []Item
	cursor    int
	checked   map[int]bool
	help      string
	confirmed bool
	cancelled bool
}

// NewMultiList builds a checklist over items, all unchecked.
func NewMultiList(title string, items []Item, help string) MultiListModel {
	return MultiListModel{
		title:   title,
		items:   items,
		cursor:  firstSelectable(items, 0),
		checked: make(map[int]bool),
		help:    help,
	}
}

func (m MultiListModel) Init() tea.Cmd { return nil }

func (m MultiListModel) Update(msg tea.Msg) (tea.Model, tea.Cmd) {
	keyMsg, ok := msg.(tea.KeyMsg)
	if !ok {
		return m, nil
	}
	switch keyMsg.String() {
	case "ctrl+c", "esc":
		m.cancelled = true
		return m, tea.Quit
	case "up", "k":
		m.cursor = prevSelectable(m.items, m.cursor)
		return m, nil
	case "down", "j":
		m.cursor = nextSelectable(m.items, m.cursor)
		return m, nil
	case " ":
		if len(m.items) > 0 && !m.items[m.cursor].Disabled {
			m.checked = toggled(m.checked, m.cursor)
		}
		return m, nil
	case "a":
		m.checked = toggleAll(m.items, m.checked)
		return m, nil
	case "enter":
		if len(m.checked) == 0 {
			return m, nil
		}
		m.confirmed = true
		return m, tea.Quit
	}
	return m, nil
}

func toggled(checked map[int]bool, i int) map[int]bool {
	next := make(map[int]bool, len(checked))
	for k, v := range checked {
		next[k] = v
	}
	if next[i] {
		delete(next, i)
	} else {
		next[i] = true
	}
	return next
}

// toggleAll checks every selectable row when any row is unchecked, and
// clears the selection when every selectable row is already checked. That
// makes "a" a clean toggle rather than a one-way switch.
func toggleAll(items []Item, checked map[int]bool) map[int]bool {
	selectable := 0
	for _, item := range items {
		if !item.Disabled {
			selectable++
		}
	}
	if len(checked) >= selectable && selectable > 0 {
		return make(map[int]bool)
	}
	next := make(map[int]bool, selectable)
	for i, item := range items {
		if !item.Disabled {
			next[i] = true
		}
	}
	return next
}

// Checked returns the indices the person selected, ascending.
func (m MultiListModel) Checked() []int {
	out := make([]int, 0, len(m.checked))
	for i := range m.items {
		if m.checked[i] {
			out = append(out, i)
		}
	}
	return out
}

func (m MultiListModel) View() string {
	if m.confirmed || m.cancelled {
		return ""
	}
	var b strings.Builder
	b.WriteString(Header(m.title))
	b.WriteString("\n")
	for i, item := range m.items {
		writeCheckItem(&b, item, i == m.cursor, m.checked[i])
	}
	b.WriteString("\n")
	help := m.help
	if help == "" {
		help = "↑/↓ move • space toggle • a select all • enter confirm • esc cancel"
	}
	b.WriteString(helpStyle.Render(help))
	return b.String()
}

func writeCheckItem(b *strings.Builder, item Item, active, checked bool) {
	box := "[ ]"
	if checked {
		box = checkedGlyphStyle.Render("[x]")
	}
	style := itemStyle
	cursor := "  "
	switch {
	case item.Disabled:
		style = disabledItemStyle
	case active:
		style = selectedItemStyle
		cursor = "▸ "
	}
	b.WriteString(style.Render(cursor + box + " " + item.Title))
	b.WriteString("\n")
	if item.Detail != "" {
		b.WriteString(detailStyle.Render(item.Detail))
		b.WriteString("\n")
	}
}

// RunMultiList runs a full-screen checklist and returns the checked indices,
// or ErrCancelled.
func RunMultiList(title string, items []Item, help string) ([]int, error) {
	m := NewMultiList(title, items, help)
	final, err := tea.NewProgram(m).Run()
	if err != nil {
		return nil, err
	}
	fm := final.(MultiListModel)
	if fm.cancelled {
		return nil, ErrCancelled
	}
	return fm.Checked(), nil
}

package tui

import (
	"testing"

	tea "github.com/charmbracelet/bubbletea"
)

func key(s string) tea.KeyMsg {
	switch s {
	case "up":
		return tea.KeyMsg{Type: tea.KeyUp}
	case "down":
		return tea.KeyMsg{Type: tea.KeyDown}
	case "left":
		return tea.KeyMsg{Type: tea.KeyLeft}
	case "right":
		return tea.KeyMsg{Type: tea.KeyRight}
	case "enter":
		return tea.KeyMsg{Type: tea.KeyEnter}
	case "esc":
		return tea.KeyMsg{Type: tea.KeyEsc}
	case " ":
		return tea.KeyMsg{Type: tea.KeySpace}
	default:
		return tea.KeyMsg{Type: tea.KeyRunes, Runes: []rune(s)}
	}
}

func TestItemStyleFor_HeaderAccentsAreDistinctAndColored(t *testing.T) {
	primary := itemStyleFor(Item{Disabled: true, Accent: AccentPrimary}, false)
	secondary := itemStyleFor(Item{Disabled: true, Accent: AccentSecondary}, false)
	plain := itemStyleFor(Item{Disabled: true}, false)

	if primary.GetForeground() != colorAccent {
		t.Errorf("AccentPrimary header foreground = %v, want colorAccent", primary.GetForeground())
	}
	if secondary.GetForeground() != colorSecondary {
		t.Errorf("AccentSecondary header foreground = %v, want colorSecondary", secondary.GetForeground())
	}
	if primary.GetForeground() == secondary.GetForeground() {
		t.Error("ONLINE and OFFLINE header colors must differ")
	}
	if plain.GetForeground() == primary.GetForeground() || plain.GetForeground() == secondary.GetForeground() {
		t.Error("a disabled row without an accent must keep the plain faint style, not a header color")
	}
}

func TestListModel_NavigateAndSelect(t *testing.T) {
	items := []Item{{Title: "one"}, {Title: "two"}, {Title: "three"}}
	m := NewList("pick one", items, "")

	next, _ := m.Update(key("down"))
	m = next.(ListModel)
	if m.cursor != 1 {
		t.Fatalf("cursor = %d, want 1", m.cursor)
	}

	next, cmd := m.Update(key("enter"))
	m = next.(ListModel)
	if !m.confirmed || m.selected != 1 {
		t.Fatalf("confirmed = %v, selected = %d, want true, 1", m.confirmed, m.selected)
	}
	if cmd == nil {
		t.Fatal("expected tea.Quit command after enter")
	}
	if m.View() != "" {
		t.Fatalf("View() after confirm = %q, want empty", m.View())
	}
}

func TestListModel_SkipsDisabledItems(t *testing.T) {
	items := []Item{{Title: "one"}, {Title: "two", Disabled: true}, {Title: "three"}}
	m := NewList("pick one", items, "")

	next, _ := m.Update(key("down"))
	m = next.(ListModel)
	if m.cursor != 2 {
		t.Fatalf("cursor = %d, want 2 (skip the disabled row)", m.cursor)
	}
}

func TestListModel_EscCancels(t *testing.T) {
	m := NewList("pick one", []Item{{Title: "one"}}, "")
	next, cmd := m.Update(key("esc"))
	m = next.(ListModel)
	if !m.cancelled || m.confirmed {
		t.Fatalf("cancelled = %v, confirmed = %v, want true, false", m.cancelled, m.confirmed)
	}
	if cmd == nil {
		t.Fatal("expected tea.Quit command after esc")
	}
}

func TestMultiListModel_ToggleAndConfirm(t *testing.T) {
	items := []Item{{Title: "models"}, {Title: "chats"}, {Title: "configs"}}
	m := NewMultiList("remove what", items, "")

	next, _ := m.Update(key(" "))
	m = next.(MultiListModel)
	if got := m.Checked(); len(got) != 1 || got[0] != 0 {
		t.Fatalf("Checked() = %v, want [0]", got)
	}

	next, _ = m.Update(key("down"))
	m = next.(MultiListModel)
	next, _ = m.Update(key(" "))
	m = next.(MultiListModel)
	if got := m.Checked(); len(got) != 2 {
		t.Fatalf("Checked() = %v, want 2 entries", got)
	}

	// Toggling row 0 again unchecks it.
	next, _ = m.Update(key("up"))
	m = next.(MultiListModel)
	next, _ = m.Update(key(" "))
	m = next.(MultiListModel)
	if got := m.Checked(); len(got) != 1 || got[0] != 1 {
		t.Fatalf("Checked() after untoggle = %v, want [1]", got)
	}

	next, cmd := m.Update(key("enter"))
	m = next.(MultiListModel)
	if !m.confirmed {
		t.Fatal("expected confirmed after enter with a non-empty selection")
	}
	if cmd == nil {
		t.Fatal("expected tea.Quit command after enter")
	}
}

func TestMultiListModel_EnterRequiresASelection(t *testing.T) {
	items := []Item{{Title: "models"}, {Title: "chats"}}
	m := NewMultiList("remove what", items, "")

	next, cmd := m.Update(key("enter"))
	m = next.(MultiListModel)
	if m.confirmed {
		t.Fatal("enter with nothing checked must not confirm")
	}
	if cmd != nil {
		t.Fatal("enter with nothing checked must not quit")
	}
}

func TestMultiListModel_SelectAllTogglesEveryRow(t *testing.T) {
	items := []Item{{Title: "models"}, {Title: "chats"}, {Title: "configs"}}
	m := NewMultiList("remove what", items, "")

	next, _ := m.Update(key("a"))
	m = next.(MultiListModel)
	if got := m.Checked(); len(got) != 3 {
		t.Fatalf("Checked() after select-all = %v, want all 3 rows", got)
	}

	next, _ = m.Update(key("a"))
	m = next.(MultiListModel)
	if got := m.Checked(); len(got) != 0 {
		t.Fatalf("Checked() after second select-all = %v, want none (toggle back off)", got)
	}
}

func TestMultiListModel_SelectAllSkipsDisabledRows(t *testing.T) {
	items := []Item{{Title: "models"}, {Title: "locked", Disabled: true}}
	m := NewMultiList("remove what", items, "")

	next, _ := m.Update(key("a"))
	m = next.(MultiListModel)
	got := m.Checked()
	if len(got) != 1 || got[0] != 0 {
		t.Fatalf("Checked() = %v, want only the selectable row [0]", got)
	}
}

func TestMultiListModel_EscCancelsWithoutConfirming(t *testing.T) {
	m := NewMultiList("remove what", []Item{{Title: "models"}}, "")
	next, _ := m.Update(key(" "))
	m = next.(MultiListModel)
	next, cmd := m.Update(key("esc"))
	m = next.(MultiListModel)
	if !m.cancelled || m.confirmed {
		t.Fatalf("cancelled = %v, confirmed = %v, want true, false", m.cancelled, m.confirmed)
	}
	if cmd == nil {
		t.Fatal("expected tea.Quit command after esc")
	}
}

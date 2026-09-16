package tui

import "testing"

func TestConfirmModel_DefaultYesAcceptedOnEnter(t *testing.T) {
	m := NewConfirm("delete everything?", true, true)
	next, cmd := m.Update(key("enter"))
	m = next.(ConfirmModel)
	if !m.confirmed || !m.yes {
		t.Fatalf("confirmed = %v, yes = %v, want true, true", m.confirmed, m.yes)
	}
	if cmd == nil {
		t.Fatal("expected tea.Quit command after enter")
	}
}

func TestConfirmModel_ArrowsFlipTheAnswer(t *testing.T) {
	m := NewConfirm("delete everything?", true, true)
	next, _ := m.Update(key("right"))
	m = next.(ConfirmModel)
	if m.yes {
		t.Fatal("right arrow should move the highlight to No")
	}
	next, _ = m.Update(key("enter"))
	m = next.(ConfirmModel)
	if !m.confirmed || m.yes {
		t.Fatalf("confirmed = %v, yes = %v, want true, false", m.confirmed, m.yes)
	}
}

func TestConfirmModel_EscCancelsRegardlessOfDefault(t *testing.T) {
	m := NewConfirm("delete everything?", true, true)
	next, cmd := m.Update(key("esc"))
	m = next.(ConfirmModel)
	if !m.cancelled || m.confirmed {
		t.Fatalf("cancelled = %v, confirmed = %v, want true, false", m.cancelled, m.confirmed)
	}
	if cmd == nil {
		t.Fatal("expected tea.Quit command after esc")
	}
	if m.View() != "" {
		t.Fatalf("View() after cancel = %q, want empty", m.View())
	}
}

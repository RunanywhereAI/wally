package tui

import tea "github.com/charmbracelet/bubbletea"

// ConfirmModel is a yes/no prompt: left/right or y/n move the highlight,
// enter accepts it, esc or ctrl+c cancels outright.
type ConfirmModel struct {
	prompt      string
	destructive bool
	yes         bool
	confirmed   bool
	cancelled   bool
}

// NewConfirm builds a yes/no prompt defaulting to defaultYes. destructive
// renders the prompt text in the danger color, for anything about to delete
// or revoke something.
func NewConfirm(prompt string, defaultYes, destructive bool) ConfirmModel {
	return ConfirmModel{prompt: prompt, yes: defaultYes, destructive: destructive}
}

func (m ConfirmModel) Init() tea.Cmd { return nil }

func (m ConfirmModel) Update(msg tea.Msg) (tea.Model, tea.Cmd) {
	keyMsg, ok := msg.(tea.KeyMsg)
	if !ok {
		return m, nil
	}
	switch keyMsg.String() {
	case "ctrl+c", "esc":
		m.cancelled = true
		return m, tea.Quit
	case "enter":
		m.confirmed = true
		return m, tea.Quit
	case "left", "h", "y":
		m.yes = true
	case "right", "l", "n":
		m.yes = false
	}
	return m, nil
}

func (m ConfirmModel) View() string {
	if m.confirmed || m.cancelled {
		return ""
	}
	prompt := m.prompt
	if m.destructive {
		prompt = dangerStyle.Render(prompt)
	} else {
		prompt = titleStyle.Render(prompt)
	}

	yesBtn, noBtn := inactiveButtonStyle.Render("Yes"), inactiveButtonStyle.Render("No")
	if m.yes {
		yesBtn = activeButtonStyle.Render("Yes")
	} else {
		noBtn = activeButtonStyle.Render("No")
	}

	return prompt + "\n\n  " + yesBtn + "  " + noBtn + "\n\n" +
		helpStyle.Render("←/→ move • enter confirm • esc cancel")
}

// RunConfirm runs a full-screen yes/no prompt and returns the answer, or
// ErrCancelled if the person backed out instead of answering.
func RunConfirm(prompt string, defaultYes, destructive bool) (bool, error) {
	m := NewConfirm(prompt, defaultYes, destructive)
	final, err := tea.NewProgram(m).Run()
	if err != nil {
		return false, err
	}
	fm := final.(ConfirmModel)
	if fm.cancelled {
		return false, ErrCancelled
	}
	return fm.yes, nil
}

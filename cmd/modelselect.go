package cmd

import (
	"bufio"
	"fmt"
	"io"
	"strings"

	"github.com/RunanywhereAI/wally/config"
	"github.com/RunanywhereAI/wally/errmap"
	"github.com/RunanywhereAI/wally/runanywhere"
)

const (
	ansiYellow = "\x1b[33m"
	ansiReset  = "\x1b[0m"
)

type prompter interface {
	Confirm(question string) (bool, error)
	Ask(question string) (string, error)
}

// unrunnableLocal reports a saved default that points at an on-device model this
// build cannot run (engine off, weights not downloaded, or a non-GGUF backend),
// so it is skipped rather than dead-ending every launch.
func unrunnableLocal(model string) bool {
	if !runanywhere.IsLocalModel(model) {
		return false
	}
	if !runanywhere.OnDeviceEnabled() {
		return true
	}
	m, ok := runanywhere.FindInstalledModel(model)
	return !ok || !runanywhere.EngineServable(m)
}

// selectModel resolves which model a launch uses, handling the not-signed-in
// edge cases in plan 5.5. An explicit name or a configured default wins; a
// signed-in caller with neither is asked to name one; a signed-out caller falls
// back to an installed on-device tool-calling model after a yellow warning it
// can choose to stop showing.
func selectModel(explicit string, signedIn bool, p prompter, out io.Writer, color bool) (string, error) {
	if explicit != "" {
		return explicit, nil
	}
	prefs, _ := config.LoadPrefs()
	if prefs.DefaultModel != "" && !unrunnableLocal(prefs.DefaultModel) {
		return prefs.DefaultModel, nil
	}
	if signedIn {
		name, err := p.Ask("Which model? Name one, or set a default with: wally harness")
		if err != nil {
			return "", err
		}
		if name = strings.TrimSpace(name); name != "" {
			return name, nil
		}
		return "", errmap.NewNoModelSpecified()
	}

	local, ok := runanywhere.FirstToolCallingLocalModel()
	if !ok || !runanywhere.OnDeviceEnabled() {
		return "", errmap.NewNotSignedIn().WithDetail("On-device inference is not enabled in this build yet, and you are not signed in.")
	}

	if prefs.SkipOnDeviceNoLoginAsk {
		if prefs.AllowOnDeviceNoLogin {
			return local.ID, nil
		}
		return "", errmap.NewNotSignedIn()
	}

	warn := "You are not signed in to Wally Cloud."
	if color {
		warn = ansiYellow + warn + ansiReset
	}
	fmt.Fprintln(out, warn)

	allow, err := p.Confirm("Continue with an on-device model?")
	if err != nil {
		return "", err
	}
	if !allow {
		return "", errmap.NewNotSignedIn()
	}
	again, err := p.Confirm("Show this message again next time?")
	if err != nil {
		return "", err
	}
	prefs.AllowOnDeviceNoLogin = true
	prefs.SkipOnDeviceNoLoginAsk = !again
	_ = config.SavePrefs(prefs)
	return local.ID, nil
}

type stdinPrompter struct {
	r   *bufio.Reader
	out io.Writer
}

func newStdinPrompter(in io.Reader, out io.Writer) *stdinPrompter {
	return &stdinPrompter{r: bufio.NewReader(in), out: out}
}

func (p *stdinPrompter) Confirm(question string) (bool, error) {
	fmt.Fprintf(p.out, "%s [y/N] ", question)
	line, err := p.r.ReadString('\n')
	if err != nil && line == "" {
		return false, err
	}
	a := strings.ToLower(strings.TrimSpace(line))
	return a == "y" || a == "yes", nil
}

func (p *stdinPrompter) Ask(question string) (string, error) {
	fmt.Fprintf(p.out, "%s\n> ", question)
	line, err := p.r.ReadString('\n')
	if err != nil && line == "" {
		return "", err
	}
	return strings.TrimSpace(line), nil
}

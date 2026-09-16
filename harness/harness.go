package harness

import "github.com/RunanywhereAI/wally/runanywhere"

// cleanup restores the environment and deletes any temp config the launch wrote;
// it must run when the child exits.
type Wire func(ep runanywhere.Endpoint, model string, args []string) (env, argv []string, cleanup func(), err error)

type Harness struct {
	Name    string
	Command string
	Summary string
	Wire    Wire
	// Impl is the concrete harness, type-asserted for the capability
	// interfaces below. A launcher reaches them through the accessors.
	Impl any
}

type Installable interface {
	InstallHint() string
}

// Installer is a harness that can install its tool. InstallCommand is one shell
// command line, the single source both the displayed hint and the auto-install
// run take, so what a person sees is exactly what runs.
type Installer interface {
	InstallCommand() string
}

// Uninstaller is a harness that can remove its tool from the machine.
// UninstallCommand is one shell command line, run only after the person
// confirms a destructive prompt.
type Uninstaller interface {
	UninstallCommand() string
}

type ModelLimits interface {
	NeedsModelLimits() bool
}

type ConfigPreserving interface {
	PreservesConfig() bool
}

type Restorable interface {
	Restore() error
}

func (h Harness) InstallHint() (string, bool) {
	if i, ok := h.Impl.(Installable); ok {
		return i.InstallHint(), true
	}
	return "", false
}

func (h Harness) InstallCommand() (string, bool) {
	if i, ok := h.Impl.(Installer); ok {
		return i.InstallCommand(), true
	}
	return "", false
}

func (h Harness) UninstallCommand() (string, bool) {
	if u, ok := h.Impl.(Uninstaller); ok {
		return u.UninstallCommand(), true
	}
	return "", false
}

func (h Harness) NeedsModelLimits() bool {
	i, ok := h.Impl.(ModelLimits)
	return ok && i.NeedsModelLimits()
}

func (h Harness) PreservesConfig() bool {
	i, ok := h.Impl.(ConfigPreserving)
	return ok && i.PreservesConfig()
}

func (h Harness) Restore() error {
	if r, ok := h.Impl.(Restorable); ok {
		return r.Restore()
	}
	return nil
}

var Registry []Harness

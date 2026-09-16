// Package errmap maps known errors to one-line user-facing messages.
package errmap

import (
	"errors"

	"github.com/RunanywhereAI/wally/runanywhere"
)

// Kind identifies which case an Error maps to, so a caller can switch on it
// (an HTTP status, an exit code) without string-matching the message.
type Kind int

const (
	KindUnknown Kind = iota
	KindNotSignedIn
	KindOnDeviceNotEnabled
	KindModelNotInstalled
	KindDaemonNotRunning
	KindHarnessNotInstalled
	KindNoModelSpecified
	KindModelNotAvailable
	KindConsoleUnreachable
	KindModelBackendUnsupported
)

// Error is a user-facing error: a plain-language sentence, optionally ending
// in a command that Format highlights, plus the technical detail a
// --verbose caller wants instead of the plain sentence.
type Error struct {
	kind   Kind
	prefix string // sentence up to and including the fix's lead-in, e.g. "Not signed in. Run: "
	fix    string // the command itself, highlighted when color is enabled; empty if there is none
	detail string // technical detail for --verbose; empty if the plain message already says everything
	cause  error
}

func (e *Error) Error() string { return e.prefix + e.fix }

func (e *Error) Unwrap() error { return e.cause }

func (e *Error) Kind() Kind { return e.kind }

// Detail is the technical text for a --verbose caller: the wrapped error's
// message, or the plain message itself when there is nothing more specific.
func (e *Error) Detail() string {
	if e.detail != "" {
		return e.detail
	}
	return e.Error()
}

// NewNotSignedIn maps runanywhere.ErrNotSignedIn to its fix.
func NewNotSignedIn() *Error {
	return &Error{
		kind:   KindNotSignedIn,
		prefix: "Not signed in. Run: ",
		fix:    "wally login",
		cause:  runanywhere.ErrNotSignedIn,
	}
}

// NewOnDeviceNotEnabled maps runanywhere.ErrOnDeviceNotEnabled. There is no
// fix command yet, so the message is the sentinel's own text verbatim.
func NewOnDeviceNotEnabled() *Error {
	return &Error{
		kind:   KindOnDeviceNotEnabled,
		prefix: runanywhere.ErrOnDeviceNotEnabled.Error(),
		cause:  runanywhere.ErrOnDeviceNotEnabled,
	}
}

// NewModelNotInstalled reports that model is not on this machine and how to
// pull it.
func NewModelNotInstalled(model string) *Error {
	return &Error{
		kind:   KindModelNotInstalled,
		prefix: `model "` + model + `" is not on this machine. Run: `,
		fix:    "wally models pull " + model,
	}
}

// NewDaemonNotRunning reports that the background daemon a command needs is
// down and how to start it.
func NewDaemonNotRunning() *Error {
	return &Error{
		kind:   KindDaemonNotRunning,
		prefix: "The daemon is not running. Start it from the menu bar, or run: ",
		fix:    "wally serve",
	}
}

// NewHarnessNotInstalled reports that the named coding harness is not on
// PATH and how to install it. hint is the exact install command, e.g.
// "npm i -g opencode-ai" or "curl -fsSL https://opencode.ai/install | bash".
func NewHarnessNotInstalled(harness, hint string) *Error {
	return &Error{
		kind:   KindHarnessNotInstalled,
		prefix: harness + " is not installed on this machine. Install it with: ",
		fix:    hint,
	}
}

// NewNoModelSpecified reports that no model was named and none is configured.
func NewNoModelSpecified() *Error {
	return &Error{
		kind:   KindNoModelSpecified,
		prefix: "No model specified. Name one, for example: ",
		fix:    "wally run <model>",
	}
}

// NewModelNotAvailable reports that a named cloud model is not one the
// signed-in account can use, and how to see the ones it can.
func NewModelNotAvailable(model string) *Error {
	return &Error{
		kind:   KindModelNotAvailable,
		prefix: `model "` + model + `" is not available to your account. Run: `,
		fix:    "wally models list",
	}
}

// NewModelBackendUnsupported reports that an installed on-device model uses a
// backend this build cannot run yet (for example MLX). Only GGUF runs today.
func NewModelBackendUnsupported(model string) *Error {
	return &Error{
		kind:   KindModelBackendUnsupported,
		prefix: `model "` + model + `" needs a backend that is not in this build yet. Only GGUF models run on-device now.`,
	}
}

// NewConsoleUnreachable reports that the wally console could not be reached, so
// a caller can say so plainly instead of surfacing a raw network error.
func NewConsoleUnreachable() *Error {
	return &Error{
		kind:   KindConsoleUnreachable,
		prefix: "The Wally console is unreachable right now. Check your connection and try again.",
	}
}

// WithDetail attaches technical detail (a raw error, a status code, a
// stack) for a --verbose caller. It has no effect on the default message.
func (e *Error) WithDetail(detail string) *Error {
	e.detail = detail
	return e
}

const (
	ansiBlue  = "\x1b[34m"
	ansiReset = "\x1b[0m"
)

// ColorEnabled decides whether Format may emit ANSI color. noColorSet is
// whether the NO_COLOR environment variable is present at all, per
// https://no-color.org ("regardless of its value"). The caller does the
// lookup and passes the presence check in, so this stays a pure function of
// its two inputs. tty is whether the destination the message is headed for
// is a terminal.
func ColorEnabled(noColorSet, tty bool) bool {
	if noColorSet {
		return false
	}
	return tty
}

// Format renders err as the one-line message wally shows by default.
//
// err may be an *Error, or any error matched via errors.Is against a
// runanywhere sentinel (so a caller that never constructed an *Error still
// gets the mapped message). Anything else falls back to err.Error().
//
// When color is true, the fix command, the part of the message a person
// needs to type, is highlighted in blue; otherwise the message is plain
// text. Callers decide color with ColorEnabled rather than Format sniffing
// a writer or the environment itself, so this function stays pure and
// testable.
func Format(err error, color bool) string {
	if err == nil {
		return ""
	}

	var mapped *Error
	switch {
	case errors.As(err, &mapped):
	case errors.Is(err, runanywhere.ErrNotSignedIn):
		mapped = NewNotSignedIn()
	case errors.Is(err, runanywhere.ErrOnDeviceNotEnabled):
		mapped = NewOnDeviceNotEnabled()
	default:
		return err.Error()
	}

	if mapped.fix == "" {
		return mapped.prefix
	}
	if !color {
		return mapped.prefix + mapped.fix
	}
	return mapped.prefix + ansiBlue + mapped.fix + ansiReset
}

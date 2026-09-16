package cmd

import (
	"errors"
	"fmt"
	"io"

	"github.com/spf13/cobra"

	"github.com/RunanywhereAI/wally/credstore"
	"github.com/RunanywhereAI/wally/tui"
)

// scopeKind distinguishes how a scope is sized and removed: a plain
// directory, a union of several (the "all data" convenience row), or the
// credstore-backed session, which has no wally-owned directory of its own.
type scopeKind int

const (
	scopeDir scopeKind = iota
	scopeUnion
	scopeCredentials
)

// uninstallScope is one row of the uninstall checklist. paths holds the
// directories a scopeDir/scopeUnion scope owns; for scopeCredentials it
// holds credstore's profile directory, used only to size whatever credstore
// leaves there, never to remove it directly (removal always goes through
// credstore.Clear so the OS keystore is cleared too).
type uninstallScope struct {
	title string
	kind  scopeKind
	paths []string
}

// buildUninstallScopes resolves every real, wally-owned location the
// uninstall command can touch. "All data" is deliberately the union of the
// four directory scopes, not a raw wipe of credstore's profile directory:
// credstore's fallback credential files live directly under that directory,
// and a directory-level scope must never sign the person out as a side
// effect. Signing out is its own scope below.
func buildUninstallScopes() ([]uninstallScope, error) {
	chats, err := chatsDir()
	if err != nil {
		return nil, err
	}
	prefs, err := prefsFilePath()
	if err != nil {
		return nil, err
	}
	harness, err := harnessStateDir()
	if err != nil {
		return nil, err
	}
	models := modelsDir()
	profile, err := credstore.ProfileDir()
	if err != nil {
		return nil, err
	}

	return []uninstallScope{
		{title: "Models", kind: scopeDir, paths: []string{models}},
		{title: "Chats", kind: scopeDir, paths: []string{chats}},
		{title: "Configs", kind: scopeDir, paths: []string{prefs}},
		{title: "Harness integration state", kind: scopeDir, paths: []string{harness}},
		{title: "All data", kind: scopeUnion, paths: []string{models, chats, prefs, harness}},
		{title: "Sign-in / credentials", kind: scopeCredentials, paths: []string{profile}},
	}, nil
}

// size reports the scope's on-disk footprint. For scopeCredentials this is
// not the session itself (that mostly lives in the OS keychain, not on
// disk) but whatever credstore's profile directory holds outside the other
// scopes' own subdirectories.
func (s uninstallScope) size() (int64, error) {
	switch s.kind {
	case scopeCredentials:
		if len(s.paths) == 0 {
			return 0, nil
		}
		return dirSizeExcluding(s.paths[0], "chats", "harness", prefsFileName)
	default:
		var total int64
		for _, p := range s.paths {
			n, err := dirSize(p)
			if err != nil {
				return 0, err
			}
			total += n
		}
		return total, nil
	}
}

// detail reports the scope's status line: signed-in identity for the
// credentials scope, on-disk size for every other.
func (s uninstallScope) detail() string {
	if s.kind == scopeCredentials {
		store, err := credstore.New()
		if err == nil {
			if creds, err := store.Load(); err == nil && creds.SignedIn() {
				return "signed in as " + creds.Email
			}
		}
		return "not signed in"
	}
	size, err := s.size()
	if err != nil {
		return "could not read size: " + err.Error()
	}
	if size == 0 {
		return "nothing on disk"
	}
	return formatBytes(size) + " on disk"
}

// remove deletes the scope and reports what happened. It never partially
// signs someone out: scopeCredentials always goes through credstore.Clear,
// which clears the OS keystore entry and the file fallback together and
// verifies the session is actually gone.
func (s uninstallScope) remove() (string, error) {
	if s.kind == scopeCredentials {
		store, err := credstore.New()
		if err != nil {
			return "", err
		}
		creds, err := store.Load()
		if err != nil {
			return "", err
		}
		wasSignedIn := creds.SignedIn()
		if err := store.Clear(); err != nil {
			return "", err
		}
		if !wasSignedIn {
			return "not signed in, nothing to remove", nil
		}
		return "signed out", nil
	}

	removedAny := false
	for _, p := range s.paths {
		existed, err := removeDir(p)
		if err != nil {
			return "", err
		}
		removedAny = removedAny || existed
	}
	if !removedAny {
		return "nothing to remove", nil
	}
	return "removed", nil
}

func newUninstallCmd() *cobra.Command {
	return &cobra.Command{
		Use:     "uninstall",
		Short:   "Remove wally data from this machine: models, chats, configs, harness state, or your sign-in",
		GroupID: groupAbout,
		Args:    cobra.NoArgs,
		RunE: func(cmd *cobra.Command, _ []string) error {
			return runUninstall(cmd.OutOrStdout(), tui.RunMultiList, confirmDeletion)
		},
	}
}

func confirmDeletion(prompt string) (bool, error) {
	return tui.RunConfirm(prompt, false, true)
}

// runUninstall is the full flow: size every scope, let the person pick which
// to remove, name exactly what that means and wait for an explicit yes, then
// delete. select and confirm are injected so tests can drive the flow
// without a terminal; nothing under uninstallScope.remove is ever called
// before confirm returns true.
func runUninstall(out io.Writer, selectScopes func(title string, items []tui.Item, help string) ([]int, error), confirm func(string) (bool, error)) error {
	scopes, err := buildUninstallScopes()
	if err != nil {
		return err
	}

	items := make([]tui.Item, len(scopes))
	for i, s := range scopes {
		items[i] = tui.Item{Title: s.title, Detail: s.detail()}
	}

	checked, err := selectScopes("Remove wally data", items, "")
	if errors.Is(err, tui.ErrCancelled) {
		fmt.Fprintln(out, "Nothing removed.")
		return nil
	}
	if err != nil {
		return err
	}

	chosen := make([]uninstallScope, 0, len(checked))
	var total int64
	for _, idx := range checked {
		chosen = append(chosen, scopes[idx])
		size, err := scopes[idx].size()
		if err != nil {
			return err
		}
		total += size
	}

	prompt := fmt.Sprintf("Delete %d scope(s), reclaiming about %s? This cannot be undone.", len(chosen), formatBytes(total))
	yes, err := confirm(prompt)
	if errors.Is(err, tui.ErrCancelled) {
		fmt.Fprintln(out, "Nothing removed.")
		return nil
	}
	if err != nil {
		return err
	}
	if !yes {
		fmt.Fprintln(out, "Nothing removed.")
		return nil
	}

	for _, s := range chosen {
		note, err := s.remove()
		if err != nil {
			fmt.Fprintf(out, "%s: could not remove: %v\n", s.title, err)
			continue
		}
		fmt.Fprintf(out, "%s: %s\n", s.title, note)
	}
	return nil
}

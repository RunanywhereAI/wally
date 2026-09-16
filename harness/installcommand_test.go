package harness

import (
	"strings"
	"testing"
)

// TestInstallCommandIsABareRunnableLine checks every harness's InstallCommand
// against the one property that makes it safe to hand straight to a shell:
// no prose wrapped around it, and InstallHint quoting the exact same string,
// so what a person sees and what wally executes can never drift apart.
func TestInstallCommandIsABareRunnableLine(t *testing.T) {
	cases := []struct {
		name string
		impl interface {
			Installable
			Installer
		}
	}{
		{"opencode", OpenCode{}},
		{"hermes", Hermes{}},
		{"openclaw", OpenClaw{}},
		{"deepseek", DeepSeek{}},
		{"claude-code", ClaudeCode{}},
		{"claude-desktop", ClaudeDesktop{}},
	}

	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			command := tc.impl.InstallCommand()
			if strings.TrimSpace(command) == "" {
				t.Fatal("InstallCommand() is empty")
			}
			if strings.ContainsRune(command, '—') {
				t.Errorf("InstallCommand() = %q, must not contain an em dash", command)
			}
			for _, prose := range []string{"install it with", "run this again", "`"} {
				if strings.Contains(command, prose) {
					t.Errorf("InstallCommand() = %q, must be a bare command line, not prose", command)
				}
			}

			hint := tc.impl.InstallHint()
			if hint != "Run: "+command {
				t.Errorf("InstallHint() = %q, want %q (the exact command InstallCommand runs)", hint, "Run: "+command)
			}
			if strings.ContainsRune(hint, '—') {
				t.Errorf("InstallHint() = %q, must not contain an em dash", hint)
			}
		})
	}
}

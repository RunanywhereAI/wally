package harness

import (
	"runtime"
	"strings"
	"testing"
)

// TestUninstallCommandMatchesInstaller checks every harness that ships an
// Uninstaller: npm-installed tools go out through npm, and the command is a
// bare, unattended shell line the same way InstallCommand is (no prompts, no
// prose wrapped around it).
func TestUninstallCommandMatchesInstaller(t *testing.T) {
	cases := []struct {
		name string
		impl Uninstaller
		want string
	}{
		{"openclaw", OpenClaw{}, "npm uninstall -g openclaw"},
		{"deepseek", DeepSeek{}, "npm uninstall -g @deepseek-ai/dsh"},
	}

	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			got := tc.impl.UninstallCommand()
			if got != tc.want {
				t.Errorf("UninstallCommand() = %q, want %q", got, tc.want)
			}
			if strings.TrimSpace(got) == "" {
				t.Fatal("UninstallCommand() is empty")
			}
			if strings.ContainsRune(got, '—') {
				t.Errorf("UninstallCommand() = %q, must not contain an em dash", got)
			}
		})
	}
}

func TestOpenCodeUninstallCommand(t *testing.T) {
	command := OpenCode{}.UninstallCommand()
	if runtime.GOOS == "windows" {
		if command != "npm uninstall -g opencode-ai" {
			t.Errorf("UninstallCommand() = %q, want the npm removal", command)
		}
		return
	}
	if command != "opencode uninstall --force" {
		t.Errorf("UninstallCommand() = %q, want opencode's own uninstall subcommand with --force", command)
	}
}

func TestHermesUninstallCommand(t *testing.T) {
	// --full drops the interactive "keep ~/.hermes/?" branch, --yes drops
	// the confirmation prompt; both are needed for this to run unattended.
	command := Hermes{}.UninstallCommand()
	if !strings.Contains(command, "--full") || !strings.Contains(command, "--yes") {
		t.Errorf("UninstallCommand() = %q, want both --full and --yes", command)
	}
}

func TestClaudeCodeUninstallCommand(t *testing.T) {
	command := ClaudeCode{}.UninstallCommand()
	if runtime.GOOS == "windows" {
		if !strings.Contains(command, `.local\bin\claude.exe`) || !strings.Contains(command, `.local\share\claude`) {
			t.Errorf("UninstallCommand() = %q, want it to remove both the launcher and the install dir", command)
		}
		return
	}
	if command != "rm -f ~/.local/bin/claude && rm -rf ~/.local/share/claude" {
		t.Errorf("UninstallCommand() = %q, want the native installer's documented removal", command)
	}
	if strings.Contains(command, "~/.claude") {
		t.Error("UninstallCommand() must not remove ~/.claude or ~/.claude.json: other Claude surfaces write there too")
	}
}

// TestClaudeDesktopHasNoUninstaller: it is a GUI app dragged in from a
// downloaded disk image, with no CLI uninstall path, so the harness must not
// claim one. The manager falls back to telling the person to remove it by
// hand.
func TestClaudeDesktopHasNoUninstaller(t *testing.T) {
	h := Harness{Name: "claude-desktop", Impl: ClaudeDesktop{}}
	if _, ok := h.UninstallCommand(); ok {
		t.Error("claude-desktop must not report an UninstallCommand: it is a GUI app with no CLI uninstaller")
	}
}

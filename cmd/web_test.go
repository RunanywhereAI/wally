package cmd

import "testing"

// TestNewWebCmd checks the command's wiring, not its RunE: RunE spawns a real
// daemon process through ensureDaemon, which every other daemon-launching
// command in this package (serve, chat, run) leaves untested for the same
// reason.
func TestNewWebCmd(t *testing.T) {
	cmd := newWebCmd()

	if got, want := cmd.Use, "web"; got != want {
		t.Errorf("Use = %q, want %q", got, want)
	}
	if got, want := cmd.GroupID, groupServe; got != want {
		t.Errorf("GroupID = %q, want %q", got, want)
	}
	if cmd.Short == "" {
		t.Error("Short must not be empty")
	}
	if cmd.RunE == nil {
		t.Error("RunE must be set")
	}
	if err := cmd.Args(cmd, []string{"extra"}); err == nil {
		t.Error("Args should reject a positional argument")
	}
}

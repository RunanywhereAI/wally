package errmap

import (
	"errors"
	"fmt"
	"strings"
	"testing"

	"github.com/RunanywhereAI/wally/runanywhere"
)

func TestFormat_Plain(t *testing.T) {
	tests := []struct {
		name string
		err  error
		want string
	}{
		{
			name: "not signed in",
			err:  NewNotSignedIn(),
			want: "Not signed in. Run: wally login",
		},
		{
			name: "on-device not enabled",
			err:  NewOnDeviceNotEnabled(),
			want: "on-device models are not enabled in this build yet",
		},
		{
			name: "model not installed",
			err:  NewModelNotInstalled("gemma-4"),
			want: `model "gemma-4" is not on this machine. Run: wally models pull gemma-4`,
		},
		{
			name: "daemon not running",
			err:  NewDaemonNotRunning(),
			want: "The daemon is not running. Start it from the menu bar, or run: wally serve",
		},
		{
			name: "harness not installed",
			err:  NewHarnessNotInstalled("opencode", "npm i -g opencode-ai"),
			want: "opencode is not installed on this machine. Install it with: npm i -g opencode-ai",
		},
		{
			name: "raw sentinel, not wrapped by a constructor",
			err:  runanywhere.ErrNotSignedIn,
			want: "Not signed in. Run: wally login",
		},
		{
			name: "raw sentinel wrapped by fmt.Errorf",
			err:  fmt.Errorf("resolve: %w", runanywhere.ErrOnDeviceNotEnabled),
			want: "on-device models are not enabled in this build yet",
		},
		{
			name: "unmapped error falls back to its own text",
			err:  errors.New("dial tcp: connection refused"),
			want: "dial tcp: connection refused",
		},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			if got := Format(tt.err, false); got != tt.want {
				t.Errorf("Format(%v, false) = %q, want %q", tt.err, got, tt.want)
			}
		})
	}
}

func TestFormat_Nil(t *testing.T) {
	if got := Format(nil, false); got != "" {
		t.Errorf("Format(nil, false) = %q, want empty string", got)
	}
	if got := Format(nil, true); got != "" {
		t.Errorf("Format(nil, true) = %q, want empty string", got)
	}
}

func TestFormat_Color(t *testing.T) {
	tests := []struct {
		name    string
		err     error
		wantFix string // the exact substring that must be wrapped in blue; empty means no fix exists
	}{
		{"not signed in", NewNotSignedIn(), "wally login"},
		{"model not installed", NewModelNotInstalled("gemma-4"), "wally models pull gemma-4"},
		{"daemon not running", NewDaemonNotRunning(), "wally serve"},
		{"harness not installed", NewHarnessNotInstalled("opencode", "npm i -g opencode-ai"), "npm i -g opencode-ai"},
		{"on-device not enabled has no fix to highlight", NewOnDeviceNotEnabled(), ""},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			colored := Format(tt.err, true)
			plain := Format(tt.err, false)

			if tt.wantFix == "" {
				if colored != plain {
					t.Errorf("Format(%v, true) = %q, want unchanged from plain %q (no fix to highlight)", tt.err, colored, plain)
				}
				if strings.Contains(colored, ansiBlue) {
					t.Errorf("Format(%v, true) = %q, must not contain ANSI blue when there is no fix", tt.err, colored)
				}
				return
			}

			want := strings.Replace(plain, tt.wantFix, ansiBlue+tt.wantFix+ansiReset, 1)
			if colored != want {
				t.Errorf("Format(%v, true) = %q, want %q", tt.err, colored, want)
			}
			if strings.Contains(plain, ansiBlue) {
				t.Errorf("Format(%v, false) = %q, must not contain ANSI blue", tt.err, plain)
			}
			if !strings.Contains(colored, ansiBlue) || !strings.Contains(colored, ansiReset) {
				t.Errorf("Format(%v, true) = %q, want it wrapped in blue/reset", tt.err, colored)
			}
		})
	}
}

func TestFormat_NoEmDash(t *testing.T) {
	errs := []error{
		NewNotSignedIn(),
		NewOnDeviceNotEnabled(),
		NewModelNotInstalled("gemma-4"),
		NewDaemonNotRunning(),
		NewHarnessNotInstalled("opencode", "npm i -g opencode-ai"),
	}

	for _, err := range errs {
		for _, color := range []bool{false, true} {
			got := Format(err, color)
			if strings.ContainsRune(got, '—') {
				t.Errorf("Format(%v, %v) = %q, contains an em-dash", err, color, got)
			}
		}
	}
}

func TestModelNotInstalled_Interpolation(t *testing.T) {
	tests := []string{"gemma-4", "llama3.2-vision", "tiny-tool"}
	for _, model := range tests {
		t.Run(model, func(t *testing.T) {
			want := "wally models pull " + model
			got := Format(NewModelNotInstalled(model), false)
			if !strings.HasSuffix(got, want) {
				t.Errorf("Format(NewModelNotInstalled(%q), false) = %q, want suffix %q", model, got, want)
			}
		})
	}
}

func TestError_Unwrap(t *testing.T) {
	if !errors.Is(NewNotSignedIn(), runanywhere.ErrNotSignedIn) {
		t.Error("errors.Is(NewNotSignedIn(), runanywhere.ErrNotSignedIn) = false, want true")
	}
	if !errors.Is(NewOnDeviceNotEnabled(), runanywhere.ErrOnDeviceNotEnabled) {
		t.Error("errors.Is(NewOnDeviceNotEnabled(), runanywhere.ErrOnDeviceNotEnabled) = false, want true")
	}
	if errors.Is(NewDaemonNotRunning(), runanywhere.ErrNotSignedIn) {
		t.Error("errors.Is(NewDaemonNotRunning(), runanywhere.ErrNotSignedIn) = true, want false")
	}
}

func TestError_Kind(t *testing.T) {
	tests := []struct {
		err  *Error
		want Kind
	}{
		{NewNotSignedIn(), KindNotSignedIn},
		{NewOnDeviceNotEnabled(), KindOnDeviceNotEnabled},
		{NewModelNotInstalled("gemma-4"), KindModelNotInstalled},
		{NewDaemonNotRunning(), KindDaemonNotRunning},
		{NewHarnessNotInstalled("opencode", "npm i -g opencode-ai"), KindHarnessNotInstalled},
	}
	for _, tt := range tests {
		if got := tt.err.Kind(); got != tt.want {
			t.Errorf("%v.Kind() = %v, want %v", tt.err, got, tt.want)
		}
	}
}

func TestError_Detail(t *testing.T) {
	err := NewDaemonNotRunning()
	if got, want := err.Detail(), err.Error(); got != want {
		t.Errorf("Detail() = %q before WithDetail, want it to fall back to Error() %q", got, want)
	}

	err.WithDetail("dial unix /tmp/wally.sock: connect: connection refused")
	if got, want := err.Detail(), "dial unix /tmp/wally.sock: connect: connection refused"; got != want {
		t.Errorf("Detail() = %q after WithDetail, want %q", got, want)
	}
	if got, want := err.Error(), "The daemon is not running. Start it from the menu bar, or run: wally serve"; got != want {
		t.Errorf("Error() = %q after WithDetail, want unchanged %q", got, want)
	}
}

func TestColorEnabled(t *testing.T) {
	tests := []struct {
		name       string
		noColorSet bool
		tty        bool
		want       bool
	}{
		{"tty and NO_COLOR unset", false, true, true},
		{"non-tty and NO_COLOR unset", false, false, false},
		{"tty but NO_COLOR set", true, true, false},
		{"non-tty and NO_COLOR set", true, false, false},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			if got := ColorEnabled(tt.noColorSet, tt.tty); got != tt.want {
				t.Errorf("ColorEnabled(%v, %v) = %v, want %v", tt.noColorSet, tt.tty, got, tt.want)
			}
		})
	}
}

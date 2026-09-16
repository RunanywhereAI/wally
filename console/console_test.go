package console

import (
	"testing"

	"github.com/RunanywhereAI/wally/config"
)

func TestResolveBaseURL(t *testing.T) {
	t.Run("falls back to config default", func(t *testing.T) {
		t.Setenv(envBaseURLOverride, "")
		if got := ResolveBaseURL(); got != config.ConsoleAPIURL() {
			t.Fatalf("ResolveBaseURL() = %q, want %q", got, config.ConsoleAPIURL())
		}
	})

	t.Run("env override wins", func(t *testing.T) {
		t.Setenv(envBaseURLOverride, "https://console-dev.example.com/api-dev")
		if got, want := ResolveBaseURL(), "https://console-dev.example.com/api-dev"; got != want {
			t.Fatalf("ResolveBaseURL() = %q, want %q", got, want)
		}
	})
}

func TestResolveWebOrigin(t *testing.T) {
	t.Run("falls back to config default", func(t *testing.T) {
		t.Setenv(envWebOriginOverride, "")
		if got := ResolveWebOrigin(); got != config.ConsoleWebOrigin() {
			t.Fatalf("ResolveWebOrigin() = %q, want %q", got, config.ConsoleWebOrigin())
		}
	})

	t.Run("env override wins", func(t *testing.T) {
		t.Setenv(envWebOriginOverride, "https://console-dev.example.com")
		if got, want := ResolveWebOrigin(), "https://console-dev.example.com"; got != want {
			t.Fatalf("ResolveWebOrigin() = %q, want %q", got, want)
		}
	})
}

func TestSessionTokenSafe(t *testing.T) {
	cases := []struct {
		name  string
		token string
		want  bool
	}{
		{"empty", "", false},
		{"ordinary jwt-shaped token", "abc123.DEF-456_ghi~jkl+mno/pqr=", true},
		{"header injection attempt", "token\r\nX-Injected: true", false},
		{"too long", string(make([]byte, 8193)), false},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			if got := sessionTokenSafe(tc.token); got != tc.want {
				t.Fatalf("sessionTokenSafe(%q) = %v, want %v", tc.token, got, tc.want)
			}
		})
	}
}

func TestDisplaySafe(t *testing.T) {
	if !displaySafe("person@example.com", 320) {
		t.Fatal("expected an ordinary email to be display safe")
	}
	if displaySafe("person@example.com\x1b[31m", 320) {
		t.Fatal("expected an escape sequence to be rejected")
	}
	if displaySafe("", 320) {
		t.Fatal("expected an empty string to be rejected")
	}
}

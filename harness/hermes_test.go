package harness

import (
	"runtime"
	"slices"
	"strings"
	"testing"

	"github.com/RunanywhereAI/wally/runanywhere"
)

func TestHermesKeyVariable(t *testing.T) {
	cases := []struct {
		name    string
		baseURL string
		want    string
	}{
		{"loopback takes no key", "http://127.0.0.1:11535/v1", ""},
		{"localhost takes no key", "http://localhost:11535/v1", ""},
		{"bare ip takes no key", "http://192.168.1.5:11535/v1", ""},
		{"vendor host", "https://api.anthropic.com/v1", "ANTHROPIC_API_KEY"},
		{"vendor host without api. label", "https://runanywhere.ai/v1", "RUNANYWHERE_API_KEY"},
		{"openai is host-gated on its own domain", "https://api.openai.com/v1", ""},
		{"openrouter is host-gated on its own domain", "https://openrouter.ai/v1", ""},
		{"ollama is host-gated on its own domain", "https://ollama.com/v1", ""},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			if got := hermesKeyVariable(tc.baseURL); got != tc.want {
				t.Errorf("hermesKeyVariable(%q) = %q, want %q", tc.baseURL, got, tc.want)
			}
		})
	}
}

func TestHermesWireSetsCustomEndpointEnv(t *testing.T) {
	ep := runanywhere.Endpoint{BaseURL: "https://api.runanywhere.ai/v1", APIKey: "sk-test"}
	env, argv, cleanup, err := Hermes{}.Wire(ep, "qwen3-coder", nil)
	if err != nil {
		t.Fatalf("Wire: %v", err)
	}

	want := map[string]string{
		"CUSTOM_BASE_URL":           ep.BaseURL,
		"HERMES_INFERENCE_PROVIDER": "custom",
		"HERMES_INFERENCE_MODEL":    "qwen3-coder",
		"HERMES_MODEL":              "qwen3-coder",
		"RUNANYWHERE_API_KEY":       "sk-test",
		"OPENAI_BASE_URL":           ep.BaseURL,
	}
	got := map[string]string{}
	for _, kv := range env {
		key, value, _ := strings.Cut(kv, "=")
		got[key] = value
	}
	for key, value := range want {
		if got[key] != value {
			t.Errorf("env[%s] = %q, want %q", key, got[key], value)
		}
	}

	wantArgv := []string{"--provider", "custom", "--model", "qwen3-coder", "--tui"}
	if !slices.Equal(argv, wantArgv) {
		t.Errorf("argv = %v, want %v (default --tui when no args given)", argv, wantArgv)
	}
	if cleanup == nil {
		t.Fatal("cleanup is nil")
	}
	cleanup()
}

func TestHermesWireTheirArgsWinWhole(t *testing.T) {
	ep := runanywhere.Endpoint{BaseURL: "http://127.0.0.1:11535/v1"}
	_, argv, _, err := Hermes{}.Wire(ep, "qwen3-coder", []string{"-z", "write a test"})
	if err != nil {
		t.Fatalf("Wire: %v", err)
	}
	want := []string{"--provider", "custom", "--model", "qwen3-coder", "-z", "write a test"}
	if !slices.Equal(argv, want) {
		t.Errorf("argv = %v, want %v", argv, want)
	}
}

func TestHermesWireOpenAIBaseURLAlwaysSetToTheSameEndpoint(t *testing.T) {
	// OPENAI_BASE_URL is not consulted for routing (CUSTOM_BASE_URL wins in
	// the resolver's own precedence); it exists only so
	// hermes_cli.main._has_any_provider_configured sees a provider and skips
	// the first-run setup wizard. It must always be set, loopback or not
	// (that guard fires regardless of endpoint), and always the same value
	// as CUSTOM_BASE_URL, so nothing that does read it resolves anywhere
	// different.
	ep := runanywhere.Endpoint{BaseURL: "http://127.0.0.1:11535/v1", Local: true}
	env, _, _, err := Hermes{}.Wire(ep, "qwen3-coder", nil)
	if err != nil {
		t.Fatalf("Wire: %v", err)
	}
	got := map[string]string{}
	for _, kv := range env {
		key, value, _ := strings.Cut(kv, "=")
		got[key] = value
	}
	if got["OPENAI_BASE_URL"] != ep.BaseURL {
		t.Errorf("OPENAI_BASE_URL = %q, want %q", got["OPENAI_BASE_URL"], ep.BaseURL)
	}
	if got["OPENAI_BASE_URL"] != got["CUSTOM_BASE_URL"] {
		t.Errorf("OPENAI_BASE_URL = %q, CUSTOM_BASE_URL = %q, want the same value", got["OPENAI_BASE_URL"], got["CUSTOM_BASE_URL"])
	}
}

func TestHermesInstallCommandSkipsTheSetupWizard(t *testing.T) {
	// hermes-agent's installer runs `hermes setup` interactively at the end
	// of a plain install (scripts/install.sh run_setup_wizard); wally
	// already names its own provider and endpoint, so that wizard has
	// nothing left to ask and must not run.
	command := Hermes{}.InstallCommand()
	if runtime.GOOS == "windows" {
		if !strings.Contains(command, "-SkipSetup") {
			t.Errorf("InstallCommand() = %q, want it to carry -SkipSetup", command)
		}
		return
	}
	if !strings.Contains(command, "--skip-setup") {
		t.Errorf("InstallCommand() = %q, want it to carry --skip-setup", command)
	}
}

func TestHermesWireLoopbackGetsNoKeyVariable(t *testing.T) {
	ep := runanywhere.Endpoint{BaseURL: "http://127.0.0.1:11535/v1", APIKey: "sk-test", Local: true}
	env, _, _, err := Hermes{}.Wire(ep, "qwen3-coder", nil)
	if err != nil {
		t.Fatalf("Wire: %v", err)
	}
	for _, kv := range env {
		key, _, _ := strings.Cut(kv, "=")
		if key == "RUNANYWHERE_API_KEY" {
			t.Errorf("env set %s for a loopback endpoint, want no key variable", key)
		}
	}
}

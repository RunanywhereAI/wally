package harness

import (
	"encoding/json"
	"strings"
	"testing"

	"github.com/RunanywhereAI/wally/runanywhere"
)

func TestOpenCodeBuildConfig(t *testing.T) {
	raw, err := OpenCode{}.BuildConfig("qwen3-coder", "http://127.0.0.1:11535/v1", "sk-test")
	if err != nil {
		t.Fatalf("BuildConfig: %v", err)
	}

	var config map[string]any
	if err := json.Unmarshal([]byte(raw), &config); err != nil {
		t.Fatalf("BuildConfig produced invalid JSON: %v", err)
	}
	if config["model"] != "runanywhere/qwen3-coder" {
		t.Errorf("model = %v, want runanywhere/qwen3-coder", config["model"])
	}
	provider := config["provider"].(map[string]any)["runanywhere"].(map[string]any)
	options := provider["options"].(map[string]any)
	if options["baseURL"] != "http://127.0.0.1:11535/v1" {
		t.Errorf("baseURL = %v, want the endpoint's base URL", options["baseURL"])
	}
	if options["apiKey"] != "sk-test" {
		t.Errorf("apiKey = %v, want sk-test", options["apiKey"])
	}
}

func TestOpenCodeBuildConfigPlaceholderKey(t *testing.T) {
	raw, err := OpenCode{}.BuildConfig("qwen3-coder", "http://127.0.0.1:11535/v1", "")
	if err != nil {
		t.Fatalf("BuildConfig: %v", err)
	}
	var config map[string]any
	if err := json.Unmarshal([]byte(raw), &config); err != nil {
		t.Fatalf("BuildConfig produced invalid JSON: %v", err)
	}
	provider := config["provider"].(map[string]any)["runanywhere"].(map[string]any)
	options := provider["options"].(map[string]any)
	if options["apiKey"] != "local" {
		t.Errorf("apiKey = %v, want the local placeholder", options["apiKey"])
	}
}

func TestOpenCodeWire(t *testing.T) {
	ep := runanywhere.Endpoint{BaseURL: "http://127.0.0.1:11535/v1", APIKey: "sk-test"}
	env, argv, cleanup, err := OpenCode{}.Wire(ep, "qwen3-coder", []string{"--continue"})
	if err != nil {
		t.Fatalf("Wire: %v", err)
	}
	if len(env) != 1 || !strings.HasPrefix(env[0], "OPENCODE_CONFIG_CONTENT=") {
		t.Fatalf("env = %v, want a single OPENCODE_CONFIG_CONTENT entry", env)
	}
	if len(argv) != 1 || argv[0] != "--continue" {
		t.Errorf("argv = %v, want the caller's args passed through unchanged", argv)
	}
	if cleanup == nil {
		t.Fatal("cleanup is nil")
	}
	cleanup() // must not panic; OpenCode writes no file to remove.
}

func TestOpenCodeInstallHintNamesTool(t *testing.T) {
	hint := OpenCode{}.InstallHint()
	if !strings.Contains(hint, "opencode") {
		t.Errorf("InstallHint() = %q, want it to name opencode", hint)
	}
}

func TestOpenCodePreservesConfig(t *testing.T) {
	o := OpenCode{}
	if !o.PreservesConfig() {
		t.Error("PreservesConfig() = false, want true: OpenCode never touches a config file")
	}
}

package harness

import (
	"encoding/json"
	"os"
	"path/filepath"
	"strings"
	"testing"

	"github.com/RunanywhereAI/wally/runanywhere"
)

func TestMergeOpenClawConfigPreservesOtherKeys(t *testing.T) {
	existing := `{
		"models": {"providers": {"anthropic": {"apiKey": "keep-me"}}},
		"agents": {"defaults": {"other": "keep-me-too"}},
		"gateway": {"token": "keep-me-three"}
	}`
	raw, err := mergeOpenClawConfig([]byte(existing), "qwen3-coder", "http://127.0.0.1:11535/v1", "")
	if err != nil {
		t.Fatalf("mergeOpenClawConfig: %v", err)
	}

	var config map[string]any
	if err := json.Unmarshal([]byte(raw), &config); err != nil {
		t.Fatalf("merged config is invalid JSON: %v", err)
	}

	gateway := config["gateway"].(map[string]any)
	if gateway["token"] != "keep-me-three" {
		t.Errorf("gateway.token = %v, want the person's own token preserved", gateway["token"])
	}
	providers := config["models"].(map[string]any)["providers"].(map[string]any)
	anthropic := providers["anthropic"].(map[string]any)
	if anthropic["apiKey"] != "keep-me" {
		t.Errorf("anthropic provider = %v, want it preserved alongside ours", anthropic)
	}
	defaults := config["agents"].(map[string]any)["defaults"].(map[string]any)
	if defaults["other"] != "keep-me-too" {
		t.Errorf("agents.defaults.other = %v, want it preserved", defaults["other"])
	}

	runanywhere := providers["runanywhere"].(map[string]any)
	if runanywhere["baseUrl"] != "http://127.0.0.1:11535/v1" {
		t.Errorf("runanywhere.baseUrl = %v, want the endpoint's base URL", runanywhere["baseUrl"])
	}
	primary := defaults["model"].(map[string]any)["primary"]
	if primary != "runanywhere/qwen3-coder" {
		t.Errorf("agents.defaults.model.primary = %v, want runanywhere/qwen3-coder", primary)
	}
}

func TestOpenClawWireNeverTouchesTheRealConfig(t *testing.T) {
	dir := t.TempDir()
	realConfig := filepath.Join(dir, "openclaw.json")
	original := []byte(`{"models":{"providers":{"anthropic":{"apiKey":"keep-me"}}}}`)
	if err := os.WriteFile(realConfig, original, 0o600); err != nil {
		t.Fatalf("seed real config: %v", err)
	}

	t.Setenv("OPENCLAW_STATE_DIR", dir)
	t.Setenv("OPENCLAW_CONFIG_PATH", realConfig)

	ep := runanywhere.Endpoint{BaseURL: "http://127.0.0.1:11535/v1"}
	env, argv, cleanup, err := OpenClaw{}.Wire(ep, "qwen3-coder", nil)
	if err != nil {
		t.Fatalf("Wire: %v", err)
	}
	defer cleanup()

	after, err := os.ReadFile(realConfig)
	if err != nil {
		t.Fatalf("read real config after Wire: %v", err)
	}
	if string(after) != string(original) {
		t.Fatalf("Wire mutated the real config: got %s, want it unchanged (%s)", after, original)
	}

	var tempPath string
	for _, kv := range env {
		if v, ok := strings.CutPrefix(kv, "OPENCLAW_CONFIG_PATH="); ok {
			tempPath = v
		}
	}
	if tempPath == "" {
		t.Fatal("Wire did not set OPENCLAW_CONFIG_PATH")
	}
	if tempPath == realConfig {
		t.Fatal("Wire pointed OPENCLAW_CONFIG_PATH at the real config instead of a temp copy")
	}
	merged, err := os.ReadFile(tempPath)
	if err != nil {
		t.Fatalf("read temp config: %v", err)
	}
	var mergedConfig map[string]any
	if err := json.Unmarshal(merged, &mergedConfig); err != nil {
		t.Fatalf("temp config is invalid JSON: %v", err)
	}
	providers := mergedConfig["models"].(map[string]any)["providers"].(map[string]any)
	if _, ok := providers["anthropic"]; !ok {
		t.Error("temp config dropped the person's existing anthropic provider")
	}
	if _, ok := providers["runanywhere"]; !ok {
		t.Error("temp config is missing the runanywhere provider Wire adds")
	}

	if len(argv) != 2 || argv[0] != "tui" || argv[1] != "--local" {
		t.Errorf("argv = %v, want the default [tui --local] when no args given", argv)
	}

	cleanup()
	if _, err := os.Stat(tempPath); !os.IsNotExist(err) {
		t.Errorf("cleanup did not remove the temp config at %s", tempPath)
	}
}

func TestOpenClawWireTheirArgsWinWhole(t *testing.T) {
	dir := t.TempDir()
	t.Setenv("OPENCLAW_STATE_DIR", dir)
	t.Setenv("OPENCLAW_CONFIG_PATH", filepath.Join(dir, "does-not-exist.json"))

	ep := runanywhere.Endpoint{BaseURL: "http://127.0.0.1:11535/v1"}
	_, argv, cleanup, err := OpenClaw{}.Wire(ep, "qwen3-coder", []string{"channels", "add"})
	if err != nil {
		t.Fatalf("Wire: %v", err)
	}
	defer cleanup()
	if len(argv) != 2 || argv[0] != "channels" || argv[1] != "add" {
		t.Errorf("argv = %v, want the caller's own args passed through unchanged", argv)
	}
}

func TestOpenClawRestoreSweepsStaleTempFiles(t *testing.T) {
	path, err := writeTempFile(openClawTempPattern, "{}")
	if err != nil {
		t.Fatalf("writeTempFile: %v", err)
	}
	c := OpenClaw{}
	if err := c.Restore(); err != nil {
		t.Fatalf("Restore: %v", err)
	}
	if _, err := os.Stat(path); !os.IsNotExist(err) {
		t.Errorf("Restore did not remove the stale temp config at %s", path)
	}
}

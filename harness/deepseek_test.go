package harness

import (
	"encoding/json"
	"os"
	"slices"
	"strings"
	"testing"

	"github.com/RunanywhereAI/wally/runanywhere"
)

func TestDeepSeekWantsHeadless(t *testing.T) {
	cases := []struct {
		name string
		args []string
		want bool
	}{
		{"no args opens the web ui", nil, false},
		{"a prompt is headless", []string{"write a test"}, true},
		{"a flag first is the web app being configured", []string{"--port", "8080"}, false},
		{"empty first token is not a prompt", []string{""}, false},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			if got := deepSeekWantsHeadless(tc.args); got != tc.want {
				t.Errorf("deepSeekWantsHeadless(%v) = %v, want %v", tc.args, got, tc.want)
			}
		})
	}
}

func TestBuildDeepSeekSettingsOmitsKeyEnvForLocal(t *testing.T) {
	raw := buildDeepSeekSettings("qwen3-coder", "http://127.0.0.1:11535/v1", "")
	var settings map[string]any
	if err := json.Unmarshal([]byte(raw), &settings); err != nil {
		t.Fatalf("buildDeepSeekSettings produced invalid JSON: %v", err)
	}
	provider := settings["llm-pi-ai"].(map[string]any)["providers"].(map[string]any)["runanywhere"].(map[string]any)
	if _, ok := provider["apiKeyEnv"]; ok {
		t.Error("apiKeyEnv present for a local endpoint, want it omitted so the route stays keyless")
	}
	if provider["baseURL"] != "http://127.0.0.1:11535/v1" {
		t.Errorf("baseURL = %v, want the endpoint's base URL", provider["baseURL"])
	}
}

func TestBuildDeepSeekSettingsSetsKeyEnvWhenGiven(t *testing.T) {
	raw := buildDeepSeekSettings("qwen3-coder", "https://api.runanywhere.ai/v1", "RUNANYWHERE_API_KEY")
	var settings map[string]any
	if err := json.Unmarshal([]byte(raw), &settings); err != nil {
		t.Fatalf("buildDeepSeekSettings produced invalid JSON: %v", err)
	}
	provider := settings["llm-pi-ai"].(map[string]any)["providers"].(map[string]any)["runanywhere"].(map[string]any)
	if provider["apiKeyEnv"] != "RUNANYWHERE_API_KEY" {
		t.Errorf("apiKeyEnv = %v, want RUNANYWHERE_API_KEY", provider["apiKeyEnv"])
	}
}

func TestBuildDeepSeekPatchEscapesSingleQuotes(t *testing.T) {
	patch := buildDeepSeekPatch("/tmp/it's a path.json", "model's-name")
	if !strings.Contains(patch, `'/tmp/it''s a path.json'`) {
		t.Errorf("patch = %q, want the settings path single-quote escaped", patch)
	}
	if !strings.Contains(patch, `'model''s-name'`) {
		t.Errorf("patch = %q, want the model id single-quote escaped", patch)
	}
}

func TestDeepSeekWireOpensWebByDefault(t *testing.T) {
	ep := runanywhere.Endpoint{BaseURL: "http://127.0.0.1:11535/v1"}
	env, argv, cleanup, err := DeepSeek{}.Wire(ep, "qwen3-coder", nil)
	if err != nil {
		t.Fatalf("Wire: %v", err)
	}
	defer cleanup()
	if len(argv) < 3 || argv[0] != "web" || argv[1] != "--patch" {
		t.Errorf("argv = %v, want [web --patch <path>] when the caller gave no prompt", argv)
	}
	if len(env) != 1 || env[0] != deepSeekKeyVariable+"=local" {
		t.Errorf("env = %v, want a placeholder %s for a local endpoint with no real key", env, deepSeekKeyVariable)
	}
}

func TestDeepSeekWireSettingsAlwaysCarryAPIKeyEnv(t *testing.T) {
	ep := runanywhere.Endpoint{BaseURL: "http://127.0.0.1:11535/v1"}
	_, argv, cleanup, err := DeepSeek{}.Wire(ep, "qwen3-coder", nil)
	if err != nil {
		t.Fatalf("Wire: %v", err)
	}
	defer cleanup()

	patchPath := argv[2] // [web --patch <path>]
	patchContents, err := os.ReadFile(patchPath)
	if err != nil {
		t.Fatalf("patch file was not written: %v", err)
	}
	_, after, ok := strings.Cut(string(patchContents), "path: '")
	if !ok {
		t.Fatalf("patch contents missing the settings path: %s", patchContents)
	}
	settingsPath, _, _ := strings.Cut(after, "'")

	data, err := os.ReadFile(settingsPath)
	if err != nil {
		t.Fatalf("read settings file: %v", err)
	}
	var settings map[string]any
	if err := json.Unmarshal(data, &settings); err != nil {
		t.Fatalf("settings file is invalid JSON: %v", err)
	}
	provider := settings["llm-pi-ai"].(map[string]any)["providers"].(map[string]any)["runanywhere"].(map[string]any)
	if provider["apiKeyEnv"] != deepSeekKeyVariable {
		t.Errorf("apiKeyEnv = %v, want %s even for a keyless local endpoint (dsh fails every request without one)", provider["apiKeyEnv"], deepSeekKeyVariable)
	}
}

func TestDeepSeekWireHeadlessOnPrompt(t *testing.T) {
	ep := runanywhere.Endpoint{BaseURL: "https://api.runanywhere.ai/v1", APIKey: "sk-test"}
	env, argv, cleanup, err := DeepSeek{}.Wire(ep, "qwen3-coder", []string{"write a test"})
	if err != nil {
		t.Fatalf("Wire: %v", err)
	}
	defer cleanup()
	if len(argv) < 4 || argv[0] != "--profile" || argv[1] != "headless" || argv[2] != "--patch" {
		t.Errorf("argv = %v, want [--profile headless --patch <path> write a test]", argv)
	}
	if argv[len(argv)-1] != "write a test" {
		t.Errorf("argv = %v, want the person's prompt appended after the overlay", argv)
	}
	found := false
	for _, kv := range env {
		if kv == "RUNANYWHERE_API_KEY=sk-test" {
			found = true
		}
	}
	if !found {
		t.Errorf("env = %v, want RUNANYWHERE_API_KEY=sk-test for a keyed endpoint", env)
	}
}

func TestDeepSeekWireRespectsNamedProfile(t *testing.T) {
	ep := runanywhere.Endpoint{BaseURL: "http://127.0.0.1:11535/v1"}
	_, argv, cleanup, err := DeepSeek{}.Wire(ep, "qwen3-coder", []string{"--profile", "custom"})
	if err != nil {
		t.Fatalf("Wire: %v", err)
	}
	defer cleanup()
	want := []string{"--patch"}
	if len(argv) < 2 || !slices.Equal(argv[:1], want) || argv[1] == "" {
		t.Errorf("argv = %v, want --patch <path> ahead of the person's own --profile custom", argv)
	}
	if !slices.Contains(argv, "--profile") || !slices.Contains(argv, "custom") {
		t.Errorf("argv = %v, want the person's own --profile custom preserved", argv)
	}
}

func TestDeepSeekWireRejectsConflictingPatchArg(t *testing.T) {
	ep := runanywhere.Endpoint{BaseURL: "http://127.0.0.1:11535/v1"}
	_, _, _, err := DeepSeek{}.Wire(ep, "qwen3-coder", []string{"--patch", "evil.yml"})
	if err == nil {
		t.Fatal("Wire did not reject a caller-supplied --patch, which we already manage")
	}
}

func TestDeepSeekWireCleanupRemovesTempFiles(t *testing.T) {
	ep := runanywhere.Endpoint{BaseURL: "http://127.0.0.1:11535/v1"}
	_, argv, cleanup, err := DeepSeek{}.Wire(ep, "qwen3-coder", nil)
	if err != nil {
		t.Fatalf("Wire: %v", err)
	}
	patchPath := argv[2]
	patchContents, err := os.ReadFile(patchPath)
	if err != nil {
		t.Fatalf("patch file was not written: %v", err)
	}
	// Pull the settings path back out of the patch it's quoted into, so
	// cleanup of both files can be checked without Wire exposing it directly.
	_, after, ok := strings.Cut(string(patchContents), "path: '")
	if !ok {
		t.Fatalf("patch contents missing the settings path: %s", patchContents)
	}
	settingsPath, _, _ := strings.Cut(after, "'")
	if _, err := os.Stat(settingsPath); err != nil {
		t.Fatalf("settings file was not written: %v", err)
	}

	cleanup()
	if _, err := os.Stat(patchPath); !os.IsNotExist(err) {
		t.Errorf("cleanup did not remove the patch file at %s", patchPath)
	}
	if _, err := os.Stat(settingsPath); !os.IsNotExist(err) {
		t.Errorf("cleanup did not remove the settings file at %s", settingsPath)
	}
}

func TestDeepSeekRestoreSweepsStaleTempFiles(t *testing.T) {
	settingsPath, err := writeTempFile(deepSeekSettingsGlob, "{}")
	if err != nil {
		t.Fatalf("writeTempFile: %v", err)
	}
	patchPath, err := writeTempFile(deepSeekPatchGlob, "[]")
	if err != nil {
		t.Fatalf("writeTempFile: %v", err)
	}
	d := DeepSeek{}
	if err := d.Restore(); err != nil {
		t.Fatalf("Restore: %v", err)
	}
	if _, err := os.Stat(settingsPath); !os.IsNotExist(err) {
		t.Errorf("Restore did not remove the stale settings file at %s", settingsPath)
	}
	if _, err := os.Stat(patchPath); !os.IsNotExist(err) {
		t.Errorf("Restore did not remove the stale patch file at %s", patchPath)
	}
}

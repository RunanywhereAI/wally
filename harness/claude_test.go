package harness

import (
	"os"
	"path/filepath"
	"slices"
	"strings"
	"testing"

	"github.com/RunanywhereAI/wally/catalog"
	"github.com/RunanywhereAI/wally/runanywhere"
)

func TestAnthropicBaseURLStripsV1(t *testing.T) {
	got := anthropicBaseURL("http://127.0.0.1:11535/v1")
	if got != "http://127.0.0.1:11535" {
		t.Errorf("anthropicBaseURL = %q, want the /v1 suffix stripped", got)
	}
}

func TestClaudeCodeWireSetsAnthropicEnv(t *testing.T) {
	ep := runanywhere.Endpoint{BaseURL: "https://api.runanywhere.ai/v1", APIKey: "sk-test"}
	env, argv, cleanup, err := ClaudeCode{}.Wire(ep, "qwen3-coder", []string{"--continue"})
	if err != nil {
		t.Fatalf("Wire: %v", err)
	}

	got := map[string]string{}
	for _, kv := range env {
		key, value, _ := strings.Cut(kv, "=")
		got[key] = value
	}
	want := map[string]string{
		"ANTHROPIC_BASE_URL":             "https://api.runanywhere.ai",
		"ANTHROPIC_AUTH_TOKEN":           "sk-test",
		"ANTHROPIC_API_KEY":              "",
		"ANTHROPIC_MODEL":                "qwen3-coder",
		"ANTHROPIC_DEFAULT_HAIKU_MODEL":  "qwen3-coder",
		"ANTHROPIC_DEFAULT_OPUS_MODEL":   "qwen3-coder",
		"ANTHROPIC_DEFAULT_SONNET_MODEL": "qwen3-coder",
		"CLAUDE_CODE_SUBAGENT_MODEL":     "qwen3-coder",
	}
	for key, value := range want {
		got, ok := got[key]
		if !ok {
			t.Errorf("env missing %s", key)
			continue
		}
		if got != value {
			t.Errorf("env[%s] = %q, want %q", key, got, value)
		}
	}

	if !slices.Equal(argv, []string{"--continue"}) {
		t.Errorf("argv = %v, want the caller's args passed through unchanged", argv)
	}
	if cleanup == nil {
		t.Fatal("cleanup is nil")
	}
	cleanup() // must not panic
}

func TestClaudeCodeWireLocalEndpointGetsPlaceholderToken(t *testing.T) {
	ep := runanywhere.Endpoint{BaseURL: "http://127.0.0.1:11535/v1", Local: true}
	env, _, cleanup, err := ClaudeCode{}.Wire(ep, "qwen3-0.6b", nil)
	if err != nil {
		t.Fatalf("Wire: %v", err)
	}
	defer cleanup()

	got := map[string]string{}
	for _, kv := range env {
		key, value, _ := strings.Cut(kv, "=")
		got[key] = value
	}
	if got["ANTHROPIC_AUTH_TOKEN"] == "" {
		t.Error("ANTHROPIC_AUTH_TOKEN is empty for a local endpoint, want a non-empty placeholder so Claude Code does not try to open a login flow")
	}
	if v, ok := got["ANTHROPIC_API_KEY"]; !ok || v != "" {
		t.Errorf("ANTHROPIC_API_KEY = %q (present=%v), want present and empty", v, ok)
	}
}

func TestClaudeCodeWireNeverTouchesTheRealConfig(t *testing.T) {
	dir := t.TempDir()
	claudeDir := filepath.Join(dir, ".claude")
	if err := os.MkdirAll(claudeDir, 0o755); err != nil {
		t.Fatalf("seed claude dir: %v", err)
	}
	settingsPath := filepath.Join(claudeDir, "settings.json")
	original := []byte(`{"model":"keep-me"}`)
	if err := os.WriteFile(settingsPath, original, 0o600); err != nil {
		t.Fatalf("seed settings.json: %v", err)
	}
	t.Setenv("HOME", dir)

	ep := runanywhere.Endpoint{BaseURL: "http://127.0.0.1:11535/v1", APIKey: "sk-test"}
	_, _, cleanup, err := ClaudeCode{}.Wire(ep, "qwen3-coder", nil)
	if err != nil {
		t.Fatalf("Wire: %v", err)
	}
	cleanup()

	after, err := os.ReadFile(settingsPath)
	if err != nil {
		t.Fatalf("read settings.json after Wire: %v", err)
	}
	if string(after) != string(original) {
		t.Fatalf("Wire touched the real config: got %s, want it unchanged (%s)", after, original)
	}
}

func TestClaudeCodeInstallHintNamesTheNativeInstaller(t *testing.T) {
	hint := ClaudeCode{}.InstallHint()
	if !strings.Contains(hint, "claude.ai/install") {
		t.Errorf("InstallHint() = %q, want the native claude.ai installer", hint)
	}
	if strings.Contains(hint, "npm") {
		t.Errorf("InstallHint() = %q, want the native installer, not npm", hint)
	}
}

func TestClaudeCodePreservesConfig(t *testing.T) {
	c := ClaudeCode{}
	if !c.PreservesConfig() {
		t.Error("PreservesConfig() = false, want true: Wire never opens ~/.claude")
	}
}

// wireEnv runs Wire and returns its env as a lookup map, so a test can assert
// on individual keys without caring about ordering.
func wireEnv(t *testing.T, model string) map[string]string {
	t.Helper()
	ep := runanywhere.Endpoint{BaseURL: "https://api.runanywhere.ai/v1", APIKey: "sk-test"}
	env, _, cleanup, err := ClaudeCode{}.Wire(ep, model, nil)
	if err != nil {
		t.Fatalf("Wire: %v", err)
	}
	t.Cleanup(cleanup)

	got := map[string]string{}
	for _, kv := range env {
		key, value, _ := strings.Cut(kv, "=")
		got[key] = value
	}
	return got
}

// TestClaudeCodeWireMatchesOllamaModelEnvVarNames pins the four model-tier
// env var names to ollama/cmd/launch/claude.go's modelEnvVars, character for
// character, so Claude Code resolves opus/sonnet/subagent calls through our
// model the same way ollama's launcher does.
func TestClaudeCodeWireMatchesOllamaModelEnvVarNames(t *testing.T) {
	t.Setenv("WALLY_PROFILE_DIR", t.TempDir())

	got := wireEnv(t, "qwen3-coder")
	for _, key := range []string{
		"ANTHROPIC_DEFAULT_OPUS_MODEL",
		"ANTHROPIC_DEFAULT_SONNET_MODEL",
		"ANTHROPIC_DEFAULT_HAIKU_MODEL",
		"CLAUDE_CODE_SUBAGENT_MODEL",
	} {
		if v, ok := got[key]; !ok || v != "qwen3-coder" {
			t.Errorf("env[%s] = %q (present=%v), want %q", key, v, ok, "qwen3-coder")
		}
	}
}

// TestClaudeCodeWireSetsAutoCompactWindowForCachedModel covers a model
// present in the catalog cache with a known context window: Wire must set
// CLAUDE_CODE_AUTO_COMPACT_WINDOW to that number.
func TestClaudeCodeWireSetsAutoCompactWindowForCachedModel(t *testing.T) {
	t.Setenv("WALLY_PROFILE_DIR", t.TempDir())
	if err := catalog.Save([]catalog.Model{
		{ID: "qwen3-coder", ContextWindow: 262_144, MaxOutputTokens: 32_768},
	}); err != nil {
		t.Fatalf("seed catalog cache: %v", err)
	}

	got := wireEnv(t, "qwen3-coder")
	if v, ok := got["CLAUDE_CODE_AUTO_COMPACT_WINDOW"]; !ok || v != "262144" {
		t.Errorf("env[CLAUDE_CODE_AUTO_COMPACT_WINDOW] = %q (present=%v), want %q", v, ok, "262144")
	}
}

// TestClaudeCodeWireOmitsAutoCompactWindowForUncachedModel covers a model
// with no cache entry at all: the var must be entirely absent, never a
// guessed number.
func TestClaudeCodeWireOmitsAutoCompactWindowForUncachedModel(t *testing.T) {
	t.Setenv("WALLY_PROFILE_DIR", t.TempDir())

	got := wireEnv(t, "some-model-not-in-the-cache")
	if v, ok := got["CLAUDE_CODE_AUTO_COMPACT_WINDOW"]; ok {
		t.Errorf("env[CLAUDE_CODE_AUTO_COMPACT_WINDOW] = %q, want the var omitted for an uncached model", v)
	}
}

// TestClaudeCodeWireOmitsAutoCompactWindowForZeroWindow covers a model that
// is cached but whose context window is unknown (zero, e.g. Catalog
// succeeded but Models did not): still no var, never a fake 0 or a guess.
func TestClaudeCodeWireOmitsAutoCompactWindowForZeroWindow(t *testing.T) {
	t.Setenv("WALLY_PROFILE_DIR", t.TempDir())
	if err := catalog.Save([]catalog.Model{
		{ID: "qwen3-coder", InputPerMTok: 300, OutputPerMTok: 1200},
	}); err != nil {
		t.Fatalf("seed catalog cache: %v", err)
	}

	got := wireEnv(t, "qwen3-coder")
	if v, ok := got["CLAUDE_CODE_AUTO_COMPACT_WINDOW"]; ok {
		t.Errorf("env[CLAUDE_CODE_AUTO_COMPACT_WINDOW] = %q, want the var omitted when the cached window is 0", v)
	}
}

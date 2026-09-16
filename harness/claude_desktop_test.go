package harness

import (
	"encoding/json"
	"os"
	"path/filepath"
	"runtime"
	"strings"
	"testing"

	"github.com/RunanywhereAI/wally/runanywhere"
)

func TestApplyThenRestoreClaudeDesktopGatewayRoundTrips(t *testing.T) {
	// The Claude Desktop harness targets the macOS "Library/Application
	// Support" layout (it rejects non-darwin at Wire); this round-trip exercises
	// those paths, which do not resolve on Windows.
	if runtime.GOOS == "windows" {
		t.Skip("Claude Desktop gateway apply/restore is darwin-only")
	}
	home := t.TempDir()
	t.Setenv("HOME", home)

	// A person who already uses a third-party gateway of their own, plus a
	// custom key elsewhere in the same files, both of which must survive
	// apply and restore untouched.
	thirdPartyConfig := filepath.Join(claudeDesktopSupportRoot(home, true), "claude_desktop_config.json")
	writeClaudeDesktopJSONForTest(t, thirdPartyConfig, map[string]any{
		"deploymentMode": "1p",
		"theirOwnKey":    "keep-me",
	})
	profilePath := claudeDesktopProfilePath(home)
	metaPath := claudeDesktopMetaPath(home)
	writeClaudeDesktopJSONForTest(t, metaPath, map[string]any{
		"entries": []any{map[string]any{"id": "someone-elses-gateway", "name": "Someone Else"}},
	})

	if err := applyClaudeDesktopGateway("sk-test", "http://127.0.0.1:11535", "RunAnywhere: qwen3-coder"); err != nil {
		t.Fatalf("applyClaudeDesktopGateway: %v", err)
	}

	applied := readClaudeDesktopJSONForTest(t, profilePath)
	if applied["inferenceGatewayBaseUrl"] != "http://127.0.0.1:11535" {
		t.Errorf("inferenceGatewayBaseUrl = %v, want the daemon root", applied["inferenceGatewayBaseUrl"])
	}
	if applied["inferenceGatewayApiKey"] != "sk-test" {
		t.Errorf("inferenceGatewayApiKey = %v, want sk-test", applied["inferenceGatewayApiKey"])
	}
	appliedConfig := readClaudeDesktopJSONForTest(t, thirdPartyConfig)
	if appliedConfig["deploymentMode"] != "3p" {
		t.Errorf("deploymentMode = %v, want 3p while applied", appliedConfig["deploymentMode"])
	}
	if appliedConfig["theirOwnKey"] != "keep-me" {
		t.Errorf("theirOwnKey = %v, want the person's own key preserved", appliedConfig["theirOwnKey"])
	}
	appliedMeta := readClaudeDesktopJSONForTest(t, metaPath)
	entries, _ := appliedMeta["entries"].([]any)
	foundTheirs, foundOurs := false, false
	for _, e := range entries {
		entry, _ := e.(map[string]any)
		switch entry["id"] {
		case "someone-elses-gateway":
			foundTheirs = true
		case claudeDesktopProfileID:
			foundOurs = true
		}
	}
	if !foundTheirs {
		t.Error("apply dropped the person's own gateway entry")
	}
	if !foundOurs {
		t.Error("apply did not add wally's own entry")
	}

	if err := restoreClaudeDesktopGateway(); err != nil {
		t.Fatalf("restoreClaudeDesktopGateway: %v", err)
	}

	restoredConfig := readClaudeDesktopJSONForTest(t, thirdPartyConfig)
	if restoredConfig["deploymentMode"] != "1p" {
		t.Errorf("deploymentMode = %v, want 1p after restore", restoredConfig["deploymentMode"])
	}
	if restoredConfig["theirOwnKey"] != "keep-me" {
		t.Errorf("theirOwnKey = %v, want the person's own key still preserved after restore", restoredConfig["theirOwnKey"])
	}
	restoredProfile := readClaudeDesktopJSONForTest(t, profilePath)
	for _, key := range claudeDesktopProfileKeys {
		if _, ok := restoredProfile[key]; ok {
			t.Errorf("restore left %s in the profile, want it stripped", key)
		}
	}
	restoredMeta := readClaudeDesktopJSONForTest(t, metaPath)
	if _, ok := restoredMeta["appliedId"]; ok {
		t.Error("restore left appliedId set")
	}
	restoredEntries, _ := restoredMeta["entries"].([]any)
	foundTheirsAfter := false
	for _, e := range restoredEntries {
		entry, _ := e.(map[string]any)
		switch entry["id"] {
		case claudeDesktopProfileID:
			t.Error("restore did not remove wally's own meta entry")
		case "someone-elses-gateway":
			foundTheirsAfter = true
		}
	}
	if !foundTheirsAfter {
		t.Error("restore dropped the person's own gateway entry")
	}
}

func TestRestoreClaudeDesktopGatewaySafeWhenNothingApplied(t *testing.T) {
	t.Setenv("HOME", t.TempDir())
	if err := restoreClaudeDesktopGateway(); err != nil {
		t.Fatalf("restoreClaudeDesktopGateway on a clean home: %v", err)
	}
}

func TestClaudeDesktopWireRejectsNonDarwin(t *testing.T) {
	if runtime.GOOS == "darwin" {
		t.Skip("this platform check only fires off darwin")
	}
	_, _, _, err := ClaudeDesktop{}.Wire(runanywhere.Endpoint{BaseURL: "http://127.0.0.1:11535/v1"}, "qwen3-coder", nil)
	if err == nil {
		t.Fatal("Wire did not reject a non-macOS platform")
	}
}

func TestClaudeDesktopWireAppliesAndCleanupRestores(t *testing.T) {
	if runtime.GOOS != "darwin" {
		t.Skip("claude-desktop only wires on macOS")
	}
	t.Setenv("HOME", t.TempDir())

	ep := runanywhere.Endpoint{BaseURL: "http://127.0.0.1:11535/v1", APIKey: "sk-test"}
	_, argv, cleanup, err := ClaudeDesktop{}.Wire(ep, "qwen3-coder", nil)
	if err != nil {
		t.Fatalf("Wire: %v", err)
	}
	if len(argv) != 4 || argv[0] != "-n" || argv[1] != "-W" || argv[2] != "-a" || argv[3] != "Claude" {
		t.Errorf("argv = %v, want [-n -W -a Claude]", argv)
	}

	home, _ := claudeDesktopHome()
	profile := readClaudeDesktopJSONForTest(t, claudeDesktopProfilePath(home))
	if profile["inferenceGatewayApiKey"] != "sk-test" {
		t.Errorf("inferenceGatewayApiKey = %v, want sk-test", profile["inferenceGatewayApiKey"])
	}

	cleanup()

	restored := readClaudeDesktopJSONForTest(t, claudeDesktopProfilePath(home))
	if _, ok := restored["inferenceGatewayApiKey"]; ok {
		t.Error("cleanup did not restore the profile")
	}
}

func TestClaudeDesktopInstallHintNamesDownloadPage(t *testing.T) {
	hint := ClaudeDesktop{}.InstallHint()
	if !strings.Contains(hint, "https://claude.ai/download") {
		t.Errorf("InstallHint() = %q, want it to name https://claude.ai/download", hint)
	}
}

func writeClaudeDesktopJSONForTest(t *testing.T, path string, value map[string]any) {
	t.Helper()
	if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
		t.Fatalf("seed %s: %v", path, err)
	}
	data, err := json.Marshal(value)
	if err != nil {
		t.Fatalf("marshal seed for %s: %v", path, err)
	}
	if err := os.WriteFile(path, data, 0o600); err != nil {
		t.Fatalf("seed %s: %v", path, err)
	}
}

func readClaudeDesktopJSONForTest(t *testing.T, path string) map[string]any {
	t.Helper()
	data, err := os.ReadFile(path)
	if err != nil {
		t.Fatalf("read %s: %v", path, err)
	}
	var parsed map[string]any
	if err := json.Unmarshal(data, &parsed); err != nil {
		t.Fatalf("parse %s: %v", path, err)
	}
	return parsed
}

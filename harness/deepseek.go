package harness

import (
	"encoding/json"
	"fmt"
	"os"
	"strings"

	"github.com/RunanywhereAI/wally/runanywhere"
)

const (
	deepSeekProviderID   = "runanywhere"
	deepSeekKeyVariable  = "RUNANYWHERE_API_KEY"
	deepSeekSettingsGlob = "wally-deepseek-settings-*.json"
	deepSeekPatchGlob    = "wally-deepseek-patch-*.yml"
)

// DeepSeek wires the endpoint into DeepSeek Harness (dsh) through a --patch
// overlay on argv, never a byte written into $DSH_HOME. dsh composes its
// plugin tree from layers and --patch is the last one, so the overlay
// outranks everything without touching the person's own settings, profiles,
// sessions or credentials. Two files carry the whole integration: a settings
// document holding our provider, and a patch pointing dsh's settings row at
// it and selecting that provider for a fresh agent's default model. The key
// never enters either file: apiKeyEnv names an environment variable and dsh
// resolves it itself per request. The variable is always set, even to a
// placeholder for a local daemon endpoint with no real key: dsh's llm-pi-ai
// provider refuses a request with "No API key for provider" when apiKeyEnv
// resolves to nothing, the same way OpenCode and Claude Code refuse to start
// keyless (verified live: `wally deepseek` against the local daemon failed
// with that exact error before this was unconditional).
type DeepSeek struct{}

func (d DeepSeek) Wire(ep runanywhere.Endpoint, model string, args []string) (env, argv []string, cleanup func(), err error) {
	if containsString(args, "--patch") || hasPrefixIn(args, "--patch=") {
		return nil, nil, nil, fmt.Errorf("conflicting argument: wally already manages --patch for dsh")
	}

	key := ep.APIKey
	if key == "" {
		key = "local"
	}
	settings := buildDeepSeekSettings(model, ep.BaseURL, deepSeekKeyVariable)
	settingsPath, err := writeTempFile(deepSeekSettingsGlob, settings)
	if err != nil {
		return nil, nil, nil, err
	}
	patch := buildDeepSeekPatch(settingsPath, model)
	patchPath, err := writeTempFile(deepSeekPatchGlob, patch)
	if err != nil {
		os.Remove(settingsPath)
		return nil, nil, nil, err
	}

	env = []string{deepSeekKeyVariable + "=" + key}

	// --patch belongs to us, so it goes ahead of anything dsh itself parses.
	// A profile the person already named on their own args is theirs to
	// drive; otherwise a prompt selects the headless profile and the bare
	// case opens the web UI, matching dsh's own terminal-entry contract.
	var launch []string
	switch {
	case containsString(args, "--profile"):
		launch = []string{"--patch", patchPath}
	case deepSeekWantsHeadless(args):
		launch = []string{"--profile", "headless", "--patch", patchPath}
	default:
		launch = []string{"web", "--patch", patchPath}
	}
	argv = append(launch, args...)
	cleanup = func() {
		os.Remove(settingsPath)
		os.Remove(patchPath)
	}
	return env, argv, cleanup, nil
}

// InstallCommand matches ollama/cmd/launch/deepseek_harness.go's own
// verified command; dsh's own quick-start docs show only
// "npx @deepseek-ai/dsh", which this resolves to a durable PATH entry
// instead of a per-run fetch.
func (DeepSeek) InstallCommand() string {
	return "npm install -g @deepseek-ai/dsh@latest"
}

// InstallHint is the exact command InstallCommand runs, so what a person
// sees and what wally executes never drift apart.
func (d DeepSeek) InstallHint() string {
	return "Run: " + d.InstallCommand()
}

// UninstallCommand reverses InstallCommand: npm owns the install, so npm
// owns the removal.
func (DeepSeek) UninstallCommand() string {
	return "npm uninstall -g @deepseek-ai/dsh"
}

func (DeepSeek) PreservesConfig() bool { return true }

func (DeepSeek) NeedsModelLimits() bool { return true }

// Restore sweeps any wally-written settings/patch temp files a crashed
// launch never got to clean up; safe to call any time, including when none
// exist.
func (DeepSeek) Restore() error {
	if err := removeGlob(deepSeekSettingsGlob); err != nil {
		return err
	}
	return removeGlob(deepSeekPatchGlob)
}

func init() {
	d := DeepSeek{}
	Registry = append(Registry, Harness{
		Name:    "deepseek",
		Command: "dsh",
		Summary: "open DeepSeek Harness against a model",
		Wire:    d.Wire,
		Impl:    d,
	})
}

// deepSeekWantsHeadless reports whether the person gave dsh a job to do
// rather than flags for its web app: the FIRST token only, since a later
// bare word is a flag's own value (`--port 8080` is the web app being
// configured, not a prompt), matching how dsh reads its own command line.
func deepSeekWantsHeadless(args []string) bool {
	return len(args) > 0 && args[0] != "" && args[0][0] != '-'
}

func hasPrefixIn(list []string, prefix string) bool {
	for _, v := range list {
		if strings.HasPrefix(v, prefix) {
			return true
		}
	}
	return false
}

// buildDeepSeekSettings is the document dsh reads our provider out of. JSON
// on purpose: the settings file's extension picks the format, keeping a
// hand-written YAML document out of this. keyVariable is normally always
// deepSeekKeyVariable (Wire never omits it: dsh's llm-pi-ai provider fails
// every request with "No API key for provider" when apiKeyEnv is absent,
// even against a keyless local endpoint); empty stays supported here so the
// document's own contract is testable independent of what Wire happens to
// pass today.
func buildDeepSeekSettings(model, baseURL, keyVariable string) string {
	provider := map[string]any{
		"displayName": "RunAnywhere",
		"api":         "openai-completions",
		"baseURL":     baseURL,
		"models": []any{
			map[string]any{"id": model, "name": model},
		},
	}
	if keyVariable != "" {
		provider["apiKeyEnv"] = keyVariable
	}
	settings := map[string]any{
		"llm-pi-ai": map[string]any{
			"providers": map[string]any{
				deepSeekProviderID: provider,
			},
		},
	}
	data, _ := json.Marshal(settings)
	return string(data)
}

// buildDeepSeekPatch is the --patch overlay pointing dsh's settings row at
// settingsPath and selecting model on our provider for a fresh agent. YAML
// because a cordis patch is YAML; safe to build by hand because every value
// is either a fixed string or one of settingsPath/model, both single-quote
// escaped below.
func buildDeepSeekPatch(settingsPath, model string) string {
	var b strings.Builder
	b.WriteString("- id: settings\n")
	b.WriteString("  config:\n")
	b.WriteString("    path: " + yamlSingleQuoted(settingsPath) + "\n")
	b.WriteString("- id: agent-default-model\n")
	b.WriteString("  config:\n")
	b.WriteString("    provider: " + deepSeekProviderID + "\n")
	b.WriteString("    model: " + yamlSingleQuoted(model) + "\n")
	return b.String()
}

func yamlSingleQuoted(v string) string {
	return "'" + strings.ReplaceAll(v, "'", "''") + "'"
}

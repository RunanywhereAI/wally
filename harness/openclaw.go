package harness

import (
	"encoding/json"
	"errors"
	"os"
	"path/filepath"

	"github.com/RunanywhereAI/wally/runanywhere"
)

const openClawTempPattern = "wally-openclaw-*.json"

// OpenClaw wires the endpoint into OpenClaw through a temp config it points
// OPENCLAW_CONFIG_PATH at, merged from whatever the person already has so
// their own providers, agents and gateway token survive the run. Their real
// openclaw.json is only ever read, never written.
//
// tui --local is the default when the caller passes no arguments: bare
// "openclaw" opens a TUI that talks to a background gateway daemon, and a
// machine that never installed that service gets a window repeating "not
// connected to gateway". --local runs the agent runtime embedded in the
// same process instead, which needs nothing else running.
type OpenClaw struct{}

func (c OpenClaw) Wire(ep runanywhere.Endpoint, model string, args []string) (env, argv []string, cleanup func(), err error) {
	state := openClawStateDir()
	if state == "" {
		return nil, nil, nil, errors.New("could not work out where openclaw keeps its state")
	}
	config, err := mergeOpenClawConfig(readOpenClawConfig(), model, ep.BaseURL, ep.APIKey)
	if err != nil {
		return nil, nil, nil, err
	}
	path, err := writeTempFile(openClawTempPattern, config)
	if err != nil {
		return nil, nil, nil, err
	}
	env = []string{
		// Pinned before the config path: OpenClaw derives the state
		// directory from the config file's own folder when this is unset,
		// which would move the person's agents and sessions into the temp
		// directory for the run.
		"OPENCLAW_STATE_DIR=" + state,
		"OPENCLAW_CONFIG_PATH=" + path,
	}
	argv = effectiveArgs([]string{"tui", "--local"}, args)
	cleanup = func() { os.Remove(path) }
	return env, argv, cleanup, nil
}

// InstallCommand matches ollama/cmd/launch/openclaw.go's own verified
// command.
func (OpenClaw) InstallCommand() string {
	return "npm install -g openclaw@latest"
}

// InstallHint is the exact command InstallCommand runs, so what a person
// sees and what wally executes never drift apart.
func (c OpenClaw) InstallHint() string {
	return "Run: " + c.InstallCommand()
}

// UninstallCommand reverses InstallCommand: npm owns the install, so npm
// owns the removal.
func (OpenClaw) UninstallCommand() string {
	return "npm uninstall -g openclaw"
}

func (OpenClaw) PreservesConfig() bool { return true }

func (OpenClaw) NeedsModelLimits() bool { return true }

// Restore sweeps any wally-written OpenClaw temp config a crashed launch
// never got to clean up; safe to call any time, including when none exist.
func (OpenClaw) Restore() error {
	return removeGlob(openClawTempPattern)
}

func init() {
	c := OpenClaw{}
	Registry = append(Registry, Harness{
		Name:    "openclaw",
		Command: "openclaw",
		Summary: "open OpenClaw against a model",
		Wire:    c.Wire,
		Impl:    c,
	})
}

// openClawStateDir mirrors resolveConfigDir's precedence: OPENCLAW_STATE_DIR,
// then OPENCLAW_HOME, then the platform home directory. os.UserHomeDir is
// used for the last (rather than a bare HOME lookup) so this resolves
// correctly on Windows too, where HOME is not normally set.
func openClawStateDir() string {
	if v := os.Getenv("OPENCLAW_STATE_DIR"); v != "" {
		return v
	}
	if v := os.Getenv("OPENCLAW_HOME"); v != "" {
		return filepath.Join(v, ".openclaw")
	}
	if home, err := os.UserHomeDir(); err == nil && home != "" {
		return filepath.Join(home, ".openclaw")
	}
	return ""
}

// readOpenClawConfig returns the person's current config document, or nil
// when they have none. OPENCLAW_CONFIG_PATH overrides the default location
// the same way it does for OpenClaw itself.
func readOpenClawConfig() []byte {
	path := os.Getenv("OPENCLAW_CONFIG_PATH")
	if path == "" {
		state := openClawStateDir()
		if state == "" {
			return nil
		}
		path = filepath.Join(state, "openclaw.json")
	}
	data, err := os.ReadFile(path)
	if err != nil {
		return nil
	}
	return data
}

// mergeOpenClawConfig adds (or replaces) the runanywhere provider and
// selects it as the primary model, preserving every other key in existing
// byte for byte where untouched. A plain overwrite of "models" or "agents"
// would drop the person's other providers, their agents and their gateway
// token, so each level is read, updated and written back individually.
func mergeOpenClawConfig(existing []byte, model, baseURL, apiKey string) (string, error) {
	config := map[string]any{}
	if len(existing) > 0 {
		var parsed map[string]any
		if err := json.Unmarshal(existing, &parsed); err == nil {
			config = parsed
		}
	}

	key := apiKey
	if key == "" {
		key = "local"
	}
	entry := map[string]any{
		"id":    model,
		"name":  model,
		"input": []string{"text"},
		// Only capabilities checked against this gateway: it returns usage
		// on the final streaming chunk when stream_options.include_usage is
		// set, and it takes max_tokens rather than max_completion_tokens.
		"compat": map[string]any{
			"supportsUsageInStreaming": true,
			"maxTokensField":           "max_tokens",
		},
	}

	models := asMap(config["models"])
	models["mode"] = "merge"
	providers := asMap(models["providers"])
	providers["runanywhere"] = map[string]any{
		"baseUrl": baseURL,
		"apiKey":  key,
		"api":     "openai-completions",
		"models":  []any{entry},
	}
	models["providers"] = providers
	config["models"] = models

	agents := asMap(config["agents"])
	defaults := asMap(agents["defaults"])
	modelBlock := asMap(defaults["model"])
	modelBlock["primary"] = "runanywhere/" + model
	defaults["model"] = modelBlock
	agents["defaults"] = defaults
	config["agents"] = agents

	data, err := json.Marshal(config)
	if err != nil {
		return "", err
	}
	return string(data), nil
}

func asMap(v any) map[string]any {
	if m, ok := v.(map[string]any); ok {
		return m
	}
	return map[string]any{}
}

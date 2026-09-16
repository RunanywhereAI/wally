package harness

import (
	"encoding/json"
	"runtime"

	"github.com/RunanywhereAI/wally/runanywhere"
)

const openCodeConfigVar = "OPENCODE_CONFIG_CONTENT"

// OpenCode wires the endpoint into OpenCode purely through
// OPENCODE_CONFIG_CONTENT on the child process. No OpenCode or project
// config file is ever read or written, so there is nothing to preserve or
// restore beyond the child's own environment.
type OpenCode struct{}

// BuildConfig is the OPENCODE_CONFIG_CONTENT document for model at baseURL.
// Exposed so the contract is testable without spawning a child. apiKey empty
// (a local, header-ignoring endpoint) gets a placeholder: an OpenAI client
// always sends an Authorization header, and OpenCode declining to start
// without one would be worse than a value the local server ignores.
func (OpenCode) BuildConfig(model, baseURL, apiKey string) (string, error) {
	key := apiKey
	if key == "" {
		key = "local"
	}
	config := map[string]any{
		"$schema": "https://opencode.ai/config.json",
		"provider": map[string]any{
			"runanywhere": map[string]any{
				"npm":  "@ai-sdk/openai-compatible",
				"name": "RunAnywhere",
				"options": map[string]any{
					"baseURL": baseURL,
					"apiKey":  key,
				},
				"models": map[string]any{
					model: map[string]any{"name": model},
				},
			},
		},
		"model": "runanywhere/" + model,
	}
	data, err := json.Marshal(config)
	if err != nil {
		return "", err
	}
	return string(data), nil
}

func (o OpenCode) Wire(ep runanywhere.Endpoint, model string, args []string) (env, argv []string, cleanup func(), err error) {
	config, err := o.BuildConfig(model, ep.BaseURL, ep.APIKey)
	if err != nil {
		return nil, nil, nil, err
	}
	return []string{openCodeConfigVar + "=" + config}, args, func() {}, nil
}

// InstallCommand mirrors OpenCode's own two supported paths: the curl
// installer everywhere else, npm on Windows where there is no shell script
// to pipe into (verified against ollama/cmd/launch/opencode.go, which ships
// both).
func (OpenCode) InstallCommand() string {
	if runtime.GOOS == "windows" {
		return "npm install -g opencode-ai@latest"
	}
	return "curl -fsSL https://opencode.ai/install | bash"
}

// InstallHint is the exact command InstallCommand runs, so what a person
// sees and what wally executes never drift apart.
func (o OpenCode) InstallHint() string {
	return "Run: " + o.InstallCommand()
}

// UninstallCommand mirrors OpenCode's own uninstaller: its own `uninstall`
// subcommand on the curl-installed unix path, npm on Windows where
// InstallCommand used npm too. Verified against the vendor's own docs
// (opencode.ai/docs/cli/, sourced from anomalyco/opencode's cli.mdx,
// fetched 2026-09-16): the install script itself carries no uninstall logic,
// but the installed binary ships `opencode uninstall`, and --force is its
// own flag for skipping the confirmation prompt, which is what makes this
// safe to run unattended.
func (OpenCode) UninstallCommand() string {
	if runtime.GOOS == "windows" {
		return "npm uninstall -g opencode-ai"
	}
	return "opencode uninstall --force"
}

func (OpenCode) PreservesConfig() bool { return true }

func (OpenCode) NeedsModelLimits() bool { return true }

func init() {
	o := OpenCode{}
	Registry = append(Registry, Harness{
		Name:    "opencode",
		Command: "opencode",
		Summary: "open OpenCode against a model",
		Wire:    o.Wire,
		Impl:    o,
	})
}

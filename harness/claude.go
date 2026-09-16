package harness

import (
	"runtime"
	"strconv"
	"strings"

	"github.com/RunanywhereAI/wally/catalog"
	"github.com/RunanywhereAI/wally/runanywhere"
)

// ClaudeCode wires the endpoint into Claude Code, the Anthropic-protocol
// CLI, entirely through the environment. It never reads or writes the real
// ~/.claude directory, so there is nothing to preserve or restore beyond
// the child's own environment.
//
// The credential rides on ANTHROPIC_AUTH_TOKEN, sent as a bearer token, not
// ANTHROPIC_API_KEY: Claude Code's own docs say that when ANTHROPIC_API_KEY
// is set it "prompts you once to approve the key" instead of running
// keyless (code.claude.com/docs/en/setup, fetched 2026-09-16), and
// wally-legacy/src/commands/cmd_editors.cpp hit that prompt plus a
// claude.ai-connectors-disabled warning and fixed it the same way; ollama's
// own claude.go wires the tool the same way. ANTHROPIC_API_KEY is set to
// empty so a real Anthropic key already sitting in the caller's shell
// cannot outrank our token.
type ClaudeCode struct{}

func (ClaudeCode) Wire(ep runanywhere.Endpoint, model string, args []string) (env, argv []string, cleanup func(), err error) {
	token := ep.APIKey
	if token == "" {
		token = "local"
	}
	env = []string{
		"ANTHROPIC_BASE_URL=" + anthropicBaseURL(ep.BaseURL),
		"ANTHROPIC_AUTH_TOKEN=" + token,
		"ANTHROPIC_API_KEY=",
		"ANTHROPIC_MODEL=" + model,
		// Claude Code resolves its own background requests (titles,
		// summaries) through the haiku alias rather than the main model.
		// Left unset, those calls report Claude Code's own default model as
		// the one that answered even though the endpoint above served every
		// request (measured in wally-legacy: a GLM pair and a Qwen pair both
		// mislabelled as Claude until this variable was added).
		"ANTHROPIC_DEFAULT_HAIKU_MODEL=" + model,
	}
	env = append(env, modelEnvVars(model)...)
	return env, args, func() {}, nil
}

// modelEnvVars routes every Claude Code model tier through our model and,
// when the model's context window is known, sets
// CLAUDE_CODE_AUTO_COMPACT_WINDOW so Claude Code compacts against the real
// limit instead of assuming its own catalog's 200k default and warning
// "unrecognized_model" for a model it has never heard of. This mirrors
// ollama/cmd/launch/claude.go's modelEnvVars exactly (env var names and the
// conditional-injection shape); the only difference is the value source,
// since wally's cloud catalog is dynamic (catalog.Load, refreshed from
// console.Models) rather than ollama's static table.
func modelEnvVars(model string) []string {
	env := []string{
		"ANTHROPIC_DEFAULT_OPUS_MODEL=" + model,
		"ANTHROPIC_DEFAULT_SONNET_MODEL=" + model,
		"CLAUDE_CODE_SUBAGENT_MODEL=" + model,
	}

	if window := contextWindow(model); window > 0 {
		env = append(env, "CLAUDE_CODE_AUTO_COMPACT_WINDOW="+strconv.FormatInt(window, 10))
	}

	return env
}

// contextWindow reads the model's context window from the local catalog
// cache. It never makes a live call: a cache miss or an uncached model
// returns 0, which the caller treats as "unknown" and omits the env var
// rather than injecting a guessed number.
func contextWindow(model string) int64 {
	models, err := catalog.Load()
	if err != nil {
		return 0
	}
	for _, m := range models {
		if m.ID == model {
			return m.ContextWindow
		}
	}
	return 0
}

// anthropicBaseURL is ep.BaseURL with its trailing "/v1" removed. The
// Anthropic SDK Claude Code embeds appends "/v1/messages" itself, so
// ANTHROPIC_BASE_URL has to be the daemon root, not the OpenAI-shaped root
// runanywhere.Endpoint carries for the OpenAI-compatible harnesses; the
// daemon itself mounts the Anthropic route at POST /v1/messages
// (daemon/router.go), so a base URL still carrying /v1 would double it up.
func anthropicBaseURL(baseURL string) string {
	return strings.TrimSuffix(baseURL, "/v1")
}

// InstallCommand matches Claude Code's own documented native installer
// (code.claude.com/docs/en/setup, fetched 2026-09-16) and
// ollama/cmd/launch/claude.go's own claudeInstallerCommand, which install
// through claude.ai rather than npm.
func (ClaudeCode) InstallCommand() string {
	if runtime.GOOS == "windows" {
		return "irm https://claude.ai/install.ps1 | iex"
	}
	return "curl -fsSL https://claude.ai/install.sh | bash"
}

// InstallHint is the exact command InstallCommand runs, so what a person
// sees and what wally executes never drift apart.
func (c ClaudeCode) InstallHint() string {
	return "Run: " + c.InstallCommand()
}

// UninstallCommand removes exactly the native installer's own footprint,
// matching Anthropic's own documented removal (code.claude.com/docs/en/setup,
// "Uninstall Claude Code", fetched 2026-09-16): there is no `claude
// uninstall` subcommand (confirmed against a locally installed binary's
// --help, and the install script itself only downloads and delegates to
// `claude install`), so the docs' own two-line rm/Remove-Item recipe is the
// vendor's answer. Deliberately leaves ~/.claude and ~/.claude.json alone:
// the docs call that out as a separate, more destructive step, since other
// Claude surfaces (the VS Code/JetBrains extensions, the desktop app) also
// write there and would recreate it anyway. This removes the harness, not
// every trace of Claude on the machine.
func (ClaudeCode) UninstallCommand() string {
	if runtime.GOOS == "windows" {
		return `Remove-Item -Path "$env:USERPROFILE\.local\bin\claude.exe" -Force; Remove-Item -Path "$env:USERPROFILE\.local\share\claude" -Recurse -Force`
	}
	return "rm -f ~/.local/bin/claude && rm -rf ~/.local/share/claude"
}

func (ClaudeCode) PreservesConfig() bool { return true }

func (ClaudeCode) NeedsModelLimits() bool { return true }

func init() {
	c := ClaudeCode{}
	Registry = append(Registry, Harness{
		Name:    "claude-code",
		Command: "claude",
		Summary: "open Claude Code against a model",
		Wire:    c.Wire,
		Impl:    c,
	})
}

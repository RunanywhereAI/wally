package harness

import (
	"runtime"
	"strings"
	"unicode"

	"github.com/RunanywhereAI/wally/runanywhere"
)

// Hermes wires the endpoint into Hermes entirely through environment
// variables and argv, never through its real ~/.hermes/config.yaml.
//
// CUSTOM_BASE_URL, not OPENAI_BASE_URL: Hermes' runtime resolver
// (hermes_cli/runtime_provider_backends.py) documents that OPENAI_BASE_URL
// is deliberately not consulted. The environment alone is not enough either:
// model.provider in the person's own config.yaml outranks
// HERMES_INFERENCE_PROVIDER, so a model id Hermes cannot map to a known
// vendor would otherwise route to a guessed provider (or fail for want of
// that vendor's key) instead of ours. --provider custom --model on argv is
// what actually pins the route, and it is read only on the -z/--oneshot and
// --tui entry points, which is why --tui is the default when the caller
// passes no arguments of their own.
type Hermes struct{}

func (h Hermes) Wire(ep runanywhere.Endpoint, model string, args []string) (env, argv []string, cleanup func(), err error) {
	env = []string{
		"CUSTOM_BASE_URL=" + ep.BaseURL,
		"HERMES_INFERENCE_PROVIDER=custom",
		// Both names: the TUI entry point reads HERMES_MODEL and the oneshot
		// path reads HERMES_INFERENCE_MODEL, and which one runs depends on
		// args this func does not control.
		"HERMES_INFERENCE_MODEL=" + model,
		"HERMES_MODEL=" + model,
		// Not for routing — the resolver's own base_url precedence
		// (hermes_cli/runtime_provider_backends.py) is explicit >
		// CUSTOM_BASE_URL > config > OPENROUTER_BASE_URL > default, and
		// never reads this. It is here only so
		// _has_any_provider_configured (hermes_cli/main.py) sees a
		// provider and skips straight into the session instead of the
		// first-run setup wizard: that check tests a fixed set of env var
		// names none of which is CUSTOM_BASE_URL or HERMES_INFERENCE_*,
		// and its own comment ("OPENAI_BASE_URL alone counts — local
		// models... often need no API key") names exactly this case. Same
		// value as CUSTOM_BASE_URL always, so nothing that does read it
		// (the model-catalog probe, an auxiliary/background model slot)
		// resolves anywhere different.
		"OPENAI_BASE_URL=" + ep.BaseURL,
	}
	if ep.APIKey != "" {
		// A key only reaches a host whose own registrable label names it
		// (GHSA-76xc-57q6-vm5m). A loopback or unrecognized endpoint gets no
		// key variable at all, which is what Hermes expects for those:
		// verified directly against hermes_cli/runtime_provider_backends.py
		// _resolve_openrouter_runtime, the terminal rung a bare `--provider
		// custom` request with no other credential always reaches — it
		// substitutes its own "no-key-required" placeholder there ("Local
		// no-auth servers get a placeholder key — the OpenAI SDK requires a
		// non-empty string"), so nothing here needs to invent one first.
		if v := hermesKeyVariable(ep.BaseURL); v != "" {
			env = append(env, v+"="+ep.APIKey)
		}
	}
	argv = append([]string{"--provider", "custom", "--model", model}, effectiveArgs([]string{"--tui"}, args)...)
	return env, argv, func() {}, nil
}

// InstallCommand mirrors Hermes' own installer split by platform (verified
// against ollama/cmd/launch/hermes.go and the project's install docs): a
// curl-piped shell script everywhere but Windows, a PowerShell one there.
//
// --skip-setup / -SkipSetup, matching ollama/cmd/launch/hermes.go's own
// hermesInstallScript and hermesWindowsInstallCmd exactly: hermes-agent's
// installer (scripts/install.sh run_setup_wizard) otherwise runs `hermes
// setup` interactively at the end of a fresh install, which walks a person
// through picking and authenticating a provider — including, for several of
// them, an OAuth flow that opens a browser. wally already names its own
// provider and endpoint on argv/env (see Wire), so that wizard has nothing
// left to ask and only costs a prompt (or, unattended, a hang on
// /dev/tty). PowerShell needed a different call shape to carry the flag at
// all: `iex (irm ...)` evaluates the downloaded script inline with no way to
// pass it an argument, so this uses `&` on an explicit scriptblock instead,
// the same technique ollama's hermesWindowsInstallCmd uses.
func (Hermes) InstallCommand() string {
	if runtime.GOOS == "windows" {
		return "& ([scriptblock]::Create((irm https://hermes-agent.nousresearch.com/install.ps1))) -SkipSetup"
	}
	return "curl -fsSL https://hermes-agent.nousresearch.com/install.sh | bash -s -- --skip-setup"
}

// InstallHint is the exact command InstallCommand runs, so what a person
// sees and what wally executes never drift apart.
func (h Hermes) InstallHint() string {
	return "Run: " + h.InstallCommand()
}

// UninstallCommand runs Hermes' own uninstaller: verified against two
// independent pages on hermes-agent.nousresearch.com/docs (the "Updating &
// Uninstalling" guide and the CLI reference), which agree on the flag set.
// --full removes the interactive "keep ~/.hermes/?" branch entirely and
// --yes drops the confirmation prompt, so together they run unattended; the
// command is the same subcommand of the same binary regardless of which
// platform installer put it there. Assumed, not verified against a live
// install: the docs' gateway-service teardown is documented only under a
// separate manual-uninstall path, so whether `--full --yes` alone also stops
// a registered gateway service is unconfirmed. Low risk here since wally
// always installs with --skip-setup, the flag that would normally register
// that service in the first place.
func (Hermes) UninstallCommand() string {
	return "hermes uninstall --full --yes"
}

// PreservesConfig is trivially true: Wire never opens ~/.hermes/config.yaml.
func (Hermes) PreservesConfig() bool { return true }

func init() {
	h := Hermes{}
	Registry = append(Registry, Harness{
		Name:    "hermes",
		Command: "hermes",
		Summary: "open a Hermes coding session against a model",
		Wire:    h.Wire,
		Impl:    h,
	})
}

// hermesKeyVariable ports hermes_cli/runtime_provider._host_derived_api_key:
// strip the scheme, drop leading api./www. labels, and name the registrable
// label. Empty for loopback, a bare host, or an IP, which Hermes treats as
// keyless.
func hermesKeyVariable(baseURL string) string {
	host := baseURL
	if i := strings.Index(host, "://"); i != -1 {
		host = host[i+3:]
	}
	if i := strings.IndexAny(host, "/:"); i != -1 {
		host = host[:i]
	}
	if host == "" || host == "localhost" {
		return ""
	}

	labels := strings.Split(host, ".")
	last := labels[len(labels)-1]
	if last != "" && unicode.IsDigit(rune(last[len(last)-1])) {
		return ""
	}
	for len(labels) > 0 && (labels[0] == "api" || labels[0] == "www") {
		labels = labels[1:]
	}
	if len(labels) < 2 {
		return ""
	}

	var vendor strings.Builder
	for _, r := range labels[len(labels)-2] {
		switch {
		case unicode.IsLetter(r), unicode.IsDigit(r):
			vendor.WriteRune(unicode.ToUpper(r))
		default:
			vendor.WriteRune('_')
		}
	}
	name := vendor.String()
	if name == "" || !unicode.IsLetter(rune(name[0])) {
		return ""
	}
	switch name {
	case "OPENAI", "OPENROUTER", "OLLAMA":
		// Host-gated on their own vendors' domains; borrowing the name would
		// hand our token to a check that is not about us.
		return ""
	}
	return name + "_API_KEY"
}

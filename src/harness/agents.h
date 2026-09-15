#ifndef WALLY_HARNESS_AGENTS_H
#define WALLY_HARNESS_AGENTS_H

#include <cstdint>
#include <string>
#include <vector>

/// Coding agents that speak OpenAI directly.
///
/// These need no translator: `Resolve` already hands back an OpenAI-compatible
/// base URL, which is the shape they want. Only the way they are told about it
/// differs, and that is the whole content of this file. Tools that speak the
/// Anthropic Messages API live in `cmd_editors.cpp` behind a shim instead.
namespace wally::harness {

/// One OpenAI-shaped agent wally can point at a model.
///
/// The table below is the whole integration surface: a new agent is a row plus,
/// where its handoff is new, one more case in `LaunchAgent`.
struct Agent {
    /// How the endpoint reaches the tool.
    enum class Handoff {
        /// Hermes' bare-custom endpoint, entirely through the environment.
        ///
        /// `CUSTOM_BASE_URL`, not `OPENAI_BASE_URL`: the runtime resolver
        /// (`hermes_cli/runtime_provider_backends.py`) says outright that
        /// OPENAI_BASE_URL "is deliberately NOT consulted", and only the
        /// setup wizard reads it. The model goes through
        /// `HERMES_INFERENCE_MODEL` rather than `--model`, because the
        /// top-level flag is consumed only by `-z/--oneshot` and `--tui` and
        /// falls through as None on a plain interactive run.
        CustomEndpointEnvironment,
        /// A complete config written for the child's lifetime and named by
        /// `OPENCLAW_CONFIG_PATH`. OpenClaw has no base-URL variable: a custom
        /// endpoint is a `models.providers` entry and the model is selected as
        /// `<provider>/<id>` in the same document.
        ConfigFile,
        /// DeepSeek Harness, through a `--patch` overlay on the argv.
        ///
        /// dsh composes its plugin tree from layers and `--patch <file>` is the
        /// last one, so an overlay outranks everything without a byte being
        /// written into `$DSH_HOME`. Two rows carry the whole integration: the
        /// settings row is pointed at a JSON document holding our provider (the
        /// settings file's extension picks its format, so no YAML is written by
        /// hand), and the default-model row names that provider for a fresh
        /// agent. The key never enters either file — `apiKeyEnv` names an
        /// environment variable and dsh resolves it per request.
        PatchOverlay,
    };

    /// What the reader types after `wally`.
    const char* id;
    /// The executable on PATH, which is not always the name people know the
    /// tool by: DeepSeek Harness ships as `dsh`.
    const char* command;
    const char* summary;
    Handoff handoff;
    /// What to run when the person passed no arguments of their own, space
    /// separated, or empty to start the tool bare.
    ///
    /// `openclaw` alone opens a TUI that talks to a gateway daemon, and a
    /// machine that never installed that service gets a window repeating
    /// "not connected to gateway". `tui --local` runs the agent runtime
    /// embedded in the same process instead, which is the one that needs
    /// nothing else running.
    const char* default_args;
};

/// Every agent, in the order they appear in `wally --help`.
extern const Agent kAgents[];
extern const int kAgentCount;

/// The OpenClaw config naming `model` at `base_url`, merged onto whatever the
/// person already has in `openclaw.json`.
///
/// Merged rather than replaced because `OPENCLAW_CONFIG_PATH` swaps the whole
/// document: a bare provider block would drop their wizard state, their agents
/// and their gateway token, and OpenClaw would run onboarding on every launch.
/// `existing` is that document, or empty when they have none.
/// `context_window`, `max_output` and the prices come from the console catalog;
/// a 0 for any of them leaves that field out, and OpenClaw falls back to its own
/// default — 128k and no spend, which is what a missing `contextWindow` looked
/// like in the status bar.
std::string BuildOpenClawConfig(const std::string& existing, const std::string& model,
                                const std::string& base_url, const std::string& api_key,
                                std::int64_t context_window, std::int64_t max_output,
                                std::int64_t input_per_mtok, std::int64_t output_per_mtok);

/// The environment variable Hermes will accept a key for at `base_url`.
///
/// Hermes gates credentials on the host (GHSA-76xc-57q6-vm5m): a key only
/// reaches an endpoint whose registrable label names it, so the variable is
/// `<VENDOR>_API_KEY` where the vendor is that label. Empty for loopback and
/// bare hosts, which take no key at all — Hermes substitutes
/// `no-key-required` there.
std::string HermesKeyVariable(const std::string& base_url);

/// The settings document dsh reads our provider out of. JSON on purpose: the
/// settings file's extension picks the format, which keeps a hand-written YAML
/// document out of this.
std::string BuildDeepSeekSettings(const std::string& model, const std::string& base_url,
                                  const std::string& key_variable,
                                  std::int64_t context_window, std::int64_t max_output);

/// Whether the person's arguments carry a prompt, which is what picks dsh's
/// headless profile over its web ui. Exposed for the test.
bool DeepSeekWantsHeadless(const std::vector<std::string>& args);

/// The `--patch` overlay pointing dsh's settings row at `settings_path` and
/// selecting `model` on our provider for a fresh agent.
///
/// YAML because a cordis patch is YAML, and safe to build by hand because every
/// value in it is either a fixed string or a path wally just created.
std::string BuildDeepSeekPatch(const std::string& settings_path, const std::string& model);

/// The line to print before launching Hermes when `context_window` is known.
///
/// Hermes takes a context-window hint from exactly one place: `config.yaml`
/// under `HERMES_HOME` (`model.context_length`, or a `custom_providers` entry
/// at global or per-model scope — `hermes_cli/config_providers.py`,
/// `config_defaults.py`). There is no path-override env var or flag: `HERMES_CONFIG`
/// and `HERMES_CONFIG_PATH` are reserved names on Hermes' own env-var denylist
/// but nothing in the installed source (Hermes Agent 0.21.3) ever reads them, and
/// `_parser.py` has no context/token flag. The only other lever, `HERMES_HOME`
/// itself, does not swap in a second config file — it swaps in a second Hermes
/// entirely: `-z` and `--tui` alike load rules, memory, `AGENTS.md` and preloaded
/// skills from that same tree (`oneshot.py`'s own docstring says so), so a fresh
/// `HERMES_HOME` throws away the person's SOUL.md, sessions and skills for the
/// run rather than adding one field to what they already have. Writing to their
/// real `~/.hermes/config.yaml` is off the table outright. So the number is
/// surfaced, not injected: the person can add it to their own config.yaml if they
/// want Hermes to budget the session against the full window. Empty when
/// `context_window` is not known, so a caller prints nothing.
std::string HermesContextHint(std::int64_t context_window);

/// The argv Hermes gets for `Agent::Handoff::CustomEndpointEnvironment`:
/// `--provider custom --model <model>` ahead of `child_args` (the person's own
/// args, or `default_args` when they gave none).
///
/// Pinned in front, not appended after: Hermes' argparse keeps the last value
/// of a repeated flag, so a person who passes their own `--provider` or
/// `--model` later in `child_args` still wins, and one who passes neither
/// gets ours. Those two flags are the only thing that route Hermes off
/// `model.provider: auto` in the person's config.yaml — `HERMES_INFERENCE_PROVIDER`
/// loses to it — and they are read only on the `-z/--oneshot` and `--tui`
/// paths, which is why `default_args` for this row is `--tui`.
std::vector<std::string> HermesArgv(const std::string& model,
                                    const std::vector<std::string>& child_args);

/// Resolves `model`, hands the endpoint to `agent` the way its handoff wants,
/// and runs it with `args` forwarded. Returns the tool's exit code.
///
/// An empty `model` runs the tool exactly as the person has it configured, the
/// same contract as `wally opencode`. Nothing wally writes outlives the child:
/// the environment is restored and any config file is deleted on the way out.
int LaunchAgent(const Agent& agent, const std::string& model,
                const std::vector<std::string>& args);

}  // namespace wally::harness

#endif  // WALLY_HARNESS_AGENTS_H

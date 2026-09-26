#include <chrono>
#include <cstdint>
#include <cstdlib>
#include <filesystem>
#include <fstream>
#include <memory>
#include <optional>
#include <set>
#include <string>
#include <system_error>
#include <thread>
#include <vector>

#include <nlohmann/json.hpp>

#include "anthropic/messages.h"
#include "cli_formatter.h"
#include "commands/editor_env.h"
#include "commands/commands.h"
#include "config/cli_paths.h"
#include "io/output.h"
#include "desktop/claude_profile.h"
#include "harness/catalog_models.h"
#include "harness/declared_harness.h"
#include "harness/harness.h"

namespace wally::commands {
namespace {

/// CLI11 callbacks return void, so a non-zero status leaves as the runtime
/// error the app turns back into an exit code.
void fail(int status) {
    if (status != 0) {
        throw CLI::RuntimeError(status);
    }
}


/// One editor or agent wally can point at a model.
///
/// The list is the whole integration surface: a new target is a row here plus
/// whatever `apply` has to set. Everything before that — resolving the model,
/// serving it, translating the wire format — is shared, which is the point.
/// How a tool is told where the model lives.
enum class Wiring {
    /// Variables in the launched process. Works for anything that reads
    /// ANTHROPIC_BASE_URL itself, or spawns something that does.
    Environment,
    /// Claude Desktop's third-party gateway profile, because it ignores the
    /// environment for authentication and says so.
    ClaudeProfile,
};

struct Editor {
    /// What the reader types after `wally`.
    const char* id;
    /// An executable on PATH, or empty when this is a desktop app.
    const char* command;
    /// A macOS application bundle, or empty when `command` is on PATH.
    const char* bundle;
    const char* summary;
    Wiring wiring;
    /// What the translator declares upstream for this tool (`X-RA-Harness`),
    /// so the endpoint attributes the traffic to it rather than to nobody.
    harness::DeclaredHarness declared;
};

/// Only tools that speak the Anthropic Messages API belong here. Anything
/// OpenAI-shaped needs no translator and goes through `wally opencode`.
///
/// Claude Desktop earns its place because it forwards a fixed set of variables
/// to the Claude Code it runs inside itself, and ANTHROPIC_BASE_URL is one of
/// them. That is the same trick as `wally claude-code`, one process further out.
constexpr Editor kEditors[] = {
    {"claude-code", "claude", "", "Open Claude Code with a model", Wiring::Environment,
     harness::DeclaredHarness::kClaudeCode},
    {"claude-desktop", "", "Claude.app", "Open Claude Desktop with a model",
     Wiring::ClaudeProfile, harness::DeclaredHarness::kClaudeDesktop},
};

/// Where `editor`'s application bundle is, or empty when it is not installed.
std::string BundlePath(const Editor& editor) {
    if (editor.bundle[0] == '\0') {
        return {};
    }
    const char* home = std::getenv("HOME");
    std::vector<std::string> roots{"/Applications/"};
    if (home != nullptr) {
        roots.push_back(std::string(home) + "/Applications/");
    }
    for (const std::string& root : roots) {
        const std::string path = root + editor.bundle;
        std::ifstream probe(path + "/Contents/Info.plist");
        if (probe.good()) {
            return path;
        }
    }
    return {};
}

/// `open -n -W -a <bundle>`: a new instance, waited on until it quits.
///
/// Waiting is the point: the translator has to outlive the app exactly, and no
/// longer. `-n` is what makes the wait mean that. open(1) without it "waits
/// until the applications it opens **or that were already open** have exited",
/// so a copy the reader already had running would both miss the wiring and hold

/// Sets `name` for the child, remembering what was there so it can be undone.
class ScopedEnv {
  public:
    ScopedEnv(std::string name, const std::string& value) : name_(std::move(name)) {
        const char* previous = std::getenv(name_.c_str());
        had_previous_ = previous != nullptr;
        if (had_previous_) {
            previous_ = previous;
        }
        Set(value);
    }

    ~ScopedEnv() {
        if (had_previous_) {
            Set(previous_);
        } else {
#if defined(_WIN32)
            _putenv_s(name_.c_str(), "");
#else
            unsetenv(name_.c_str());
#endif
        }
    }

    ScopedEnv(const ScopedEnv&) = delete;
    ScopedEnv& operator=(const ScopedEnv&) = delete;

  private:
    void Set(const std::string& value) {
#if defined(_WIN32)
        _putenv_s(name_.c_str(), value.c_str());
#else
        setenv(name_.c_str(), value.c_str(), 1);
#endif
    }

    std::string name_;
    std::string previous_;
    bool had_previous_ = false;
};

/// Removes `name` for the child and restores it on scope exit — the mirror of
/// ScopedEnv. Used to keep a stray ANTHROPIC_API_KEY in the reader's shell from
/// reaching Claude Code: our bearer token already outranks it, but its mere
/// presence makes Claude Code prompt to approve the key and warn that claude.ai
/// connectors are off.
class ScopedUnsetEnv {
  public:
    explicit ScopedUnsetEnv(std::string name) : name_(std::move(name)) {
        const char* previous = std::getenv(name_.c_str());
        had_previous_ = previous != nullptr;
        if (!had_previous_) {
            return;
        }
        previous_ = previous;
#if defined(_WIN32)
        _putenv_s(name_.c_str(), "");
#else
        unsetenv(name_.c_str());
#endif
    }

    ~ScopedUnsetEnv() {
        if (!had_previous_) {
            return;
        }
#if defined(_WIN32)
        _putenv_s(name_.c_str(), previous_.c_str());
#else
        setenv(name_.c_str(), previous_.c_str(), 1);
#endif
    }

    ScopedUnsetEnv(const ScopedUnsetEnv&) = delete;
    ScopedUnsetEnv& operator=(const ScopedUnsetEnv&) = delete;

  private:
    std::string name_;
    std::string previous_;
    bool had_previous_ = false;
};

/// A wally-owned config directory for the Claude Code we launch, seeded from the
/// reader's real `~/.claude` so their settings, agents, rules, skills and memory
/// come along, but WITHOUT the login: a separate dir has no claude.ai session to
/// collide with, which is exactly what silences the "connectors are disabled"
/// warning. `.credentials.json` (the login file) and the runtime/cache trees are
/// never copied. Full context is seeded on first run; the small settings files
/// refresh every run so later edits to the real profile flow through.
///
/// On macOS the active login is a Keychain entry keyed to the config-dir path,
/// so even the account metadata in `~/.claude.json` is safe to bring — verified
/// that a seeded dir still prints no warning.
std::string PrepareClaudeConfigDir() {
    namespace fs = std::filesystem;
    const std::string ours_str = paths::state_dir() + "/claude";
    const fs::path ours(ours_str);
    std::error_code ec;

    const char* home = std::getenv("HOME");
#if defined(_WIN32)
    // PowerShell and cmd.exe leave HOME unset; Claude Code's home there is the profile.
    if (home == nullptr || *home == 0) {
        home = std::getenv("USERPROFILE");
    }
#endif
    // An empty value would resolve against the working directory, so treat it as unset.
    const bool has_home = home != nullptr && *home != 0;
    const fs::path og_dir = has_home ? fs::path(home) / ".claude" : fs::path();
    const fs::path og_json = has_home ? fs::path(home) / ".claude.json" : fs::path();

    const bool first_run = !fs::exists(ours, ec);
    fs::create_directories(ours, ec);

    // Ours to own, never seeded from the real profile: the login, plus the
    // runtime and cache trees. Everything else in ~/.claude is context to keep.
    static const std::set<std::string> kRuntime = {
        ".credentials.json",     "projects",       "sessions",
        "shell-snapshots",       "statsig",        "cache",
        "caches",                "telemetry",      "downloads",
        "uploads",               "paste-cache",    "file-history",
        "backups",               "history.jsonl",  ".last-cleanup",
        "chrome",                "ide",            "session-env",
        "mcp-needs-auth-cache.json", "stats-cache.json", ".last-update-result.json",
    };

    if (first_run && !og_dir.empty() && fs::exists(og_dir, ec)) {
        for (fs::directory_iterator it(og_dir, ec), end; it != end; it.increment(ec)) {
            if (ec) {
                break;
            }
            if (kRuntime.count(it->path().filename().string()) != 0) {
                continue;
            }
            std::error_code copy_ec;
            fs::copy(it->path(), ours / it->path().filename(),
                     fs::copy_options::recursive | fs::copy_options::overwrite_existing, copy_ec);
        }
    }

    // A cheap refresh every run so edits to the real settings and memory flow
    // through without re-copying the heavy trees.
    if (!og_dir.empty()) {
        for (const char* file : {"settings.json", "CLAUDE.md"}) {
            const fs::path src = og_dir / file;
            if (fs::exists(src, ec)) {
                fs::copy_file(src, ours / file, fs::copy_options::overwrite_existing, ec);
            }
        }
    }
    if (!og_json.empty() && fs::exists(og_json, ec)) {
        fs::copy_file(og_json, ours / ".claude.json", fs::copy_options::overwrite_existing, ec);
    }

    // Mark onboarding done so a first-ever Claude Code launch skips its setup
    // wizard — the gateway and auth are already wired. Patch the seeded file, or
    // write a minimal one when the reader has no ~/.claude.json of their own.
    {
        const fs::path claude_json = ours / ".claude.json";
        nlohmann::json doc = nlohmann::json::object();
        std::ifstream in(claude_json, std::ios::binary);
        if (in.good()) {
            nlohmann::json parsed = nlohmann::json::parse(in, nullptr, /*allow_exceptions=*/false);
            if (parsed.is_object()) {
                doc = std::move(parsed);
            }
        }
        doc["hasCompletedOnboarding"] = true;
        std::ofstream out(claude_json, std::ios::binary | std::ios::trunc);
        if (out.good()) {
            out << doc.dump(2) << '\n';
        }
    }

    return ours_str;
}

/// The context window `/v1/models` advertises for `model`, or 0 when it can't be
/// learned. A failed catalog fetch WARNS and returns 0 — it must never block a
/// launch. Fed to Claude Code as CLAUDE_CODE_MAX_CONTEXT_TOKENS, this is what
/// makes its auto-compaction fire at the model's real limit instead of a guessed
/// default (which overruns qwen/gemma's 256k and wastes glm's 1M).
std::int64_t CloudContextWindow(const std::string& model) {
    account::Credentials credentials;
    std::string error;
    if (!account::Load(&credentials, &error) || !credentials.signed_in()) {
        return 0;
    }
    const account::ConsoleClient console;
    std::vector<account::ModelInfo> models;
    if (console.FetchModels(credentials.console_url, credentials.access_token, &models, &error) !=
        account::IdentityResult::Ok) {
        out::status_line("could not read the model catalog (" + error +
                         "); launching without a context-window hint");
        return 0;
    }
    for (const account::ModelInfo& info : models) {
        if (info.id == model) {
            return info.context_window;
        }
    }
    return 0;
}

/// Starts the translator and holds it open, printing what to point at it.
///
/// Worth having beyond debugging: it is how anything that speaks the Anthropic
/// API but is not on the list above gets wired up, without wally needing to know
/// that tool exists.
int Serve(const Editor& editor, const std::string& model,
          const GlobalOptions& options) {
    harness::Endpoint endpoint;
    if (!harness::Resolve(model, &endpoint, options, editor.id)) {
        return 1;
    }
    anthropic::Shim shim;
    // `--serve` is named after a tool too (`wally claude-code --serve`), so the
    // endpoint it holds open declares that tool.
    if (!anthropic::Start(endpoint, model, editor.declared, &shim, options.verbose)) {
        harness::Release(endpoint);
        return 1;
    }
    out::result_line("ANTHROPIC_BASE_URL=" + shim.base_url);
    out::result_line("ANTHROPIC_AUTH_TOKEN=" + shim.auth_token);
    out::status_line("serving " + model + "; press Ctrl-C to stop");
    // No signal handling: Ctrl-C ends the process, and the OS reclaims the port
    // and the model. Anything subtler would be pretending this outlives it.
    for (;;) {
        std::this_thread::sleep_for(std::chrono::seconds(1));
    }
}

/// Puts the app back on Anthropic without starting anything.
///
/// The way out when a run was interrupted before it could undo itself.
int Restore(const Editor& editor) {
    std::string failure;
    if (!desktop::RestoreGateway(&failure)) {
        out::error_line(failure);
        return 1;
    }
    out::status_line(std::string(editor.id) + " is back on Anthropic; restart it to pick that up");
    return 0;
}

int Run(const Editor& editor, const std::string& model,
        const std::vector<std::string>& args, const GlobalOptions& options) {
    const bool is_bundle = editor.bundle[0] != '\0';
    std::string bundle;
    if (is_bundle) {
#if defined(__APPLE__)
        bundle = BundlePath(editor);
        if (bundle.empty()) {
            out::error_line(std::string(editor.bundle) + " is not installed");
            return 1;
        }
#else
        out::error_line(std::string(editor.id) + " is a macOS application");
        return 1;
#endif
    }

    if (model.empty()) {
        // No model named means no wiring to do, so the tool runs exactly as the
        // reader has it configured. Same contract as `wally opencode`.
        return is_bundle ? harness::Launch("open", {}, OpenArgs(bundle, {}, args, model))
                         : harness::Launch(editor.command, {}, args);
    }

    harness::Endpoint endpoint;
    if (!harness::Resolve(model, &endpoint, options, editor.id)) {
        return 1;
    }

    // Claude Desktop only lists gateway models it can map to an Anthropic
    // family, so the gateway answers under one of those ids while serving the
    // model the reader asked for. Only the desktop app needs this; the CLI
    // takes the real id happily.
    const std::string advertised =
        editor.wiring == Wiring::ClaudeProfile ? std::string("claude-sonnet-4-5") : model;

    // Claude Desktop's picker is Anthropic-family, so each catalog model is
    // offered under a family name and the shim routes a request naming that
    // family back to the real id. The launched model is first, so it stays the
    // default (Sonnet). The CLI path takes real ids directly and needs none of it.
    anthropic::ModelAliases desktop_aliases;
    if (editor.wiring == Wiring::ClaudeProfile) {
        const std::vector<harness::CatalogModel> catalog = harness::CatalogModels(endpoint, model);
        static const char* const kFamilies[] = {"claude-sonnet-4-5", "claude-opus-5",
                                                 "claude-haiku-4-5-20251001"};
        for (std::size_t i = 0; i < catalog.size() && i < 3; ++i) {
            desktop_aliases.emplace_back(kFamilies[i], catalog[i].id);
        }
    }

    anthropic::Shim shim;
    if (!anthropic::Start(endpoint, model, editor.declared, &shim, options.verbose, advertised,
                          desktop_aliases)) {
        harness::Release(endpoint);
        return 1;
    }
    out::status_line(std::string(editor.id) + " will talk to " + model + " through " +
                shim.base_url);
    if (advertised != model) {
        out::status_line("advertised to the app as " + advertised + "; the picker shows " + model);
    }

    int status = 0;
    if (editor.wiring == Wiring::ClaudeProfile) {
        // The profile, not the environment. Written before the app starts and
        // taken back when it exits, so a crash here is the one case that leaves
        // it applied — which is what `--restore` is for.
        std::string failure;
        if (!desktop::ApplyGateway(shim.base_url, shim.auth_token, desktop_aliases,
                                   "RunAnywhere · " + model, &failure)) {
            out::error_line(failure);
            anthropic::Stop(&shim);
            harness::Release(endpoint);
            return 1;
        }
        // A new instance reads the gateway profile at startup. The one already
        // running keeps the profile it started with, and keeps whatever the
        // reader has open in it, which is the trade we want.
        status = harness::Launch("open", {}, OpenArgs(bundle, shim, args, model));
        if (!desktop::RestoreGateway(&failure)) {
            out::error_line(failure);
        }
    } else if (is_bundle) {
        // An app that reads the variables itself, or spawns something that
        // does. A process only ever gets the environment it was started with,
        // so the wiring reaches a new instance and not the running one — which
        // is the whole reason `OpenArgs` passes `-n`.
        status = harness::Launch("open", {}, OpenArgs(bundle, shim, args, model));
    } else {
        // Scoped so the reader's own environment is back before we report
        // anything, and before a later call in the same process reads it.
        const ScopedEnv base("ANTHROPIC_BASE_URL", shim.base_url);
        // The bearer token only (auth precedence rank 2), never ANTHROPIC_API_KEY
        // (rank 3): the token already outranks any key, and setting a key is what
        // makes Claude Code prompt to approve it and warn that claude.ai
        // connectors are off. A stray key in the reader's shell is unset for the
        // same reason.
        const ScopedEnv token("ANTHROPIC_AUTH_TOKEN", shim.auth_token);
        const ScopedUnsetEnv no_key("ANTHROPIC_API_KEY");
        // Its own config dir, seeded from the reader's ~/.claude minus the login,
        // so there is no claude.ai session to collide with (no warning) but their
        // settings and memory still apply. See PrepareClaudeConfigDir.
        const ScopedEnv config_dir("CLAUDE_CONFIG_DIR", PrepareClaudeConfigDir());
        // Claude Code budgets against the local server's configured window,
        // or the hosted catalog when available.
        std::optional<ScopedEnv> context_window;
        {
            const std::int64_t context = endpoint.serving ? endpoint.context_window
                                                         : CloudContextWindow(model);
            if (context > 0) {
                context_window.emplace("CLAUDE_CODE_MAX_CONTEXT_TOKENS", std::to_string(context));
                out::status_line("context window: " + std::to_string(context) + " tokens");
            }
        }
        // TELL CLAUDE CODE WHICH MODEL IT IS TALKING TO, because otherwise it
        // labels our answers with its own default and reports that as fact.
        //
        // Wally 0.5.6 printed `using glm-5.3-flash`, returned the GLM sentinel,
        // and the wrapped tool's own JSON reported
        // `modelUsage.claude-sonnet-5` / `provider: firstParty`. Every hosted
        // model did the same, because nothing here ever set a model and the
        // wrapped tool has no other way to know.
        //
        // THIS CHANGES NO ROUTING. `anthropic::RequestToOpenAI` sets
        // `openai["model"] = runtime.model` on every request it forwards
        // (`src/anthropic/translate.cpp`), so the selected model is already what
        // serves and already what the ledger charges -- nine usage rows
        // reconciled correctly against the deployed catalog on 2026-09-12. Only
        // the label was wrong, and these two variables are what make it true.
        //
        // BOTH, and the second is the one that is easy to miss. Claude Code
        // makes background requests of its own -- titles, summaries -- and
        // resolves them through the `haiku` alias, not the main model. Those
        // reached us too and were served by `runtime.model` like everything
        // else: measured on the same day, an auxiliary GLM pair of 766/599
        // tokens costing 1,778 micros, and 816/821 for Qwen costing 2,790.
        // Without the second variable those calls stay labelled as a Claude
        // model that nothing here ever contacted.
        //
        // `ANTHROPIC_SMALL_FAST_MODEL` used to be the variable for that and is
        // documented as deprecated in favour of `ANTHROPIC_DEFAULT_HAIKU_MODEL`,
        // so it is deliberately not set. Names and precedence checked against
        // code.claude.com/docs/en/env-vars on 2026-09-13 rather than recalled:
        // `ANTHROPIC_MODEL` is read before the `model` settings key, and
        // `--model` or `/model` still override it, which is correct -- a reader
        // who asks for something else inside the session should get it.
        const ScopedEnv selected_model("ANTHROPIC_MODEL", model);
        // Claude Code's picker is Anthropic-family (Opus/Sonnet/Haiku), not a model
        // list, so each catalog model is bound to a family slot: they all then show
        // in the picker, labelled with their real ids. The launched model is first,
        // so it stays on Haiku, the background-task default.
        const std::vector<harness::CatalogModel> catalog = harness::CatalogModels(endpoint, model);
        const char* const kFamilySlots[] = {"ANTHROPIC_DEFAULT_HAIKU_MODEL",
                                             "ANTHROPIC_DEFAULT_SONNET_MODEL",
                                             "ANTHROPIC_DEFAULT_OPUS_MODEL"};
        std::optional<ScopedEnv> family_slots[3];
        for (std::size_t i = 0; i < catalog.size() && i < 3; ++i) {
            family_slots[i].emplace(kFamilySlots[i], catalog[i].id);
        }
        status = harness::Launch(editor.command, {}, args);
    }

    anthropic::Stop(&shim);
    harness::Release(endpoint);
    return status;
}

}  // namespace

/// the translator open behind it.
std::vector<std::string> OpenArgs(const std::string& bundle, const anthropic::Shim& shim,
                                  const std::vector<std::string>& passthrough,
                                  const std::string& model) {
    std::vector<std::string> args{"-n", "-W"};
    if (shim.running) {
        // `open --env` is what carries them across; launchd would otherwise
        // start the app with the reader's login environment instead of ours.
        args.push_back("--env");
        args.push_back("ANTHROPIC_BASE_URL=" + shim.base_url);
        // Bearer token only, no ANTHROPIC_API_KEY: the token outranks a key and
        // setting a key is what makes Claude Code warn about claude.ai
        // connectors being off. See the ScopedEnv path below.
        args.push_back("--env");
        args.push_back("ANTHROPIC_AUTH_TOKEN=" + shim.auth_token);
        // The same two the terminal path sets, for the same reason: without them
        // the wrapped app reports its own default model as the one that answered.
        // A bundle gets no inherited environment at all -- launchd starts it from
        // the reader's login session -- so anything the terminal path sets with
        // ScopedEnv has to be listed here too or the desktop launch silently
        // keeps the wrong labels. `model` is empty on the no-wiring path, and an
        // empty value would read as "unset this", so both are conditional.
        if (!model.empty()) {
            args.push_back("--env");
            args.push_back("ANTHROPIC_MODEL=" + model);
            args.push_back("--env");
            args.push_back("ANTHROPIC_DEFAULT_HAIKU_MODEL=" + model);
        }
    }
    args.push_back("-a");
    args.push_back(bundle);
    if (!passthrough.empty()) {
        args.push_back("--args");
        args.insert(args.end(), passthrough.begin(), passthrough.end());
    }
    return args;
}

void register_editors(CLI::App& app, GlobalOptions& options) {
    for (const Editor& editor : kEditors) {
        auto model = std::make_shared<std::string>();
        auto restore = std::make_shared<bool>(false);
        auto rest = std::make_shared<std::vector<std::string>>();
        auto serve = std::make_shared<bool>(false);
        auto* command = app.add_subcommand(editor.id, editor.summary);
        const std::string invocation = "wally " + std::string(editor.id);
        command->footer(examples_footer({
            {invocation + " -m qwen3-4b-instruct-2507",
             "The certified local coding model"},
            {invocation + " -m glm-5.3-flash", "A hosted model (needs `wally account login`)"},
        }));
        command->add_option("-m,--model", *model,
                            "A model on this machine, or a hosted one from your account");
        command->add_flag("--serve", *serve,
                          "Print the endpoint and keep it open instead of launching");
        if (editor.wiring == Wiring::ClaudeProfile) {
            command->add_flag("--restore", *restore,
                              "Put Claude Desktop back on Anthropic and exit");
        }
        // Tokens after the wally flags belong to the tool, its own flags
        // included. They reach here as positionals because `run()` inserts a
        // `--` ahead of them (see SplitPassthroughArgv); CLI11 would otherwise
        // read a leading `--flag` as an unknown wally option and reject it.
        command->add_option("args", *rest, "Passed through to the tool")->allow_extra_args();
        command->prefix_command();
        command->callback([&options, &editor, model, rest, serve, restore] {
            if (*restore) {
                fail(Restore(editor));
                return;
            }
            // A missing harness shows only that it is missing and how to get
            // it, before any model resolution or preamble. --serve holds the
            // endpoint open without launching the tool, so it needs none present.
            // Claude Desktop is an app bundle; the rest are CLIs on PATH.
            const bool needs_tool = !*serve;
            if (needs_tool && editor.bundle[0] != '\0') {
#if defined(__APPLE__)
                if (BundlePath(editor).empty()) {
                    out::error_line(std::string(editor.id) + " is not installed on this machine");
                    out::status_line(
                        "download it from https://claude.ai/download, then run this again");
                    fail(1);
                    return;
                }
#else
                out::error_line(std::string(editor.id) + " is a macOS application");
                fail(1);
                return;
#endif
            } else if (needs_tool && !harness::EnsureInstalled(editor.command)) {
                fail(127);
                return;
            }
            const std::string effective = ResolveDefaultModel(*model, options.no_color);
            fail(*serve ? Serve(editor, effective, options)
                        : Run(editor, effective, *rest, options));
        });
    }
}

}  // namespace wally::commands

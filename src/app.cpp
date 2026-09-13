#include "app.h"

#include <exception>
#include <memory>
#include <set>
#include <string>
#include <utility>
#include <vector>

#include <CLI11.hpp>

#include "bootstrap.h"
#include "cli_formatter.h"
#include "commands/commands.h"
#include "io/output.h"

#include "rac/core/rac_logger.h"

#ifndef WALLY_VERSION
#define WALLY_VERSION "0.0.0-dev"
#endif

namespace wally {

void configure_app(CLI::App& app, GlobalOptions& options) {
    app.set_version_flag("--version,-V", std::string("wally ") + WALLY_VERSION);
    app.require_subcommand(0, 1);
    app.fallthrough(true);

    app.add_flag("--json", options.json, "Machine-readable JSON output on stdout");
    app.add_flag("-v,--verbose", options.verbose, "Debug logging on stderr");
    app.add_flag("-q,--quiet", options.quiet, "Errors only on stderr");
    app.add_flag("--no-progress", options.no_progress, "Disable progress rendering");
    app.add_flag("--no-color", options.no_color, "Disable colored --help output");
    app.add_option("--home", options.home_override,
                   "RunAnywhere home directory (default: $RUNANYWHERE_HOME or "
                   "~/.local/share/runanywhere; models live under <home>/Models)");

    // Control-plane connection. validation happens in resolve_connection().
    // Developer/SDK-facing, not something a person reaches for day to day --
    // group("") drops them out of the default --help listing the same way
    // `telemetry` is hidden below, while leaving them fully parseable
    // (flags and RUNANYWHERE_* env fallbacks both still resolve).
    app.add_option("--environment", options.environment,
                   "SDK environment: development (default, keyless OSS → baked staging "
                   "backend) or production (API key + https URL).")
        ->envname("RUNANYWHERE_ENVIRONMENT")
        ->check(CLI::IsMember({"dev", "development", "prod", "production"}))
        ->group("");
    app.add_option("--base-url", options.base_url,
                   "Backend base URL. Optional in development (baked staging URL). "
                   "Required https for production.")
        ->envname("RUNANYWHERE_BASE_URL")
        ->group("");
    app.add_option("--api-key", options.api_key,
                   "Control-plane API key (required for production; omit for "
                   "keyless development)")
        ->envname("RUNANYWHERE_API_KEY")
        ->group("");

    // Namespaces first (the spec grammar), then the terminal aliases, then the
    // infrastructure commands — that is the order `--help` lists them in.
    commands::register_llm(app, options);
    commands::register_vlm(app, options);
    commands::register_tool(app, options);  // must follow register_llm (extends the `llm` group)
    commands::register_stt(app, options);
    commands::register_tts(app, options);
    commands::register_vad(app, options);
    commands::register_embed(app, options);
    commands::register_rerank(app, options);
    commands::register_image(app, options);
    commands::register_diarize(app, options);
    commands::register_segment(app, options);
    commands::register_voice(app, options);
    commands::register_rag(app, options);
    commands::register_models(app, options);
    commands::register_lora(app, options);

    commands::register_llm_aliases(app, options);
    commands::register_models_aliases(app, options);

    commands::register_serve(app, options);
    commands::register_bench(app, options);
    commands::register_backends(app, options);
    commands::register_info(app, options);
    commands::register_about(app, options);
    commands::register_version(app, options);
    commands::register_auth(app, options);
    commands::register_account(app, options);
    commands::register_usage(app, options);
    commands::register_editors(app, options);
    commands::register_harness(app, options);
    commands::register_default_models(app, options);
    commands::register_telemetry(app, options);

    // `--help` groups: CLI11 prints one heading per distinct group string, in
    // the order each group is first seen (Formatter::make_subcommands), so
    // this order is the print order. Centralized here rather than one
    // ->group() call per register_* file: 36 top-level commands with no
    // grouping at all used to land in a single default SUBCOMMANDS: bucket.
    // Grouped so the split a reader cares about is visible at a glance: what
    // runs on this machine, versus what talks to the hosted console. The
    // coding agents sit between the two because they do both — a local model or
    // a hosted one behind the same command — so they carry the "(local or
    // hosted)" tag rather than landing in either camp.
    constexpr const char* kGenerate = "Generate (on-device)";
    constexpr const char* kModels = "On-device models";
    constexpr const char* kAgents = "Coding agents (local or hosted)";
    constexpr const char* kCloud = "Cloud account";
    constexpr const char* kServe = "Serve & benchmark (on-device)";
    constexpr const char* kAbout = "About";
    const std::vector<std::pair<const char*, const char*>> help_groups = {
        {"llm", kGenerate},      {"vlm", kGenerate},      {"stt", kGenerate},
        {"tts", kGenerate},      {"vad", kGenerate},      {"embed", kGenerate},
        {"rerank", kGenerate},   {"image", kGenerate},    {"diarize", kGenerate},
        {"segment", kGenerate},  {"voice", kGenerate},    {"rag", kGenerate},
        {"run", kModels},        {"chat", kModels},       {"ls", kModels},
        {"show", kModels},       {"pull", kModels},       {"rm", kModels},
        {"models", kModels},     {"lora", kModels},
        {"opencode", kAgents},        {"claude-code", kAgents},
        {"claude-desktop", kAgents},
        {"clion", kAgents},           {"rustrover", kAgents},
        {"default-models", kAgents},
        {"auth", kCloud},        {"login", kCloud},       {"logout", kCloud},
        {"whoami", kCloud},      {"usage", kCloud},
        {"serve", kServe},       {"bench", kServe},       {"backends", kServe},
        {"info", kAbout},        {"about", kAbout},       {"version", kAbout},
    };
    // configure_app() runs ahead of run()'s own try/catch (and tests call it
    // directly with none at all), so a typo here must never propagate as an
    // uncaught exception -- that crashed the Windows CI binaries outright
    // (0xC0000409, no diagnostic) the one time a name here didn't match.
    // Report it and keep going with the default flat listing rather than
    // taking the whole CLI down over a --help cosmetic.
    for (const auto& [name, group] : help_groups) {
        try {
            app.get_subcommand(name)->group(group);
        } catch (const CLI::OptionNotFound&) {
            out::error_line(std::string("internal: --help grouping named an unknown "
                                        "subcommand '") +
                            name + "', skipping it");
        }
    }
    // Internal debug tool, not a command a user reaches for. An empty group
    // string drops a subcommand out of the default listing entirely
    // (Formatter::make_subcommands) while it stays fully callable —
    // `wally telemetry --help` still works.
    try {
        app.get_subcommand("telemetry")->group("");
    } catch (const CLI::OptionNotFound&) {
        // Nothing to hide if it isn't there.
    }
}

namespace {

/// The subcommands that hand the terminal to another tool and forward the rest
/// of the command line to it. Kept in step with register_editors and
/// register_harness; a name here that is not a real subcommand is harmless.
bool IsPassthroughCommand(const std::string& token) {
    static const std::set<std::string> kNames = {"claude-code", "claude-desktop", "clion",
                                                 "rustrover", "opencode"};
    return kNames.count(token) != 0;
}

/// wally's own flags on those subcommands. `-m`/`--model` take a following
/// value; the rest are booleans. The `=` forms carry their value inline.
bool ConsumesFollowingValue(const std::string& token) {
    return token == "-m" || token == "--model";
}
bool IsWallyFlag(const std::string& token) {
    return token == "-m" || token == "--model" || token.rfind("--model=", 0) == 0 ||
           token.rfind("-m=", 0) == 0 || token == "--serve" || token == "--restore" ||
           token == "--cloud";
}

}  // namespace

/// Inserts a `--` ahead of the first token that belongs to the wrapped tool, so
/// CLI11 stops reading the tool's own flags (`--dangerously-skip-permissions`,
/// `-p`) as unknown wally options and rejecting the whole line. Left untouched
/// when this is not a passthrough command, a `--` is already present, or nothing
/// but wally flags follow. `argv` includes the program name at index 0.
std::vector<std::string> SplitPassthroughArgv(const std::vector<std::string>& argv) {
    std::vector<std::string> out = argv;

    std::size_t sub = 0;
    for (std::size_t i = 1; i < out.size(); ++i) {
        if (IsPassthroughCommand(out[i])) {
            sub = i;
            break;
        }
    }
    if (sub == 0) {
        return out;
    }

    for (std::size_t i = sub + 1; i < out.size();) {
        if (out[i] == "--") {
            return out;  // the reader separated it already
        }
        if (!IsWallyFlag(out[i])) {
            out.insert(out.begin() + static_cast<std::ptrdiff_t>(i), "--");
            return out;
        }
        i += ConsumesFollowingValue(out[i]) ? 2 : 1;
    }
    return out;  // only wally flags, nothing to forward
}

int run(int argc, char** argv) {
    GlobalOptions options;

    // Decided ahead of CLI11's own parse: a subcommand inherits its parent's
    // formatter_ at construction time (App::App), which configure_app()
    // triggers below, so the color decision has to already be settled before
    // that call. Plain argv scan rather than parsing --no-color for real.
    bool no_color_requested = false;
    for (int i = 1; i < argc; ++i) {
        if (std::string(argv[i]) == "--no-color") {
            no_color_requested = true;
            break;
        }
    }

    CLI::App app{"RunAnywhere on-device AI CLI — llm, vlm, stt, tts, vad, embed, rerank, "
                 "image, rag, voice and the models that back them"};
    app.formatter(std::make_shared<CliFormatter>(color_output_enabled(no_color_requested)));
    configure_app(app, options);
    // Every subcommand here loads a model on this machine; a hosted console
    // model (glm-5.3-flash, ...) has no path through `run`/`llm generate` at
    // all, and that dead end used to be the only place someone learned the
    // cloud path exists.
    app.footer(
        "A model your account has on the hosted console (not this machine) runs through "
        "`wally claude-code -m <id>` or `wally opencode --cloud -m <id>`, not "
        "`run`/`llm generate`.");

    // A `--` before the wrapped tool's own arguments, added for the reader, so
    // `wally claude-code --dangerously-skip-permissions` forwards the flag
    // instead of failing on it. Kept alive for the whole parse below.
    std::vector<std::string> forwarded =
        SplitPassthroughArgv(std::vector<std::string>(argv, argv + argc));
    std::vector<char*> forwarded_argv;
    forwarded_argv.reserve(forwarded.size());
    for (std::string& token : forwarded) {
        forwarded_argv.push_back(token.data());
    }

    int exit_code = 0;
    try {
        app.parse(static_cast<int>(forwarded_argv.size()), forwarded_argv.data());
        if (app.get_subcommands().empty()) {
            // Bare `wally` prints help like `ollama` does.
            out::status_line(app.help());
        }
    } catch (const CLI::CallForHelp& e) {
        exit_code = app.exit(e);
    } catch (const CLI::CallForVersion& e) {
        exit_code = app.exit(e);
    } catch (const CLI::RuntimeError& e) {
        exit_code = (e.get_exit_code() != 0) ? e.get_exit_code() : 1;
    } catch (const CLI::ParseError& e) {
        app.exit(e);  // prints the usage message to stderr
        exit_code = 2;
    } catch (const std::exception& e) {
        out::error_line(e.what());
        exit_code = 1;
    }

    shutdown();
    return exit_code;
}

}  // namespace wally

// Called from Swift, before MLX.register() — measured, not inferred: the
// Swift host logs 3 more INFO lines during that call (Swift callbacks
// registered, MLX backend registered, RunAnywhereMLX backend registered
// successfully), all before wally_run_main ever runs, so muting only inside
// wally_run_main left 5 RAC lines on `wally --version` instead of the 2 the
// old comment here assumed. Splitting the mute into its own entry point,
// called from WallyMLX.swift ahead of MLX.register(), is what actually gets
// there.
extern "C" void wally_quiet_sdk_logging() {
    rac_logger_set_min_level(RAC_LOG_ERROR);
}

extern "C" int wally_run_main(int argc, char** argv) {
    // Covers `wally-cxx` and any other entry that skips the Swift host, where
    // wally_quiet_sdk_logging() above is never called. Idempotent with it.
    //
    // The 2 RAC lines still on stderr on every entry point are backend
    // registration WARNs emitted during static initialisation, which
    // completes before any entry point runs. No call from inside the process
    // can catch them; silencing them needs a pre-registration hook in the
    // kit, and the kit owns backend registration.
    //
    // `--verbose` raises the level again in bootstrap().
    rac_logger_set_min_level(RAC_LOG_ERROR);
    return wally::run(argc, argv);
}

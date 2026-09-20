#include "app.h"

#include <cstdio>
#include <cstdlib>
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
#include "desktop/claude_profile.h"
#include "io/output.h"

#include "rac/core/rac_logger.h"

#ifndef WALLY_VERSION
#define WALLY_VERSION "0.0.0-dev"
#endif

namespace wally {

void configure_app(CLI::App& app, GlobalOptions& options) {
    // Set before any subcommand registers: a subcommand copies its parent's
    // help flag at construction.
    app.set_help_flag("-h,--help", "Show help");
    app.set_version_flag("--version,-V", std::string("wally ") + WALLY_VERSION,
                         "Show the wally version");
    app.require_subcommand(0, 1);
    app.fallthrough(true);

    app.add_flag("--json", options.json, "Print results as JSON");
    app.add_flag("-v,--verbose", options.verbose, "Debug logging on stderr");
    app.add_flag("-q,--quiet", options.quiet, "Errors only");
    app.add_flag("--no-progress", options.no_progress, "Disable progress rendering")->group("");
    app.add_flag("--no-color", options.no_color, "Disable colored --help output")->group("");
    app.add_option("--home", options.home_override,
                   "RunAnywhere home directory (default: $RUNANYWHERE_HOME or "
                   "~/.local/share/runanywhere; models live under <home>/Models)")
        ->group("");

    // Top-level shortcuts that run the matching command and exit, like -V for
    // version. `-un` is not a valid single-dash short (that parses as `-u -n`),
    // so uninstall takes `-U`. Kept out of --help: the `update` and `uninstall`
    // commands are the documented spelling.
    app.add_flag_callback(
        "-u,--update", [] { std::exit(commands::run_update(false)); },
        "Update wally to the latest release")
        ->group("");
    app.add_flag_callback(
        "-U,--uninstall", [] { std::exit(commands::run_uninstall(false)); },
        "Uninstall wally, its models and config")
        ->group("");

    // Control-plane connection (environment / base URL / API key) is not exposed
    // as CLI flags: resolve_connection() reads RUNANYWHERE_ENVIRONMENT /
    // RUNANYWHERE_BASE_URL / RUNANYWHERE_API_KEY straight from the environment
    // (bootstrap.cpp), so a dev build still overrides via env with no
    // developer-only flags cluttering the surface.

    // Registration order is the --help print order: run first (the primary
    // verb), then llm and models, serve, the coding agents, the cloud account,
    // then diagnostics and maintenance. bench/backends/telemetry are registered
    // but hidden from the list further down.
    commands::register_llm_aliases(app, options);  // `run`
    commands::register_llm(app, options);          // `llm` (must precede register_tool)
    commands::register_tool(app, options);         // attaches to `llm`
    // TEMP(llm-only cut): every non-LLM modality is hidden from --help and from
    // execution for this release. Re-enable the full surface by uncommenting
    // this block as a whole -- nothing else has to change.
    // commands::register_vlm(app, options);
    // commands::register_stt(app, options);
    // commands::register_tts(app, options);
    // commands::register_vad(app, options);
    // commands::register_embed(app, options);
    // commands::register_rerank(app, options);
    // commands::register_image(app, options);
    // commands::register_diarize(app, options);
    // commands::register_segment(app, options);
    // commands::register_voice(app, options);
    // commands::register_rag(app, options);
    // commands::register_lora(app, options);
    commands::register_models(app, options);
    commands::register_serve(app, options);

    commands::register_editors(app, options);
    commands::register_harness(app, options);      // coding agents
    commands::register_default_models(app, options);

    commands::register_account(app, options);      // account login/logout/whoami/usage
    commands::register_usage(app, options);        // attaches `usage` under `account`
    // `auth` (device sign-in against the control plane) is a developer path that
    // duplicates `account login`; unregister it. Uncomment to restore.
    // commands::register_auth(app, options);

    commands::register_info(app, options);
    commands::register_about(app, options);
    commands::register_version(app, options);
    commands::register_update(app, options);
    commands::register_uninstall(app, options);
    commands::register_help(app, options);

    commands::register_bench(app, options);        // hidden below
    commands::register_backends(app, options);     // hidden below
    commands::register_telemetry(app, options);    // hidden below

    // Flat help: one "Commands" section, in registration order. The group is
    // set by walking the registered subcommands (not by name), so a rename can
    // never leave a stale string to crash the CLI (0xC0000409).
    for (CLI::App* sub : app.get_subcommands({})) {
        if (!sub->get_name().empty()) sub->group("Commands");
    }
    // Diagnostic and advanced commands: callable, but kept out of the list.
    for (const char* hidden : {"bench", "backends", "telemetry"}) {
        try {
            app.get_subcommand(hidden)->group("");
        } catch (const CLI::OptionNotFound&) {
            // Nothing to hide if it isn't there.
        }
    }
}

namespace {

/// Friendly reply to a missing or wrong (sub)command: walk to the deepest
/// command that actually parsed and print its help, instead of CLI11's terse
/// "A subcommand is required" / "The following argument was not expected" line.
/// Detected by exception TYPE at the call site (RequiredError / ExtrasError) --
/// ExtrasError::get_name() is the app name, not "ExtrasError", so a string match
/// would miss every wrong-argument case.
void PrintTypoHelp(const CLI::App& app) {
    const CLI::App* ctx = &app;
    // The names above `ctx`, so its usage line reads `wally models ...` the
    // way `wally models --help` prints it.
    std::string parents;
    for (;;) {
        const CLI::App* next = nullptr;
        for (const CLI::App* sub : ctx->get_subcommands({})) {
            if (sub->parsed()) {
                next = sub;
                break;
            }
        }
        if (next == nullptr) break;
        parents = parents.empty() ? ctx->get_name() : parents + " " + ctx->get_name();
        ctx = next;
    }
    out::error_line("You typed it wrong..! Use -h/--help on the sub command to know its options");
    std::fputs(ctx->help(parents).c_str(), stderr);
}

/// The subcommands that hand the terminal to another tool and forward the rest
/// of the command line to it. Kept in step with register_editors and
/// register_harness; a name here that is not a real subcommand is harmless.
bool IsPassthroughCommand(const std::string& token) {
    static const std::set<std::string> kNames = {"claude-code", "claude-desktop", "opencode",
                                                 "hermes",      "openclaw",       "deepseek"};
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

namespace {

/// Puts Claude Desktop back on Anthropic when a previous run could not.
///
/// The gateway profile is the one piece of wiring wally leaves on disk rather
/// than in a child process, so it is the one that survives wally being killed —
/// close the terminal mid-session and the app is left pointing at a port
/// nothing is serving, with no error that names us. Every later wally run heals
/// it here, whatever the person actually typed.
///
/// Only our own profile: `GatewayApplied()` is false for a gateway somebody
/// else configured, and for the run that is deliberately re-applying ours.
void RestoreStaleDesktopGateway(int argc, char** argv) {
    // Runs before CLI11 parses, so the invoked subcommand is read off argv by
    // hand: the first token that is neither a root option nor a root option's
    // value. Only `--home` takes a value; the rest are flags. Everything after
    // that first token belongs to the subcommand, so a `claude-desktop` among
    // another agent's forwarded arguments (`wally opencode ... claude-desktop`)
    // is not this command being invoked and must not skip the heal. `--quiet`,
    // a root flag, is read the same way as `--no-color` above; under it the
    // heal still happens and only the status line is held back.
    std::string subcommand;
    bool quiet = false;
    for (int i = 1; i < argc; ++i) {
        const std::string arg = argv[i];
        if (arg == "--home") {
            ++i;  // its value, not a subcommand
            continue;
        }
        if (arg == "-q" || arg == "--quiet") {
            quiet = true;
            continue;
        }
        if (!arg.empty() && arg.front() == '-') {
            continue;  // any other root flag
        }
        subcommand = arg;
        break;
    }
    if (subcommand == "claude-desktop") {
        return;
    }
    if (!desktop::GatewayApplied()) {
        return;
    }
    std::string failure;
    if (desktop::RestoreGateway(&failure) && !quiet) {
        out::status_line("claude desktop was still pointed at a wally endpoint; put it back");
    }
}

}  // namespace

int run(int argc, char** argv) {
    GlobalOptions options;
    RestoreStaleDesktopGateway(argc, argv);

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

    // Named "wally" outright rather than from argv[0], so the usage line reads
    // the same whether the binary was run through the install wrapper, by full
    // path, or as wally-cxx.
    CLI::App app{"Run language models locally or in the cloud, and wire coding tools to them",
                 "wally"};
    // Help is intentionally plain: keep the tree/layout, drop all color.
    static_cast<void>(no_color_requested);
    app.formatter(std::make_shared<CliFormatter>(false));
    configure_app(app, options);
    // `run` and `llm` only load models on this machine; a hosted model
    // (glm-5.3-flash, ...) is reached through a coding tool, and this block is
    // where a first-time reader learns that path exists.
    app.footer(examples_footer({
                   {"wally models pull qwen3-0.6b", "Download a model"},
                   {"wally run qwen3-0.6b \"write a haiku\"", "Run it on this machine"},
                   {"wally claude-code -m glm-5.3-flash", "Claude Code on a hosted model"},
                   {"wally opencode --cloud -m glm-5.3-flash", "opencode on a hosted model"},
                   {"wally models list --all", "Browse the catalog"},
               }) +
               "\n\nUse \"wally <command> --help\" for more information about a command.");

    // A `--` before the wrapped tool's own arguments, added for the reader, so
    // `wally claude-code --dangerously-skip-permissions` forwards the flag
    // instead of failing on it. Kept alive for the whole parse below.
    std::vector<std::string> raw(argv, argv + argc);
    // `wally help [command]` is a plain-word alias for `--help`, answered here
    // before the parse. Routing it through app.parse() instead would hand
    // `wally help opencode` to SplitPassthroughArgv, which inserts a `--` and
    // forwards the `--help` to the wrapped tool rather than describing the wally
    // command. Prints to stdout as `--help` does, and reaches shutdown() the
    // same way the CallForHelp path below does.
    if (raw.size() >= 2 && raw[1] == "help") {
        if (raw.size() >= 3 && !raw[2].empty() && raw[2][0] != '-') {
            try {
                // The parent's name, so the usage line reads `wally opencode`
                // exactly as `wally opencode --help` prints it.
                std::fputs(app.get_subcommand(raw[2])->help(app.get_name()).c_str(), stdout);
                shutdown();
                return 0;
            } catch (const CLI::Error&) {
                // No such command: fall back to the top-level help.
            }
        }
        std::fputs(app.help().c_str(), stdout);
        shutdown();
        return 0;
    }
    std::vector<std::string> forwarded = SplitPassthroughArgv(raw);
    std::vector<char*> forwarded_argv;
    forwarded_argv.reserve(forwarded.size());
    for (std::string& token : forwarded) {
        forwarded_argv.push_back(token.data());
    }

    int exit_code = 0;
    try {
        app.parse(static_cast<int>(forwarded_argv.size()), forwarded_argv.data());
        if (app.get_subcommands().empty()) {
            // Bare `wally` prints the top-level help.
            out::status_line(app.help());
        }
    } catch (const CLI::CallForHelp& e) {
        exit_code = app.exit(e);
    } catch (const CLI::CallForVersion& e) {
        exit_code = app.exit(e);
    } catch (const CLI::RuntimeError& e) {
        exit_code = (e.get_exit_code() != 0) ? e.get_exit_code() : 1;
    } catch (const CLI::RequiredError&) {
        PrintTypoHelp(app);  // missing required (sub)command
        exit_code = 2;
    } catch (const CLI::ExtrasError&) {
        PrintTypoHelp(app);  // an unexpected / wrong (sub)command or argument
        exit_code = 2;
    } catch (const CLI::ParseError& e) {
        app.exit(e);  // any other parse error: keep CLI11's own message
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

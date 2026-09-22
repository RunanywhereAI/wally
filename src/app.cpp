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
    // Visible in --help, unlike the control-plane flags below: all three are
    // things a person running models day to day reaches for -- piping output
    // into a script or log (--no-progress, --no-color) or keeping models on
    // another disk (--home) -- not a developer-only knob for a non-default
    // backend.
    app.add_flag("--no-progress", options.no_progress, "Disable progress rendering");
    app.add_flag("--no-color", options.no_color, "Disable colored --help output");
    app.add_option("--home", options.home_override,
                   "Where models live (default $RUNANYWHERE_HOME or ~/.local/share/runanywhere)");

    // `-u`/`-U` (aliases for `update`/`uninstall`) are deliberately NOT
    // registered as CLI11 flags here. `app.fallthrough(true)` above is
    // inherited by every subcommand at construction, so a flag by these names
    // anywhere in the tree is reachable from inside any subcommand's own
    // argument list -- `wally models list -u` would climb straight back up to
    // this app and fire the shortcut instead of erroring on an unknown
    // `models list` option, quietly running an update in place of the
    // requested command. `add_flag_callback` made this worse by calling
    // std::exit() mid-parse, which also skips run()'s shutdown(). The
    // shortcuts are still honoured, but only when `-u`/`-U`/`--update`/
    // `--uninstall` is the entire command line -- see the argv check in
    // run(), which reaches the normal shutdown() path. The documented
    // spelling is still `wally update` / `wally uninstall`.

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

    // Grouped help, by what the reader is trying to do. Sections print in the
    // order their first command was registered above; commands inside a
    // section keep registration order too. A namespace (models, account) is
    // listed as its children with full paths ("models pull"), which the
    // formatter does when a command has visible children.
    //
    // Everything unnamed here is hidden from the page but still parses:
    // `llm generate|stream|tool-call` (the explicit forms behind `run`),
    // `info` (a terser `about`), `version` (-V), `help` (-h), and the
    // diagnostics. Set by name with a guard, so a rename can never leave a
    // stale string to crash the CLI (0xC0000409).
    struct Section {
        const char* group;
        std::vector<const char*> names;
    };
    const Section sections[] = {
        {"Chat", {"run", "serve"}},
        {"Models", {"models"}},
        {"Coding tools", {"opencode", "claude-code", "claude-desktop", "hermes", "openclaw", "deepseek"}},
        {"Account", {"account"}},
        {"Wally", {"about", "update", "uninstall"}},
    };
    for (CLI::App* sub : app.get_subcommands({})) {
        if (!sub->get_name().empty()) sub->group("");
    }
    for (const Section& section : sections) {
        for (const char* name : section.names) {
            try {
                app.get_subcommand(name)->group(section.group);
            } catch (const CLI::OptionNotFound&) {
                // Not registered in this build; nothing to place.
            }
        }
    }
}

namespace {

/// Friendly reply to a parse error on a (sub)command: walk to the deepest
/// command that actually parsed and print CLI11's own message for what went
/// wrong, followed by that command's help, instead of stopping at the terse
/// top-level usage line. CLI11 already tells the two cases apart correctly in
/// e.what() -- "model is required" / "A subcommand is required" for something
/// left out, "The following argument was not expected: -x" for something
/// misspelled or extra -- so this only needs to walk down to where parsing
/// actually got to; it must not attach its own "you typed it wrong" framing,
/// which would misdescribe an omitted required argument as a typo. Handles
/// both RequiredError and ExtrasError (their common base), detected by
/// exception TYPE at the call site -- ExtrasError::get_name() is the app name,
/// not "ExtrasError", so a string match on e.what() would miss cases.
void PrintParseErrorHelp(const CLI::App& app, const CLI::ParseError& e) {
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
    out::error_line(e.what());
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
           token == "--cloud" || token == "-h" || token == "--help";
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

    // `-u`/`-U` top-level shortcuts, handled here instead of as CLI11 flags
    // (see the comment in configure_app() for why) so they can be guarded to
    // the one shape that can't be confused with a subcommand's own arguments:
    // the entire command line is the shortcut and nothing else. Goes through
    // the same shutdown() every other exit from run() does, unlike the old
    // std::exit()-in-a-callback version.
    if (argc == 2) {
        const std::string only_arg = argv[1];
        if (only_arg == "-u" || only_arg == "--update") {
            const int code = commands::run_update(false);
            shutdown();
            return code;
        }
        if (only_arg == "-U" || only_arg == "--uninstall") {
            const int code = commands::run_uninstall(false);
            shutdown();
            return code;
        }
    }

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
    CLI::App app{"Run models on this machine or on your RunAnywhere account", "wally"};
    // Two colors and nothing else, on a terminal only: bold section headings,
    // cyan for anything you can type. Piped or redirected output, --no-color
    // and NO_COLOR all get the identical plain text.
    app.formatter(std::make_shared<CliFormatter>(color_output_enabled(no_color_requested)));
    configure_app(app, options);
    // Show the download, chat, and coding-tool paths together. Each example
    // can be pasted, including its explanatory shell comment.
    app.footer(examples_footer({
        {"wally models pull qwen3-4b-instruct-2507 && "
         "wally run qwen3-4b-instruct-2507",
         "Download a local model and chat on this machine"},
        {"wally opencode -m qwen3-4b-instruct-2507",
         "Use the certified local model in a coding tool"},
        {"wally account login && wally opencode --cloud -m glm-5.3-flash",
         "Sign in and use a cloud model"},
    }, "Get started") + "\n\nRun \"wally <command> --help\" for details.");

    // A `--` before the wrapped tool's own arguments, added for the reader, so
    // `wally claude-code --dangerously-skip-permissions` forwards the flag
    // instead of failing on it. Kept alive for the whole parse below.
    std::vector<std::string> raw(argv, argv + argc);
    // `wally help [command]` is a plain-word alias for `--help`, answered here
    // before the parse so the named command is looked up without invoking
    // its callback. Prints to stdout as `--help` does, and reaches shutdown()
    // the same way the CallForHelp path below does.
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
    } catch (const CLI::RequiredError& e) {
        PrintParseErrorHelp(app, e);  // a required (sub)command or option was left out
        exit_code = 2;
    } catch (const CLI::ExtrasError& e) {
        PrintParseErrorHelp(app, e);  // an unexpected or misspelled (sub)command or argument
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

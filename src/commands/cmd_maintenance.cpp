/**
 * @file cmd_maintenance.cpp
 * @brief `wally help` (a plain-word mirror of `--help`) and `wally uninstall`
 *        (remove wally, its on-device models, and its config — never the coding
 *        tools a person installed themselves).
 */

#include "commands/commands.h"

#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <filesystem>
#include <memory>
#include <string>
#include <system_error>
#include <vector>

#if defined(__APPLE__)
#include <climits>
#include <cstdlib>
#include <mach-o/dyld.h>
#elif defined(__linux__)
#include <climits>
#include <unistd.h>
#endif

#include "account/credentials.h"
#include "config/cli_paths.h"
#include "io/output.h"
#include "util/term.h"

namespace wally::commands {
namespace {

namespace fs = std::filesystem;

// The path of the running wally binary, symlinks resolved. Empty when the
// platform gives no answer; uninstall then just skips deleting the binary.
std::string self_executable() {
#if defined(__APPLE__)
    char raw[PATH_MAX];
    uint32_t size = sizeof(raw);
    if (_NSGetExecutablePath(raw, &size) != 0) {
        return {};
    }
    char resolved[PATH_MAX];
    return realpath(raw, resolved) != nullptr ? std::string(resolved) : std::string(raw);
#elif defined(__linux__)
    char buffer[PATH_MAX];
    const ssize_t len = ::readlink("/proc/self/exe", buffer, sizeof(buffer) - 1);
    return len > 0 ? std::string(buffer, static_cast<size_t>(len)) : std::string();
#else
    return {};
#endif
}

// The documented default RunAnywhere base for this platform. Needs no rac_init,
// so uninstall can find the model store even though it never boots the kit.
std::string platform_base() {
    const char* home = std::getenv("HOME");
    if (home == nullptr) {
        return {};
    }
#if defined(__APPLE__)
    return std::string(home) + "/Library/Application Support/RunAnywhere";
#else
    if (const char* xdg = std::getenv("XDG_DATA_HOME"); xdg != nullptr && xdg[0] != '\0') {
        return std::string(xdg) + "/RunAnywhere";
    }
    return std::string(home) + "/.local/share/RunAnywhere";
#endif
}

// The on-device model store, matching harness/local_models.cpp: {base}/Models
// or {base}/RunAnywhere/Models. Every base it can derive is checked — the env
// override, the kit's answer, and the platform default — and the first store
// that actually exists wins, so a missing rac_init cannot hide it.
std::string models_directory() {
    std::vector<std::string> bases;
    if (const char* env = std::getenv("RUNANYWHERE_HOME"); env != nullptr && env[0] != '\0') {
        bases.emplace_back(env);
    }
    if (std::string home = paths::resolve_home(""); !home.empty()) {
        bases.push_back(std::move(home));
    }
    if (std::string fallback = platform_base(); !fallback.empty()) {
        bases.push_back(std::move(fallback));
    }

    std::error_code ec;
    for (const std::string& base : bases) {
        for (const char* sub : {"RunAnywhere/Models", "Models"}) {
            const fs::path candidate = fs::path(base) / sub;
            if (fs::is_directory(candidate, ec)) {
                return candidate.string();
            }
        }
    }
    return bases.empty() ? std::string() : (fs::path(bases.front()) / "Models").string();
}

std::string human_size(const fs::path& target) {
    std::error_code ec;
    if (!fs::exists(target, ec)) {
        return {};
    }
    std::uintmax_t bytes = 0;
    if (fs::is_directory(target, ec)) {
        for (const auto& entry : fs::recursive_directory_iterator(
                 target, fs::directory_options::skip_permission_denied, ec)) {
            if (entry.is_regular_file(ec)) {
                bytes += entry.file_size(ec);
            }
        }
    } else if (fs::is_regular_file(target, ec)) {
        bytes = fs::file_size(target, ec);
    }
    const char* units[] = {"B", "KiB", "MiB", "GiB"};
    double value = static_cast<double>(bytes);
    int unit = 0;
    while (value >= 1024.0 && unit < 3) {
        value /= 1024.0;
        ++unit;
    }
    char formatted[32];
    std::snprintf(formatted, sizeof(formatted), "%.1f %s", value, units[unit]);
    return formatted;
}

bool confirm(const std::string& question) {
    std::fprintf(stderr, "%s [y/N] ", question.c_str());
    std::fflush(stderr);
    char buffer[16] = {};
    if (std::fgets(buffer, sizeof(buffer), stdin) == nullptr) {
        return false;
    }
    return buffer[0] == 'y' || buffer[0] == 'Y';
}

struct Target {
    std::string label;
    fs::path path;
};

}  // namespace

// External (not in the anonymous namespace above) so the top-level
// `--uninstall` flag can call it too, via commands.h. Still uses the internal
// helpers above -- anonymous-namespace names stay visible through the TU.
int run_uninstall(bool yes) {
    std::vector<Target> targets;

    if (const std::string models = models_directory(); !models.empty()) {
        targets.push_back({"models", models});
    }
    if (const std::string config = account::ProfileDirectory(); !config.empty()) {
        targets.push_back({"config", config});
    }

    const std::string exe = self_executable();
    if (!exe.empty()) {
        targets.push_back({"binary", exe});
        // The Metal shader bundles the installer placed beside the binary. Only
        // *.bundle next to wally, never anything else on PATH.
        std::error_code ec;
        for (const auto& entry : fs::directory_iterator(fs::path(exe).parent_path(), ec)) {
            if (entry.path().extension() == ".bundle") {
                targets.push_back({"bundle", entry.path()});
            }
        }
    }

    std::error_code ec;
    std::vector<Target> present;
    for (const auto& target : targets) {
        if (fs::exists(target.path, ec)) {
            present.push_back(target);
        }
    }
    if (present.empty()) {
        out::status_line("nothing to uninstall; wally is already gone.");
        return 0;
    }

    out::status_line("wally uninstall will delete:");
    for (const auto& target : present) {
        const std::string size = human_size(target.path);
        out::status_line("  " + target.label + "  " + target.path.string() +
                         (size.empty() ? "" : "  (" + size + ")"));
    }
    out::status_line("your coding tools (claude-code, opencode, ...) are left untouched.");

    if (!yes) {
        // Never delete without a real confirmation. A non-interactive shell
        // (piped or redirected stdin) cannot answer, so it must pass --yes on
        // purpose rather than have the prompt silently skipped.
        if (!term::stdin_is_tty()) {
            out::error_line(
                "uninstall needs a terminal to confirm; re-run with --yes to delete "
                "non-interactively.");
            return 1;
        }
        if (!confirm("delete all of the above?")) {
            out::status_line("aborted; nothing was deleted.");
            return 1;
        }
    }

    // Everything but the running binary first; the binary last, because on a
    // unix filesystem deleting the file the process is executing is safe — the
    // inode lives until the process exits.
    int failures = 0;
    fs::path binary;
    for (const auto& target : present) {
        if (target.label == "binary") {
            binary = target.path;
            continue;
        }
        std::error_code remove_ec;
        fs::remove_all(target.path, remove_ec);
        if (remove_ec) {
            out::error_line("could not delete " + target.path.string() + ": " +
                            remove_ec.message());
            ++failures;
        }
    }
    if (!binary.empty()) {
        std::error_code remove_ec;
        fs::remove(binary, remove_ec);
        if (remove_ec) {
            out::error_line("could not delete " + binary.string() + ": " + remove_ec.message());
            ++failures;
        }
    }

    if (failures != 0) {
        return 1;
    }
    out::status_line("wally is uninstalled. thanks for trying it.");
    return 0;
}

void register_help(CLI::App& app, GlobalOptions& options) {
    static_cast<void>(options);
    CLI::App* cmd = app.add_subcommand("help", "Show help (same as --help)");
    auto topic = std::make_shared<std::string>();
    cmd->add_option("command", *topic, "show help for this command");
    cmd->callback([&app, topic] {
        if (!topic->empty()) {
            try {
                std::fputs(app.get_subcommand(*topic)->help().c_str(), stdout);
                return;
            } catch (const CLI::Error&) {
                // No such subcommand: fall through to the top-level help.
            }
        }
        std::fputs(app.help().c_str(), stdout);
    });
}

void register_uninstall(CLI::App& app, GlobalOptions& options) {
    static_cast<void>(options);
    auto yes = std::make_shared<bool>(false);
    CLI::App* cmd = app.add_subcommand(
        "uninstall",
        "Remove wally, its on-device models, and its config (leaves your coding tools)");
    cmd->add_flag("-y,--yes", *yes, "Delete without asking for confirmation");
    cmd->callback([yes] {
        const int code = run_uninstall(*yes);
        if (code != 0) {
            throw CLI::RuntimeError(code);
        }
    });
}

}  // namespace wally::commands

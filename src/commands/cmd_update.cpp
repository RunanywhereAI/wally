/**
 * @file cmd_update.cpp
 * @brief `wally update` — re-run the installer, telling it our version.
 *
 * The installer already knows how to fetch the latest release; the only thing
 * it cannot know on its own is what the caller is running. So update hands it
 * this build's version, and the script decides: pull the newer release, or
 * report that this machine is already current and download nothing.
 */

#include "commands/commands.h"

#include <cstdlib>
#include <memory>
#include <string>

#include "io/output.h"

#ifndef WALLY_VERSION
#define WALLY_VERSION "0.0.0-dev"
#endif

namespace wally::commands {
namespace {

// The same script the install line in the README pipes to a shell. Kept as one
// constant so the update path and the documented install path cannot drift.
constexpr const char* kInstallUrl =
    "https://raw.githubusercontent.com/RunanywhereAI/wally/main/install.sh";

}  // namespace

int run_update(bool nightly) {
#if defined(_WIN32)
    static_cast<void>(nightly);
    // The installer is a POSIX shell script; the Windows bottle updates
    // through its own channel, not this command.
    out::error_line("wally update is not available on Windows; reinstall from the release page");
    return 1;
#else
    // WALLY_VERSION is a compile-time constant and the flag is a fixed token,
    // so the command line carries nothing a caller could inject.
    std::string command = "curl -fsSL ";
    command += kInstallUrl;
    command += " | sh -s --";
    if (nightly) {
        command += " --nightly";
    }
    command += " --version=";
    command += WALLY_VERSION;

    out::status_line("checking for a newer wally...");
    // std::system returns a wait-status, not the exit code; a non-zero one
    // means the installer already explained why on its own stderr.
    return std::system(command.c_str()) != 0 ? 1 : 0;
#endif
}

void register_update(CLI::App& app, GlobalOptions& options) {
    static_cast<void>(options);
    auto nightly = std::make_shared<bool>(false);
    CLI::App* cmd = app.add_subcommand("update", "Update wally to the latest release");
    cmd->add_flag("--nightly", *nightly, "Track the development channel");
    cmd->callback([nightly]() {
        if (run_update(*nightly) != 0) {
            throw CLI::RuntimeError(1);
        }
    });
}

}  // namespace wally::commands

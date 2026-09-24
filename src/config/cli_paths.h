/**
 * @file cli_paths.h
 * @brief wally directory resolution.
 *
 * One knob controls where models live: the RunAnywhere HOME directory.
 *   resolution: --home flag → $RUNANYWHERE_HOME → ${XDG_DATA_HOME:-~/.local/share}/runanywhere
 * Models are derived BY COMMONS from that home via rac_model_paths_*
 * (home named "runanywhere" → <home>/Models/{framework}/<id>, the same layout
 * the Linux test rig and Playground tooling use).
 *
 * Config (secure store) stays under ${XDG_CONFIG_HOME:-~/.config}/runanywhere;
 * REPL history under ${XDG_STATE_HOME:-~/.local/state}/runanywhere.
 */

#ifndef WALLY_CONFIG_CLI_PATHS_H
#define WALLY_CONFIG_CLI_PATHS_H

#include <string>

namespace wally::paths {

/**
 * Resolve the RunAnywhere home (storage base dir) — see file header for the
 * precedence. Returns empty string only when $HOME is unresolvable.
 */
std::string resolve_home(const std::string& override_dir);

/**
 * ${XDG_STATE_HOME:-~/.local/state}/runanywhere (not created). On Windows,
 * with XDG_STATE_HOME unset, %LOCALAPPDATA%/RunAnywhere/state even when HOME
 * is set (MSYS2 / Git Bash).
 */
std::string state_dir();

/** Strip one trailing '/' (keeps root "/"). */
std::string normalize_dir(std::string dir);

}  // namespace wally::paths

#endif  // WALLY_CONFIG_CLI_PATHS_H

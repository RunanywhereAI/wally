#include "harness/harness.h"

#include <cerrno>
#include <chrono>
#include <cstdio>
#include <cstdlib>
#include <filesystem>
#include <cstring>
#include <iostream>
#include <string>
#include <thread>
#include <vector>

#if defined(_WIN32)
#include <process.h>
#include <winsock2.h>
// windows.h after winsock2.h, never before: the reverse order pulls in the
// Winsock 1 declarations and they clash. Needed for CreateProcessW, which is
// how a .cmd/.bat target is launched (the _spawn* family cannot).
#include <windows.h>
#include <algorithm>
#include <cctype>
// Winsock spells these differently: a socket is an unsigned SOCKET rather than
// a file descriptor, and getsockname takes an int length rather than socklen_t.
using wally_socklen_t = int;
#else
#include <arpa/inet.h>
#include <netinet/in.h>
#include <sys/socket.h>
#include <sys/wait.h>
#include <unistd.h>
using wally_socklen_t = socklen_t;
#endif

#if defined(WALLY_HAS_SERVER)
#include "rac/server/rac_server.h"
#endif

#include "account/credentials.h"
#include "account/model_cache.h"
#include "cli_formatter.h"
#include "commands/commands.h"
#include "io/output.h"
#include "bootstrap.h"
#include "harness/catalog_models.h"
#include "catalog/catalog.h"
#include "harness/local_models.h"
#include "harness/opencode.h"
#include "util/term.h"

namespace wally::harness {

#if defined(_WIN32)
// Quote one argument so the child re-parses it as a single token. The _spawn*
// family joins argv into a command line WITHOUT quoting, so an argument that
// contains a space would otherwise arrive split in two. Rules per the
// documented MSVCRT parser: double the run of backslashes that precedes a quote
// (or the closing quote), and backslash-escape embedded quotes. The POSIX path
// needs none of this -- execvp hands argv to the child verbatim.
std::string QuoteWindowsArg(const std::string& arg) {
    if (!arg.empty() && arg.find_first_of(" \t\n\v\"") == std::string::npos) {
        return arg;
    }
    std::string quoted = "\"";
    for (std::size_t i = 0;; ++i) {
        std::size_t backslashes = 0;
        while (i < arg.size() && arg[i] == '\\') {
            ++i;
            ++backslashes;
        }
        if (i == arg.size()) {
            quoted.append(backslashes * 2, '\\');
            break;
        }
        if (arg[i] == '"') {
            quoted.append(backslashes * 2 + 1, '\\');
            quoted.push_back('"');
        } else {
            quoted.append(backslashes, '\\');
            quoted.push_back(arg[i]);
        }
    }
    quoted.push_back('"');
    return quoted;
}
#endif

// Builds the command line that runs a Windows batch target through cmd.exe. See
// the header for the contract. Deliberately free of Windows headers so the
// escaping — the security-sensitive part — is unit-tested on every platform.
bool BuildBatchCommandLine(const std::string& script, const std::vector<std::string>& args,
                           std::string* command_line, std::string* error) {
    // cmd.exe reprocesses metacharacters that CreateProcess passes through, so a
    // batch target reopens the injection hole that quoting for a normal child
    // closes (CVE-2024-24576). A double quote, a percent (environment
    // expansion), and CR/LF cannot be neutralised inside a cmd-quoted token, so
    // a token carrying one is refused rather than run.
    const auto offending = [](const std::string& token) -> const char* {
        for (const char c : token) {
            if (c == '"') return "a double quote";
            if (c == '%') return "a percent sign";
            if (c == '\r') return "a carriage return";
            if (c == '\n') return "a newline";
        }
        return nullptr;
    };
    // Wrap a token (already known to hold no double quote) so both cmd.exe and
    // the target's C runtime read it as one literal: the quotes make cmd treat
    // & | < > ( ) ^ as text, and doubling a trailing backslash run stops the
    // closing quote being escaped when the child re-parses the line.
    const auto quote = [](const std::string& token) {
        std::size_t trailing = 0;
        while (trailing < token.size() && token[token.size() - 1 - trailing] == '\\') {
            ++trailing;
        }
        return "\"" + token + std::string(trailing, '\\') + "\"";
    };

    if (const char* bad = offending(script)) {
        if (error != nullptr) *error = std::string("cannot launch this tool: its path holds ") + bad;
        return false;
    }
    std::string inner = quote(script);
    for (const std::string& arg : args) {
        if (const char* bad = offending(arg)) {
            if (error != nullptr) {
                *error = std::string("cannot pass ") + bad + " to a Windows .cmd/.bat tool";
            }
            return false;
        }
        inner += ' ';
        inner += quote(arg);
    }
    // /d skips any AutoRun, /s makes cmd strip exactly the one outer quote pair
    // added here and run the remainder verbatim, /c runs and exits.
    if (command_line != nullptr) *command_line = "cmd.exe /d /s /c \"" + inner + "\"";
    return true;
}

namespace {


/// A port nothing is listening on, found by letting the OS pick one and giving
/// it straight back. There is a race between closing and the server binding,
/// but the alternative is a fixed port that collides with a second wally.
///
int FreePort() {
#if defined(_WIN32)
    // Winsock has to be initialised before any socket call, and the server that
    // would otherwise do it has not started yet. The count is per-process and
    // refcounted, so starting it here and leaving it up is harmless.
    static const bool ready = [] {
        WSADATA data;
        return WSAStartup(MAKEWORD(2, 2), &data) == 0;
    }();
    if (!ready) {
        return 0;
    }
    const SOCKET sock = socket(AF_INET, SOCK_STREAM, 0);
    // INVALID_SOCKET, not a negative number: SOCKET is unsigned on Windows, so
    // the usual `< 0` check silently passes for a failed call.
    if (sock == INVALID_SOCKET) {
        return 0;
    }
#else
    const int sock = socket(AF_INET, SOCK_STREAM, 0);
    if (sock < 0) {
        return 0;
    }
#endif
    sockaddr_in address{};
    address.sin_family = AF_INET;
    address.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    address.sin_port = 0;
    int port = 0;
    if (bind(sock, reinterpret_cast<sockaddr*>(&address), sizeof(address)) == 0) {
        wally_socklen_t length = static_cast<wally_socklen_t>(sizeof(address));
        if (getsockname(sock, reinterpret_cast<sockaddr*>(&address), &length) == 0) {
            port = ntohs(address.sin_port);
        }
    }
#if defined(_WIN32)
    closesocket(sock);
#else
    close(sock);
#endif
    return port;
}

constexpr const char* kConfigVariable = "OPENCODE_CONFIG_CONTENT";

bool ConfirmModelPull(const std::string& model) {
    std::fprintf(stderr, "%s is not installed. Download it now? [y/N] ",
                 model.c_str());
    std::fflush(stderr);
    std::string answer;
    return std::getline(std::cin, answer) &&
           !answer.empty() &&
           (answer.front() == 'y' || answer.front() == 'Y');
}

void SetConfigVariable(const std::string& value) {
#if defined(_WIN32)
    _putenv_s(kConfigVariable, value.c_str());
#else
    setenv(kConfigVariable, value.c_str(), 1);
#endif
}

void UnsetConfigVariable() {
#if defined(_WIN32)
    _putenv_s(kConfigVariable, "");
#else
    unsetenv(kConfigVariable);
#endif
}

/// How you get a harness we do not ship. Kept beside the spawn so a missing
/// tool answers the only question the person actually has.
// The real, documented install command for each harness we do not ship, so the
// person can copy the line and run it. Verified against each tool's own docs:
// opencode-ai and @deepseek-ai/dsh are npm packages; Claude Code and Hermes ship
// a native install script (npm for Claude Code is deprecated); OpenClaw's npm
// package is openclaw@latest.
std::string InstallHint(const std::string& tool) {
    if (tool == "opencode") {
        return "install it with `npm i -g opencode-ai`, then run this again";
    }
    if (tool == "openclaw") {
        return "install it with `npm i -g openclaw@latest`, then run this again";
    }
    if (tool == "dsh") {
        return "install it with `npm i -g @deepseek-ai/dsh`, then run this again";
    }
    if (tool == "claude") {
#if defined(_WIN32)
        return "install it with `irm https://claude.ai/install.ps1 | iex` in PowerShell, then run "
               "this again";
#else
        return "install it with `curl -fsSL https://claude.ai/install.sh | bash`, then run this again";
#endif
    }
    if (tool == "hermes") {
#if defined(_WIN32)
        return "install it with `iex (irm "
               "https://raw.githubusercontent.com/NousResearch/hermes-agent/main/scripts/install.ps1)`"
               " in PowerShell, then run this again";
#else
        return "install it with `curl -fsSL https://hermes-agent.nousresearch.com/install.sh | bash "
               "-s -- --skip-setup`, then run this again";
#endif
    }
    return "install " + tool + " and put it on PATH, then run this again";
}

#if defined(_WIN32)
constexpr char kPathSeparator = ';';
#else
constexpr char kPathSeparator = ':';
#endif

/// The file names a launchable `tool` can take in a directory. Windows carries
/// the extension in the name (an npm shim is `tool.cmd`, a native build
/// `tool.exe`); POSIX has just the bare name.
std::vector<std::string> ExecutableNames(const std::string& tool) {
#if defined(_WIN32)
    return {tool + ".exe", tool + ".cmd", tool + ".bat", tool};
#else
    return {tool};
#endif
}

bool DirHasTool(const std::filesystem::path& dir, const std::string& tool) {
    for (const std::string& name : ExecutableNames(tool)) {
        std::error_code ec;
        const std::filesystem::path candidate = dir / name;
        if (std::filesystem::is_regular_file(candidate, ec) ||
            std::filesystem::is_symlink(candidate, ec)) {
            return true;
        }
    }
    return false;
}

bool OnPath(const std::string& tool) {
    const char* path = std::getenv("PATH");
    if (path == nullptr) {
        return false;
    }
    const std::string haystack(path);
    std::size_t at = 0;
    while (at <= haystack.size()) {
        const std::size_t end = haystack.find(kPathSeparator, at);
        std::string dir =
            haystack.substr(at, end == std::string::npos ? std::string::npos : end - at);
#if !defined(_WIN32)
        // POSIX reads an empty PATH component as the working directory, and
        // execvp honours that. Skipping it here made this preflight stricter
        // than the exec it is meant to predict, so `./tool` on an empty
        // component was reported missing and never run.
        if (dir.empty()) {
            dir = ".";
        }
#endif
        if (!dir.empty() && DirHasTool(dir, tool)) {
            return true;
        }
        if (end == std::string::npos) {
            break;
        }
        at = end + 1;
    }
    return false;
}

/// Per-user install locations a fresh harness install lands in before the shell
/// has picked it up on PATH: npm's global bin, the native installers' own bin,
/// and on Windows the AppData npm shims. Probed so a just-installed tool is not
/// wrongly reported missing before its bin directory reaches PATH.
std::vector<std::filesystem::path> CommonInstallDirs() {
    std::vector<std::filesystem::path> dirs;
#if defined(_WIN32)
    if (const char* appdata = std::getenv("APPDATA"); appdata != nullptr && *appdata != 0) {
        dirs.emplace_back(std::filesystem::path(appdata) / "npm");
    }
    if (const char* local = std::getenv("LOCALAPPDATA"); local != nullptr && *local != 0) {
        dirs.emplace_back(std::filesystem::path(local) / "npm");
        dirs.emplace_back(std::filesystem::path(local) / "hermes" / "bin");
    }
    if (const char* profile = std::getenv("USERPROFILE"); profile != nullptr && *profile != 0) {
        dirs.emplace_back(std::filesystem::path(profile) / ".local" / "bin");
    }
#else
    if (const char* home = std::getenv("HOME"); home != nullptr && *home != 0) {
        dirs.emplace_back(std::filesystem::path(home) / ".local" / "bin");
        dirs.emplace_back(std::filesystem::path(home) / ".npm-global" / "bin");
    }
#endif
    return dirs;
}

/// The common install dir that holds `tool`, or empty when none does. This only
/// answers the "installed but not yet on PATH" case; a tool already on PATH is
/// OnPath's job.
std::filesystem::path LocateOffPath(const std::string& tool) {
    for (const std::filesystem::path& dir : CommonInstallDirs()) {
        if (DirHasTool(dir, tool)) {
            return dir;
        }
    }
    return {};
}

/// Puts `dir` first on this process's PATH so the launch below can exec a tool
/// found off PATH. The child inherits the change; nothing outside wally sees it.
void PrependToPath(const std::filesystem::path& dir) {
    const char* existing = std::getenv("PATH");
    std::string updated = dir.string();
    if (existing != nullptr && *existing != 0) {
        updated += kPathSeparator;
        updated += existing;
    }
#if defined(_WIN32)
    static_cast<void>(_putenv_s("PATH", updated.c_str()));
#else
    static_cast<void>(setenv("PATH", updated.c_str(), 1));
#endif
}

#if defined(_WIN32)
/// The resolved file `tool` runs as, searched along PATH in the same
/// preference order `_spawnvp` uses (native `.exe` before an npm `.cmd`/`.bat`
/// shim). Empty when nothing matches. Used to learn a target's extension before
/// launching it, since a batch file needs the command processor.
std::filesystem::path ResolveOnPath(const std::string& tool) {
    const char* path = std::getenv("PATH");
    if (path == nullptr) {
        return {};
    }
    const std::string haystack(path);
    std::size_t at = 0;
    while (at <= haystack.size()) {
        const std::size_t end = haystack.find(kPathSeparator, at);
        const std::string dir =
            haystack.substr(at, end == std::string::npos ? std::string::npos : end - at);
        if (!dir.empty()) {
            for (const std::string& name : ExecutableNames(tool)) {
                std::error_code ec;
                const std::filesystem::path candidate = std::filesystem::path(dir) / name;
                if (std::filesystem::is_regular_file(candidate, ec) ||
                    std::filesystem::is_symlink(candidate, ec)) {
                    return candidate;
                }
            }
        }
        if (end == std::string::npos) {
            break;
        }
        at = end + 1;
    }
    return {};
}

std::wstring WidenUtf8(const std::string& text) {
    if (text.empty()) {
        return {};
    }
    const int size =
        MultiByteToWideChar(CP_UTF8, 0, text.data(), static_cast<int>(text.size()), nullptr, 0);
    std::wstring wide(static_cast<std::size_t>(size), L'\0');
    MultiByteToWideChar(CP_UTF8, 0, text.data(), static_cast<int>(text.size()), wide.data(), size);
    return wide;
}

/// Runs a fully-formed command line through CreateProcessW and waits for it,
/// returning the child's exit code. Used for the batch path, where _spawnvp's
/// own argv joining would fight the quoting the command processor needs.
int SpawnCommandLine(const std::string& tool, const std::string& command_line) {
    std::wstring wide = WidenUtf8(command_line);
    std::vector<wchar_t> buffer(wide.begin(), wide.end());
    buffer.push_back(L'\0');
    STARTUPINFOW startup{};
    startup.cb = sizeof(startup);
    PROCESS_INFORMATION process{};
    if (CreateProcessW(nullptr, buffer.data(), nullptr, nullptr, TRUE, 0, nullptr, nullptr, &startup,
                       &process) == 0) {
        out::error_line(tool + " could not be launched");
        return 127;
    }
    WaitForSingleObject(process.hProcess, INFINITE);
    DWORD code = 0;
    GetExitCodeProcess(process.hProcess, &code);
    CloseHandle(process.hThread);
    CloseHandle(process.hProcess);
    return static_cast<int>(code);
}
#endif

int Spawn(const std::string& tool, const std::vector<std::string>& args) {
    // Checked before the fork, not after: a failed exec happens in the child,
    // where the only thing it can report back is the exit code a shell uses
    // for "command not found" — so without this the person sees nothing at all.
    if (!EnsureInstalled(tool)) {
        return 127;
    }

    std::vector<std::string> owned;
#if defined(_WIN32)
    owned.push_back(QuoteWindowsArg(tool));
    for (const auto& arg : args) owned.push_back(QuoteWindowsArg(arg));
#else
    owned.push_back(tool);
    owned.insert(owned.end(), args.begin(), args.end());
#endif
    std::vector<char*> argv;
    argv.reserve(owned.size() + 1);
    for (std::string& piece : owned) {
        argv.push_back(piece.data());
    }
    argv.push_back(nullptr);

#if defined(_WIN32)
    // A .cmd/.bat target — an npm-installed CLI is a `tool.cmd` shim — cannot be
    // launched through CreateProcess, which is what _spawnvp uses: it resolves
    // the script and then fails to start it. Route those through the command
    // processor instead; a native `.exe` keeps the direct spawn.
    const std::filesystem::path resolved = ResolveOnPath(tool);
    std::string extension = resolved.extension().string();
    std::transform(extension.begin(), extension.end(), extension.begin(),
                   [](unsigned char c) { return static_cast<char>(std::tolower(c)); });
    if (extension == ".cmd" || extension == ".bat") {
        std::string command_line;
        std::string error;
        if (!BuildBatchCommandLine(resolved.string(), args, &command_line, &error)) {
            out::error_line(tool + ": " + error);
            return 1;
        }
        return SpawnCommandLine(tool, command_line);
    }

    // _spawnvp joins argv with bare spaces, so quote each piece or a prompt such
    // as "fix the tests" reaches the tool as three separate arguments.
    std::vector<std::string> quoted;
    quoted.reserve(owned.size());
    for (const std::string& piece : owned) {
        quoted.push_back(QuoteWindowsArg(piece));
    }
    argv.clear();
    for (std::string& piece : quoted) {
        argv.push_back(piece.data());
    }
    argv.push_back(nullptr);
    const intptr_t rc = _spawnvp(_P_WAIT, tool.c_str(), argv.data());
    if (rc < 0) {
        out::error_line(tool + " is not on PATH");
        return 127;
    }
    return static_cast<int>(rc);
#else
    // fork rather than exec: the local server lives in this process, and
    // replacing the image would take it down with us before the tool ran.
    const pid_t child = fork();
    if (child < 0) {
        out::error_line("could not start " + tool);
        return 1;
    }
    if (child == 0) {
        execvp(tool.c_str(), argv.data());
        // Only reached when exec failed. 127 is what a shell reports for a
        // command it cannot find, and the parent cannot tell why otherwise.
        _exit(127);
    }
    int status = 0;
    // status is undefined after a failed waitpid, so a bare 0 there would read
    // as a clean exit while the child is still running.
    while (waitpid(child, &status, 0) < 0) {
        if (errno == EINTR) {
            continue;
        }
        out::error_line("lost track of " + tool);
        return 1;
    }
    if (WIFEXITED(status)) {
        return WEXITSTATUS(status);
    }
    return 1;
#endif
}

long long EpochSeconds() {
    return std::chrono::duration_cast<std::chrono::seconds>(
               std::chrono::system_clock::now().time_since_epoch())
        .count();
}

/// The refresh half of the same dance `wally account usage` uses: exchange the refresh
/// token for a new access token and persist it, so later commands in the same
/// session do not pay for the refresh again.
bool RefreshSession(const account::ConsoleClient& console, account::Credentials* credentials,
                    std::string* error, bool* unavailable) {
    if (unavailable != nullptr) {
        *unavailable = false;
    }
    if (credentials->refresh_token.empty()) {
        if (error != nullptr) {
            *error = "the cloud session cannot be refreshed; run `wally account login`";
        }
        return false;
    }
    account::Grant grant;
    if (!console.Refresh(credentials->console_url, credentials->refresh_token, &grant, error,
                         unavailable)) {
        return false;
    }
    credentials->access_token = grant.access_token;
    if (!grant.refresh_token.empty()) {
        credentials->refresh_token = grant.refresh_token;
    }
    credentials->expires_at = EpochSeconds() + (grant.expires_in > 0 ? grant.expires_in : 3600);
    return account::Save(*credentials, error);
}

}  // namespace

bool ModelIdIsSafe(const std::string& id) {
    if (id.empty() || id.size() > 512) {
        return false;
    }
    for (const unsigned char byte : id) {
        if (byte < 0x20 || byte == 0x7f) {
            return false;
        }
        // `/` and `\` never appear in a real id — LocalModels() yields a bare
        // directory name — and `< > " ' &` are exactly what an unescaped XML
        // attribute or a shell word cannot survive.
        if (byte == '<' || byte == '>' || byte == '"' || byte == '\'' || byte == '&' ||
            byte == '/' || byte == '\\') {
            return false;
        }
    }
    return true;
}

bool VerifyCloudSession(const account::ConsoleClient& console, account::Credentials* credentials,
                        std::string* email, std::string* error, bool* unverified) {
    if (unverified != nullptr) {
        *unverified = false;
    }
    if (credentials == nullptr) {
        if (error != nullptr) {
            *error = "internal cloud session error";
        }
        return false;
    }
    // An EXPIRED token refreshes first, and that refresh is itself a console
    // call that can be rate limited. This is the path a real user hits, because
    // tokens expire hourly: the refresh 429'd and the launch was refused before
    // the identity check below was ever reached (InferenceInfra#444).
    if (credentials->access_token_expired(EpochSeconds())) {
        bool refresh_unavailable = false;
        if (!RefreshSession(console, credentials, error, &refresh_unavailable)) {
            if (unverified != nullptr) {
                *unverified = refresh_unavailable;
            }
            return false;
        }
    }
    account::Identity identity;
    account::IdentityResult result =
        console.WhoAmI(credentials->console_url, credentials->access_token, &identity, error);
    if (result == account::IdentityResult::Unauthorized) {
        bool refresh_unavailable = false;
        if (!RefreshSession(console, credentials, error, &refresh_unavailable)) {
            if (unverified != nullptr) {
                *unverified = refresh_unavailable;
            }
            return false;
        }
        identity = account::Identity{};
        result = console.WhoAmI(credentials->console_url, credentials->access_token, &identity, error);
    }
    if (result != account::IdentityResult::Ok) {
        // Separate "the console says this session is bad" from "the console
        // could not be asked". Only the first should stop a launch.
        if (unverified != nullptr) {
            *unverified = result == account::IdentityResult::Unavailable;
        }
        return false;
    }
    if (email != nullptr) {
        *email = identity.email;
    }
    return true;
}

bool Resolve(const std::string& model, Endpoint* endpoint, const GlobalOptions& options,
             const std::string& harness_command) {
    if (endpoint == nullptr || model.empty()) {
        return false;
    }
    if (!ModelIdIsSafe(model)) {
        out::error_line("'" + model + "' is not a valid model id");
        return false;
    }
    // The kit consumer brings the SDK up through bootstrap rather than the
    // CLI's own lazy Start(), and bootstrap is also what resolves the storage
    // home the model walk below needs.
    Bootstrapped env;
    if (bootstrap(options, &env) != RAC_SUCCESS) {
        return false;
    }

    std::string base_url;
    std::string api_key;
    std::string console_url;
    bool serving = false;
    std::int64_t context_window = 0;

    // The names a person types (`qwen3-4b-instruct-2507`,
    // `mlx-qwen3-4b-instruct-2507`, `qwen3-4b-instruct`) are
    // catalog ids, aliases and `models list` merge keys; the directory on disk
    // is the registry id (`mlx-qwen3-4b-instruct-2507-4bit`). Accept every
    // spelling the catalog does, the same way `run` and `models pull` do, and
    // prefer a downloaded variant of a merged row over one that is not here.
    std::vector<std::string> wanted{model};
    if (const catalog::CatalogEntry* entry = catalog::find(model)) {
        wanted.push_back(entry->id);
    }
    size_t count = 0;
    const catalog::CatalogEntry* all = catalog::all(&count);
    for (size_t i = 0; i < count; ++i) {
        if (catalog::merge_key_for(all[i].id) == model) {
            wanted.push_back(all[i].id);
        }
    }

    // First spelling wins, except that a directory with no weight file in it
    // (a cancelled pull leaves the manifest and a `.part`) loses to any variant
    // whose weights are actually there. Beyond that the load below is what
    // decides whether the model opens; the walk cannot judge completeness.
    const LocalModel* local = nullptr;
    const std::vector<LocalModel> installed = LocalModels(env.home);
    for (const std::string& id : wanted) {
        for (const LocalModel& candidate : installed) {
            if (candidate.id != id) continue;
            const bool has_weights = !candidate.path.empty() || candidate.framework == "CoreML";
            if (local == nullptr || (has_weights && local->path.empty())) {
                local = &candidate;
            }
        }
        if (local != nullptr && !local->path.empty()) break;
    }

    if (local != nullptr) {
        const catalog::CatalogEntry* local_entry = catalog::find(local->id);
        if (local_entry == nullptr || !local_entry->harness_compatible) {
            out::error_line(
                "This model is not certified for coding harnesses. Tool calls or long-context "
                "operation may fail.");
            const std::string command =
                harness_command.empty() ? "opencode" : harness_command;
            out::status_line("Try: wally " + command +
                             " -m qwen3-4b-instruct-2507");
            return false;
        }
        // A directory with the manifest but no weights is a pull that did not
        // finish. Serving it fails inside llama.cpp with "No .gguf file found",
        // which reads as a bug; say what it is instead.
        if (local->path.empty() && local->framework != "CoreML") {
            out::error_line(model + " is on this machine but incomplete (a cancelled download?)");
            out::status_line("run `wally models pull " + model + "` to finish it");
            return false;
        }
        // Any backend the kit registered. The server's rac_llm_create(path)
        // looks the path up in the registry and routes on the framework it
        // finds ("Found model by path ... framework=7 ... Routed to plugin:
        // mlx", kit 0.20.37), so an MLX directory reaches MLX the same way a
        // GGUF reaches llama.cpp. An older kit routed on the path alone and
        // this used to refuse anything but LlamaCpp here; the load below is
        // now the honest gate, and it fails loudly for a framework this
        // binary does not have.
        const int port = FreePort();
        if (port == 0) {
            out::error_line("could not find a free port for the local server");
            return false;
        }
#if !defined(WALLY_HAS_SERVER)
        // This kit was built without the OpenAI-compatible server, so there is
        // nothing here that can serve a file on disk. An upstream model still
        // works, and saying which is the case beats starting nothing and
        // reporting success.
        out::error_line(model +
                        " is on this machine, but this build has no local server to serve it");
        out::status_line("point at an upstream model instead, or use a build with the server");
        return false;
#else
        rac_server_config_t config = RAC_SERVER_CONFIG_DEFAULT;
        config.host = "127.0.0.1";
        config.port = static_cast<uint16_t>(port);
        // A single-file model (GGUF) is its file; a directory model (MLX
        // safetensors shards, Core ML) is its directory, which is also what
        // `wally serve` hands the server. Passing one shard of three worked only
        // because the server resolves the path through the registry.
        const std::string path = local->framework == "LlamaCpp" && !local->path.empty()
                                     ? local->path
                                     : local->dir;
        config.model_path = path.c_str();
        config.model_id = model.c_str();
        // Sized from this machine, not a constant: a coding agent's opening
        // request is a 15k-token system prompt, and the fixed 8k this used to
        // pass rejected it. See LocalContextSize.
        config.context_size = static_cast<int32_t>(LocalContextSize(local->id));
        config.threads = 0;  // Let the backend choose for this machine.
        config.enable_cors = RAC_FALSE;
        config.verbose = options.verbose ? RAC_TRUE : RAC_FALSE;
        out::status_line("serving " + model + " on 127.0.0.1:" + std::to_string(port) + " (" +
                         std::to_string(config.context_size) + " token context)");
        const rac_result_t started = rac_server_start(&config);
        if (started != RAC_SUCCESS) {
            out::error_line("the local server would not start for " + model + ": " +
                            out::describe_result(started));
            out::status_line("check `wally models show " + model + "` and finish its download "
                             "with `wally models pull " + model + "` (using the same --home)");
            return false;
        }
        serving = true;
        context_window = config.context_size;
        base_url = "http://127.0.0.1:" + std::to_string(port) + "/v1";
#endif  // WALLY_HAS_SERVER
    } else {
        const catalog::CatalogEntry* requested = catalog::find(model);
        if (requested != nullptr && requested->harness_compatible) {
            if (!term::stdin_is_tty()) {
                out::error_line(model + " is not downloaded on this machine");
                out::status_line("run `wally models pull " + model +
                                 "` first (using the same --home)");
                return false;
            }
            if (!ConfirmModelPull(model)) {
                out::status_line("download cancelled; run `wally models pull " +
                                 model + "` when ready");
                return false;
            }
            if (commands::pull_model_flow(options, requested->id) != 0) {
                return false;
            }
            return Resolve(model, endpoint, options, harness_command);
        }

        account::Credentials credentials;
        std::string load_error;
        if (!account::Load(&credentials, &load_error)) {
            out::error_line(load_error);
            return false;
        }
        if (!credentials.signed_in()) {
            // A known local model that has not been pulled should not send a
            // keyless user into the cloud login flow. Signed-in users still
            // get the normal catalog refresh for a hosted id of this spelling.
            if (catalog::find(model) != nullptr || wanted.size() > 1) {
                out::error_line(model + " is not downloaded on this machine");
                out::status_line("run `wally models pull " + model +
                                 "` first (using the same --home)");
            } else {
                ReportNotSignedIn();
            }
            return false;
        }
        // Keep the catalog fresh for next time without blocking this launch, and
        // catch a mistyped hosted id from the cache. Local models already took
        // the branch above; fail open when the cache is empty (offline / never
        // refreshed) so a launch is never blocked for lack of a network call.
        account::RefreshModelCacheIfStale(account::kModelCacheTtlSeconds);
        if (account::CacheHasModels() && !account::ModelIsCached(model)) {
            // Not in the cache: it may just be stale. Refresh live and retry, so
            // a valid new model launches instead of being wrongly rejected.
            if (!RefreshAndRecheckModel(credentials, model)) {
                return false;
            }
        }
        // signed_in() only proves a token is present, not that it is real: a
        // hand-written credentials.json satisfies it with any non-empty
        // string. Everything past this point is destructive to a caller's
        // running app or session, so confirm the session against the console
        // first — the same identity check `wally account whoami` makes, with the
        // same refresh-on-401 dance `wally account usage` uses.
        const account::ConsoleClient console;
        std::string email;
        std::string verify_error;
        bool unverified = false;
        if (!VerifyCloudSession(console, &credentials, &email, &verify_error, &unverified)) {
            if (!unverified) {
                ReportCloudSessionInvalid(model);
                return false;
            }
            // The console could not be ASKED - it is rate limiting or down.
            // That is no disproof of the session already on disk, and refusing
            // here locked every signed-in person out of their own harness while
            // a load test ran against the same console (InferenceInfra#444).
            // Go in on the stored session; the harness's own calls surface the
            // real error if it is still there.
            out::status_line("could not confirm the cloud session (" + verify_error +
                             ") - continuing on the stored session");
        }
        base_url = credentials.console_url + "/v1";
        api_key = credentials.access_token;
        console_url = credentials.console_url;
        out::status_line("using " + model + (email.empty() ? "" : " as " + email));
    }

    endpoint->base_url = base_url;
    endpoint->api_key = api_key;
    endpoint->console_url = console_url;
    endpoint->serving = serving;
    endpoint->context_window = context_window;
    endpoint->max_output = LocalOutputSize(context_window);
    return true;
}

void Release(const Endpoint& endpoint) {
    if (endpoint.serving) {
#if defined(WALLY_HAS_SERVER)
        rac_server_stop();
#endif
    }
}

bool EnsureInstalled(const std::string& tool) {
    if (OnPath(tool)) {
        return true;
    }
    // Not on PATH, but a fresh install often sits in a well-known bin the shell
    // has not picked up yet (a just-run `npm i -g`, or a Windows AppData shim).
    // If it does, put that directory on PATH for this run so the launch can exec
    // it, rather than telling the person to install what is already there.
    const std::filesystem::path dir = LocateOffPath(tool);
    if (!dir.empty()) {
        PrependToPath(dir);
        return true;
    }
    out::error_line(tool + " is not installed on this machine");
    out::status_line(InstallHint(tool));
    return false;
}

void ReportCloudSessionInvalid(const std::string& model) {
    // Stderr, on its own line: a red "Error:" a person cannot miss, and the
    // action `wally account login` highlighted so the fix stands out. Color is dropped
    // under NO_COLOR or when stderr is not a terminal.
    const cli_color::Palette pal = cli_color::make_palette(color_output_enabled(false));
    out::status_line(std::string(pal.red) + "Error:" + pal.reset + " You cannot use " + model +
                     ", your cloud session is no longer valid, do: " + pal.bold_cyan + "wally account login" +
                     pal.reset + " and try again");
}

void ReportNotSignedIn() {
    const cli_color::Palette pal = cli_color::make_palette(color_output_enabled(false));
    out::status_line(std::string(pal.red) + "Error:" + pal.reset +
                     " You are not logged in, log in with " + pal.bold_cyan + "wally account login" +
                     pal.reset);
}

bool RefreshAndRecheckModel(const account::Credentials& credentials, const std::string& model) {
    const cli_color::Palette pal = cli_color::make_palette(color_output_enabled(false));
    out::status_line("could not find '" + model + "' in the catalog");
    out::status_line(std::string(pal.blue) + "fetching the latest catalog..." + pal.reset);
    bool busy = false;
    if (!account::RefreshModelCacheNow(credentials, &busy)) {
        out::status_line(std::string(pal.red) + "Error:" + pal.reset +
                         " sorry, the server is busy, try again after some time");
        return false;
    }
    out::status_line(std::string(pal.green) + "catalog updated, checking your model" + pal.reset);
    if (!account::ModelIsCached(model)) {
        out::error_line("unknown model '" + model + "'");
        // The cache is fresh (just refreshed), so this list is accurate.
        const std::vector<std::string> available = account::CachedModelIds();
        if (!available.empty()) {
            out::status_line("available models:");
            for (const std::string& id : available) {
                out::status_line("  " + id);
            }
        }
        return false;
    }
    out::status_line(std::string(pal.blue) + "found the model, launching the harness" + pal.reset);
    return true;
}

int Launch(const std::string& tool, const std::string& model,
           const std::vector<std::string>& args, const GlobalOptions& options) {
    if (model.empty()) {
        // Nothing to wire, so do not pretend to: run the tool as the user has
        // it configured.
        return Spawn(tool, args);
    }

    Endpoint endpoint;
    if (!Resolve(model, &endpoint, options, tool)) {
        return 1;
    }

    const std::string config =
        BuildOpenCodeConfig(model, endpoint.base_url, endpoint.api_key, CatalogModels(endpoint, model));
    const char* previous = std::getenv(kConfigVariable);
    const std::string restored = previous != nullptr ? previous : std::string();
    const bool had_previous = previous != nullptr;
    SetConfigVariable(config);

    const int status = Spawn(tool, args);

    // Launch runs more than once in a process during tests, and a stale value
    // here would override the tool's own configuration on a later call that
    // named no model.
    if (had_previous) {
        SetConfigVariable(restored);
    } else {
        UnsetConfigVariable();
    }
    Release(endpoint);
    return status;
}

}  // namespace wally::harness

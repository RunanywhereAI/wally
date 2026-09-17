#include "harness/harness.h"

#include <cerrno>
#include <chrono>
#include <cstdlib>
#include <filesystem>
#include <cstring>
#include <string>
#include <thread>
#include <vector>

#if defined(_WIN32)
#include <process.h>
#include <winsock2.h>
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
#include "harness/local_models.h"

namespace wally::harness {
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

/// JSON string escaping, for the handful of characters that can appear in a
/// model id, a path or a key. Not a general encoder: it exists so a Windows
/// path with backslashes does not silently produce invalid config.
std::string Quote(const std::string& text) {
    std::string out = "\"";
    for (const char c : text) {
        switch (c) {
            case '"': out += "\\\""; break;
            case '\\': out += "\\\\"; break;
            case '\n': out += "\\n"; break;
            case '\r': out += "\\r"; break;
            case '\t': out += "\\t"; break;
            default: out += c;
        }
    }
    return out + "\"";
}

/// The provider block opencode reads out of OPENCODE_CONFIG_CONTENT.
///
/// Inline rather than a file on purpose: writing to the user's project or to
/// ~/.config/opencode would outlive the session and change how opencode behaves
/// when they run it themselves.
std::string OpencodeConfig(const std::string& model, const std::string& base_url,
                           const std::string& api_key) {
    // A key is always present because opencode's OpenAI client sends an
    // Authorization header regardless; a local server ignores what is in it.
    const std::string key = api_key.empty() ? std::string("local") : api_key;
    return std::string("{\"provider\":{\"runanywhere\":{") +
           "\"npm\":\"@ai-sdk/openai-compatible\"," + "\"name\":\"RunAnywhere\"," +
           "\"options\":{\"baseURL\":" + Quote(base_url) + ",\"apiKey\":" + Quote(key) + "}," +
           "\"models\":{" + Quote(model) + ":{\"name\":" + Quote(model) + "}}}}," +
           "\"model\":" + Quote("runanywhere/" + model) + "}";
}

constexpr const char* kConfigVariable = "OPENCODE_CONFIG_CONTENT";

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

int Spawn(const std::string& tool, const std::vector<std::string>& args) {
    // Checked before the fork, not after: a failed exec happens in the child,
    // where the only thing it can report back is the exit code a shell uses
    // for "command not found" — so without this the person sees nothing at all.
    if (!EnsureInstalled(tool)) {
        return 127;
    }

    std::vector<std::string> owned;
    owned.push_back(tool);
    owned.insert(owned.end(), args.begin(), args.end());
    std::vector<char*> argv;
    argv.reserve(owned.size() + 1);
    for (std::string& piece : owned) {
        argv.push_back(piece.data());
    }
    argv.push_back(nullptr);

#if defined(_WIN32)
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

/// The refresh half of the same dance `wally usage` uses: exchange the refresh
/// token for a new access token and persist it, so later commands in the same
/// session do not pay for the refresh again.
bool RefreshSession(const account::ConsoleClient& console, account::Credentials* credentials,
                    std::string* error, bool* unavailable) {
    if (unavailable != nullptr) {
        *unavailable = false;
    }
    if (credentials->refresh_token.empty()) {
        if (error != nullptr) {
            *error = "the cloud session cannot be refreshed; run `wally login`";
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

bool Resolve(const std::string& model, Endpoint* endpoint) {
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
    if (bootstrap(GlobalOptions{}, &env) != RAC_SUCCESS) {
        return false;
    }

    std::string base_url;
    std::string api_key;
    std::string console_url;
    bool serving = false;

    const LocalModel* local = nullptr;
    const std::vector<LocalModel> installed = LocalModels(env.home);
    for (const LocalModel& candidate : installed) {
        // Completeness used to be checked against the catalog's file list.
        // The walk cannot do that, and does not need to: it only yields a
        // directory that already holds weights or a download manifest, and the
        // load below is what actually decides whether the model opens.
        if (candidate.id == model) {
            local = &candidate;
            break;
        }
    }

    if (local != nullptr) {
        // The server creates its handle with rac_llm_create(path), which routes
        // on the path alone rather than asking the registry what framework the
        // model belongs to. An MLX directory does not look like anything it
        // recognises, so it lands on llama.cpp and fails to load. Saying so
        // beats starting a server that answers every request with an error.
        if (local->framework != "LlamaCpp") {
            out::error_line(model + " runs on " + local->framework +
                       ", and the local server can only serve LlamaCpp models today");
            out::status_line("use a GGUF model here, or point at an upstream one");
            return false;
        }
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
        const std::string path = local->path.empty() ? local->dir : local->path;
        config.model_path = path.c_str();
        config.model_id = model.c_str();
        // The per-run context setting went with the old CLI; the server default
        // it fell back to is what every run used in practice anyway.
        config.context_size = 8192;
        out::status_line("serving " + model + " on 127.0.0.1:" + std::to_string(port));
        if (rac_server_start(&config) != RAC_SUCCESS) {
            out::error_line("the local server would not start for " + model);
            return false;
        }
        serving = true;
        base_url = "http://127.0.0.1:" + std::to_string(port) + "/v1";
#endif  // WALLY_HAS_SERVER
    } else {
        account::Credentials credentials;
        std::string load_error;
        if (!account::Load(&credentials, &load_error)) {
            out::error_line(load_error);
            return false;
        }
        if (!credentials.signed_in()) {
            ReportNotSignedIn();
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
        // first — the same identity check `wally whoami` makes, with the same
        // refresh-on-401 dance `wally usage` uses.
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
    // action `wally login` highlighted so the fix stands out. Color is dropped
    // under NO_COLOR or when stderr is not a terminal.
    const cli_color::Palette pal = cli_color::make_palette(color_output_enabled(false));
    out::status_line(std::string(pal.red) + "Error:" + pal.reset + " You cannot use " + model +
                     ", your cloud session is no longer valid, do: " + pal.bold_cyan + "wally login" +
                     pal.reset + " and try again");
}

void ReportNotSignedIn() {
    const cli_color::Palette pal = cli_color::make_palette(color_output_enabled(false));
    out::status_line(std::string(pal.red) + "Error:" + pal.reset +
                     " You are not logged in, log in with " + pal.bold_cyan + "wally login" +
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
           const std::vector<std::string>& args) {
    if (model.empty()) {
        // Nothing to wire, so do not pretend to: run the tool as the user has
        // it configured.
        return Spawn(tool, args);
    }

    Endpoint endpoint;
    if (!Resolve(model, &endpoint)) {
        return 1;
    }

    const std::string config = OpencodeConfig(model, endpoint.base_url, endpoint.api_key);
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

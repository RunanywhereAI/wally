#include "account/credentials.h"

#include "account/baked_endpoints.h"

#include <algorithm>
#include <cctype>
#include <cerrno>
#include <charconv>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <filesystem>
#include <fstream>
#include <iterator>
#include <limits>
#include <nlohmann/json.hpp>
#include <string>
#include <string_view>
#include <system_error>
#include <vector>

#if defined(_WIN32)
#define WIN32_LEAN_AND_MEAN
#include <wincrypt.h>
#include <windows.h>
#else
#include <fcntl.h>
#include <pwd.h>
#include <unistd.h>

#include <sys/stat.h>
#include <sys/types.h>
#endif

namespace wally::account {
namespace {

namespace fs = std::filesystem;
using Json = nlohmann::json;

// Two hosts, two jobs, two different deployments. Keep them adjacent: the bug
// they replace was one constant doing both, and it broke every command that
// talks to the console.
//
// The API is the control plane — /v1/auth/cli/start, /v1/auth/cli/poll,
// /v1/auth/cli/refresh, /v1/auth/me, /v1/cli/* — and it is served by Cloud Run.
// The web console is the page a person approves a sign-in on, and it is served
// by Railway. Measured 2026-09-04, before InferenceInfra#484 moved the auth
// paths under /v1/auth, so the paths below are the ones that answered then:
//
//   inference.runanywhere.ai  /auth/cli/start 422  /v1/me 405  Google Frontend
//   console.runanywhere.ai    /auth/cli/start 404  /v1/me 404  railway-hikari
//
// 422 and 405 are those endpoints rejecting a bad body and a wrong verb, which
// is how you know they are there. 404 with the console's own SPA HTML is how
// you know they are not. Point the API constant at the Railway host — which is
// what it used to be — and login, whoami and usage all 404.
constexpr const char* kProductionConsoleApi = "https://inference.runanywhere.ai";

// The approval page, under both names it answers to. One deployment: measured
// 2026-09-04, both return `server: railway-hikari` and the same `etag:
// "nqkdmw"` for /cloud/cli, so this is two DNS names for one build and not two
// origins' worth of trust.
//
// The raw Railway name is here because it is what the control plane actually
// hands out today:
//
//   POST https://inference.runanywhere.ai/v1/auth/cli/start
//     -> verification_url: https://runanywhere-frontend-production.up.railway.app/cloud/cli?code=...
//
// Trusting only the custom domain would refuse every real sign-in. Delete the
// Railway entry once the control plane's configured console origin is the
// custom domain — that is a one-line config change on the API side, and this
// list is the thing waiting on it.
constexpr const char* kProductionConsoleWeb[] = {
    "https://console.runanywhere.ai",
    "https://runanywhere-frontend-production.up.railway.app",
};

// Collapsing the API host into the approval host breaks one side or the other,
// so say so at compile time rather than in a bug report.
static_assert(std::string_view(kProductionConsoleApi) !=
                  std::string_view(kProductionConsoleWeb[0]),
              "the API host and the browser approval host are different deployments");
constexpr std::uintmax_t kMaximumCredentialBytes = 1024 * 1024;
#if defined(_WIN32)
constexpr const char* kFileName = "credentials.dat";
#else
constexpr const char* kFileName = "credentials.json";
#endif

std::string Env(const char* name) {
    const char* value = std::getenv(name);
    return value != nullptr ? std::string(value) : std::string();
}

/// `name` (the current, documented variable) wins whenever it's set. `legacy`
/// is read only when `name` is unset, so an installed rcli-era override still
/// works after the wally rename — with a one-line notice, since it's a name
/// nobody should be setting a year from now.
std::string EnvWithLegacyFallback(const char* name, const char* legacy) {
    std::string value = Env(name);
    if (!value.empty()) {
        return value;
    }
    std::string legacy_value = Env(legacy);
    if (!legacy_value.empty()) {
        std::fprintf(stderr, "warning: %s is deprecated, use %s instead\n", legacy, name);
    }
    return legacy_value;
}

std::string Lower(std::string value) {
    std::transform(value.begin(), value.end(), value.begin(),
                   [](unsigned char c) { return static_cast<char>(std::tolower(c)); });
    return value;
}

bool HasUnsafeCharacter(const std::string& text) {
    return std::any_of(text.begin(), text.end(),
                       [](unsigned char c) { return c <= 0x20 || c == 0x7f || c == '\\'; });
}

struct ParsedUrl {
    std::string scheme;
    std::string host;
    std::string port;
    std::string suffix;
    bool bracketed = false;
};

/// Reduces a console URL's path to the prefix every endpoint hangs off, or
/// fails if it is not one.
///
/// A console is not always at the root of its host. Development is reached at
/// `https://inference.runanywhere.ai/api-dev`, where the load balancer strips
/// the prefix and forwards to the dev control plane; the same host without it
/// is production. Refusing the path -- which this did until it was found --
/// leaves no way to name the dev console at all, so `--console-url` had to be
/// given the backend's own Cloud Run hostname, which bypasses the load balancer
/// and therefore reaches different code than any real client does.
///
/// A query or fragment is still refused. Every caller builds an endpoint by
/// appending to this string, and `?a=1` + `/v1/auth/me` is not a URL.
bool NormalizeBasePath(const std::string& suffix, std::string* base_path) {
    if (suffix.empty() || suffix == "/") {
        base_path->clear();
        return true;
    }
    if (suffix.front() != '/' || suffix.find_first_of("?#") != std::string::npos ||
        suffix.find("//") != std::string::npos || suffix.find("/.") != std::string::npos ||
        suffix.size() > 256) {
        return false;
    }
    *base_path = suffix.back() == '/' ? suffix.substr(0, suffix.size() - 1) : suffix;
    return true;
}

bool ValidPort(const std::string& port) {
    if (port.empty()) {
        return true;
    }
    unsigned value = 0;
    const auto result = std::from_chars(port.data(), port.data() + port.size(), value);
    return result.ec == std::errc{} && result.ptr == port.data() + port.size() && value > 0 &&
           value <= 65535;
}

bool ValidHost(const std::string& host, bool bracketed) {
    if (host.empty() || host.front() == '.' || host.back() == '.') {
        return false;
    }
    if (bracketed) {
        return std::all_of(host.begin(), host.end(), [](unsigned char c) {
            return std::isxdigit(c) != 0 || c == ':' || c == '.';
        });
    }
    return std::all_of(host.begin(), host.end(), [](unsigned char c) {
        return std::isalnum(c) != 0 || c == '-' || c == '.';
    });
}

bool ParseUrl(const std::string& input, bool allow_suffix, ParsedUrl* parsed) {
    if (parsed == nullptr || input.empty() || HasUnsafeCharacter(input)) {
        return false;
    }
    const std::size_t scheme_end = input.find("://");
    if (scheme_end == std::string::npos) {
        return false;
    }
    ParsedUrl value;
    value.scheme = Lower(input.substr(0, scheme_end));
    if (value.scheme != "https" && value.scheme != "http") {
        return false;
    }

    const std::size_t authority_start = scheme_end + 3;
    const std::size_t authority_end = input.find_first_of("/?#", authority_start);
    const std::string authority = input.substr(
        authority_start,
        authority_end == std::string::npos ? std::string::npos : authority_end - authority_start);
    value.suffix = authority_end == std::string::npos ? std::string() : input.substr(authority_end);
    if (authority.empty() || authority.find('@') != std::string::npos ||
        (!allow_suffix && !value.suffix.empty() && value.suffix != "/")) {
        return false;
    }

    if (authority.front() == '[') {
        const std::size_t close = authority.find(']');
        if (close == std::string::npos) {
            return false;
        }
        value.bracketed = true;
        value.host = Lower(authority.substr(1, close - 1));
        const std::string remainder = authority.substr(close + 1);
        if (!remainder.empty()) {
            if (remainder.front() != ':' || remainder.size() == 1) {
                return false;
            }
            value.port = remainder.substr(1);
        }
    } else {
        if (std::count(authority.begin(), authority.end(), ':') > 1) {
            return false;
        }
        const std::size_t colon = authority.rfind(':');
        value.host = Lower(authority.substr(0, colon));
        if (colon != std::string::npos) {
            value.port = authority.substr(colon + 1);
            if (value.port.empty()) {
                return false;
            }
        }
    }
    if (!ValidHost(value.host, value.bracketed) || !ValidPort(value.port)) {
        return false;
    }

    const bool loopback = value.host == "localhost" || value.host == "127.0.0.1" ||
                          (value.bracketed && value.host == "::1");
    if (value.scheme == "http" && !loopback) {
        return false;
    }
    *parsed = std::move(value);
    return true;
}

std::string RenderOrigin(const ParsedUrl& url) {
    std::string rendered = url.scheme + "://";
    rendered += url.bracketed ? "[" + url.host + "]" : url.host;
    if (!url.port.empty()) {
        rendered += ":" + url.port;
    }
    return rendered;
}

std::string HomeDirectory() {
#if defined(_WIN32)
    const std::string local = Env("LOCALAPPDATA");
    if (!local.empty()) {
        return local;
    }
    return Env("USERPROFILE");
#else
    const std::string home = Env("HOME");
    if (!home.empty()) {
        return home;
    }
    const passwd* entry = getpwuid(getuid());
    return entry != nullptr && entry->pw_dir != nullptr ? entry->pw_dir : std::string();
#endif
}

bool EnsureSecureDirectory(const std::string& directory, std::string* error) {
    std::error_code code;
    const fs::file_status before = fs::symlink_status(directory, code);
    if (!code && fs::is_symlink(before)) {
        if (error != nullptr) {
            *error = "credential directory may not be a symbolic link";
        }
        return false;
    }
    code.clear();
    fs::create_directories(directory, code);
    if (code) {
        if (error != nullptr) {
            *error = "could not create the credential directory";
        }
        return false;
    }
    const bool is_directory = fs::is_directory(directory, code);
    if (code || !is_directory) {
        if (error != nullptr) {
            *error = "could not create the credential directory";
        }
        return false;
    }
#if !defined(_WIN32)
    if (::chmod(directory.c_str(), S_IRWXU) != 0) {
        if (error != nullptr) {
            *error = "could not restrict the credential directory";
        }
        return false;
    }
    struct stat metadata{};
    if (::lstat(directory.c_str(), &metadata) != 0 || !S_ISDIR(metadata.st_mode) ||
        metadata.st_uid != geteuid()) {
        if (error != nullptr) {
            *error = "credential directory has unsafe ownership";
        }
        return false;
    }
#endif
    return true;
}

#if defined(_WIN32)

bool Protect(const std::string& plaintext, std::vector<unsigned char>* protected_bytes,
             std::string* error) {
    if (plaintext.size() > std::numeric_limits<DWORD>::max()) {
        if (error != nullptr) {
            *error = "credential document is too large";
        }
        return false;
    }
    DATA_BLOB input{static_cast<DWORD>(plaintext.size()),
                    reinterpret_cast<BYTE*>(const_cast<char*>(plaintext.data()))};
    DATA_BLOB output{};
    if (!CryptProtectData(&input, L"RunAnywhere Wally cloud session", nullptr, nullptr, nullptr,
                          CRYPTPROTECT_UI_FORBIDDEN, &output)) {
        if (error != nullptr) {
            *error = "Windows could not protect the cloud session";
        }
        return false;
    }
    protected_bytes->assign(output.pbData, output.pbData + output.cbData);
    LocalFree(output.pbData);
    return true;
}

bool Unprotect(const std::vector<unsigned char>& protected_bytes, std::string* plaintext,
               std::string* error) {
    if (protected_bytes.size() > std::numeric_limits<DWORD>::max()) {
        if (error != nullptr) {
            *error = "credential document is too large";
        }
        return false;
    }
    DATA_BLOB input{static_cast<DWORD>(protected_bytes.size()),
                    const_cast<BYTE*>(protected_bytes.data())};
    DATA_BLOB output{};
    if (!CryptUnprotectData(&input, nullptr, nullptr, nullptr, nullptr, CRYPTPROTECT_UI_FORBIDDEN,
                            &output)) {
        if (error != nullptr) {
            *error = "Windows could not unlock the cloud session";
        }
        return false;
    }
    plaintext->assign(reinterpret_cast<const char*>(output.pbData), output.cbData);
    SecureZeroMemory(output.pbData, output.cbData);
    LocalFree(output.pbData);
    return true;
}

bool ReadDocument(const std::string& path, std::string* document, bool* exists,
                  std::string* error) {
    std::error_code size_error;
    const std::uintmax_t size = fs::file_size(path, size_error);
    if (!size_error && size > kMaximumCredentialBytes) {
        *exists = true;
        if (error != nullptr) {
            *error = "protected cloud session exceeds the safety limit";
        }
        return false;
    }
    std::ifstream file(path, std::ios::binary);
    if (!file) {
        std::error_code code;
        *exists = fs::exists(path, code);
        if (!*exists) {
            return true;
        }
        if (error != nullptr) {
            *error = "could not read the cloud session";
        }
        return false;
    }
    *exists = true;
    // Named iterators on purpose: passing the two temporaries directly is a most
    // vexing parse, which MSVC resolves as a function declaration and then fails
    // to convert at the call below. Clang and GCC accept the same line, so this
    // only ever broke on Windows.
    std::istreambuf_iterator<char> first(file);
    const std::istreambuf_iterator<char> last;
    const std::vector<unsigned char> protected_bytes(first, last);
    return Unprotect(protected_bytes, document, error);
}

bool WriteDocument(const std::string& path, const std::string& document, std::string* error) {
    std::vector<unsigned char> protected_bytes;
    if (!Protect(document, &protected_bytes, error)) {
        return false;
    }
    const std::string temporary = path + ".tmp." + std::to_string(GetCurrentProcessId()) + "." +
                                  std::to_string(GetTickCount64());
    HANDLE file = CreateFileA(temporary.c_str(), GENERIC_WRITE, 0, nullptr, CREATE_NEW,
                              FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_NOT_CONTENT_INDEXED, nullptr);
    if (file == INVALID_HANDLE_VALUE) {
        if (error != nullptr) {
            *error = "could not create the protected cloud session";
        }
        return false;
    }
    DWORD written = 0;
    const bool wrote =
        WriteFile(file, protected_bytes.data(), static_cast<DWORD>(protected_bytes.size()),
                  &written, nullptr) != 0 &&
        written == protected_bytes.size() && FlushFileBuffers(file) != 0;
    CloseHandle(file);
    if (!wrote || !MoveFileExA(temporary.c_str(), path.c_str(),
                               MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH)) {
        DeleteFileA(temporary.c_str());
        if (error != nullptr) {
            *error = "could not store the protected cloud session";
        }
        return false;
    }
    return true;
}

#else

bool ReadDocument(const std::string& path, std::string* document, bool* exists,
                  std::string* error) {
    int flags = O_RDONLY;
#if defined(O_CLOEXEC)
    flags |= O_CLOEXEC;
#endif
#if defined(O_NOFOLLOW)
    flags |= O_NOFOLLOW;
#endif
    const int fd = ::open(path.c_str(), flags);
    if (fd < 0) {
        if (errno == ENOENT) {
            *exists = false;
            return true;
        }
        if (error != nullptr) {
            *error = "could not securely read the cloud session";
        }
        return false;
    }
    *exists = true;
    struct stat metadata{};
    if (::fstat(fd, &metadata) != 0 || !S_ISREG(metadata.st_mode) || metadata.st_uid != geteuid() ||
        metadata.st_nlink != 1 || metadata.st_size < 0 ||
        static_cast<std::uintmax_t>(metadata.st_size) > kMaximumCredentialBytes) {
        ::close(fd);
        if (error != nullptr) {
            *error = "cloud session has unsafe file metadata";
        }
        return false;
    }
    // A prior version, another tool, or a permissive umask may have left this
    // group- or world-readable; the bearer token inside is as good as a
    // password, so say so once rather than quietly tightening it and leaving
    // the reader to wonder whether it was ever exposed.
    const bool was_exposed = (metadata.st_mode & (S_IRWXG | S_IRWXO)) != 0;
    if (::fchmod(fd, S_IRUSR | S_IWUSR) != 0) {
        ::close(fd);
        if (error != nullptr) {
            *error = "cloud session has unsafe file metadata";
        }
        return false;
    }
    if (was_exposed) {
        std::fprintf(stderr,
                     "warning: %s was readable beyond your user; permissions have been "
                     "restricted to your user only\n",
                     path.c_str());
    }
    std::string body;
    char buffer[4096];
    while (true) {
        const ssize_t count = ::read(fd, buffer, sizeof(buffer));
        if (count == 0) {
            break;
        }
        if (count < 0) {
            if (errno == EINTR) {
                continue;
            }
            ::close(fd);
            if (error != nullptr) {
                *error = "could not read the cloud session";
            }
            return false;
        }
        body.append(buffer, static_cast<std::size_t>(count));
        if (body.size() > kMaximumCredentialBytes) {
            ::close(fd);
            if (error != nullptr) {
                *error = "cloud session exceeds the safety limit";
            }
            return false;
        }
    }
    if (::close(fd) != 0) {
        if (error != nullptr) {
            *error = "could not finish reading the cloud session";
        }
        return false;
    }
    *document = std::move(body);
    return true;
}

bool WriteDocument(const std::string& path, const std::string& document, std::string* error) {
    std::string pattern = path + ".tmp.XXXXXX";
    std::vector<char> temporary(pattern.begin(), pattern.end());
    temporary.push_back('\0');
    const int fd = ::mkstemp(temporary.data());
    if (fd < 0) {
        if (error != nullptr) {
            *error = "could not create a temporary cloud session";
        }
        return false;
    }
    const std::string temporary_path(temporary.data());
    bool ok = ::fchmod(fd, S_IRUSR | S_IWUSR) == 0;
    std::size_t offset = 0;
    while (ok && offset < document.size()) {
        const ssize_t count = ::write(fd, document.data() + offset, document.size() - offset);
        if (count < 0 && errno == EINTR) {
            continue;
        }
        if (count <= 0) {
            ok = false;
            break;
        }
        offset += static_cast<std::size_t>(count);
    }
    ok = ok && ::fsync(fd) == 0;
    ok = ::close(fd) == 0 && ok;
    if (ok) {
        ok = ::rename(temporary_path.c_str(), path.c_str()) == 0;
    }
    if (!ok) {
        ::unlink(temporary_path.c_str());
        if (error != nullptr) {
            *error = "could not atomically store the cloud session";
        }
        return false;
    }
    return true;
}

#endif

}  // namespace

std::string DefaultConsoleUrl() {
    const std::string configured = EnvWithLegacyFallback("WALLY_CONSOLE_URL", "RCLI_CONSOLE_URL");
    if (!configured.empty()) {
        return configured;
    }
    // A dev build carries its control plane compiled in (see
    // baked_endpoints.h.in): the env override above still wins, production
    // builds generate an empty macro and fall through unchanged.
    if (WALLY_BAKED_CONSOLE_API_URL[0] != '\0') {
        return WALLY_BAKED_CONSOLE_API_URL;
    }
    return kProductionConsoleApi;
}

std::vector<std::string> TrustedBrowserOrigins(const std::string& console_url) {
    // An operator who declared one has said which console they trust, and that
    // is then the only one.
    const std::string declared = EnvWithLegacyFallback("WALLY_CONSOLE_WEB_URL", "RCLI_CONSOLE_WEB_URL");
    std::string normalized;
    if (!declared.empty() && NormalizeConsoleUrl(declared, &normalized, nullptr)) {
        return {normalized};
    }
    if (console_url == kProductionConsoleApi) {
        return {std::begin(kProductionConsoleWeb), std::end(kProductionConsoleWeb)};
    }
    // A dev build that baked its control plane also baked which browser
    // console may approve sign-ins for it (a local console on loopback,
    // typically). Trust holds pairwise: the baked web origin is honored only
    // while talking to the baked API, never for an arbitrary --base-url.
    if (WALLY_BAKED_CONSOLE_WEB_ORIGIN[0] != '\0' &&
        console_url == std::string(WALLY_BAKED_CONSOLE_API_URL)) {
        std::string baked_web;
        if (NormalizeConsoleUrl(WALLY_BAKED_CONSOLE_WEB_ORIGIN, &baked_web, nullptr)) {
            return {baked_web, console_url};
        }
    }
    // Anything else — a dev console, a loopback stub — is trusted only at its
    // own origin. That is the rule that held before, and it is the safe answer
    // for a console whose approval page we know nothing about.
    return {console_url};
}

bool BrowserUrlIsTrusted(const std::string& url, const std::vector<std::string>& origins) {
    for (const std::string& origin : origins) {
        if (BrowserUrlMatchesConsole(url, origin)) {
            return true;
        }
    }
    return false;
}

bool NormalizeConsoleUrl(const std::string& input, std::string* normalized, std::string* error) {
    ParsedUrl parsed;
    std::string base_path;
    if (!ParseUrl(input, true, &parsed) || !NormalizeBasePath(parsed.suffix, &base_path)) {
        if (error != nullptr) {
            *error =
                "console URL must be an HTTPS origin with an optional path, "
                "or HTTP on exact loopback";
        }
        return false;
    }
    if (normalized != nullptr) {
        *normalized = RenderOrigin(parsed) + base_path;
    }
    return true;
}

bool BrowserUrlIsSafe(const std::string& url) {
    ParsedUrl parsed;
    return ParseUrl(url, true, &parsed);
}

bool BrowserUrlMatchesConsole(const std::string& url, const std::string& console_url) {
    ParsedUrl browser;
    ParsedUrl console;
    // Origins, not full URLs: a console's base path says where its API lives,
    // and says nothing about which pages on that host may approve a sign-in.
    return ParseUrl(url, true, &browser) && ParseUrl(console_url, true, &console) &&
           RenderOrigin(browser) == RenderOrigin(console);
}

bool SessionTokenIsSafe(const std::string& token) {
    return !token.empty() && token.size() <= 8192 &&
           std::all_of(token.begin(), token.end(), [](unsigned char c) {
               const bool ascii_alphanumeric =
                   (c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z') || (c >= '0' && c <= '9');
               return ascii_alphanumeric || c == '-' || c == '.' || c == '_' || c == '~' ||
                      c == '+' || c == '/' || c == '=';
           });
}

std::string ProfileDirectory() {
    const std::string override_dir = EnvWithLegacyFallback("WALLY_PROFILE_DIR", "RCLI_PROFILE_DIR");
    if (!override_dir.empty()) {
        return override_dir;
    }
#if defined(_WIN32)
    const std::string home = HomeDirectory();
    return home.empty() ? std::string() : home + "/RunAnywhere/Wally";
#else
    const std::string xdg = Env("XDG_CONFIG_HOME");
    if (!xdg.empty()) {
        return xdg + "/wally";
    }
    const std::string home = HomeDirectory();
    return home.empty() ? std::string() : home + "/.config/wally";
#endif
}

std::string CredentialsPath() {
    const std::string directory = ProfileDirectory();
    return directory.empty() ? std::string() : (fs::path(directory) / kFileName).string();
}

bool Load(Credentials* out, std::string* error) {
    if (out == nullptr) {
        if (error != nullptr) {
            *error = "internal credential load error";
        }
        return false;
    }
    Credentials credentials;
    std::string normalized;
    if (!NormalizeConsoleUrl(DefaultConsoleUrl(), &normalized, error)) {
        return false;
    }
    credentials.console_url = normalized;

    const std::string path = CredentialsPath();
    if (path.empty()) {
        if (error != nullptr) {
            *error = "no user profile directory is available";
        }
        return false;
    }
    if (!EnsureSecureDirectory(ProfileDirectory(), error)) {
        return false;
    }
    bool exists = false;
    std::string document;
    if (!ReadDocument(path, &document, &exists, error)) {
        return false;
    }
    if (!exists) {
        *out = std::move(credentials);
        return true;
    }
    try {
        const Json object = Json::parse(document);
        if (!object.is_object()) {
            if (error != nullptr) {
                *error = "cloud session file is not a JSON object";
            }
            return false;
        }
        // A missing or empty console_url is not a malformed one: `credentials`
        // already carries the DefaultConsoleUrl()-derived origin set above, so
        // only a value the document actually specifies gets validated. Without
        // this, a hand-edited or partially-written file that simply omits the
        // field fails with "console URL must be HTTPS" instead of falling back.
        const std::string stored_url = object.value("console_url", std::string());
        if (!stored_url.empty() && !NormalizeConsoleUrl(stored_url, &credentials.console_url, error)) {
            return false;
        }
        credentials.email = object.value("email", std::string());
        const std::string access_token = object.value("access_token", std::string());
        const std::string refresh_token = object.value("refresh_token", std::string());
        if ((!access_token.empty() && !SessionTokenIsSafe(access_token)) ||
            (!refresh_token.empty() && !SessionTokenIsSafe(refresh_token))) {
            if (error != nullptr) {
                *error = "cloud session contains an invalid token encoding";
            }
            return false;
        }
        credentials.access_token = access_token;
        credentials.refresh_token = refresh_token;
        credentials.expires_at = object.value("expires_at", 0LL);
    } catch (const Json::exception&) {
        if (error != nullptr) {
            *error = "cloud session file is not valid JSON";
        }
        return false;
    }
    *out = std::move(credentials);
    return true;
}

Credentials Load() {
    Credentials credentials;
    std::string error;
    if (!Load(&credentials, &error)) {
        return {};
    }
    return credentials;
}

bool Save(const Credentials& credentials, std::string* error) {
    std::string normalized;
    if (!NormalizeConsoleUrl(credentials.console_url, &normalized, error)) {
        return false;
    }
    const std::string directory = ProfileDirectory();
    const std::string path = CredentialsPath();
    if (directory.empty() || path.empty()) {
        if (error != nullptr) {
            *error = "no user profile directory is available";
        }
        return false;
    }
    if (!EnsureSecureDirectory(directory, error)) {
        return false;
    }
    if ((!credentials.access_token.empty() && !SessionTokenIsSafe(credentials.access_token)) ||
        (!credentials.refresh_token.empty() && !SessionTokenIsSafe(credentials.refresh_token))) {
        if (error != nullptr) {
            *error = "refusing to store an invalid cloud session token";
        }
        return false;
    }
    const Json document = {{"console_url", normalized},
                           {"email", credentials.email},
                           {"access_token", credentials.access_token},
                           {"refresh_token", credentials.refresh_token},
                           {"expires_at", credentials.expires_at}};
    return WriteDocument(path, document.dump(2) + "\n", error);
}

bool Clear(std::string* error) {
    const std::string path = CredentialsPath();
    if (path.empty()) {
        return true;
    }
    if (!EnsureSecureDirectory(ProfileDirectory(), error)) {
        return false;
    }
    std::error_code code;
    fs::remove(path, code);
    if (code) {
        if (error != nullptr) {
            *error = "could not remove the local cloud session";
        }
        return false;
    }
    return true;
}

}  // namespace wally::account

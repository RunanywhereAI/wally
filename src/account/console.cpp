#include "account/console.h"

#include <algorithm>
#include <array>
#include <cctype>
#include <charconv>
#include <chrono>
#include <cstdint>
#include <limits>
#include <map>
#include <nlohmann/json.hpp>
#include <string>
#include <utility>

#if defined(_WIN32)
#include <windows.h>
#include <winhttp.h>
#else
#include <curl/curl.h>
#include <mutex>
#endif

#include "account/console_contract.h"
#include "account/credentials.h"

namespace wally::account {
namespace {

using Json = nlohmann::json;
constexpr std::size_t kMaximumResponseBytes = 1024 * 1024;

// Both transports share the same defaults: 10 s to connect, 30 s in all. A
// request's own `timeout_ms` bounds the whole call instead; the connect phase
// never gets more than its usual share of it.
constexpr int kConnectTimeoutMs = 10000;
constexpr int kTotalTimeoutMs = 30000;

int TotalTimeoutMs(const HttpRequest& input) {
    return input.timeout_ms > 0 ? input.timeout_ms : kTotalTimeoutMs;
}
int ConnectTimeoutMs(const HttpRequest& input) {
    return std::min(kConnectTimeoutMs, TotalTimeoutMs(input));
}

#if defined(_WIN32)

class WinHttpHandle {
   public:
    explicit WinHttpHandle(HINTERNET value = nullptr) : value_(value) {}
    ~WinHttpHandle() {
        if (value_ != nullptr) {
            WinHttpCloseHandle(value_);
        }
    }

    WinHttpHandle(const WinHttpHandle&) = delete;
    WinHttpHandle& operator=(const WinHttpHandle&) = delete;

    HINTERNET get() const { return value_; }
    explicit operator bool() const { return value_ != nullptr; }

   private:
    HINTERNET value_;
};

bool Utf8ToWide(const std::string& input, std::wstring* output) {
    if (output == nullptr ||
        input.size() > static_cast<std::size_t>(std::numeric_limits<int>::max())) {
        return false;
    }
    if (input.empty()) {
        output->clear();
        return true;
    }
    const int input_size = static_cast<int>(input.size());
    const int output_size =
        MultiByteToWideChar(CP_UTF8, MB_ERR_INVALID_CHARS, input.data(), input_size, nullptr, 0);
    if (output_size <= 0) {
        return false;
    }
    output->resize(static_cast<std::size_t>(output_size));
    return MultiByteToWideChar(CP_UTF8, MB_ERR_INVALID_CHARS, input.data(), input_size,
                               output->data(), output_size) == output_size;
}

using Deadline = std::chrono::steady_clock::time_point;

int RemainingTimeout(const Deadline& deadline) {
    const auto remaining = std::chrono::duration_cast<std::chrono::milliseconds>(
                               deadline - std::chrono::steady_clock::now())
                               .count();
    if (remaining <= 0) {
        return 0;
    }
    return static_cast<int>(std::min<std::int64_t>(remaining, std::numeric_limits<int>::max()));
}

bool SetReceiveTimeout(HINTERNET request, const Deadline& deadline, bool include_headers) {
    const int remaining = RemainingTimeout(deadline);
    if (remaining == 0) {
        return false;
    }
    DWORD timeout = static_cast<DWORD>(remaining);
    if (include_headers && !WinHttpSetOption(request, WINHTTP_OPTION_RECEIVE_RESPONSE_TIMEOUT,
                                             &timeout, sizeof(timeout))) {
        return false;
    }
    return WinHttpSetOption(request, WINHTTP_OPTION_RECEIVE_TIMEOUT, &timeout, sizeof(timeout)) !=
           FALSE;
}

bool WinHttpTransport(const HttpRequest& input, HttpResponse* output, std::string* error) {
    std::wstring url;
    std::wstring method;
    if (!Utf8ToWide(input.url, &url) || !Utf8ToWide(input.method, &method) ||
        url.size() > std::numeric_limits<DWORD>::max()) {
        if (error != nullptr) {
            *error = "could not create the console HTTP request";
        }
        return false;
    }

    URL_COMPONENTS components{};
    components.dwStructSize = sizeof(components);
    components.dwSchemeLength = static_cast<DWORD>(-1);
    components.dwHostNameLength = static_cast<DWORD>(-1);
    components.dwUrlPathLength = static_cast<DWORD>(-1);
    components.dwExtraInfoLength = static_cast<DWORD>(-1);
    if (!WinHttpCrackUrl(url.c_str(), static_cast<DWORD>(url.size()), ICU_REJECT_USERPWD,
                         &components) ||
        (components.nScheme != INTERNET_SCHEME_HTTP &&
         components.nScheme != INTERNET_SCHEME_HTTPS)) {
        if (error != nullptr) {
            *error = "could not create the console HTTP request";
        }
        return false;
    }

    const std::wstring host(components.lpszHostName, components.dwHostNameLength);
    std::wstring path;
    if (components.dwUrlPathLength != 0) {
        path.assign(components.lpszUrlPath, components.dwUrlPathLength);
    }
    if (components.dwExtraInfoLength != 0) {
        path.append(components.lpszExtraInfo, components.dwExtraInfoLength);
    }
    if (path.empty()) {
        path = L"/";
    }
    const bool loopback =
        _wcsicmp(host.c_str(), L"localhost") == 0 || host == L"127.0.0.1" || host == L"::1";
    const DWORD access_type =
        loopback ? WINHTTP_ACCESS_TYPE_NO_PROXY : WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY;
    const WinHttpHandle session(WinHttpOpen(L"wally-cloud-auth/1", access_type,
                                            WINHTTP_NO_PROXY_NAME, WINHTTP_NO_PROXY_BYPASS, 0));
    const int total_timeout = TotalTimeoutMs(input);
    const int connect_timeout = ConnectTimeoutMs(input);
    if (!session || !WinHttpSetTimeouts(session.get(), connect_timeout, connect_timeout,
                                        total_timeout, total_timeout)) {
        if (error != nullptr) {
            *error = "could not initialize the console HTTP client";
        }
        return false;
    }

    const Deadline deadline =
        std::chrono::steady_clock::now() + std::chrono::milliseconds(total_timeout);
    const WinHttpHandle connection(
        WinHttpConnect(session.get(), host.c_str(), components.nPort, 0));
    const DWORD flags = components.nScheme == INTERNET_SCHEME_HTTPS ? WINHTTP_FLAG_SECURE : 0;
    const WinHttpHandle request(
        connection ? WinHttpOpenRequest(connection.get(), method.c_str(), path.c_str(), nullptr,
                                        WINHTTP_NO_REFERER, WINHTTP_DEFAULT_ACCEPT_TYPES, flags)
                   : nullptr);
    if (!connection || !request) {
        if (error != nullptr) {
            *error = "could not create the console HTTP client";
        }
        return false;
    }

    const int initial_timeout = RemainingTimeout(deadline);
    DWORD redirect_policy = WINHTTP_OPTION_REDIRECT_POLICY_NEVER;
    if (initial_timeout == 0 ||
        !WinHttpSetTimeouts(request.get(), std::min(connect_timeout, initial_timeout),
                            std::min(connect_timeout, initial_timeout), initial_timeout,
                            initial_timeout) ||
        !WinHttpSetOption(request.get(), WINHTTP_OPTION_REDIRECT_POLICY, &redirect_policy,
                          sizeof(redirect_policy))) {
        if (error != nullptr) {
            *error = "could not configure the console HTTP client";
        }
        return false;
    }
    // Do not set WINHTTP_OPTION_SECURITY_FLAGS: WinHTTP's default verifies the
    // server certificate chain and hostname for WINHTTP_FLAG_SECURE requests.

    std::wstring headers = L"Accept: application/json\r\nContent-Type: application/json\r\n";
    if (!input.bearer_token.empty()) {
        std::wstring token;
        if (!Utf8ToWide(input.bearer_token, &token)) {
            if (error != nullptr) {
                *error = "could not create the console HTTP request";
            }
            return false;
        }
        headers += L"Authorization: Bearer " + token + L"\r\n";
    }
    if (headers.size() > std::numeric_limits<DWORD>::max() ||
        input.body.size() > std::numeric_limits<DWORD>::max()) {
        if (error != nullptr) {
            *error = "could not create the console HTTP request";
        }
        return false;
    }

    void* body =
        input.body.empty() ? WINHTTP_NO_REQUEST_DATA : const_cast<char*>(input.body.data());
    const DWORD body_size = static_cast<DWORD>(input.body.size());
    if (!WinHttpSendRequest(request.get(), headers.c_str(), static_cast<DWORD>(headers.size()),
                            body, body_size, body_size, 0) ||
        !SetReceiveTimeout(request.get(), deadline, true) ||
        !WinHttpReceiveResponse(request.get(), nullptr)) {
        if (error != nullptr) {
            *error = "could not reach the RunAnywhere console";
        }
        return false;
    }

    DWORD status = 0;
    DWORD status_size = sizeof(status);
    if (!WinHttpQueryHeaders(request.get(), WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
                             WINHTTP_HEADER_NAME_BY_INDEX, &status, &status_size,
                             WINHTTP_NO_HEADER_INDEX) ||
        status < 100 || status > 599) {
        if (error != nullptr) {
            *error = "could not reach the RunAnywhere console";
        }
        return false;
    }

    output->status = 0;
    output->body.clear();
    output->headers.clear();
    // Only Retry-After is read back on this path: it is the one response header
    // a caller acts on (a 429 with a wait hint), and querying a named header is
    // cheaper than enumerating them all.
    {
        wchar_t retry_after_name[] = L"Retry-After";
        DWORD retry_size = 0;
        WinHttpQueryHeaders(request.get(), WINHTTP_QUERY_CUSTOM, retry_after_name,
                            WINHTTP_NO_OUTPUT_BUFFER, &retry_size, WINHTTP_NO_HEADER_INDEX);
        if (retry_size > 0 && retry_size < 256) {
            std::wstring wide(retry_size / sizeof(wchar_t), L'\0');
            if (WinHttpQueryHeaders(request.get(), WINHTTP_QUERY_CUSTOM, retry_after_name,
                                    wide.data(), &retry_size, WINHTTP_NO_HEADER_INDEX)) {
                std::string value;
                for (const wchar_t wc : wide) {
                    if (wc != L'\0' && wc < 128) {
                        value.push_back(static_cast<char>(wc));
                    }
                }
                if (!value.empty()) {
                    output->headers["retry-after"] = value;
                }
            }
        }
    }
    std::array<char, 8192> buffer{};
    while (true) {
        DWORD received = 0;
        if (!SetReceiveTimeout(request.get(), deadline, false) ||
            !WinHttpReadData(request.get(), buffer.data(), static_cast<DWORD>(buffer.size()),
                             &received)) {
            output->body.clear();
            if (error != nullptr) {
                *error = "could not reach the RunAnywhere console";
            }
            return false;
        }
        if (received == 0) {
            break;
        }
        if (output->body.size() > kMaximumResponseBytes ||
            received > kMaximumResponseBytes - output->body.size()) {
            output->body.clear();
            if (error != nullptr) {
                *error = "console response exceeded the safety limit";
            }
            return false;
        }
        output->body.append(buffer.data(), received);
    }
    output->status = static_cast<int>(status);
    return true;
}

#else

struct ResponseBuffer {
    std::string* body = nullptr;
    bool too_large = false;
};

std::size_t CollectBody(char* bytes, std::size_t size, std::size_t count, void* userdata) {
    auto* buffer = static_cast<ResponseBuffer*>(userdata);
    if (buffer == nullptr || buffer->body == nullptr ||
        (count != 0 && size > std::numeric_limits<std::size_t>::max() / count)) {
        return 0;
    }
    const std::size_t length = size * count;
    if (buffer->body->size() > kMaximumResponseBytes ||
        length > kMaximumResponseBytes - buffer->body->size()) {
        buffer->too_large = true;
        return 0;
    }
    buffer->body->append(bytes, length);
    return length;
}

// Collects one header line into the response map, keyed by a lowercased name.
// curl hands the status line and a trailing blank line here too; both lack a
// colon and are skipped.
std::size_t CollectHeader(char* bytes, std::size_t size, std::size_t count, void* userdata) {
    auto* headers = static_cast<std::map<std::string, std::string>*>(userdata);
    const std::size_t length = size * count;
    if (headers == nullptr || count != 0 && size > std::numeric_limits<std::size_t>::max() / count) {
        return 0;
    }
    const std::string line(bytes, length);
    const std::size_t colon = line.find(':');
    if (colon == std::string::npos) {
        return length;
    }
    std::string name = line.substr(0, colon);
    std::transform(name.begin(), name.end(), name.begin(),
                   [](unsigned char c) { return static_cast<char>(std::tolower(c)); });
    std::string value = line.substr(colon + 1);
    const auto trim = [](std::string& s) {
        const auto not_space = [](unsigned char c) { return std::isspace(c) == 0; };
        s.erase(s.begin(), std::find_if(s.begin(), s.end(), not_space));
        s.erase(std::find_if(s.rbegin(), s.rend(), not_space).base(), s.end());
    };
    trim(name);
    trim(value);
    if (!name.empty() && headers->size() < 100) {
        (*headers)[name] = value;
    }
    return length;
}

#endif

bool DefaultTransport(const HttpRequest& input, HttpResponse* output, std::string* error) {
    if (output == nullptr) {
        if (error != nullptr) {
            *error = "internal console transport error";
        }
        return false;
    }
    if (!BrowserUrlIsSafe(input.url)) {
        if (error != nullptr) {
            *error = "refusing an unsafe console request URL";
        }
        return false;
    }
    if (!input.bearer_token.empty() && !SessionTokenIsSafe(input.bearer_token)) {
        if (error != nullptr) {
            *error = "refusing an invalid console bearer token";
        }
        return false;
    }

#if defined(_WIN32)
    return WinHttpTransport(input, output, error);
#else
    static std::once_flag curl_once;
    static CURLcode curl_init_result = CURLE_FAILED_INIT;
    std::call_once(curl_once, [] { curl_init_result = curl_global_init(CURL_GLOBAL_DEFAULT); });
    if (curl_init_result != CURLE_OK) {
        if (error != nullptr) {
            *error = "could not initialize the console HTTP client";
        }
        return false;
    }

    CURL* request = curl_easy_init();
    if (request == nullptr) {
        if (error != nullptr) {
            *error = "could not create the console HTTP client";
        }
        return false;
    }

    curl_slist* headers = nullptr;
    const auto add_header = [&headers](const std::string& value) {
        curl_slist* updated = curl_slist_append(headers, value.c_str());
        if (updated == nullptr) {
            return false;
        }
        headers = updated;
        return true;
    };
    bool configured =
        add_header("Accept: application/json") && add_header("Content-Type: application/json");
    const std::string authorization = "Authorization: Bearer " + input.bearer_token;
    if (configured && !input.bearer_token.empty()) {
        configured = add_header(authorization);
    }

    output->status = 0;
    output->body.clear();
    output->headers.clear();
    ResponseBuffer response{&output->body, false};
    configured =
        configured && curl_easy_setopt(request, CURLOPT_URL, input.url.c_str()) == CURLE_OK &&
        curl_easy_setopt(request, CURLOPT_CUSTOMREQUEST, input.method.c_str()) == CURLE_OK &&
        curl_easy_setopt(request, CURLOPT_HTTPHEADER, headers) == CURLE_OK &&
        curl_easy_setopt(request, CURLOPT_CONNECTTIMEOUT_MS,
                         static_cast<long>(ConnectTimeoutMs(input))) == CURLE_OK &&
        curl_easy_setopt(request, CURLOPT_TIMEOUT_MS,
                         static_cast<long>(TotalTimeoutMs(input))) == CURLE_OK &&
        curl_easy_setopt(request, CURLOPT_NOSIGNAL, 1L) == CURLE_OK &&
        curl_easy_setopt(request, CURLOPT_NOPROXY, "localhost,127.0.0.1,::1") == CURLE_OK &&
        curl_easy_setopt(request, CURLOPT_FOLLOWLOCATION, 0L) == CURLE_OK &&
        curl_easy_setopt(request, CURLOPT_SSL_VERIFYPEER, 1L) == CURLE_OK &&
        curl_easy_setopt(request, CURLOPT_SSL_VERIFYHOST, 2L) == CURLE_OK &&
        curl_easy_setopt(request, CURLOPT_USERAGENT, "wally-cloud-auth/1") == CURLE_OK &&
        curl_easy_setopt(request, CURLOPT_WRITEFUNCTION, CollectBody) == CURLE_OK &&
        curl_easy_setopt(request, CURLOPT_WRITEDATA, &response) == CURLE_OK &&
        curl_easy_setopt(request, CURLOPT_HEADERFUNCTION, CollectHeader) == CURLE_OK &&
        curl_easy_setopt(request, CURLOPT_HEADERDATA, &output->headers) == CURLE_OK;
    if (configured && !input.body.empty()) {
        configured =
            input.body.size() <= static_cast<std::size_t>(std::numeric_limits<long>::max()) &&
            curl_easy_setopt(request, CURLOPT_POSTFIELDS, input.body.data()) == CURLE_OK &&
            curl_easy_setopt(request, CURLOPT_POSTFIELDSIZE,
                             static_cast<long>(input.body.size())) == CURLE_OK;
    }

    const CURLcode result = configured ? curl_easy_perform(request) : CURLE_FAILED_INIT;
    long status = 0;
    const bool received = result == CURLE_OK &&
                          curl_easy_getinfo(request, CURLINFO_RESPONSE_CODE, &status) == CURLE_OK;
    curl_slist_free_all(headers);
    curl_easy_cleanup(request);
    if (!received || response.too_large || status < 100 || status > 599) {
        output->status = 0;
        output->body.clear();
        output->headers.clear();
        if (error != nullptr) {
            // Names the origin actually contacted: with WALLY_CONSOLE_URL unset
            // that is the production console, and a bare "could not reach the
            // console" reads as a local dev server nobody pointed us at.
            *error = response.too_large ? "console response exceeded the safety limit"
                                        : "could not reach the RunAnywhere console at " + input.url;
        }
        return false;
    }
    output->status = static_cast<int>(status);
    return true;
#endif
}

void HttpError(const char* operation, const std::string& origin, const HttpResponse& response,
               std::string* error) {
    if (error == nullptr) {
        return;
    }
    // Names which console answered: with WALLY_CONSOLE_URL unset that is
    // production, and a bare "failed with HTTP 404" reads as a bug rather
    // than as the wrong console having been asked.
    *error = std::string("console ") + operation + " (" + origin + ") failed with HTTP " +
             std::to_string(response.status);
    // A 429 means overload, not a broken request. Tell the person how long the
    // server asked them to wait, so "try again" is actionable rather than a
    // guess.
    if (response.status == 429) {
        const int wait = response.retry_after_seconds();
        *error += wait >= 0 ? "; the console is rate limiting, retry after " + std::to_string(wait) +
                                  "s"
                            : "; the console is rate limiting, wait a moment and retry";
    }
}

bool ParseObject(const HttpResponse& response, Json* object, std::string* error) {
    if (object == nullptr) {
        if (error != nullptr) {
            *error = "internal console response error";
        }
        return false;
    }
    try {
        Json parsed = Json::parse(response.body);
        if (!parsed.is_object()) {
            if (error != nullptr) {
                *error = "console returned a JSON value instead of an object";
            }
            return false;
        }
        *object = std::move(parsed);
        return true;
    } catch (const Json::exception&) {
        // Do not include the response body: an upstream error can echo a token.
        if (error != nullptr) {
            *error = "console returned malformed JSON";
        }
        return false;
    }
}

// Parse a response body straight into its generated contract type. The typed
// `get<T>` throws when a required field is missing or the wrong shape or an enum
// value is unknown, so a response that does not match the pinned contract is a
// clean failure here rather than a wrong value read field by name downstream.
template <typename T>
bool ParseContract(const HttpResponse& response, T* value, std::string* error) {
    Json object;
    if (!ParseObject(response, &object, error)) {
        return false;
    }
    try {
        *value = object.get<T>();
        return true;
    } catch (const Json::exception&) {
        // Never the body: it can carry a token on an error path.
        if (error != nullptr) {
            *error = "console returned a response that did not match the contract";
        }
        return false;
    }
}

bool Send(const Transport& transport, HttpRequest request, HttpResponse* response,
          std::string* error) {
    if (!transport(request, response, error)) {
        if (error != nullptr && error->empty()) {
            // Names the origin actually contacted: with WALLY_CONSOLE_URL unset
            // that is the production console, and a plain "could not reach the
            // console" reads as a local server that was never told about.
            *error = "could not reach the RunAnywhere console at " + request.url;
        }
        return false;
    }
    return true;
}

bool DisplayTextIsSafe(const std::string& value, std::size_t maximum) {
    return !value.empty() && value.size() <= maximum &&
           std::all_of(value.begin(), value.end(),
                       [](unsigned char c) { return c >= 0x20 && c <= 0x7e; });
}

bool RequestCodeIsSafe(const std::string& value) {
    // The control plane mints these with Python's `token_urlsafe`, whose
    // alphabet is base64url: letters, digits, `-` and `_`. Omitting `_`
    // rejected roughly half of all real codes as malformed.
    return value.size() >= 4 && value.size() <= 64 &&
           std::all_of(value.begin(), value.end(), [](unsigned char c) {
               return (c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z') || (c >= '0' && c <= '9') ||
                      c == '-' || c == '_';
           });
}

// The plan is a closed enum in the contract, so its only text is a known-safe
// literal; render it back for the domain Grant, which carries a plain string.
std::string PlanText(const std::optional<contract::CliPlan>& plan) {
    if (!plan.has_value()) {
        return std::string();
    }
    return Json(*plan).get<std::string>();
}

// Map the wire grant fields onto the domain Grant, still sanitizing every
// string that reaches the terminal or the credential file. The contract typing
// removes the field-name guesswork; the safety checks stay.
bool MapGrant(const std::optional<std::string>& access_token,
              const std::optional<std::string>& refresh_token,
              const std::optional<std::string>& email,
              const std::optional<contract::CliPlan>& plan, std::int64_t expires_in, Grant* grant,
              std::string* error) {
    grant->access_token = access_token.value_or("");
    grant->refresh_token = refresh_token.value_or("");
    grant->email = email.value_or("");
    grant->plan = PlanText(plan);
    grant->expires_in = std::max<std::int64_t>(0, expires_in);
    if ((!grant->access_token.empty() && !SessionTokenIsSafe(grant->access_token)) ||
        (!grant->refresh_token.empty() && !SessionTokenIsSafe(grant->refresh_token)) ||
        (!grant->email.empty() && !DisplayTextIsSafe(grant->email, 320))) {
        if (error != nullptr) {
            *error = "console returned an invalid cloud session";
        }
        return false;
    }
    return true;
}

/// Percent-encode a query value. Model names carry dots and slashes, and a
/// filter is user input either way.
std::string QueryEscape(const std::string& value) {
    static const char* kHex = "0123456789ABCDEF";
    std::string out;
    for (const unsigned char c : value) {
        const bool unreserved = (c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z') ||
                                (c >= '0' && c <= '9') || c == '-' || c == '.' || c == '_' ||
                                c == '~';
        if (unreserved) {
            out += static_cast<char>(c);
        } else {
            out += '%';
            out += kHex[c >> 4];
            out += kHex[c & 0x0f];
        }
    }
    return out;
}

bool ConsoleOrigin(const std::string& input, std::string* origin, std::string* error) {
    return NormalizeConsoleUrl(input, origin, error);
}

}  // namespace

int HttpResponse::retry_after_seconds() const {
    const auto it = headers.find("retry-after");
    if (it == headers.end()) {
        return -1;
    }
    const std::string& value = it->second;
    if (value.empty() ||
        !std::all_of(value.begin(), value.end(), [](unsigned char c) { return std::isdigit(c); })) {
        return -1;
    }
    int seconds = 0;
    const auto result = std::from_chars(value.data(), value.data() + value.size(), seconds);
    if (result.ec != std::errc{} || result.ptr != value.data() + value.size()) {
        return -1;
    }
    // A day is the ceiling: anything larger is a misconfiguration, and honoring
    // it would hang a terminal for hours.
    return seconds > 86400 ? 86400 : seconds;
}

ConsoleClient::ConsoleClient(Transport transport)
    : transport_(transport ? std::move(transport) : Transport(DefaultTransport)) {}

bool ConsoleClient::BeginAuthorization(const std::string& console_url, const std::string& hostname,
                                       Authorization* authorization, std::string* error) const {
    if (authorization == nullptr) {
        if (error != nullptr) {
            *error = "internal authorization error";
        }
        return false;
    }
    std::string origin;
    if (!ConsoleOrigin(console_url, &origin, error)) {
        return false;
    }
    // The client value is the contract enum, whose only member serializes to
    // "rcli" -- InferenceInfra's CliClient StrEnum recognizes exactly that, and
    // sending anything else 422s /auth/cli/start. Renaming the wire value needs
    // a coordinated InferenceInfra change (add "wally" to the enum, re-vendor
    // this contract), which is why it is pinned here rather than free text.
    contract::CliStartRequest request;
    request.client = contract::CliClient::kRcli;
    request.hostname = hostname;
    HttpResponse response;
    if (!Send(transport_, {"POST", origin + "/auth/cli/start", Json(request).dump(), {}}, &response,
              error)) {
        return false;
    }
    if (response.status != 200) {
        HttpError("authorization", origin, response, error);
        return false;
    }

    contract::CliStartResponse parsed;
    if (!ParseContract(response, &parsed, error)) {
        return false;
    }
    authorization->request_code = parsed.request_code;
    authorization->poll_secret = parsed.poll_secret;
    authorization->verification_url = parsed.verification_url;
    if (!RequestCodeIsSafe(authorization->request_code) ||
        !SessionTokenIsSafe(authorization->poll_secret)) {
        if (error != nullptr) {
            *error = "console returned an invalid authorization request";
        }
        return false;
    }
    authorization->expires_in =
        static_cast<int>(std::clamp<std::int64_t>(parsed.expires_in, 30, 1800));
    authorization->interval = static_cast<int>(std::clamp<std::int64_t>(parsed.interval, 1, 30));
    return true;
}

PollResult ConsoleClient::Poll(const std::string& console_url, const Authorization& authorization,
                               Grant* grant, std::string* error) const {
    if (grant == nullptr) {
        if (error != nullptr) {
            *error = "internal authorization grant error";
        }
        return PollResult::Failed;
    }
    std::string origin;
    if (!ConsoleOrigin(console_url, &origin, error)) {
        return PollResult::Failed;
    }
    contract::CliPollRequest request;
    request.request_code = authorization.request_code;
    request.poll_secret = authorization.poll_secret;
    HttpResponse response;
    if (!Send(transport_, {"POST", origin + "/auth/cli/poll", Json(request).dump(), {}}, &response,
              error)) {
        return PollResult::Failed;
    }
    if (response.status != 200) {
        HttpError("poll", origin, response, error);
        return PollResult::Failed;
    }

    contract::PollResponse parsed;
    if (!ParseContract(response, &parsed, error)) {
        return PollResult::Failed;
    }
    switch (parsed.status) {
        case contract::PollStatus::kPending:
            return PollResult::Pending;
        case contract::PollStatus::kDenied:
            return PollResult::Denied;
        case contract::PollStatus::kExpired:
            return PollResult::Expired;
        case contract::PollStatus::kApproved:
            break;
    }

    if (!MapGrant(parsed.access_token, parsed.refresh_token, parsed.email, parsed.plan,
                  parsed.expires_in.value_or(0), grant, error)) {
        return PollResult::Failed;
    }
    if (grant->access_token.empty() || grant->refresh_token.empty()) {
        if (error != nullptr) {
            *error = "console approved the request without a complete session";
        }
        return PollResult::Failed;
    }
    return PollResult::Approved;
}

bool ConsoleClient::Refresh(const std::string& console_url, const std::string& refresh_token,
                            Grant* grant, std::string* error) const {
    if (grant == nullptr || !SessionTokenIsSafe(refresh_token)) {
        if (error != nullptr) {
            *error = "no refresh token is available";
        }
        return false;
    }
    std::string origin;
    if (!ConsoleOrigin(console_url, &origin, error)) {
        return false;
    }
    contract::CliRefreshRequest request;
    request.refresh_token = refresh_token;
    HttpResponse response;
    if (!Send(transport_, {"POST", origin + "/auth/cli/refresh", Json(request).dump(), {}},
              &response, error)) {
        return false;
    }
    if (response.status != 200) {
        HttpError("refresh", origin, response, error);
        return false;
    }
    contract::GrantResponse parsed;
    if (!ParseContract(response, &parsed, error)) {
        return false;
    }
    if (!MapGrant(parsed.access_token, parsed.refresh_token, parsed.email, parsed.plan,
                  parsed.expires_in, grant, error)) {
        return false;
    }
    if (grant->access_token.empty()) {
        if (error != nullptr) {
            *error = "console refreshed the session without an access token";
        }
        return false;
    }
    return true;
}

IdentityResult ConsoleClient::WhoAmI(const std::string& console_url,
                                     const std::string& access_token, Identity* identity,
                                     std::string* error) const {
    if (identity == nullptr || !SessionTokenIsSafe(access_token)) {
        if (error != nullptr) {
            *error = "no access token is available";
        }
        return IdentityResult::Failed;
    }
    std::string origin;
    if (!ConsoleOrigin(console_url, &origin, error)) {
        return IdentityResult::Failed;
    }
    HttpResponse response;
    if (!Send(transport_, {"GET", origin + "/v1/me", {}, access_token}, &response, error)) {
        return IdentityResult::Failed;
    }
    if (response.status == 401) {
        if (error != nullptr) {
            *error = "console session expired";
        }
        return IdentityResult::Unauthorized;
    }
    if (response.status != 200) {
        HttpError("identity request", origin, response, error);
        return IdentityResult::Failed;
    }

    contract::IdentityResponse parsed;
    if (!ParseContract(response, &parsed, error)) {
        return IdentityResult::Failed;
    }
    identity->email = parsed.email;
    if (!DisplayTextIsSafe(identity->email, 320)) {
        if (error != nullptr) {
            *error = "console returned an invalid account identity";
        }
        return IdentityResult::Failed;
    }
    identity->plan = PlanText(parsed.plan);
    identity->tokens_this_month = parsed.tokens_this_month;
    identity->monthly_token_limit = parsed.monthly_token_limit;
    if (identity->tokens_this_month < 0 || identity->monthly_token_limit < 0) {
        if (error != nullptr) {
            *error = "console returned invalid account usage";
        }
        return IdentityResult::Failed;
    }
    return IdentityResult::Ok;
}

IdentityResult ConsoleClient::FetchModels(const std::string& console_url,
                                          const std::string& access_token,
                                          std::vector<ModelInfo>* models,
                                          std::string* error) const {
    if (models == nullptr || !SessionTokenIsSafe(access_token)) {
        if (error != nullptr) {
            *error = "no access token is available";
        }
        return IdentityResult::Failed;
    }
    std::string origin;
    if (!ConsoleOrigin(console_url, &origin, error)) {
        return IdentityResult::Failed;
    }
    HttpResponse response;
    if (!Send(transport_, {"GET", origin + "/v1/models", {}, access_token}, &response, error)) {
        return IdentityResult::Failed;
    }
    if (response.status == 401) {
        if (error != nullptr) {
            *error = "console session expired";
        }
        return IdentityResult::Unauthorized;
    }
    if (response.status != 200) {
        HttpError("models request", origin, response, error);
        return IdentityResult::Failed;
    }
    contract::ModelList parsed;
    if (!ParseContract(response, &parsed, error)) {
        return IdentityResult::Failed;
    }
    models->clear();
    models->reserve(parsed.data.size());
    for (const contract::PublicModel& model : parsed.data) {
        ModelInfo info;
        info.id = model.id;
        info.context_window = model.max_input_tokens.value_or(0);
        info.max_output_tokens = model.max_output_tokens.value_or(0);
        models->push_back(std::move(info));
    }
    return IdentityResult::Ok;
}

IdentityResult ConsoleClient::FetchCatalog(const std::string& console_url,
                                           const std::string& access_token,
                                           std::vector<CatalogPrice>* prices,
                                           std::string* error) const {
    if (prices == nullptr || !SessionTokenIsSafe(access_token)) {
        if (error != nullptr) {
            *error = "no access token is available";
        }
        return IdentityResult::Failed;
    }
    std::string origin;
    if (!ConsoleOrigin(console_url, &origin, error)) {
        return IdentityResult::Failed;
    }
    HttpResponse response;
    if (!Send(transport_, {"GET", origin + "/v1/models/catalog", {}, access_token}, &response,
              error)) {
        return IdentityResult::Failed;
    }
    if (response.status == 401) {
        if (error != nullptr) {
            *error = "console session expired";
        }
        return IdentityResult::Unauthorized;
    }
    if (response.status != 200) {
        HttpError("model catalog request", origin, response, error);
        return IdentityResult::Failed;
    }
    contract::ModelCatalogResponse parsed;
    if (!ParseContract(response, &parsed, error)) {
        return IdentityResult::Failed;
    }
    prices->clear();
    prices->reserve(parsed.models.size());
    for (const contract::CatalogModelResponse& model : parsed.models) {
        prices->push_back(CatalogPrice{model.id, model.input_per_mtok, model.output_per_mtok});
    }
    return IdentityResult::Ok;
}

namespace {

/// A console string that is safe to print in a terminal. Anything else is
/// dropped rather than rendered: this text lands straight in the user's shell.
std::string DisplaySafe(const std::string& value, std::size_t maximum) {
    return DisplayTextIsSafe(value, maximum) ? value : std::string();
}

}  // namespace

IdentityResult ConsoleClient::FetchUsage(const std::string& console_url,
                                         const std::string& access_token, const UsageQuery& query,
                                         Usage* usage, std::string* error) const {
    if (usage == nullptr || !SessionTokenIsSafe(access_token)) {
        if (error != nullptr) {
            *error = "no access token is available";
        }
        return IdentityResult::Failed;
    }
    std::string origin;
    if (!ConsoleOrigin(console_url, &origin, error)) {
        return IdentityResult::Failed;
    }
    // One read for the whole report: a terminal draws it in a single pass, and
    // three round-trips would only give it three chances to print parts that
    // disagree about when they were taken.
    std::string url = origin + "/v1/cli/usage?days=" + std::to_string(std::clamp(query.days, 1, 365)) +
                      "&limit=" + std::to_string(std::clamp(query.limit, 1, 200));
    if (!query.model.empty()) {
        url += "&model=" + QueryEscape(query.model);
    }

    HttpResponse response;
    if (!Send(transport_, {"GET", url, {}, access_token}, &response, error)) {
        return IdentityResult::Failed;
    }
    if (response.status == 401) {
        if (error != nullptr) {
            *error = "console session expired";
        }
        return IdentityResult::Unauthorized;
    }
    if (response.status != 200) {
        HttpError("usage request", origin, response, error);
        return IdentityResult::Failed;
    }

    contract::CliUsageResponse parsed;
    if (!ParseContract(response, &parsed, error)) {
        return IdentityResult::Failed;
    }

    // Map the typed response onto the domain Usage, sanitizing every string the
    // server chose before it reaches the terminal. The numbers are already
    // typed; the strings still pass through DisplaySafe because a hostile
    // console must not be able to write escape sequences to the user's screen.
    const auto copy_totals = [](const contract::UsageTotals& from, UsageTotals* to) {
        to->requests = from.requests;
        to->prompt_tokens = from.prompt_tokens;
        to->completion_tokens = from.completion_tokens;
        to->cached_tokens = from.cached_tokens;
        to->cost_micros = from.cost_micros;
    };

    usage->credit.balance_micros = parsed.credit.balance_micros;
    usage->credit.granted_micros = parsed.credit.granted_micros;
    usage->credit.spent_micros = parsed.credit.spent_micros;
    copy_totals(parsed.totals, &usage->totals);

    // Absent on every console deployed before windowed totals shipped. The
    // window label is a closed enum, so its text is a known-safe literal.
    for (const contract::CliUsageWindow& entry : parsed.windows) {
        UsageWindow window;
        window.window = Json(entry.window).get<std::string>();
        window.seconds = entry.seconds;
        copy_totals(entry.totals, &window.totals);
        usage->windows.push_back(window);
    }

    for (const contract::UsageTimelinePoint& point : parsed.timeline) {
        UsageDay day;
        day.date = DisplaySafe(point.date, 32);
        day.requests = point.requests;
        day.prompt_tokens = point.prompt_tokens;
        day.completion_tokens = point.completion_tokens;
        day.cost_micros = point.cost_micros;
        usage->timeline.push_back(day);
    }

    for (const contract::UsageModelRollup& entry : parsed.models) {
        UsageModel row;
        row.model = DisplaySafe(entry.model, 128);
        row.requests = entry.requests;
        row.prompt_tokens = entry.prompt_tokens;
        row.completion_tokens = entry.completion_tokens;
        row.cached_tokens = entry.cached_tokens;
        row.cost_micros = entry.cost_micros;
        usage->models.push_back(row);
    }

    for (const contract::CliUsageEvent& entry : parsed.recent) {
        UsageEvent event;
        event.request_id = DisplaySafe(entry.request_id, 128);
        event.model = DisplaySafe(entry.model, 128);
        event.harness =
            entry.harness.has_value() ? DisplaySafe(Json(*entry.harness).get<std::string>(), 64)
                                      : std::string();
        event.started_at = DisplaySafe(entry.ts_start, 64);
        event.error_code = DisplaySafe(entry.error_code.value_or(""), 64);
        event.prompt_tokens = entry.prompt_tokens;
        event.completion_tokens = entry.completion_tokens;
        event.cached_tokens = entry.cached_tokens;
        event.cost_micros = entry.cost_micros;
        event.ttft_ms = entry.ttft_ms.value_or(0);
        event.status_code = static_cast<int>(entry.status_code);
        usage->events.push_back(event);
    }
    return IdentityResult::Ok;
}

CancelOutcome ConsoleClient::CancelRequest(const std::string& console_url,
                                           const std::string& access_token,
                                           const std::string& request_id, int timeout_ms,
                                           std::string* error) const {
    if (!SessionTokenIsSafe(access_token)) {
        if (error != nullptr) {
            *error = "no access token is available";
        }
        return CancelOutcome::Failed;
    }
    // The id is the server's own (its x-request-id); it is still user-facing
    // input to a path, so it is escaped rather than trusted.
    if (request_id.empty()) {
        if (error != nullptr) {
            *error = "no request id to cancel";
        }
        return CancelOutcome::Failed;
    }
    std::string origin;
    if (!ConsoleOrigin(console_url, &origin, error)) {
        return CancelOutcome::Failed;
    }
    HttpRequest request{"POST", origin + "/v1/requests/" + QueryEscape(request_id) + "/cancel", {},
                        access_token};
    request.timeout_ms = timeout_ms;
    HttpResponse response;
    if (!Send(transport_, request, &response, error)) {
        return CancelOutcome::Failed;
    }
    if (response.status == 202) {
        contract::CancelRequestResponse parsed;
        // The body is informational (the id echoed, status "cancelling"); a
        // server that answered 202 has already done the work, so a body this
        // binding cannot read is logged, not treated as a failure.
        std::string ignored;
        ParseContract(response, &parsed, &ignored);
        return CancelOutcome::Cancelled;
    }
    if (response.status == 404) {
        return CancelOutcome::NotFound;
    }
    HttpError("cancel request", origin, response, error);
    return CancelOutcome::Failed;
}

bool ConsoleClient::Revoke(const std::string& console_url, const std::string& access_token,
                           const std::string& refresh_token, std::string* error) const {
    if (access_token.empty() && refresh_token.empty()) {
        return true;
    }
    if ((!access_token.empty() && !SessionTokenIsSafe(access_token)) ||
        (!refresh_token.empty() && !SessionTokenIsSafe(refresh_token))) {
        if (error != nullptr) {
            *error = "cloud session contains an invalid token encoding";
        }
        return false;
    }
    std::string origin;
    if (!ConsoleOrigin(console_url, &origin, error)) {
        return false;
    }
    contract::CliRefreshRequest request;
    request.refresh_token = refresh_token;
    HttpResponse response;
    if (!Send(transport_, {"POST", origin + "/auth/cli/revoke", Json(request).dump(), access_token},
              &response, error)) {
        return false;
    }
    if (response.status != 200 && response.status != 204) {
        HttpError("revoke", origin, response, error);
        return false;
    }
    return true;
}

}  // namespace wally::account

namespace wally::account {

bool BeginAuthorization(const std::string& console_url, const std::string& hostname,
                        Authorization* authorization, std::string* error) {
    return ConsoleClient().BeginAuthorization(console_url, hostname, authorization, error);
}

PollResult Poll(const std::string& console_url, const Authorization& authorization, Grant* grant,
                std::string* error) {
    return ConsoleClient().Poll(console_url, authorization, grant, error);
}

bool Refresh(const std::string& console_url, const std::string& refresh_token, Grant* grant,
             std::string* error) {
    return ConsoleClient().Refresh(console_url, refresh_token, grant, error);
}

bool WhoAmI(const std::string& console_url, const std::string& token, Identity* identity,
            std::string* error) {
    return ConsoleClient().WhoAmI(console_url, token, identity, error) == IdentityResult::Ok;
}

}  // namespace wally::account

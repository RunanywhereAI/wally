#include "ide/jetbrains_profile.h"

#include <algorithm>
#include <cstdlib>
#include <filesystem>
#include <cctype>
#include <cstring>
#include <fstream>
#include <string>
#include <vector>

#include "io/output.h"
#include "harness/harness.h"

#include <nlohmann/json.hpp>

#if defined(__APPLE__)
#include <Security/Security.h>
#endif

namespace wally::ide {
namespace {

namespace fs = std::filesystem;

/// AI Assistant's marketplace id. The IDE's own `installPlugins` resolves it.
constexpr const char* kPluginID = "com.intellij.ml.llm";
/// What the plugin unpacks to inside the configuration tree.
constexpr const char* kPluginDirectory = "ml-llm";
/// The settings file behind `@State(name = "OpenAILikeLlmProviderSettings")`.
constexpr const char* kSettingsFile = "llm.provider.openai.like.xml";
constexpr const char* kComponent = "OpenAILikeLlmProviderSettings";
/// The provider selection, behind `@State(name = "LlmCustomModelsSettings")`.
constexpr const char* kModelsFile = "llm.custom.models.xml";
/// The set of providers the IDE will talk to at all.
constexpr const char* kProvidersFile = "llm.third.party.ai.providers.xml";
constexpr const char* kProvidersComponent = "LLMThirdPartyAIProvidersSettings";
/// `enableProvider` refuses to add anything until this has been accepted, so
/// the set above is ignored without it. Third-party providers are a beta
/// feature and this is the acknowledgement the IDE would otherwise ask for.
constexpr const char* kAcknowledgementKey =
    "llm.third.party.ai.services.acknowledgement.accepted";
/// Application properties, kept as a JSON blob inside a CDATA section.
constexpr const char* kPropertiesFile = "other.xml";

/// AI Assistant's own settings, which is where Chat mode reads its model from.
///
/// Separate from the provider files on purpose: the provider list decides which
/// endpoints the IDE may talk to, and this decides which model each feature
/// uses. A configured provider whose model is not named here leaves Chat mode
/// with "No compatible model is available", while the picker still lists the
/// model, because the picker reads the provider and Chat mode reads this.
constexpr const char* kChatFile = "llm.for.code.xml";
constexpr const char* kChatComponent = "LLMSettings";
/// `OPEN_AI_API_PROVIDER_ID`, which is also the credential's key.
constexpr const char* kProviderID = "OpenAIAPI";
/// The subsystem the platform prefixes credentials with.
constexpr const char* kSubsystem = "AI Assistant";
/// cpp-httplib serves HTTP/1.1 only, and the client's default is negotiated
/// upward. Left unset, the first request fails before the model is ever asked.
constexpr const char* kHttpVersion = "HTTP_1_1";

std::string Home() {
    const char* home = std::getenv("HOME");
    return home != nullptr ? std::string(home) : std::string();
}

/// The credential store's service name: subsystem and key joined by an em dash,
/// which is the separator the platform writes and therefore the one it reads.
std::string ServiceName() {
    return std::string("IntelliJ Platform ") + kSubsystem + " \xE2\x80\x94 " + kProviderID;
}

bool WriteFile(const fs::path& path, const std::string& contents, std::string* error) {
    std::error_code code;
    fs::create_directories(path.parent_path(), code);
    std::ofstream out(path, std::ios::trunc);
    if (!out) {
        *error = "cannot write " + path.string();
        return false;
    }
    out << contents;
    if (!out) {
        *error = "cannot write " + path.string();
        return false;
    }
    return true;
}

/// The settings tree, with only the two options the state class actually
/// persists. Anything else here is dropped on the IDE's next write anyway.
std::string SettingsXML(const std::string& base_url) {
    return std::string("<application>\n  <component name=\"") + kComponent + "\">\n" +
           "    <option name=\"baseUrl\" value=\"" + base_url + "\" />\n" +
           "    <option name=\"httpClientVersion\" value=\"" + kHttpVersion + "\" />\n" +
           "  </component>\n</application>\n";
}


///
/// The members are nested directly, with no element naming the collection.
/// A `<set>` wrapper — the shape most IntelliJ collections serialize to — is
/// silently dropped on load, which reads exactly like the file being ignored.
std::string ProvidersXML() {
    return std::string("<application>\n  <component name=\"") + kProvidersComponent + "\">\n" +
           "    <option name=\"enabledThirdPartyAIProviders\">\n" +
           "      <option value=\"" + kProviderID + "\" />\n" +
           "    </option>\n  </component>\n</application>\n";
}

std::string ReadFile(const fs::path& path);

/// Where a run of markup sits in a document.
struct Span {
    std::size_t start = 0;
    std::size_t end = 0;
};

/// One start tag, as written.
struct Tag {
    Span span;                  ///< `<` through `>`, inclusive of both.
    std::string name;           ///< The element name.
    std::string attribute;      ///< The value of the attribute asked for, if present.
    bool has_attribute = false;
    bool self_closing = false;
    bool closing = false;       ///< A `</name>` tag.
};

/// Past the end of a comment, CDATA section, doctype or processing instruction
/// beginning at `at`, or npos when `at` does not begin one.
///
/// These are skipped rather than searched, which is the whole reason this file
/// scans rather than pattern-matches. A commented-out `<option name="...">` or
/// one quoted inside CDATA is text belonging to the reader, and deleting
/// through it destroys their file while looking like it worked.
std::size_t SkipNonMarkup(const std::string& document, std::size_t at) {
    static constexpr struct {
        const char* open;
        const char* close;
    } kRegions[] = {
        {"<!--", "-->"},
        {"<![CDATA[", "]]>"},
        {"<?", "?>"},
    };
    for (const auto& region : kRegions) {
        const std::size_t open = std::strlen(region.open);
        if (document.compare(at, open, region.open) == 0) {
            const std::size_t close = document.find(region.close, at + open);
            if (close == std::string::npos) {
                return std::string::npos;  // Unterminated: refuse the document.
            }
            return close + std::strlen(region.close);
        }
    }
    // `<!DOCTYPE ...>` and friends: any other `<!` declaration ends at its `>`.
    if (document.compare(at, 2, "<!") == 0) {
        const std::size_t close = document.find('>', at);
        return close == std::string::npos ? std::string::npos : close + 1;
    }
    return std::string::npos;
}

bool IsNameCharacter(unsigned char character) {
    return std::isalnum(character) != 0 || character == '_' || character == '-' ||
           character == ':' || character == '.';
}

/// Reads the tag beginning at `document[at]`, which must be `<` and must not
/// begin a comment or CDATA section. `wanted` names the attribute whose value
/// is returned, if the tag carries it.
///
/// Returns false on anything it cannot read exactly, which the caller turns
/// into a refusal. Attribute order, surrounding whitespace and either quote
/// style are all accepted, because all three are legal and the IDE is not the
/// only thing that writes these files.
bool ReadTag(const std::string& document, std::size_t at, const std::string& wanted, Tag* tag) {
    if (at >= document.size() || document[at] != '<') {
        return false;
    }
    *tag = Tag{};
    tag->span.start = at;
    std::size_t index = at + 1;
    if (index < document.size() && document[index] == '/') {
        tag->closing = true;
        ++index;
    }
    const std::size_t name_start = index;
    while (index < document.size() && IsNameCharacter(static_cast<unsigned char>(document[index]))) {
        ++index;
    }
    if (index == name_start) {
        return false;
    }
    tag->name = document.substr(name_start, index - name_start);

    while (index < document.size()) {
        while (index < document.size() &&
               std::isspace(static_cast<unsigned char>(document[index])) != 0) {
            ++index;
        }
        if (index >= document.size()) {
            return false;
        }
        if (document[index] == '/') {
            tag->self_closing = true;
            ++index;
            if (index >= document.size() || document[index] != '>') {
                return false;
            }
        }
        if (document[index] == '>') {
            tag->span.end = index;
            return true;
        }
        const std::size_t attribute_start = index;
        while (index < document.size() &&
               IsNameCharacter(static_cast<unsigned char>(document[index]))) {
            ++index;
        }
        if (index == attribute_start) {
            return false;  // Not a name, not `/`, not `>`: unreadable.
        }
        const std::string attribute = document.substr(attribute_start, index - attribute_start);
        while (index < document.size() &&
               std::isspace(static_cast<unsigned char>(document[index])) != 0) {
            ++index;
        }
        if (index >= document.size() || document[index] != '=') {
            return false;  // A valueless attribute is HTML, not XML.
        }
        ++index;
        while (index < document.size() &&
               std::isspace(static_cast<unsigned char>(document[index])) != 0) {
            ++index;
        }
        if (index >= document.size() || (document[index] != '"' && document[index] != '\'')) {
            return false;
        }
        const char quote = document[index];
        const std::size_t value_start = ++index;
        // A quoted value may hold `>` legally, which is why the end of a tag
        // cannot be found by searching for that character.
        const std::size_t value_end = document.find(quote, value_start);
        if (value_end == std::string::npos) {
            return false;
        }
        if (attribute == wanted) {
            tag->attribute = document.substr(value_start, value_end - value_start);
            tag->has_attribute = true;
        }
        index = value_end + 1;
    }
    return false;
}

/// The body of the first `<element attribute="value">` at the top level, as a
/// span between its start and end tags.
///
/// `found` is false when the document simply has no such element, which is an
/// ordinary case. A false return means the document could not be read and must
/// be left alone.
bool FindElementBody(const std::string& document, const std::string& element,
                     const std::string& attribute, const std::string& value, bool* found,
                     Span* body, Span* whole) {
    *found = false;
    int depth = 0;
    std::size_t index = 0;
    bool inside = false;
    while (index < document.size()) {
        const std::size_t open = document.find('<', index);
        if (open == std::string::npos) {
            break;
        }
        const std::size_t skipped = SkipNonMarkup(document, open);
        if (skipped != std::string::npos) {
            index = skipped;
            continue;
        }
        if (document.compare(open, 2, "<!") == 0) {
            return false;  // An unterminated declaration.
        }
        Tag tag;
        if (!ReadTag(document, open, attribute, &tag)) {
            return false;
        }
        index = tag.span.end + 1;
        if (!inside) {
            if (!tag.closing && !tag.self_closing && tag.name == element && tag.has_attribute &&
                tag.attribute == value) {
                inside = true;
                depth = 1;
                whole->start = tag.span.start;
                body->start = tag.span.end + 1;
            }
            continue;
        }
        // Inside the element: count nested elements of the same name so the
        // matching close is the element's own, not a child's.
        if (tag.name == element) {
            if (tag.closing) {
                --depth;
                if (depth == 0) {
                    body->end = tag.span.start;
                    whole->end = tag.span.end + 1;
                    *found = true;
                    return true;
                }
            } else if (!tag.self_closing) {
                ++depth;
            }
        }
    }
    return !inside;  // An element that never closed is a document we cannot read.
}

/// Past the end of the element whose start tag is `tag`, or npos when its
/// closing tag cannot be found.
///
/// Elements are skipped whole rather than walked into: the children of another
/// setting belong to that setting, and an option of our own name nested inside
/// one is its business, not ours.
std::size_t SkipElement(const std::string& body, const Tag& tag) {
    if (tag.self_closing || tag.closing) {
        return tag.span.end + 1;
    }
    int depth = 1;
    std::size_t scan = tag.span.end + 1;
    while (depth > 0) {
        const std::size_t next = body.find('<', scan);
        if (next == std::string::npos) {
            return std::string::npos;
        }
        const std::size_t noise = SkipNonMarkup(body, next);
        if (noise != std::string::npos) {
            scan = noise;
            continue;
        }
        Tag inner;
        if (!ReadTag(body, next, "name", &inner)) {
            return std::string::npos;
        }
        scan = inner.span.end + 1;
        if (inner.name != tag.name || inner.self_closing) {
            continue;
        }
        depth += inner.closing ? -1 : 1;
    }
    return scan;
}

/// Removes every direct child `<option name="name" …>` from an element body.
///
/// Direct children only: an option of the same name nested inside another
/// option's map belongs to that option, not to us. Returns false when the body
/// cannot be read.
bool RemoveChildOptions(std::string* body, const std::string& name) {
    std::string result;
    result.reserve(body->size());
    std::size_t index = 0;
    std::size_t copied = 0;
    while (index < body->size()) {
        const std::size_t open = body->find('<', index);
        if (open == std::string::npos) {
            break;
        }
        const std::size_t skipped = SkipNonMarkup(*body, open);
        if (skipped != std::string::npos) {
            index = skipped;
            continue;
        }
        if (body->compare(open, 2, "<!") == 0) {
            return false;
        }
        Tag tag;
        if (!ReadTag(*body, open, "name", &tag)) {
            return false;
        }
        if (tag.closing || tag.name != "option" || !tag.has_attribute || tag.attribute != name) {
            // Past this element entirely, children included.
            const std::size_t past = SkipElement(*body, tag);
            if (past == std::string::npos) {
                return false;
            }
            index = past;
            continue;
        }
        const std::size_t element_end = SkipElement(*body, tag);
        if (element_end == std::string::npos) {
            return false;
        }
        std::size_t end = element_end;
        // Take the line the option sat on with it, so removing one does not
        // leave its indentation or a blank line behind.
        std::size_t line_start = body->rfind('\n', tag.span.start);
        line_start = line_start == std::string::npos ? 0 : line_start + 1;
        if (body->find_first_not_of(" \t", line_start) < tag.span.start) {
            line_start = tag.span.start;  // Something else shares the line; keep it.
        }
        while (end < body->size() && (*body)[end] == '\r') {
            ++end;
        }
        if (end < body->size() && (*body)[end] == '\n') {
            ++end;
        }
        result.append(*body, copied, line_start - copied);
        copied = end;
        index = end;
    }
    result.append(*body, copied, std::string::npos);
    *body = result;
    return true;
}

bool NameChatModel(const fs::path& path, const std::string& model, std::string* error) {
    std::string document = ReadFile(path);
    const std::string updated = WithChatModel(document, model);
    if (updated.empty()) {
        *error = "cannot read AI Assistant's settings in " + path.string();
        return false;
    }
    return WriteFile(path, updated, error);
}

std::string ReadFile(const fs::path& path) {
    std::ifstream in(path);
    return in ? std::string(std::istreambuf_iterator<char>(in), std::istreambuf_iterator<char>())
              : std::string();
}

/// Accepts the third-party acknowledgement in the IDE's application properties.
///
/// The properties are a JSON object inside a CDATA section inside the XML, so
/// the blob is parsed rather than pattern-matched — every other value in there
/// belongs to the reader and has to survive untouched.
bool AcceptAcknowledgement(const fs::path& path, std::string* error) {
    std::string document = ReadFile(path);
    constexpr const char* kOpen = "<component name=\"PropertyService\"><![CDATA[";
    constexpr const char* kClose = "]]></component>";
    const size_t open = document.find(kOpen);
    if (open == std::string::npos) {
        // No properties yet, which a never-launched IDE has not written. The
        // component is ours to create, beside whatever else is in the file.
        nlohmann::json properties;
        properties["keyToString"][kAcknowledgementKey] = "true";
        const std::string component = std::string("  ") + kOpen + properties.dump(2) + kClose + "\n";
        const size_t end = document.find("</application>");
        if (document.empty() || end == std::string::npos) {
            return WriteFile(path, "<application>\n" + component + "</application>\n", error);
        }
        document.insert(end, component);
        return WriteFile(path, document, error);
    }

    const size_t start = open + std::string(kOpen).size();
    const size_t close = document.find(kClose, start);
    if (close == std::string::npos) {
        *error = "cannot read the properties in " + path.string();
        return false;
    }
    nlohmann::json properties = nlohmann::json::parse(document.substr(start, close - start),
                                                     nullptr, false);
    if (properties.is_discarded()) {
        *error = "cannot read the properties in " + path.string();
        return false;
    }
    properties["keyToString"][kAcknowledgementKey] = "true";
    document.replace(start, close - start, properties.dump(2));
    return WriteFile(path, document, error);
}

#if defined(__APPLE__)
CFStringRef CopyString(const std::string& value) {
    return CFStringCreateWithBytes(nullptr, reinterpret_cast<const UInt8*>(value.data()),
                                   static_cast<CFIndex>(value.size()), kCFStringEncodingUTF8,
                                   false);
}

/// The query identifying our credential, without the secret in it.
CFMutableDictionaryRef CopyQuery() {
    CFMutableDictionaryRef query = CFDictionaryCreateMutable(
        nullptr, 0, &kCFTypeDictionaryKeyCallBacks, &kCFTypeDictionaryValueCallBacks);
    CFDictionarySetValue(query, kSecClass, kSecClassGenericPassword);
    CFStringRef service = CopyString(ServiceName());
    CFStringRef account = CopyString(kProviderID);
    CFDictionarySetValue(query, kSecAttrService, service);
    CFDictionarySetValue(query, kSecAttrAccount, account);
    CFRelease(service);
    CFRelease(account);
    return query;
}

void DropSecret() {
    CFMutableDictionaryRef query = CopyQuery();
    SecItemDelete(query);
    CFRelease(query);
}
#else
void DropSecret() {}
#endif

/// Installs AI Assistant through the IDE's own command line.
///
/// The IDE is the only thing that knows which build of the plugin matches it,
/// so asking it beats resolving a download ourselves. It also creates the
/// configuration directory on the way, which a never-launched IDE has not.
bool InstallPlugin(const Product& product, const std::string& bundle, std::string* error) {
    const std::string launcher = bundle + "/Contents/MacOS/" + product.launcher;
    out::status_line("installing JetBrains AI Assistant; this happens once and takes a minute");
    if (harness::Launch(launcher, {}, {"installPlugins", kPluginID}) != 0) {
        *error = "could not install AI Assistant into " + std::string(product.id);
        return false;
    }
    return true;
}

bool PluginInstalled(const std::string& config) {
    std::error_code code;
    return !config.empty() && fs::exists(fs::path(config) / "plugins" / kPluginDirectory, code);
}

}  // namespace

std::string WithChatModel(const std::string& document, const std::string& model) {
    const std::string id = std::string(kProviderID) + "/" + model;
    const std::string options =
        "    <option name=\"chat_preferred_llm\" value=\"" + id + "\" />\n" +
        "    <option name=\"chat_preferred_llm_per_mode\">\n" +
        "      <map>\n" +
        "        <entry key=\"chat\" value=\"" + id + "\" />\n" +
        "      </map>\n" +
        "    </option>\n";

    bool found = false;
    Span body;
    Span whole;
    if (!FindElementBody(document, "component", "name", kChatComponent, &found, &body, &whole)) {
        return {};
    }

    if (!found) {
        // No settings yet, which an IDE nobody has opened AI Assistant in has
        // not written. The component is ours to add beside whatever else is
        // already in the file — but only if we can see where the document ends.
        const std::string block =
            std::string("  <component name=\"") + kChatComponent + "\">\n" + options +
            "  </component>\n";
        if (document.find_first_not_of(" \t\r\n") == std::string::npos) {
            return "<application>\n" + block + "</application>\n";
        }
        bool root = false;
        Span root_body;
        Span root_whole;
        if (!FindElementBody(document, "application", "", "", &root, &root_body, &root_whole)) {
            return {};
        }
        // A document with content but no readable root is one to leave alone.
        // Replacing it would silently discard whatever it did hold, which is
        // exactly the settings this is supposed to preserve.
        const std::size_t end = document.rfind("</application>");
        if (end == std::string::npos) {
            return {};
        }
        std::string created = document;
        created.insert(end, block);
        return created;
    }

    std::string inner = document.substr(body.start, body.end - body.start);
    if (!RemoveChildOptions(&inner, "chat_preferred_llm") ||
        !RemoveChildOptions(&inner, "chat_preferred_llm_per_mode")) {
        return {};
    }
    // Removing the last option can leave the indentation of the line it was on.
    while (!inner.empty() && (inner.back() == ' ' || inner.back() == '\t')) {
        inner.pop_back();
    }
    if (!inner.empty() && inner.back() != '\n') {
        inner += "\n";
    }
    std::string updated = document;
    updated.replace(body.start, body.end - body.start, inner + options + "  ");
    return updated;
}


std::string BundlePath(const Product& product) {
    const std::string home = Home();
    std::vector<std::string> roots{"/Applications/"};
    if (!home.empty()) {
        roots.push_back(home + "/Applications/");
    }
    std::error_code code;
    for (const std::string& root : roots) {
        const std::string path = root + product.bundle;
        if (fs::exists(fs::path(path) / "Contents" / "Info.plist", code)) {
            return path;
        }
    }
    return {};
}

std::string ConfigDirectory(const Product& product) {
    const std::string home = Home();
    if (home.empty()) {
        return {};
    }
    const fs::path root = fs::path(home) / "Library" / "Application Support" / "JetBrains";
    std::error_code code;
    // One tree per release, so the newest name wins. Sorting the names works
    // because JetBrains pads the version the same way in every one of them.
    std::string newest;
    for (const fs::directory_entry& entry : fs::directory_iterator(root, code)) {
        const std::string name = entry.path().filename().string();
        if (name.rfind(product.config_prefix, 0) == 0 && name > newest) {
            newest = name;
        }
    }
    return newest.empty() ? std::string() : (root / newest).string();
}

bool ApplyProvider(const Product& product, const std::string& base_url,
                   const std::string& api_key, const std::string& model,
                   std::string* error) {
    const std::string bundle = BundlePath(product);
    if (bundle.empty()) {
        *error = std::string(product.bundle) + " is not installed";
        return false;
    }

    std::string config = ConfigDirectory(product);
    if (!PluginInstalled(config)) {
        if (!InstallPlugin(product, bundle, error)) {
            return false;
        }
        // The install is what creates the tree on an IDE nobody has launched.
        config = ConfigDirectory(product);
        if (!PluginInstalled(config)) {
            *error = "AI Assistant did not appear in " +
                     (config.empty() ? std::string("the configuration directory") : config);
            return false;
        }
    }

    const fs::path options = fs::path(config) / "options";
    // llm.custom.models.xml is deliberately not written. The IDE picks the
    // completion and editor models up from the provider once it can reach it,
    // and deletes any file we leave behind. AI Assistant's own settings are a
    // different matter: it keeps those, and Chat mode reads its model from
    // there rather than from the provider, so a provider alone leaves Chat with
    // "No compatible model is available" while the picker still lists it.
    if (!WriteFile(options / kSettingsFile, SettingsXML(base_url), error) ||
        !WriteFile(options / kProvidersFile, ProvidersXML(), error) ||
        !NameChatModel(options / kChatFile, model, error) ||
        !AcceptAcknowledgement(options / kPropertiesFile, error)) {
        return false;
    }
    // No credential is written, and any earlier one is removed.
    //
    // Writing the proxy's token here looked right and did nothing: the IDE
    // reported the provider with an empty key on every launch and sent none, so
    // chat came back 401 from our own proxy while the connection test passed
    // (only /v1/chat/completions checked the token). AI Assistant takes a
    // provider key from its own settings dialog and nowhere else. The proxy no
    // longer asks for one.
    (void)api_key;
    (void)bundle;
    DropSecret();
    return true;
}

bool RestoreProvider(const Product& product, std::string* error) {
    const std::string config = ConfigDirectory(product);
    if (config.empty()) {
        *error = std::string(product.id) + " has no configuration directory to clear";
        return false;
    }
    DropSecret();
    std::error_code code;
    fs::remove(fs::path(config) / "options" / kSettingsFile, code);
    fs::remove(fs::path(config) / "options" / kModelsFile, code);
    // The chat model is not removed: llm.for.code.xml is AI Assistant's own
    // settings file and mostly holds the reader's, not ours. Leaving a model id
    // that no longer resolves is what the IDE's own picker is for.
    fs::remove(fs::path(config) / "options" / kProvidersFile, code);
    return true;
}

}  // namespace wally::ide

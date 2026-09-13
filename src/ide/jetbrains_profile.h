#ifndef WALLY_IDE_JETBRAINS_PROFILE_H
#define WALLY_IDE_JETBRAINS_PROFILE_H

#include <string>

/// AI Assistant's OpenAI-compatible provider, configured from outside the IDE.
///
/// A JetBrains IDE already ships an agent — AI Assistant, and Junie behind it —
/// so pointing one at a local model is a matter of telling that agent where the
/// model lives, not of nesting a second agent inside the editor. The provider
/// it exposes for this speaks plain OpenAI, which is the shape `rac_server`
/// already serves, so nothing has to be translated on the way.
///
/// Three things have to be written before the IDE starts, and it reads all of
/// them once at launch: a base URL in its own options tree, the provider in the
/// enabled set that the Providers & API keys page shows as a dropdown, and the
/// model AI Chat should use. The first two alone leave Chat reporting "No
/// compatible model is available" while the picker still lists the model,
/// because the picker reads the provider and Chat reads its own setting.
///
/// No credential is written. AI Assistant takes a provider key from its own
/// settings dialog and nowhere else, so a key placed in the platform credential
/// store from outside is never read — the IDE reports the provider with an
/// empty key and sends none. The local proxy therefore asks for none.
///
/// None of this needs a JetBrains AI subscription. BYOK is the supported path,
/// and AI Chat honours it when the model is named in AI Assistant's settings.
namespace wally::ide {

/// The port the local server is asked for when serving a JetBrains IDE.
///
/// Fixed on purpose. The IDE reads its base URL once at startup, from a file
/// written before it launches, so a port that moved between runs would leave
/// that file naming something dead every time wally exited first. Asking for the
/// same one keeps the configuration true, and a second wally holding it only
/// costs this run a rewrite.
constexpr int kProviderPort = 11636;

/// `document` with AI Chat's model set to `model`, everything else preserved.
///
/// Pure, and exposed for that reason: this edits a file the reader owns, where
/// a mistake silently eats settings the IDE will not put back. The file I/O
/// around it is trivial; this is the part that has to be right.
///
/// `document` may be empty or may have no AI Assistant component, both of which
/// an IDE that has never opened the tool window will produce.
std::string WithChatModel(const std::string& document, const std::string& model);

/// A JetBrains IDE, named the way the reader types it.
struct Product {
    /// The subcommand: `clion`.
    const char* id;
    /// The application bundle to look for under /Applications.
    const char* bundle;
    /// The launcher inside the bundle, which doubles as the IDE's own CLI.
    const char* launcher;
    /// What its per-version configuration directory is called, before the
    /// version. JetBrains keeps one tree per release, so the newest wins.
    const char* config_prefix;
};

/// Installs AI Assistant if it is absent, points its provider at `base_url`,
/// and stores `api_key` where the IDE looks for it.
///
/// The install is the slow part and happens once; every later run finds the
/// plugin already there and only rewrites the URL. Returns false with `error`
/// set. Takes effect on the IDE's next launch, never on a running one.
bool ApplyProvider(const Product& product, const std::string& base_url,
                   const std::string& api_key, const std::string& model,
                   std::string* error);

/// Clears the base URL and drops the stored key, leaving the plugin installed.
///
/// The way out when a run left the IDE pointing at a port nothing is serving.
bool RestoreProvider(const Product& product, std::string* error);

/// The IDE's configuration directory, or empty when it has never been run.
std::string ConfigDirectory(const Product& product);

/// The application bundle's path, or empty when the IDE is not installed.
std::string BundlePath(const Product& product);

}  // namespace wally::ide

#endif  // WALLY_IDE_JETBRAINS_PROFILE_H

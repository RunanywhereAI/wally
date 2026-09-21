/**
 * @file cmd_about.cpp
 * @brief `wally about` — a styled, richer environment panel: product, system,
 * runtime backends, paths, and the signed-in account. See `wally info` for
 * the terse sibling this expands on.
 */

#include "commands/commands.h"

#include <cstdint>
#include <map>
#include <string>

#include "rac/core/rac_core.h"
#include "rac/core/rac_platform_adapter.h"
#include "runanywhere/proto/schema_lock.h"

#include "account/baked_endpoints.h"
#include "account/credentials.h"
#include "cli_formatter.h"
#include "config/cli_paths.h"
#include "device_info.h"
#include "io/output.h"

#ifndef WALLY_VERSION
#define WALLY_VERSION "0.0.0-dev"
#endif

namespace wally::commands {

namespace {

#if defined(__APPLE__)
constexpr const char* kPlatform = "macos";
#elif defined(__linux__)
constexpr const char* kPlatform = "linux";
#elif defined(_WIN32)
constexpr const char* kPlatform = "windows";
#else
constexpr const char* kPlatform = "unknown";
#endif

constexpr std::size_t kLabelWidth = 10;

// "  label    value", the label column padded to kLabelWidth. Built with
// plain string ops rather than a fixed snprintf buffer -- a path or an
// account email has no natural length cap, so nothing here should risk
// silently truncating one. The label itself is left uncolored, same
// restraint --help's descriptions get.
void row(const std::string& label, const std::string& value) {
    std::string line = "  " + label;
    line += std::string(line.length() < 2 + kLabelWidth ? 2 + kLabelWidth - line.length() : 1, ' ');
    out::result_line(line + value);
}

void heading(const cli_color::Palette& pal, const std::string& title) {
    out::result_line("");
    out::result_line(pal.bold + title + pal.reset + std::string(":"));
}

}  // namespace

void register_about(CLI::App& app, GlobalOptions& options) {
    CLI::App* cmd =
        app.add_subcommand("about", "Versions, backends, paths and account");
    cmd->callback([&options]() {
        Bootstrapped env;
        if (bootstrap(options, &env) != RAC_SUCCESS) {
            throw CLI::RuntimeError(1);
        }

        const rac_version_t commons = rac_get_version();
        const std::string commons_version = commons.string ? commons.string : "unknown";

        const DeviceSnapshot device = collect_device_snapshot();

        rac_memory_info_t memory{};
        bool memory_ok = false;
        if (const rac_platform_adapter_t* adapter = rac_get_platform_adapter()) {
            memory_ok = adapter->get_memory_info &&
                        adapter->get_memory_info(&memory, adapter->user_data) == RAC_SUCCESS;
        }

        const std::map<std::string, EngineRow> engines = collect_backend_rows();

        // Which bottle this binary is: WALLY_BAKED_CONSOLE_API_URL is compiled
        // in empty for a production build and non-empty for a dev one (see
        // baked_endpoints.h.in) -- the same compile-time fact
        // DefaultConsoleUrl() itself keys on. console_url is the *effective*
        // target though, since an env override still wins over the bake.
        const bool dev_channel = WALLY_BAKED_CONSOLE_API_URL[0] != '\0';
        const char* channel = dev_channel ? "development" : "production";
        const std::string console_url = account::DefaultConsoleUrl();

        account::Credentials credentials;
        std::string credentials_error;
        const bool signed_in =
            account::Load(&credentials, &credentials_error) && credentials.signed_in();

        if (options.json) {
            out::JsonWriter json;
            json.begin_object()
                .field("wally", WALLY_VERSION)
                .field("commons", commons_version)
                .field("idl_version", RUNANYWHERE_IDL_VERSION)
                .field("idl_schema_sha256", RUNANYWHERE_IDL_SCHEMA_SHA256)
                .field("platform", kPlatform)
                .field("channel", channel)
                .field("console", console_url)
                .field("os", device.os_version)
                .field("cpu", device.chip)
                .field("core_count", static_cast<int64_t>(device.core_count))
                .field("performance_cores", static_cast<int64_t>(device.performance_cores))
                .field("efficiency_cores", static_cast<int64_t>(device.efficiency_cores))
                .field("architecture", device.architecture);
            if (memory_ok) {
                json.field("memory_total_bytes", static_cast<int64_t>(memory.total_bytes))
                    .field("memory_available_bytes",
                           static_cast<int64_t>(memory.available_bytes));
            }

            json.begin_array("backends");
            for (const auto& [name, engine] : engines) {
                json.begin_array_object()
                    .field("name", name)
                    .field("display_name", engine.display_name)
                    .field("version", engine.version)
                    .field("priority", static_cast<int64_t>(engine.priority));
                json.begin_array("primitives");
                for (const auto& primitive : engine.primitives) {
                    json.value(primitive);
                }
                json.end_array().end_object();
            }
            json.end_array();

            json.field("home", env.home)
                .field("models_dir", env.models_dir)
                .field("state_dir", paths::state_dir())
                .field("account_signed_in", signed_in);
            if (signed_in) {
                json.field("account_email", credentials.email)
                    .field("account_console", credentials.console_url);
            }
            json.end_object();
            out::result_line(json.str());
            return;
        }

        const cli_color::Palette pal =
            cli_color::make_palette(color_output_enabled(options.no_color));

        heading(pal, "Product");
        row("wally", WALLY_VERSION);
        row("commons", commons_version);
        row("idl", std::string(RUNANYWHERE_IDL_VERSION) + " sha256 " +
                            RUNANYWHERE_IDL_SCHEMA_SHA256);
        row("platform", kPlatform);
        row("channel", channel);
        // The console URL is deliberately NOT printed here. It is an internal
        // endpoint (today `/api-dev`), it means nothing to the person reading
        // `wally about`, and it was shown twice. It stays in `--json` so
        // support and tooling can still read it.

        heading(pal, "System");
        row("os", device.os_version.empty() ? "unknown" : device.os_version);
        row("cpu", (device.chip.empty() ? "unknown" : device.chip) + " (" +
                            std::to_string(device.core_count) + " cores)");
        row("arch", device.architecture.empty() ? "unknown" : device.architecture);
        if (memory_ok) {
            row("memory", out::human_bytes(memory.available_bytes) + " available of " +
                                    out::human_bytes(memory.total_bytes));
        }

        heading(pal, "Runtime");
        if (engines.empty()) {
            row("backends", "none registered");
        } else {
            for (const auto& [name, engine] : engines) {
                std::string primitives;
                for (const auto& primitive : engine.primitives) {
                    primitives += primitives.empty() ? primitive : ", " + primitive;
                }
                std::string label = pal.bold_cyan + name + pal.reset;
                // Padding is computed from the plain (uncolored) name length,
                // same reasoning as CliFormatter -- see cli_formatter.cpp.
                const std::size_t pad =
                    name.length() < kLabelWidth ? kLabelWidth - name.length() : 1;
                out::result_line("  " + label + std::string(pad, ' ') + "priority " +
                                 std::to_string(engine.priority) + "  " + primitives);
            }
        }

        heading(pal, "Paths");
        row("home", env.home);
        row("models", env.models_dir);
        row("state", paths::state_dir());

        if (signed_in) {
            heading(pal, "Account");
            row("email", credentials.email);
        }
        out::result_line("");
    });
}

}  // namespace wally::commands

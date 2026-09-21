/**
 * @file cmd_info.cpp
 * @brief `wally info` — environment summary (versions, paths, memory, plugins).
 */

#include "commands/commands.h"

#include <string>

#include "rac/core/rac_core.h"
#include "rac/core/rac_platform_adapter.h"

#include "config/cli_paths.h"
#include "io/output.h"

#ifndef WALLY_VERSION
#define WALLY_VERSION "0.0.0-dev"
#endif

namespace wally::commands {

void register_info(CLI::App& app, GlobalOptions& options) {
    CLI::App* cmd = app.add_subcommand("info", "Show versions, paths, memory and backends");
    cmd->callback([&options]() {
        Bootstrapped env;
        if (bootstrap(options, &env) != RAC_SUCCESS) {
            throw CLI::RuntimeError(1);
        }

        const rac_version_t commons = rac_get_version();
        const std::string commons_version = commons.string ? commons.string : "unknown";

        rac_memory_info_t memory{};
        bool memory_ok = false;
        if (const rac_platform_adapter_t* adapter = rac_get_platform_adapter()) {
            memory_ok = adapter->get_memory_info &&
                        adapter->get_memory_info(&memory, adapter->user_data) == RAC_SUCCESS;
        }

#if defined(__APPLE__)
        const char* platform = "macos";
#elif defined(__linux__)
        const char* platform = "linux";
#elif defined(_WIN32)
        const char* platform = "windows";
#else
        const char* platform = "unknown";
#endif

        // Same rows `about` and `backends` show, so all three agree on the
        // count (collect_backend_rows applies the llm-only listing filter).
        const auto backends = static_cast<int64_t>(collect_backend_rows().size());

        if (options.json) {
            out::JsonWriter json;
            json.begin_object()
                .field("wally", WALLY_VERSION)
                .field("commons", commons_version)
                .field("platform", platform)
                .field("home", env.home)
                .field("models_dir", env.models_dir)
                .field("state_dir", paths::state_dir())
                .field("backends", backends);
            if (memory_ok) {
                json.field("memory_total_bytes", static_cast<int64_t>(memory.total_bytes))
                    .field("memory_available_bytes",
                           static_cast<int64_t>(memory.available_bytes));
            }
            json.end_object();
            out::result_line(json.str());
            return;
        }

        // One column for the labels so every value starts at the same offset.
        // The widest label is "platform"/"backends" (8); pad to 10.
        auto row = [](const char* label, const std::string& value) {
            std::string key(label);
            if (key.size() < 10) key.append(10 - key.size(), ' ');
            out::result_line(key + value);
        };
        row("wally", WALLY_VERSION);
        row("commons", commons_version);
        row("platform", platform);
        row("home", env.home);
        row("models", env.models_dir);
        row("backends", std::to_string(backends));
        if (memory_ok) {
            row("memory", out::human_bytes(memory.available_bytes) + " available of " +
                              out::human_bytes(memory.total_bytes));
        }
    });
}

}  // namespace wally::commands

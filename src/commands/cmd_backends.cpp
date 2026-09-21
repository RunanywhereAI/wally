/**
 * @file cmd_backends.cpp
 * @brief `wally backends` — registered engine plugins per primitive.
 */

#include "commands/commands.h"

#include <map>
#include <set>
#include <string>
#include <vector>

#include "rac/plugin/rac_engine_vtable.h"
#include "rac/plugin/rac_plugin_entry.h"
#include "rac/plugin/rac_primitive.h"

#include "io/output.h"

namespace wally::commands {

// Walk every live primitive. ONNX without RAG only advertises SEGMENT /
// DIARIZE — omitting those made a registered onnx backend invisible.
std::map<std::string, EngineRow> collect_backend_rows() {
    std::map<std::string, EngineRow> engines;
    for (int raw = 1; raw < static_cast<int>(RAC_PRIMITIVE_COUNT); ++raw) {
        if (raw == 6) {
            continue; // retired RERANK wire value
        }
        const rac_primitive_t primitive = static_cast<rac_primitive_t>(raw);
        const rac_engine_vtable_t* plugins[16] = {};
        size_t count = 0;
        if (rac_plugin_list(primitive, plugins, 16, &count) != RAC_SUCCESS) {
            continue;
        }
        for (size_t i = 0; i < count; ++i) {
            const rac_engine_metadata_t& meta = plugins[i]->metadata;
            EngineRow& row = engines[meta.name ? meta.name : "?"];
            if (meta.display_name) {
                row.display_name = meta.display_name;
            }
            if (meta.engine_version) {
                row.version = meta.engine_version;
            }
            row.priority = meta.priority;
            row.primitives.insert(rac_primitive_name(primitive));
        }
    }
    return engines;
}

void register_backends(CLI::App& app, GlobalOptions& options) {
    CLI::App* cmd = app.add_subcommand("backends", "List registered inference backends");
    cmd->callback([&options]() {
        Bootstrapped env;
        if (bootstrap(options, &env) != RAC_SUCCESS) {
            throw CLI::RuntimeError(1);
        }

        std::map<std::string, EngineRow> engines = collect_backend_rows();

        if (options.json) {
            out::JsonWriter json;
            json.begin_object().begin_array("backends");
            for (const auto& [name, row] : engines) {
                json.begin_array_object()
                    .field("name", name)
                    .field("display_name", row.display_name)
                    .field("version", row.version)
                    .field("priority", static_cast<int64_t>(row.priority));
                json.begin_array("primitives");
                for (const auto& primitive : row.primitives) {
                    json.begin_array_object().field("name", primitive).end_object();
                }
                json.end_array().end_object();
            }
            json.end_array().end_object();
            out::result_line(json.str());
            return;
        }

        if (engines.empty()) {
            out::result_line("no backends registered");
            return;
        }
        std::vector<std::vector<std::string>> rows;
        for (const auto& [name, row] : engines) {
            std::string primitives;
            for (const auto& primitive : row.primitives) {
                primitives += primitives.empty() ? primitive : ", " + primitive;
            }
            rows.push_back({name, std::to_string(row.priority), primitives});
        }
        out::table({"NAME", "PRIORITY", "PRIMITIVES"}, rows);
    });
}

}  // namespace wally::commands

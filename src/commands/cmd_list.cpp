/**
 * @file cmd_list.cpp
 * @brief `wally models list` (alias `wally models ls`) — downloaded models by
 *        default, the whole catalog with --all.
 *
 * The registry is refreshed with rescan_local so on-disk artifacts pulled by
 * previous runs (or by the test rig / playground tooling) are linked before
 * listing.
 */

#include "commands/commands.h"

#include <climits>
#include <map>
#include <memory>
#include <set>
#include <string>
#include <unordered_map>
#include <utility>
#include <vector>

#include "model_types.pb.h"
#include "rac/core/rac_core.h"
#include "rac/infrastructure/model_management/rac_model_registry.h"

#include "catalog/catalog.h"
#include "commands/model_setup.h"
#include "commands/model_labels.h"
#include "io/output.h"
#include "io/proto.h"

namespace wally::commands {

namespace {

namespace v1 = runanywhere::v1;

// The same model is registered once per backend it runs on (llama.cpp / MLX /
// ANE / NPU). `models list` collapses those into one row keyed by the catalog's
// merge_key, joining the backends into "mlx/llama.cpp"-style tags. Lower rank =
// listed first in the joined tag and preferred for the row's name/size.
int backend_rank(v1::InferenceFramework framework) {
    switch (framework) {
        case v1::INFERENCE_FRAMEWORK_MLX: return 0;
        case v1::INFERENCE_FRAMEWORK_LLAMA_CPP: return 1;
        case v1::INFERENCE_FRAMEWORK_COREML: return 2;
        case v1::INFERENCE_FRAMEWORK_QHEXRT: return 3;
        default: return 4;
    }
}

struct GroupedRow {
    std::string id;
    std::string name;
    v1::ModelCategory category = v1::MODEL_CATEGORY_UNSPECIFIED;
    int64_t size_bytes = 0;
    int name_rank = INT_MAX;   // rank of the variant that set name/category
    int size_rank = INT_MAX;   // rank of the variant that set a positive size
    bool downloaded = false;
    // Distinct backends, ordered by (rank, label) so the join is stable.
    std::set<std::pair<int, std::string>> backends;
};

// A short "how do I download one?" header for the human list. The pull id
// differs by backend, so show one example per backend this platform can run:
// llama.cpp everywhere; on Apple also MLX and ANE. Never printed in --json.
void print_pull_examples() {
    out::result_line("Download a model with `wally models pull <id>`:");
    out::result_line("  wally models pull qwen3-0.6b        # llama.cpp");
#if defined(__APPLE__)
    out::result_line("  wally models pull mlx-qwen3-0.6b    # MLX (Apple GPU)");
    out::result_line("  wally models pull ane-lfm2.5-350m   # ANE (Apple Neural Engine)");
#endif
    out::result_line("");
}

int run_list(const GlobalOptions& options, bool show_all) {
    Bootstrapped env;
    if (bootstrap(options, &env) != RAC_SUCCESS) {
        return 1;
    }

    std::string error;
    if (!refresh_registry(&error)) {
        out::status_line("warning: registry refresh failed: " + error);
    }

    // Full list + downloaded list; membership marks the DOWNLOADED column.
    rac_proto_buffer_t all_out;
    rac_proto_buffer_init(&all_out);
    v1::ModelInfoList all_models;
    const rac_result_t proto_rc = rac_model_registry_list_proto_buffer(rac_get_model_registry(), &all_out);
    if (!proto::parse_proto_buffer(&all_out, &all_models, &error) || proto_rc != RAC_SUCCESS) {
        out::error_line("failed to list models: " + error);
        return 1;
    }

    std::set<std::string> downloaded_ids;
    {
        rac_proto_buffer_t downloaded_out;
        rac_proto_buffer_init(&downloaded_out);
        v1::ModelInfoList downloaded;
        if (rac_model_registry_list_downloaded_proto_buffer(rac_get_model_registry(),
                                                            &downloaded_out) == RAC_SUCCESS &&
            proto::parse_proto_buffer(&downloaded_out, &downloaded, nullptr)) {
            for (const v1::ModelInfo& model : downloaded.models()) {
                downloaded_ids.insert(model.id());
            }
        }
    }

    // Collapse per-backend variants of the same model into one row, keyed by the
    // catalog merge_key (a non-catalog id keys as itself). Insertion order is
    // kept so the list reads the same as the registry.
    std::vector<std::string> order;
    std::unordered_map<std::string, GroupedRow> groups;
    for (const v1::ModelInfo& model : all_models.models()) {
        const bool is_downloaded =
            downloaded_ids.count(model.id()) > 0 ||
            model.registry_status() == v1::MODEL_REGISTRY_STATUS_DOWNLOADED;
        if (!show_all && !is_downloaded) {
            continue;
        }
        const std::string key = catalog::merge_key_for(model.id());
        auto it = groups.find(key);
        if (it == groups.end()) {
            GroupedRow row;
            row.id = key;
            it = groups.emplace(key, std::move(row)).first;
            order.push_back(key);
        }
        GroupedRow& row = it->second;
        const int rank = backend_rank(model.framework());
        row.backends.insert({rank, model_labels::short_backend(model.framework())});
        row.downloaded = row.downloaded || is_downloaded;
        if (rank < row.name_rank) {
            row.name_rank = rank;
            row.name = model.name();
            row.category = model.category();
        }
        const int64_t size = static_cast<int64_t>(model.download_size_bytes());
        if (size > 0 && rank < row.size_rank) {
            row.size_rank = rank;
            row.size_bytes = size;
        }
    }

    auto join_backends = [](const GroupedRow& row) {
        std::string joined;
        for (const auto& [rank, label] : row.backends) {
            (void)rank;
            if (!joined.empty()) {
                joined += "/";
            }
            joined += label;
        }
        return joined;
    };

    if (options.json) {
        out::JsonWriter json;
        json.begin_object().begin_array("models");
        for (const std::string& key : order) {
            const GroupedRow& row = groups.at(key);
            json.begin_array_object()
                .field("id", row.id)
                .field("name", row.name)
                .field("modality", model_labels::category(row.category))
                .field("backend", join_backends(row))
                .field("size_bytes", row.size_bytes)
                .field("downloaded", row.downloaded)
                .end_object();
        }
        json.end_array().end_object();
        out::result_line(json.str());
        return 0;
    }

    print_pull_examples();

    std::vector<std::vector<std::string>> rows;
    for (const std::string& key : order) {
        const GroupedRow& row = groups.at(key);
        rows.push_back({row.id, model_labels::category(row.category),
                        join_backends(row),
                        row.size_bytes > 0
                            ? out::human_bytes(static_cast<uint64_t>(row.size_bytes))
                            : "-",
                        row.downloaded ? "yes" : "no"});
    }

    if (rows.empty()) {
        out::result_line(show_all ? "no models registered"
                                  : "no models downloaded — try `wally models list --all` then "
                                    "`wally models pull <id>`");
        return 0;
    }
    out::table({"ID", "MODALITY", "BACKEND", "SIZE", "DOWNLOADED"}, rows);
    return 0;
}

}  // namespace

void configure_models_list(CLI::App* cmd, GlobalOptions& options) {
    auto show_all = std::make_shared<bool>(false);
    cmd->add_flag("--all,-a", *show_all, "Include catalog models not yet downloaded");
    cmd->callback([&options, show_all]() {
        const int exit_code = run_list(options, *show_all);
        if (exit_code != 0) {
            throw CLI::RuntimeError(exit_code);
        }
    });
}

}  // namespace wally::commands

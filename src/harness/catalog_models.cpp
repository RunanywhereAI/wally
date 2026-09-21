#include "harness/catalog_models.h"

#include "harness/local_models.h"

#include <algorithm>
#include <utility>

#include "account/console.h"

namespace wally::harness {
namespace {

/// The context size `harness::Resolve` starts a local server with.

/// Moves the entry whose id is `primary` to the front, or inserts a bare one
/// when the catalog did not carry it — the launched model is always selectable.
void PrimaryFirst(std::vector<CatalogModel>* models, const std::string& primary) {
    const auto it = std::find_if(models->begin(), models->end(),
                                 [&](const CatalogModel& m) { return m.id == primary; });
    if (it != models->end()) {
        std::rotate(models->begin(), it, it + 1);
    } else {
        models->insert(models->begin(), CatalogModel{primary, 0, 0, 0, 0});
    }
}

}  // namespace

std::vector<CatalogModel> CatalogModels(const account::ConsoleClient& console,
                                        const std::string& console_url,
                                        const std::string& access_token,
                                        const std::string& primary) {
    std::vector<CatalogModel> out;

    std::vector<account::ModelInfo> models;
    std::string error;
    console.FetchModels(console_url, access_token, &models, &error);

    std::vector<account::CatalogPrice> prices;
    console.FetchCatalog(console_url, access_token, &prices, &error);
    const auto price_for = [&prices](const std::string& id) -> std::pair<std::int64_t, std::int64_t> {
        for (const account::CatalogPrice& price : prices) {
            if (price.id == id) {
                return {price.input_per_mtok, price.output_per_mtok};
            }
        }
        return {0, 0};
    };

    for (const account::ModelInfo& info : models) {
        if (info.id.empty()) {
            continue;
        }
        const auto [in_price, out_price] = price_for(info.id);
        out.push_back(CatalogModel{info.id, info.context_window, info.max_output_tokens, in_price,
                                   out_price});
    }
    PrimaryFirst(&out, primary);
    return out;
}

std::vector<CatalogModel> CatalogModels(const std::string& console_url,
                                        const std::string& access_token,
                                        const std::string& primary) {
    const account::ConsoleClient console;
    return CatalogModels(console, console_url, access_token, primary);
}

std::vector<CatalogModel> CatalogModels(const Endpoint& endpoint, const std::string& primary) {
    if (endpoint.api_key.empty()) {
        return {CatalogModel{primary, LocalContextSize(primary), 0, 0, 0}};
    }
    return CatalogModels(endpoint.console_url, endpoint.api_key, primary);
}

}  // namespace wally::harness

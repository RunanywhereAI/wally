/**
 * @file catalog_models.h
 * @brief The console's model catalog, shaped for a harness's model picker.
 *
 * A harness shows the models its provider config lists, so wally writes every
 * model the console advertises rather than only the one the person launched.
 * The launched model stays first so it remains the default.
 */

#ifndef WALLY_HARNESS_CATALOG_MODELS_H
#define WALLY_HARNESS_CATALOG_MODELS_H

#include <cstdint>
#include <string>
#include <vector>

#include "account/console.h"
#include "harness/harness.h"

namespace wally::harness {

/// One catalog model with the window and price a harness needs to declare it.
/// A zero field means the catalog did not carry that number.
struct CatalogModel {
    std::string id;
    std::int64_t context_window = 0;
    std::int64_t max_output = 0;
    std::int64_t input_per_mtok = 0;
    std::int64_t output_per_mtok = 0;
};

/// Every hosted model the console advertises, each with its limits and price,
/// `primary` first so it stays the harness default. Fetches the live catalog
/// through `console` from `console_url` with `access_token`; `primary` is always
/// present even if the catalog does not name it. The `console` overload is the
/// test seam.
std::vector<CatalogModel> CatalogModels(const account::ConsoleClient& console,
                                        const std::string& console_url,
                                        const std::string& access_token,
                                        const std::string& primary);
std::vector<CatalogModel> CatalogModels(const std::string& console_url,
                                        const std::string& access_token,
                                        const std::string& primary);

/// The same, resolved from a launch `endpoint`. A local endpoint (empty
/// `api_key`) has no catalog, so it yields just `primary` at the context size a
/// local server is started with.
std::vector<CatalogModel> CatalogModels(const Endpoint& endpoint, const std::string& primary);

}  // namespace wally::harness

#endif  // WALLY_HARNESS_CATALOG_MODELS_H

#include "harness/declared_harness.h"

#include <algorithm>

#include <nlohmann/json.hpp>

#ifndef WALLY_VERSION
#define WALLY_VERSION "0.0.0-dev"
#endif

namespace wally::harness {

std::string HarnessHeaderValue(DeclaredHarness harness) {
    // The generated to_json is the one place the wire spelling lives.
    return nlohmann::json(harness).get<std::string>();
}

std::string UpstreamUserAgent(DeclaredHarness harness) {
    std::string needle = HarnessHeaderValue(harness);
    std::replace(needle.begin(), needle.end(), '_', '-');
    return std::string("wally/") + WALLY_VERSION + " (" + needle + ")";
}

}  // namespace wally::harness

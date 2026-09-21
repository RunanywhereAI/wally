#ifndef WALLY_HARNESS_LOCAL_MODELS_H
#define WALLY_HARNESS_LOCAL_MODELS_H

#include <cstdint>
#include <string>
#include <vector>

namespace wally::harness {

/// A model already on disk under the RunAnywhere home.
struct LocalModel {
    std::string id;
    /// The engine directory it was found in: LlamaCpp, Sherpa, MLX, ...
    std::string framework;
    /// The model's own directory.
    std::string dir;
    /// The first weight file inside it, or empty for a model whose weights are
    /// directories (CoreML .mlmodelc) rather than files.
    std::string path;
    std::int64_t bytes = 0;
};

/// Models present under `home` right now, found by walking the storage tree.
///
/// Walking rather than asking the registry: this only has to answer "is there
/// something here the local server can open", and the walk says that about a
/// model placed by hand as readily as one that was downloaded.
std::vector<LocalModel> LocalModels(const std::string& home);

/// The context window a local server is started with for `model_id`, in
/// tokens. Sized from this machine's memory in tiers (8k / 16k / 32k / 64k),
/// because a coding agent's first request is a 15k-token system prompt and a
/// fixed 8k window rejected it outright. Capped at the model's own window when
/// the catalog knows it, never below 8192, which is what every launch used
/// before. One function, so the server, the picker's declared limits, and the
/// shim all quote the same number.
std::int64_t LocalContextSize(const std::string& model_id);

}  // namespace wally::harness

#endif  // WALLY_HARNESS_LOCAL_MODELS_H

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

/// Parameter count in billions, read off the id: `bonsai-27b` -> 27,
/// `gemma-4-26b-a4b` -> 26, `lfm2.5-1.2b` -> 1.2. Only a whole `-` separated
/// token of the form `<number>b` counts, so `e2b` / `a4b` (Gemma's effective
/// and active sizes) and `q1_0` are skipped. 0 when the id carries no size.
double ParameterBillions(const std::string& model_id);

/// The smallest model a coding tool is worth launching against. Smaller ones
/// answer, but not well enough to edit code, so `Resolve` refuses them for a
/// local launch and `models list` tags the ones at or above it.
constexpr double kHarnessMinBillions = 20.0;

/// Whether a model of `weight_bytes` runs on this machine, judged against
/// physical memory: the weights, a fifth again for the runtime's working set,
/// and a gigabyte of KV cache. Fits leaves a quarter of RAM free; Tight loads
/// but swaps under a coding agent; TooBig does not load; Unknown when either
/// number is missing.
enum class Fit { Fits, Tight, TooBig, Unknown };
Fit FitFor(std::int64_t weight_bytes, std::uint64_t total_memory);
const char* FitLabel(Fit fit);

/// Physical memory on this machine, 0 when the platform adapter cannot say.
std::uint64_t TotalPhysicalMemory();

/// Whether the id names a 1-bit quantisation (`-1bit`, `-q1_0`, `-q1_k`, ...).
/// Those models answer, but degrade badly under a coding agent's long, tool
/// heavy prompts, so they are kept out of harness launches whatever their size.
bool IsOneBitQuant(const std::string& model_id);

/// The one rule behind `[harness-compatible]` in `models list` and the refusal
/// in `Resolve`: a known parameter count at or above kHarnessMinBillions, not a
/// 1-bit quant, and weights that fit here. `why` (optional) receives the reason
/// it is not.
bool HarnessCompatible(const std::string& model_id, std::int64_t weight_bytes,
                       std::uint64_t total_memory, std::string* why = nullptr);

}  // namespace wally::harness

#endif  // WALLY_HARNESS_LOCAL_MODELS_H

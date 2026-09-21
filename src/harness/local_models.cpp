#include "harness/local_models.h"

#include <algorithm>
#include <cstdlib>
#include <filesystem>
#include <string_view>
#include <system_error>

#include "rac/core/rac_platform_adapter.h"

#include "catalog/catalog.h"

namespace wally::harness {
namespace {

namespace fs = std::filesystem;

bool IsWeightFile(const fs::path& path) {
    const std::string ext = path.extension().string();
    return ext == ".gguf" || ext == ".safetensors" || ext == ".onnx" || ext == ".bin";
}

/// Written by the download orchestrator as files land rather than once they all
/// have, so a partial download carries one too. Presence means "this came from
/// a download", not "the download finished".
constexpr std::string_view kManifest = ".rac-manifest.binpb";

}  // namespace

std::vector<LocalModel> LocalModels(const std::string& home) {
    std::vector<LocalModel> models;
    if (home.empty()) {
        return models;
    }

    // Both layouts are real: the SDK's path docs describe
    // {base}/RunAnywhere/Models, and the desktop default base directory already
    // ends in "runanywhere", so models land directly under {base}/Models.
    std::error_code ec;
    fs::path root = fs::path(home) / "RunAnywhere" / "Models";
    if (!fs::is_directory(root, ec)) {
        root = fs::path(home) / "Models";
    }
    if (!fs::is_directory(root, ec)) {
        return models;
    }

    // Every step of the walk takes an error_code, including the increments.
    // Constructing the iterator with one and then advancing it with the
    // range-for's throwing operator++ only looks safe: a directory removed or
    // locked while this runs escapes as a filesystem_error out of what is a
    // best-effort listing, and this is called from the launch path.
    //
    // `walk` guards iteration and stops that level when advancing fails.
    // `probe` is for the status queries, whose false-on-error answer is already
    // the one we want, so it is reset rather than checked.
    constexpr auto kSkip = fs::directory_options::skip_permission_denied;
    std::error_code walk;
    std::error_code probe;

    for (fs::directory_iterator framework(root, kSkip, walk), no_more_frameworks;
         !walk && framework != no_more_frameworks; framework.increment(walk)) {
        if (!framework->is_directory(probe)) {
            probe.clear();
            continue;
        }
        std::error_code entries;
        for (fs::directory_iterator entry(framework->path(), kSkip, entries), no_more_entries;
             !entries && entry != no_more_entries; entry.increment(entries)) {
            if (!entry->is_directory(probe)) {
                probe.clear();
                continue;
            }
            std::string weights;
            bool manifest = false;
            std::int64_t bytes = 0;
            std::error_code files;
            for (fs::recursive_directory_iterator file(entry->path(), kSkip, files),
                 no_more_files;
                 !files && file != no_more_files; file.increment(files)) {
                if (!file->is_regular_file(probe)) {
                    probe.clear();
                    continue;
                }
                // A file that vanished between the listing and the stat reports
                // an error and an unspecified size; adding that unchecked cast
                // a -1 into a wildly wrong total.
                const std::uintmax_t size = file->file_size(probe);
                if (!probe) {
                    bytes += static_cast<std::int64_t>(size);
                }
                probe.clear();
                if (file->path().filename() == kManifest) {
                    manifest = true;
                } else if (weights.empty() && IsWeightFile(file->path())) {
                    weights = file->path().string();
                }
            }
            // A directory holding weights but no manifest was placed by hand.
            // It is still loadable, so it still counts.
            if (!manifest && weights.empty()) {
                continue;
            }
            models.push_back({entry->path().filename().string(),
                              framework->path().filename().string(), entry->path().string(),
                              weights, bytes});
        }
    }

    std::sort(models.begin(), models.end(),
              [](const LocalModel& a, const LocalModel& b) { return a.id < b.id; });
    return models;
}

std::int64_t LocalContextSize(const std::string& model_id) {
    constexpr std::int64_t kFloor = 8192;
    constexpr std::int64_t kGiB = 1024LL * 1024 * 1024;

    // Physical memory decides the tier. On Apple Silicon this is the unified
    // pool the GPU draws from too, which is why it stands in for VRAM here.
    std::int64_t tier = kFloor;
    const rac_platform_adapter_t* adapter = rac_get_platform_adapter();
    rac_memory_info_t memory{};
    if (adapter != nullptr && adapter->get_memory_info != nullptr &&
        adapter->get_memory_info(&memory, adapter->user_data) == RAC_SUCCESS &&
        memory.total_bytes > 0) {
        const std::int64_t total = static_cast<std::int64_t>(memory.total_bytes);
        if (total >= 48 * kGiB) {
            tier = 65536;
        } else if (total >= 24 * kGiB) {
            tier = 32768;
        } else if (total >= 12 * kGiB) {
            tier = 16384;
        }
    }

    // The model's own window caps it: asking llama.cpp for more than the model
    // was trained on stretches RoPE and degrades every answer. 0 means the
    // catalog does not know, and a model that is not in the catalog at all
    // (an hf.co ref, a hand-placed folder) gets the tier as is.
    std::int64_t window = tier;
    if (const catalog::CatalogEntry* entry = catalog::find(model_id)) {
        if (entry->context_length > 0) {
            window = std::min(tier, static_cast<std::int64_t>(entry->context_length));
        }
    }
    return std::max(kFloor, window);
}

Fit FitFor(std::int64_t weight_bytes, std::uint64_t total_memory) {
    if (weight_bytes <= 0 || total_memory == 0) {
        return Fit::Unknown;
    }
    constexpr double kOverhead = 1.2;
    constexpr double kKvCacheBytes = 1.0 * 1024 * 1024 * 1024;
    const double needed = static_cast<double>(weight_bytes) * kOverhead + kKvCacheBytes;
    const double total = static_cast<double>(total_memory);
    if (needed <= total * 0.75) return Fit::Fits;
    if (needed <= total) return Fit::Tight;
    return Fit::TooBig;
}

const char* FitLabel(Fit fit) {
    switch (fit) {
        case Fit::Fits: return "fits";
        case Fit::Tight: return "tight";
        case Fit::TooBig: return "too big";
        case Fit::Unknown: return "-";
    }
    return "-";
}

std::uint64_t TotalPhysicalMemory() {
    const rac_platform_adapter_t* adapter = rac_get_platform_adapter();
    rac_memory_info_t memory{};
    if (adapter != nullptr && adapter->get_memory_info != nullptr &&
        adapter->get_memory_info(&memory, adapter->user_data) == RAC_SUCCESS) {
        return memory.total_bytes;
    }
    return 0;
}

bool IsOneBitQuant(const std::string& model_id) {
    // A whole `-` separated token: `1bit`, or `q1` optionally followed by
    // `_<n>` / `_k...` (llama.cpp's Q1_0, Q1_K naming lower-cased in ids).
    std::string token;
    for (size_t i = 0; i <= model_id.size(); ++i) {
        if (i < model_id.size() && model_id[i] != '-') {
            token += model_id[i];
            continue;
        }
        if (token == "1bit" || token == "q1" || token.rfind("q1_", 0) == 0) {
            return true;
        }
        token.clear();
    }
    return false;
}

bool HarnessCompatible(const std::string& model_id, std::int64_t weight_bytes,
                       std::uint64_t total_memory, std::string* why) {
    const double billions = ParameterBillions(model_id);
    if (billions <= 0) {
        if (why != nullptr) {
            *why = "its size is not in its name, so it cannot be judged";
        }
        return false;
    }
    if (IsOneBitQuant(model_id)) {
        if (why != nullptr) {
            *why = "1-bit quantisations do not hold up under a coding agent";
        }
        return false;
    }
    if (billions < kHarnessMinBillions) {
        if (why != nullptr) {
            char size[16];
            std::snprintf(size, sizeof(size), billions < 1 ? "%.1fB" : "%.0fB", billions);
            *why = std::string("it is ") + size + "; coding tools need " +
                   std::to_string(static_cast<int>(kHarnessMinBillions)) + "B+";
        }
        return false;
    }
    const Fit fit = FitFor(weight_bytes, total_memory);
    if (fit == Fit::TooBig || fit == Fit::Tight) {
        if (why != nullptr) {
            *why = fit == Fit::TooBig ? "it does not fit in this machine's memory"
                                      : "it would swap under a coding agent on this machine";
        }
        return false;
    }
    return true;
}

double ParameterBillions(const std::string& model_id) {
    std::string token;
    for (size_t i = 0; i <= model_id.size(); ++i) {
        if (i < model_id.size() && model_id[i] != '-') {
            token += model_id[i];
            continue;
        }
        if (token.size() >= 2 && token.back() == 'b') {
            const std::string number = token.substr(0, token.size() - 1);
            char* end = nullptr;
            const double value = std::strtod(number.c_str(), &end);
            if (end != nullptr && *end == '\0' && value > 0) {
                return value;
            }
        }
        token.clear();
    }
    return 0;
}

}  // namespace wally::harness

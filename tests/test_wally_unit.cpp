/**
 * @file test_wally_unit.cpp
 * @brief wally unit tests — pure helpers, no models, no network.
 *
 * Uses the commons TestSuite harness so the Docker rig and ctest drive every
 * suite the same way (--run-all / --test-<name>).
 */

#include "test_common.h"

#include <CLI11.hpp>

#include <chrono>
#include <cstdint>
#include <cstdlib>
#include <cstring>
#include <filesystem>
#include <fstream>
#include <initializer_list>
#include <limits>
#include <random>
#include <string>
#include <system_error>
#include <vector>

#include "llm_options.pb.h"
#include "model_types.pb.h"
#include "vlm_options.pb.h"
#include "rac/core/rac_core.h"
#include "rac/foundation/rac_proto_buffer.h"
#include "rac/infrastructure/model_management/rac_model_registry.h"

#include "anthropic/translate.h"
#include "app.h"
#include "net/loopback_auth.h"
#include "catalog/catalog.h"
#include "catalog/model_ref.h"
#include "commands/bench_metrics.h"
#include "commands/engine_options.h"
#include "commands/model_labels.h"
#include "config/cli_paths.h"
#include "config/preferences.h"
#include "harness/harness.h"
#include "io/image_io.h"
#include "io/output.h"
#include "io/proto.h"

namespace {

// setenv/unsetenv helper that restores prior state on scope exit.
class EnvVar {
public:
  EnvVar(const char *name, const char *value) : name_(name) {
    if (const char *prev = std::getenv(name)) {
      had_prev_ = true;
      prev_ = prev;
    }
    if (value) {
#if defined(_WIN32)
      _putenv_s(name, value);
#else
      setenv(name, value, 1);
#endif
    } else {
#if defined(_WIN32)
      _putenv_s(name, "");
#else
      unsetenv(name);
#endif
    }
  }
  ~EnvVar() {
    if (had_prev_) {
#if defined(_WIN32)
      _putenv_s(name_.c_str(), prev_.c_str());
#else
      setenv(name_.c_str(), prev_.c_str(), 1);
#endif
    } else {
#if defined(_WIN32)
      _putenv_s(name_.c_str(), "");
#else
      unsetenv(name_.c_str());
#endif
    }
  }

private:
  std::string name_;
  std::string prev_;
  bool had_prev_ = false;
};

TestResult test_json_escape() {
  TestResult result;
  result.test_name = "json_escape";

  struct Case {
    std::string in;
    std::string expected;
  };
  const Case cases[] = {
      {"plain", "plain"},
      {"quote\"backslash\\", "quote\\\"backslash\\\\"},
      {"line\nbreak\ttab", "line\\nbreak\\ttab"},
      {std::string("ctl\x01", 4), "ctl\\u0001"},
  };
  for (const Case &c : cases) {
    const std::string actual = wally::out::json_escape(c.in);
    if (actual != c.expected) {
      result.expected = c.expected;
      result.actual = actual;
      return result;
    }
  }
  result.passed = true;
  return result;
}

TestResult test_json_writer_shape() {
  TestResult result;
  result.test_name = "json_writer_shape";

  wally::out::JsonWriter json;
  json.begin_object()
      .field("name", "qwen3-0.6b")
      .field("size", static_cast<int64_t>(640))
      .field("downloaded", true);
  json.begin_array("files");
  json.begin_array_object().field("path", "a.gguf").end_object();
  json.begin_array_object().field("path", "b.gguf").end_object();
  json.end_array();
  json.begin_array("scores").value(1.0).value(0.5).end_array();
  json.end_object();

  const std::string expected =
      R"({"name":"qwen3-0.6b","size":640,"downloaded":true,)"
      R"("files":[{"path":"a.gguf"},{"path":"b.gguf"}],)"
      R"("scores":[1,0.5]})";
  if (json.str() != expected) {
    result.expected = expected;
    result.actual = json.str();
    return result;
  }
  result.passed = true;
  return result;
}

TestResult test_json_writer_nan_is_null() {
  TestResult result;
  result.test_name = "json_writer_nan_is_null";

  // A NaN/Inf double (e.g. sherpa's unset STT confidence) has no JSON
  // literal; %g used to print it verbatim as the bareword `nan`, producing
  // invalid JSON on every stt --json call.
  wally::out::JsonWriter json;
  const double nan = std::numeric_limits<double>::quiet_NaN();
  const double inf = std::numeric_limits<double>::infinity();
  json.begin_object().field("confidence", nan).field("gain", inf);
  json.begin_array("scores").value(nan).value(0.5).end_array();
  json.end_object();

  const std::string expected = R"({"confidence":null,"gain":null,"scores":[null,0.5]})";
  if (json.str() != expected) {
    result.expected = expected;
    result.actual = json.str();
    return result;
  }
  result.passed = true;
  return result;
}

TestResult test_human_bytes() {
  TestResult result;
  result.test_name = "human_bytes";

  struct Case {
    uint64_t in;
    std::string expected;
  };
  const Case cases[] = {
      {512, "512 B"},
      {2048, "2.0 KB"},
      {640ull * 1024 * 1024, "640.0 MB"},
      {3ull * 1024 * 1024 * 1024, "3.0 GB"},
  };
  for (const Case &c : cases) {
    const std::string actual = wally::out::human_bytes(c.in);
    if (actual != c.expected) {
      result.expected = c.expected;
      result.actual = actual;
      return result;
    }
  }
  result.passed = true;
  return result;
}

TestResult test_normalize_dir() {
  TestResult result;
  result.test_name = "normalize_dir";

  if (wally::paths::normalize_dir("/a/b/") != "/a/b" ||
      wally::paths::normalize_dir("/a/b///") != "/a/b" ||
      wally::paths::normalize_dir("/") != "/" ||
      !wally::paths::normalize_dir("").empty()) {
    result.details = "trailing-slash handling broken";
    return result;
  }
  result.passed = true;
  return result;
}

TestResult test_resolve_home_precedence() {
  TestResult result;
  result.test_name = "resolve_home_precedence";

  {
    // Flag override wins over env.
    EnvVar env("RUNANYWHERE_HOME", "/from-env/runanywhere");
    if (wally::paths::resolve_home("/from-flag/runanywhere/") !=
        "/from-flag/runanywhere") {
      result.details = "flag override should win and be normalized";
      return result;
    }
    if (wally::paths::resolve_home("") != "/from-env/runanywhere") {
      result.details = "env should win when no flag given";
      return result;
    }
  }
  {
    // Default: XDG data dir under runanywhere.
    EnvVar env("RUNANYWHERE_HOME", nullptr);
#if defined(_WIN32)
    EnvVar local("LOCALAPPDATA", "C:/wally-local");
    const std::string home = wally::paths::resolve_home("");
    if (home != "C:/wally-local/RunAnywhere") {
      result.details = "expected C:/wally-local/RunAnywhere, got " + home;
      return result;
    }
  }
  {
    // A real LOCALAPPDATA is backslash-separated, and the SDK's default base
    // dir appends "/RunAnywhere" to it as-is.
    EnvVar env("RUNANYWHERE_HOME", nullptr);
    EnvVar local("LOCALAPPDATA", "C:\\wally-local");
    const std::string home = wally::paths::resolve_home("");
    if (home != "C:/wally-local/RunAnywhere") {
      result.details = "expected C:/wally-local/RunAnywhere, got " + home;
      return result;
    }
#else
    EnvVar xdg("XDG_DATA_HOME", "/xdg-data");
    const std::string home = wally::paths::resolve_home("");
    if (home != "/xdg-data/runanywhere") {
      result.details = "expected /xdg-data/runanywhere, got " + home;
      return result;
    }
#endif
  }
  result.passed = true;
  return result;
}

TestResult test_state_dir() {
  TestResult result;
  result.test_name = "state_dir";

  {
    EnvVar xdg("XDG_STATE_HOME", "/xdg-state");
    if (wally::paths::state_dir() != "/xdg-state/runanywhere") {
      result.details = "XDG_STATE_HOME not honored";
      return result;
    }
  }
#if defined(_WIN32)
  {
    // A real LOCALAPPDATA is backslash-separated while the suffix appended to
    // it is not, so the join must not leave mixed separators behind. Note the
    // backslashes here: the resolve_home case above passes a '/'-style value,
    // which is why this never surfaced.
    EnvVar xdg("XDG_STATE_HOME", nullptr);
    EnvVar home("HOME", nullptr);
    EnvVar local("LOCALAPPDATA", "C:\\wally-local");
    const std::string state = wally::paths::state_dir();
    if (state != "C:/wally-local/RunAnywhere/state") {
      result.details = "expected C:/wally-local/RunAnywhere/state, got " + state;
      return result;
    }
  }
#endif
  result.passed = true;
  return result;
}

TestResult test_catalog_lookup() {
  TestResult result;
  result.test_name = "catalog_lookup";

  size_t count = 0;
  const wally::catalog::CatalogEntry *entries = wally::catalog::all(&count);
  if (!entries || count < 10) {
    result.details = "catalog unexpectedly small";
    return result;
  }

  const wally::catalog::CatalogEntry *by_id = wally::catalog::find("qwen3-0.6b");
  const wally::catalog::CatalogEntry *by_alias = wally::catalog::find("qwen3");
  if (!by_id || by_id != by_alias) {
    result.details = "alias lookup should resolve to the same entry";
    return result;
  }
  if (wally::catalog::find("definitely-not-a-model") != nullptr) {
    result.details = "unknown id should return nullptr";
    return result;
  }
  if (wally::catalog::suggestions("qwen", 3).empty()) {
    result.details = "expected suggestions for 'qwen'";
    return result;
  }

  // Multi-file entries (VLM pairs, embeddings) must carry ≥2 required files.
  // smolvlm2 is a VLM, out of scope for the LLM-only cut (src/app.cpp,
  // src/catalog/catalog.cpp) -- commented out, not deleted, so it comes back
  // when the cut reverts.
  // const wally::catalog::CatalogEntry *vlm = wally::catalog::find("smolvlm2");
  // if (!vlm || vlm->files == nullptr || vlm->file_count != 2) {
  //   result.details = "smolvlm2 should be a two-file artifact";
  //   return result;
  // }

  const wally::catalog::CatalogEntry *mlx_llm = wally::catalog::find("mlx-qwen3");
  if (!mlx_llm ||
      mlx_llm->framework != runanywhere::v1::INFERENCE_FRAMEWORK_MLX ||
      mlx_llm->format != runanywhere::v1::MODEL_FORMAT_SAFETENSORS ||
      mlx_llm->category != runanywhere::v1::MODEL_CATEGORY_LANGUAGE ||
      mlx_llm->files == nullptr || mlx_llm->file_count != 9 ||
      !mlx_llm->supports_thinking) {
    result.details = "mlx-qwen3 should be a complete MLX language bundle";
    return result;
  }

  const wally::catalog::CatalogEntry *maple_gguf =
      wally::catalog::find("maple-preview");
  if (!maple_gguf ||
      maple_gguf->framework != runanywhere::v1::INFERENCE_FRAMEWORK_LLAMA_CPP ||
      maple_gguf->format != runanywhere::v1::MODEL_FORMAT_GGUF ||
      maple_gguf->download_size_bytes != 4984016416LL ||
      maple_gguf->context_length != 4096 || !maple_gguf->supports_thinking) {
    result.details = "maple-preview should resolve to the pinned GGUF bundle";
    return result;
  }

  const wally::catalog::CatalogEntry *mlx_maple =
      wally::catalog::find("mlx-maple-preview");
  if (!mlx_maple ||
      mlx_maple->framework != runanywhere::v1::INFERENCE_FRAMEWORK_MLX ||
      mlx_maple->format != runanywhere::v1::MODEL_FORMAT_SAFETENSORS ||
      mlx_maple->category != runanywhere::v1::MODEL_CATEGORY_LANGUAGE ||
      mlx_maple->files == nullptr || mlx_maple->file_count != 13 ||
      mlx_maple->download_size_bytes != 5330252282LL ||
      mlx_maple->context_length != 128000) {
    result.details = "mlx-maple-preview should be a complete pinned MLX bundle";
    return result;
  }
  int64_t mlx_maple_file_total = 0;
  for (size_t i = 0; i < mlx_maple->file_count; ++i) {
    const wally::catalog::CatalogFile &file = mlx_maple->files[i];
    if (file.size_bytes <= 0 ||
        std::string(file.url).find(
            "/resolve/d0a7314d6bf14c880201b599d7a701cfbc8717e6/") ==
            std::string::npos) {
      result.details = "mlx-maple-preview files must use the pinned revision";
      return result;
    }
    mlx_maple_file_total += file.size_bytes;
  }
  if (mlx_maple_file_total != mlx_maple->download_size_bytes) {
    result.details = "mlx-maple-preview file sizes must sum to the bundle size";
    return result;
  }

  // VLM (multimodal) and embedding catalog entries are out of scope for the
  // LLM-only cut (src/app.cpp, src/catalog/catalog.cpp) -- commented out, not
  // deleted, so this comes back when the cut reverts.
  /*
  const wally::catalog::CatalogEntry *mlx_vlm =
      wally::catalog::find("mlx-qwen2-vl");
  if (!mlx_vlm ||
      mlx_vlm->category != runanywhere::v1::MODEL_CATEGORY_MULTIMODAL ||
      mlx_vlm->framework != runanywhere::v1::INFERENCE_FRAMEWORK_MLX ||
      mlx_vlm->files == nullptr || mlx_vlm->file_count != 11) {
    result.details = "mlx-qwen2-vl should be a complete MLX VLM bundle";
    return result;
  }
  bool has_preprocessor = false;
  for (size_t i = 0; i < mlx_vlm->file_count; ++i) {
    has_preprocessor =
        has_preprocessor ||
        std::string(mlx_vlm->files[i].filename) == "preprocessor_config.json";
  }
  if (!has_preprocessor) {
    result.details =
        "MLX VLM catalog entry must include preprocessor_config.json";
    return result;
  }

  const wally::catalog::CatalogEntry *mlx_fastvlm =
      wally::catalog::find("mlx-fastvlm");
  if (!mlx_fastvlm ||
      mlx_fastvlm->category != runanywhere::v1::MODEL_CATEGORY_MULTIMODAL ||
      mlx_fastvlm->framework != runanywhere::v1::INFERENCE_FRAMEWORK_MLX ||
      mlx_fastvlm->files == nullptr || mlx_fastvlm->file_count != 14) {
    result.details = "mlx-fastvlm should be a complete MLX VLM bundle";
    return result;
  }
  bool has_processor_config = false;
  bool has_fastvlm_companion = false;
  for (size_t i = 0; i < mlx_fastvlm->file_count; ++i) {
    const std::string filename = mlx_fastvlm->files[i].filename;
    has_processor_config =
        has_processor_config || filename == "processor_config.json";
    has_fastvlm_companion =
        has_fastvlm_companion ||
        (!mlx_fastvlm->files[i].required &&
         (filename == "processing_fastvlm.py" || filename == "llava_qwen.py"));
  }
  if (!has_processor_config || !has_fastvlm_companion) {
    result.details = "MLX FastVLM catalog entry must include processor config "
                     "and companions";
    return result;
  }

  const wally::catalog::CatalogEntry *mlx_embed =
      wally::catalog::find("mlx-qwen3-embed");
  if (!mlx_embed ||
      mlx_embed->category != runanywhere::v1::MODEL_CATEGORY_EMBEDDING ||
      mlx_embed->framework != runanywhere::v1::INFERENCE_FRAMEWORK_MLX ||
      mlx_embed->files == nullptr || mlx_embed->file_count != 11) {
    result.details =
        "mlx-qwen3-embed should be a complete MLX embedding bundle";
    return result;
  }

  struct PortableNvidiaEmbeddingCase {
    const char *id;
    const char *alias;
    const char *revision;
    int64_t download_size_bytes;
  };
  const PortableNvidiaEmbeddingCase portable_nvidia_embeddings[] = {
      {"nemotron-3-embed-1b-q4_k_m", "nemotron-3-embed",
       "06df1fde6f7009c91f6cc3cd520081921929a678", 749352096LL},
      {"llama-nemotron-embed-1b-v2-q4_k_m", "llama-nemotron-embed",
       "bf7c9832b1d76f86777379e58b7b74805ee58006", 807690624LL},
      {"llama-embed-nemotron-8b-q4_k_m", "llama-embed-nemotron",
       "e7ae3cbae4f7693bbd75ec959bf293f39e1f2e25", 4625233184LL},
  };
  for (const PortableNvidiaEmbeddingCase &test_case :
       portable_nvidia_embeddings) {
    const wally::catalog::CatalogEntry *entry =
        wally::catalog::find(test_case.id);
    if (!entry || entry != wally::catalog::find(test_case.alias) ||
        entry->category != runanywhere::v1::MODEL_CATEGORY_EMBEDDING ||
        entry->framework != runanywhere::v1::INFERENCE_FRAMEWORK_LLAMA_CPP ||
        entry->format != runanywhere::v1::MODEL_FORMAT_GGUF ||
        entry->files != nullptr || entry->url == nullptr ||
        entry->download_size_bytes != test_case.download_size_bytes ||
        std::string(entry->url).find(test_case.revision) == std::string::npos) {
      result.details = std::string(test_case.id) +
                       " should be an exact pinned llama.cpp embedding";
      return result;
    }
  }
  */

  const wally::catalog::CatalogEntry *nemotron_nano =
      wally::catalog::find("mlx-nemotron-nano");
  if (!nemotron_nano ||
      nemotron_nano->category != runanywhere::v1::MODEL_CATEGORY_LANGUAGE ||
      nemotron_nano->framework != runanywhere::v1::INFERENCE_FRAMEWORK_MLX ||
      nemotron_nano->files == nullptr || nemotron_nano->file_count != 8 ||
      nemotron_nano->download_size_bytes != 4534806075LL ||
      nemotron_nano->context_length != 131072) {
    result.details = "mlx-nemotron-nano should be a complete pinned MLX bundle";
    return result;
  }

  const wally::catalog::CatalogEntry *nemotron_mini =
      wally::catalog::find("mlx-nemotron-mini");
  if (!nemotron_mini ||
      nemotron_mini->category != runanywhere::v1::MODEL_CATEGORY_LANGUAGE ||
      nemotron_mini->framework != runanywhere::v1::INFERENCE_FRAMEWORK_MLX ||
      nemotron_mini->format != runanywhere::v1::MODEL_FORMAT_SAFETENSORS ||
      nemotron_mini->files == nullptr || nemotron_mini->file_count != 6 ||
      nemotron_mini->download_size_bytes != 2392679103LL ||
      nemotron_mini->context_length != 4096) {
    result.details = "mlx-nemotron-mini should be a complete pinned MLX bundle";
    return result;
  }
  for (size_t i = 0; i < nemotron_mini->file_count; ++i) {
    if (std::string(nemotron_mini->files[i].url)
            .find("/resolve/b5784198153d2d71afcc97d4cc38c049abced8cd/") ==
        std::string::npos) {
      result.details = "mlx-nemotron-mini files must use the pinned revision";
      return result;
    }
  }

  // Speech recognition entries are out of scope for the LLM-only cut
  // (src/app.cpp, src/catalog/catalog.cpp) -- commented out, not deleted, so
  // this comes back when the cut reverts.
  /*
  struct NvidiaSpeechCase {
    const char *alias;
    int64_t download_size_bytes;
  };
  const NvidiaSpeechCase nvidia_speech_cases[] = {
      {"mlx-parakeet-ctc", 4250718357LL},
      {"mlx-parakeet-tdt-v2", 2471596080LL},
      {"mlx-parakeet-tdt-v3", 2508532829LL},
      {"mlx-parakeet-rnnt", 4282283914LL},
      {"mlx-nemotron-asr", 755758528LL},
  };
  for (const NvidiaSpeechCase &test_case : nvidia_speech_cases) {
    const wally::catalog::CatalogEntry *entry =
        wally::catalog::find(test_case.alias);
    if (!entry ||
        entry->category != runanywhere::v1::MODEL_CATEGORY_SPEECH_RECOGNITION ||
        entry->framework != runanywhere::v1::INFERENCE_FRAMEWORK_MLX ||
        entry->format != runanywhere::v1::MODEL_FORMAT_SAFETENSORS ||
        entry->files == nullptr || entry->file_count != 2 ||
        entry->download_size_bytes != test_case.download_size_bytes ||
        std::string(entry->files[0].url).find("/resolve/") ==
            std::string::npos) {
      result.details = std::string(test_case.alias) +
                       " should be a complete pinned MLX speech bundle";
      return result;
    }
  }
  */

  result.passed = true;
  return result;
}

TestResult test_overlay_catalog() {
  TestResult result;
  result.test_name = "overlay_catalog";

  struct Row {
    const char *id;
    const char *alias;
    runanywhere::v1::ModelCategory category;
    runanywhere::v1::InferenceFramework framework;
  };
  const Row rows[] = {
      // Non-LANGUAGE overlay rows are out of scope for the LLM-only cut
      // (src/app.cpp, src/catalog/catalog.cpp) -- commented out, not
      // deleted, so they come back when the cut reverts.
      // {"sd15", "stable-diffusion-v1-5-coreml",
      //  runanywhere::v1::MODEL_CATEGORY_IMAGE_GENERATION,
      //  runanywhere::v1::INFERENCE_FRAMEWORK_COREML},
      {"lfm2-230m-ane", "lfm2_5_230m_ane",
       runanywhere::v1::MODEL_CATEGORY_LANGUAGE,
       runanywhere::v1::INFERENCE_FRAMEWORK_COREML},
      // {"parakeet-tdt-v2-ane", "parakeet_tdt_0_6b_v2_ane",
      //  runanywhere::v1::MODEL_CATEGORY_SPEECH_RECOGNITION,
      //  runanywhere::v1::INFERENCE_FRAMEWORK_COREML},
      {"lfm2-230m-npu", "lfm2_5_230m",
       runanywhere::v1::MODEL_CATEGORY_LANGUAGE,
       runanywhere::v1::INFERENCE_FRAMEWORK_QHEXRT},
      // {"whisper-base-npu", "whisper_base",
      //  runanywhere::v1::MODEL_CATEGORY_SPEECH_RECOGNITION,
      //  runanywhere::v1::INFERENCE_FRAMEWORK_QHEXRT},
      // {"kitten-micro-npu", "kitten_micro_0_8",
      //  runanywhere::v1::MODEL_CATEGORY_SPEECH_SYNTHESIS,
      //  runanywhere::v1::INFERENCE_FRAMEWORK_QHEXRT},
      // {"embeddinggemma-npu", "embeddinggemma_300m",
      //  runanywhere::v1::MODEL_CATEGORY_EMBEDDING,
      //  runanywhere::v1::INFERENCE_FRAMEWORK_QHEXRT},
      // {"internvl-1b-npu", "internvl3_5_1b",
      //  runanywhere::v1::MODEL_CATEGORY_MULTIMODAL,
      //  runanywhere::v1::INFERENCE_FRAMEWORK_QHEXRT},
      // {"cosmos3-diffusion-npu", "cosmos3_edge_diffusion",
      //  runanywhere::v1::MODEL_CATEGORY_IMAGE_GENERATION,
      //  runanywhere::v1::INFERENCE_FRAMEWORK_QHEXRT},
  };
  for (const Row &row : rows) {
    const wally::catalog::CatalogEntry *by_alias = wally::catalog::find(row.id);
    const wally::catalog::CatalogEntry *by_id = wally::catalog::find(row.alias);
    if (!by_alias || by_alias != by_id || by_alias->category != row.category ||
        by_alias->framework != row.framework || by_alias->url == nullptr) {
      result.details = std::string(row.id) +
                       " should be a folder-URL overlay catalog row";
      return result;
    }
  }
  result.passed = true;
  return result;
}

TestResult test_nvidia_sherpa_catalog() {
  TestResult result;
  result.test_name = "nvidia_sherpa_catalog";

  // Sherpa-ONNX (speech recognition) is out of scope for the LLM-only cut
  // (src/app.cpp, src/catalog/catalog.cpp): the whole body below is
  // commented out, not deleted, so it comes back when the cut reverts.
  result.passed = true;
  return result;
  /*
  struct ExpectedFile {
    const char *filename;
    int64_t size_bytes;
  };
  constexpr ExpectedFile parakeet_v2_files[] = {
      {"encoder.int8.onnx", 652184296LL},
      {"decoder.int8.onnx", 7257753LL},
      {"joiner.int8.onnx", 1739080LL},
      {"tokens.txt", 9384LL},
  };
  constexpr ExpectedFile parakeet_v3_files[] = {
      {"encoder.int8.onnx", 652184281LL},
      {"decoder.int8.onnx", 11845275LL},
      {"joiner.int8.onnx", 6355277LL},
      {"tokens.txt", 93939LL},
  };
  constexpr ExpectedFile canary_files[] = {
      {"encoder.int8.onnx", 132678643LL},
      {"decoder.int8.onnx", 74437848LL},
      {"tokens.txt", 53555LL},
  };

  struct Case {
    const char *id;
    const char *alias;
    const char *repo;
    const char *revision;
    const ExpectedFile *files;
    size_t file_count;
    int64_t total_size_bytes;
  };
  const Case cases[] = {
      {"sherpa-nemo-parakeet-tdt-0.6b-v2-int8", "parakeet-tdt-v2",
       "csukuangfj/sherpa-onnx-nemo-parakeet-tdt-0.6b-v2-int8",
       "1ab9323565ddb038682214b292f588070a538ce2", parakeet_v2_files, 4,
       661190513LL},
      {"sherpa-nemo-parakeet-tdt-0.6b-v3-int8", "parakeet-tdt-v3",
       "csukuangfj/sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8",
       "2bda32ec70b097a55adaa07d9a7173915b43cc78", parakeet_v3_files, 4,
       670478772LL},
      {"sherpa-nemo-canary-180m-flash-int8", "canary-180m",
       "csukuangfj/sherpa-onnx-nemo-canary-180m-flash-en-es-de-fr-int8",
       "9077164e0d3dd1d5353743e89ceaa1d3a770838c", canary_files, 3,
       207170046LL},
  };

  for (const Case &test_case : cases) {
    const wally::catalog::CatalogEntry *entry =
        wally::catalog::find(test_case.id);
    if (!entry || entry != wally::catalog::find(test_case.alias)) {
      result.details =
          std::string(test_case.id) + " should resolve by exact id and alias";
      return result;
    }
    if (entry->category != runanywhere::v1::MODEL_CATEGORY_SPEECH_RECOGNITION ||
        entry->framework != runanywhere::v1::INFERENCE_FRAMEWORK_SHERPA ||
        entry->format != runanywhere::v1::MODEL_FORMAT_ONNX ||
        entry->url != nullptr || entry->files == nullptr ||
        entry->file_count != test_case.file_count ||
        entry->download_size_bytes != test_case.total_size_bytes) {
      result.details = std::string(test_case.id) +
                       " should be an exact Sherpa-ONNX STT bundle";
      return result;
    }

    const std::string base_url = std::string("https://huggingface.co/") +
                                 test_case.repo + "/resolve/" +
                                 test_case.revision + "/";
    int64_t manifest_total = 0;
    for (size_t i = 0; i < test_case.file_count; ++i) {
      const wally::catalog::CatalogFile &actual = entry->files[i];
      const ExpectedFile &expected = test_case.files[i];
      const std::string expected_url = base_url + expected.filename;
      if (actual.url == nullptr || actual.filename == nullptr ||
          std::string(actual.url) != expected_url ||
          std::string(actual.filename) != expected.filename ||
          !actual.required || actual.size_bytes != expected.size_bytes) {
        result.details = std::string(test_case.id) +
                         " has a mismatched pinned file manifest at index " +
                         std::to_string(i);
        return result;
      }
      manifest_total += actual.size_bytes;
    }
    if (manifest_total != test_case.total_size_bytes) {
      result.details = std::string(test_case.id) +
                       " per-file sizes should sum to the exact bundle total";
      return result;
    }
  }

  const wally::catalog::CatalogEntry *parakeet_ctc =
      wally::catalog::find("sherpa-nemo-parakeet-ctc-1.1b-int8");
  if (!parakeet_ctc || parakeet_ctc != wally::catalog::find("parakeet-ctc") ||
      parakeet_ctc->category !=
          runanywhere::v1::MODEL_CATEGORY_SPEECH_RECOGNITION ||
      parakeet_ctc->framework != runanywhere::v1::INFERENCE_FRAMEWORK_SHERPA ||
      parakeet_ctc->format != runanywhere::v1::MODEL_FORMAT_ONNX ||
      parakeet_ctc->url != nullptr || parakeet_ctc->files == nullptr ||
      parakeet_ctc->file_count != 2 ||
      parakeet_ctc->download_size_bytes != 1110024519LL ||
      parakeet_ctc->memory_required_bytes != 2147483648LL) {
    result.details = "Parakeet CTC should be an exact Sherpa-ONNX bundle";
    return result;
  }

  const std::string base_url =
      "https://huggingface.co/runanywhere/"
      "sherpa-onnx-nemo-parakeet-ctc-1.1b-int8/resolve/"
      "48a549f552774db3cd09dd1548f3d1a2b37bc7c5/";
  const wally::catalog::CatalogFile &model = parakeet_ctc->files[0];
  const wally::catalog::CatalogFile &tokens = parakeet_ctc->files[1];
  if (std::string(model.url) != base_url + "model.int8.onnx" ||
      std::string(model.filename) != "model.int8.onnx" || !model.required ||
      model.size_bytes != 1110014145LL || model.checksum_sha256 == nullptr ||
      std::string(model.checksum_sha256) !=
          "62f73c17a5301c048c7273cf24ef1cd0c3621d3625c5415fbafe5633d7bf2f98") {
    result.details = "Parakeet CTC model descriptor is not exact";
    return result;
  }
  if (std::string(tokens.url) != base_url + "tokens.txt" ||
      std::string(tokens.filename) != "tokens.txt" || !tokens.required ||
      tokens.size_bytes != 10374LL || tokens.checksum_sha256 == nullptr ||
      std::string(tokens.checksum_sha256) !=
          "ed16e1a4e3a3aa379138c0b1888e5d49f993c9d512b2be4d46e90a87afd54921") {
    result.details = "Parakeet CTC tokens descriptor is not exact";
    return result;
  }

  result.passed = true;
  return result;
  */
}

TestResult test_engine_hint_parsing() {
  TestResult result;
  result.test_name = "engine_hint_parsing";

  struct Case {
    std::string in;
    runanywhere::v1::InferenceFramework expected;
  };
  const Case cases[] = {
      {"", runanywhere::v1::INFERENCE_FRAMEWORK_UNSPECIFIED},
      {"mlx", runanywhere::v1::INFERENCE_FRAMEWORK_MLX},
      {"llama.cpp", runanywhere::v1::INFERENCE_FRAMEWORK_LLAMA_CPP},
      {"llama-cpp", runanywhere::v1::INFERENCE_FRAMEWORK_LLAMA_CPP},
      {"onnx", runanywhere::v1::INFERENCE_FRAMEWORK_ONNX},
      {"sherpa", runanywhere::v1::INFERENCE_FRAMEWORK_SHERPA},
      {"neurt", runanywhere::v1::INFERENCE_FRAMEWORK_COREML},
      {"ane", runanywhere::v1::INFERENCE_FRAMEWORK_COREML},
      {"qhexrt", runanywhere::v1::INFERENCE_FRAMEWORK_QHEXRT},
      {"npu", runanywhere::v1::INFERENCE_FRAMEWORK_QHEXRT},
  };
  for (const Case &c : cases) {
    runanywhere::v1::InferenceFramework actual =
        runanywhere::v1::INFERENCE_FRAMEWORK_UNSPECIFIED;
    std::string error;
    if (!wally::commands::parse_engine_hint(c.in, &actual, &error) ||
        actual != c.expected) {
      result.expected = std::to_string(static_cast<int>(c.expected));
      result.actual = std::to_string(static_cast<int>(actual));
      result.details = "input: " + c.in + " error: " + error;
      return result;
    }
  }

  runanywhere::v1::InferenceFramework actual =
      runanywhere::v1::INFERENCE_FRAMEWORK_UNSPECIFIED;
  std::string error;
  if (wally::commands::parse_engine_hint("banana", &actual, &error) ||
      error.find("unsupported engine") == std::string::npos) {
    result.details = "unsupported engine should fail with an actionable error";
    return result;
  }

  result.passed = true;
  return result;
}

void remove_registered_model(const std::string &id) {
  if (auto *registry = rac_get_model_registry()) {
    (void)rac_model_registry_remove_proto(registry, id.c_str());
  }
}

class RegisteredModelCleanup {
public:
  RegisteredModelCleanup(std::initializer_list<const char *> ids) {
    ids_.reserve(ids.size());
    for (const char *id : ids) {
      ids_.emplace_back(id);
    }
  }

  ~RegisteredModelCleanup() {
    for (const auto &id : ids_) {
      remove_registered_model(id);
    }
  }

private:
  std::vector<std::string> ids_;
};

bool get_registered_model(const std::string &id,
                          runanywhere::v1::ModelInfo *out, std::string *error) {
  rac_proto_buffer_t found;
  rac_proto_buffer_init(&found);
  const rac_result_t rc = rac_model_registry_get_proto_buffer(
      rac_get_model_registry(), id.c_str(), &found);
  const bool parsed = wally::proto::parse_proto_buffer(&found, out, error);
  if (!parsed && error && error->empty()) {
    *error = "registry get failed rc=" + std::to_string(rc);
  }
  return rc == RAC_SUCCESS && parsed;
}

TestResult test_mlx_catalog_registration() {
  TestResult result;
  result.test_name = "mlx_catalog_registration";

  const rac_result_t rc = wally::catalog::register_all();
  if (rc != RAC_SUCCESS) {
    result.details = "catalog registration failed rc=" + std::to_string(rc);
    return result;
  }
  RegisteredModelCleanup cleanup({
      "mlx-qwen3-0.6b-4bit",
      "mlx-maple-preview-2bit",
      "mlx-llama-3.2-1b-instruct-4bit",
      "mlx-qwen2-vl-2b-instruct-4bit",
      "mlx-fastvlm-0.5b-bf16",
      "mlx-qwen3-embedding-0.6b-4bit-dwq",
      "mlx-qwen3-asr-0.6b-8bit",
      "mlx-glm-asr-nano-2512-4bit",
      "mlx-llama-3.1-nemotron-nano-8b-v1-4bit",
      "mlx-nemotron-mini-4b-instruct-4bit",
      "mlx-parakeet-ctc-1.1b",
      "mlx-parakeet-tdt-0.6b-v2",
      "mlx-parakeet-tdt-0.6b-v3",
      "mlx-parakeet-rnnt-1.1b",
      "mlx-nemotron-3.5-asr-streaming-0.6b-8bit",
      "mlx-qwen3-tts-12hz-0.6b-base-8bit",
      "mlx-soprano-1.1-80m-5bit",
      "sherpa-nemo-parakeet-tdt-0.6b-v2-int8",
      "sherpa-nemo-parakeet-tdt-0.6b-v3-int8",
      "sherpa-nemo-parakeet-ctc-1.1b-int8",
      "sherpa-nemo-canary-180m-flash-int8",
      "sherpa-nemotron-3.5-asr-streaming-0.6b-320ms-int8",
  });

  runanywhere::v1::ModelInfo qwen;
  std::string error;
  if (!get_registered_model("mlx-qwen3-0.6b-4bit", &qwen, &error)) {
    result.details = error;
    return result;
  }
  if (qwen.framework() != runanywhere::v1::INFERENCE_FRAMEWORK_MLX ||
      qwen.format() != runanywhere::v1::MODEL_FORMAT_SAFETENSORS ||
      qwen.category() != runanywhere::v1::MODEL_CATEGORY_LANGUAGE ||
      !qwen.has_multi_file() || qwen.multi_file().files_size() != 9 ||
      qwen.download_size_bytes() != 351383618 || !qwen.supports_thinking()) {
    result.details = "registered MLX Qwen3 metadata is incomplete";
    return result;
  }

  runanywhere::v1::ModelInfo maple;
  if (!get_registered_model("mlx-maple-preview-2bit", &maple, &error) ||
      maple.framework() != runanywhere::v1::INFERENCE_FRAMEWORK_MLX ||
      maple.format() != runanywhere::v1::MODEL_FORMAT_SAFETENSORS ||
      maple.category() != runanywhere::v1::MODEL_CATEGORY_LANGUAGE ||
      !maple.has_multi_file() || maple.multi_file().files_size() != 13 ||
      maple.download_size_bytes() != 5330252282LL ||
      maple.context_length() != 128000) {
    result.details = "registered MLX Maple metadata is incomplete";
    return result;
  }

  // VLM, embedding and ASR registrations are out of scope for the LLM-only
  // cut (src/app.cpp, src/catalog/catalog.cpp) -- commented out, not
  // deleted, so this comes back when the cut reverts.
  /*
  runanywhere::v1::ModelInfo vlm;
  if (!get_registered_model("mlx-qwen2-vl-2b-instruct-4bit", &vlm, &error)) {
    result.details = error;
    return result;
  }
  bool preprocessor_registered = false;
  for (const auto &file : vlm.multi_file().files()) {
    preprocessor_registered = preprocessor_registered ||
                              file.filename() == "preprocessor_config.json";
  }
  if (vlm.category() != runanywhere::v1::MODEL_CATEGORY_MULTIMODAL ||
      vlm.framework() != runanywhere::v1::INFERENCE_FRAMEWORK_MLX ||
      !vlm.has_multi_file() || vlm.multi_file().files_size() != 11 ||
      !preprocessor_registered) {
    result.details = "registered MLX VLM metadata is incomplete";
    return result;
  }

  runanywhere::v1::ModelInfo fastvlm;
  if (!get_registered_model("mlx-fastvlm-0.5b-bf16", &fastvlm, &error)) {
    result.details = error;
    return result;
  }
  bool processor_registered = false;
  bool companion_registered = false;
  for (const auto &file : fastvlm.multi_file().files()) {
    processor_registered =
        processor_registered || file.filename() == "processor_config.json";
    companion_registered =
        companion_registered ||
        (file.is_optional() && (file.filename() == "processing_fastvlm.py" ||
                                file.filename() == "llava_qwen.py"));
  }
  if (fastvlm.category() != runanywhere::v1::MODEL_CATEGORY_MULTIMODAL ||
      fastvlm.framework() != runanywhere::v1::INFERENCE_FRAMEWORK_MLX ||
      !fastvlm.has_multi_file() || fastvlm.multi_file().files_size() != 14 ||
      !processor_registered || !companion_registered) {
    result.details = "registered MLX FastVLM metadata is incomplete";
    return result;
  }

  runanywhere::v1::ModelInfo embedding;
  if (!get_registered_model("mlx-qwen3-embedding-0.6b-4bit-dwq", &embedding,
                            &error)) {
    result.details = error;
    return result;
  }
  if (embedding.category() != runanywhere::v1::MODEL_CATEGORY_EMBEDDING ||
      embedding.framework() != runanywhere::v1::INFERENCE_FRAMEWORK_MLX ||
      embedding.format() != runanywhere::v1::MODEL_FORMAT_SAFETENSORS ||
      !embedding.has_multi_file() ||
      embedding.multi_file().files_size() != 11) {
    result.details = "registered MLX embedding metadata is incomplete";
    return result;
  }

  runanywhere::v1::ModelInfo qwen_asr;
  if (!get_registered_model("mlx-qwen3-asr-0.6b-8bit", &qwen_asr, &error) ||
      qwen_asr.category() !=
          runanywhere::v1::MODEL_CATEGORY_SPEECH_RECOGNITION ||
      qwen_asr.framework() != runanywhere::v1::INFERENCE_FRAMEWORK_MLX ||
      !qwen_asr.has_multi_file() || qwen_asr.multi_file().files_size() != 9) {
    result.details = "registered MLX Qwen3-ASR metadata is incomplete";
    return result;
  }

  runanywhere::v1::ModelInfo glm_asr;
  if (!get_registered_model("mlx-glm-asr-nano-2512-4bit", &glm_asr, &error) ||
      glm_asr.category() !=
          runanywhere::v1::MODEL_CATEGORY_SPEECH_RECOGNITION ||
      glm_asr.framework() != runanywhere::v1::INFERENCE_FRAMEWORK_MLX ||
      !glm_asr.has_multi_file() || glm_asr.multi_file().files_size() != 9) {
    result.details = "registered MLX GLM-ASR metadata is incomplete";
    return result;
  }
  */

  struct RegisteredNvidiaCase {
    const char *id;
    int expected_files;
    int64_t expected_size;
  };
  const RegisteredNvidiaCase registered_nvidia_cases[] = {
      {"mlx-llama-3.1-nemotron-nano-8b-v1-4bit", 8, 4534806075LL},
      {"mlx-nemotron-mini-4b-instruct-4bit", 6, 2392679103LL},
      // Speech recognition entries, out of scope for the LLM-only cut.
      // {"mlx-parakeet-ctc-1.1b", 2, 4250718357LL},
      // {"mlx-parakeet-tdt-0.6b-v2", 2, 2471596080LL},
      // {"mlx-parakeet-tdt-0.6b-v3", 2, 2508532829LL},
      // {"mlx-parakeet-rnnt-1.1b", 2, 4282283914LL},
      // {"mlx-nemotron-3.5-asr-streaming-0.6b-8bit", 2, 755758528LL},
  };
  for (const RegisteredNvidiaCase &test_case : registered_nvidia_cases) {
    runanywhere::v1::ModelInfo model;
    if (!get_registered_model(test_case.id, &model, &error) ||
        model.framework() != runanywhere::v1::INFERENCE_FRAMEWORK_MLX ||
        model.format() != runanywhere::v1::MODEL_FORMAT_SAFETENSORS ||
        !model.has_multi_file() ||
        model.multi_file().files_size() != test_case.expected_files ||
        model.download_size_bytes() != test_case.expected_size) {
      result.details =
          std::string("registered NVIDIA MLX metadata is incomplete: ") +
          test_case.id;
      return result;
    }
  }

  // Sherpa-ONNX (speech recognition) and TTS registrations are out of scope
  // for the LLM-only cut (src/app.cpp, src/catalog/catalog.cpp) --
  // commented out, not deleted, so this comes back when the cut reverts.
  /*
  struct RegisteredSherpaCase {
    const char *id;
    int expected_files;
    int64_t expected_size;
  };
  const RegisteredSherpaCase registered_sherpa_cases[] = {
      {"sherpa-nemo-parakeet-tdt-0.6b-v2-int8", 4, 661190513LL},
      {"sherpa-nemo-parakeet-tdt-0.6b-v3-int8", 4, 670478772LL},
      {"sherpa-nemo-parakeet-ctc-1.1b-int8", 2, 1110024519LL},
      {"sherpa-nemo-canary-180m-flash-int8", 3, 207170046LL},
      {"sherpa-nemotron-3.5-asr-streaming-0.6b-320ms-int8", 4,
       682215471LL},
  };
  for (const RegisteredSherpaCase &test_case : registered_sherpa_cases) {
    runanywhere::v1::ModelInfo model;
    if (!get_registered_model(test_case.id, &model, &error) ||
        model.category() !=
            runanywhere::v1::MODEL_CATEGORY_SPEECH_RECOGNITION ||
        model.framework() != runanywhere::v1::INFERENCE_FRAMEWORK_SHERPA ||
        model.format() != runanywhere::v1::MODEL_FORMAT_ONNX ||
        !model.has_multi_file() ||
        model.multi_file().files_size() != test_case.expected_files ||
        model.download_size_bytes() != test_case.expected_size) {
      result.details =
          std::string("registered NVIDIA Sherpa metadata is incomplete: ") +
          test_case.id;
      return result;
    }
    int64_t registered_file_total = 0;
    for (const auto &file : model.multi_file().files()) {
      if (!file.has_size_bytes() || file.size_bytes() <= 0) {
        result.details =
            std::string("registered NVIDIA Sherpa file size is missing: ") +
            test_case.id;
        return result;
      }
      registered_file_total += file.size_bytes();
    }
    if (registered_file_total != test_case.expected_size) {
      result.details =
          std::string("registered NVIDIA Sherpa file sizes do not sum: ") +
          test_case.id;
      return result;
    }
  }

  runanywhere::v1::ModelInfo parakeet_ctc;
  if (!get_registered_model("sherpa-nemo-parakeet-ctc-1.1b-int8", &parakeet_ctc,
                            &error)) {
    result.details = "registered Parakeet CTC metadata is missing: " + error;
    return result;
  }
  const runanywhere::v1::ModelFileDescriptor *registered_model = nullptr;
  const runanywhere::v1::ModelFileDescriptor *registered_tokens = nullptr;
  for (const auto &file : parakeet_ctc.multi_file().files()) {
    if (file.filename() == "model.int8.onnx") {
      registered_model = &file;
    } else if (file.filename() == "tokens.txt") {
      registered_tokens = &file;
    }
  }
  if (registered_model == nullptr || registered_tokens == nullptr ||
      registered_model->url() !=
          "https://huggingface.co/runanywhere/"
          "sherpa-onnx-nemo-parakeet-ctc-1.1b-int8/resolve/"
          "48a549f552774db3cd09dd1548f3d1a2b37bc7c5/model.int8.onnx" ||
      !registered_model->has_size_bytes() ||
      registered_model->size_bytes() != 1110014145LL ||
      !registered_model->has_checksum_sha256() ||
      registered_model->checksum_sha256() !=
          "62f73c17a5301c048c7273cf24ef1cd0c3621d3625c5415fbafe5633d7bf2f98" ||
      !parakeet_ctc.has_memory_required_bytes() ||
      parakeet_ctc.memory_required_bytes() != 2147483648LL) {
    result.details =
        "registered Parakeet CTC model descriptor tuple is incomplete";
    return result;
  }
  if (registered_tokens->url() !=
          "https://huggingface.co/runanywhere/"
          "sherpa-onnx-nemo-parakeet-ctc-1.1b-int8/resolve/"
          "48a549f552774db3cd09dd1548f3d1a2b37bc7c5/tokens.txt" ||
      !registered_tokens->has_size_bytes() ||
      registered_tokens->size_bytes() != 10374LL ||
      !registered_tokens->has_checksum_sha256() ||
      registered_tokens->checksum_sha256() !=
          "ed16e1a4e3a3aa379138c0b1888e5d49f993c9d512b2be4d46e90a87afd54921") {
    result.details =
        "registered Parakeet CTC tokens descriptor tuple is incomplete";
    return result;
  }

  runanywhere::v1::ModelInfo qwen_tts;
  if (!get_registered_model("mlx-qwen3-tts-12hz-0.6b-base-8bit", &qwen_tts,
                            &error) ||
      qwen_tts.category() != runanywhere::v1::MODEL_CATEGORY_SPEECH_SYNTHESIS ||
      qwen_tts.framework() != runanywhere::v1::INFERENCE_FRAMEWORK_MLX ||
      !qwen_tts.has_multi_file() || qwen_tts.multi_file().files_size() != 12) {
    result.details = "registered MLX Qwen3-TTS metadata is incomplete";
    return result;
  }

  runanywhere::v1::ModelInfo soprano;
  if (!get_registered_model("mlx-soprano-1.1-80m-5bit", &soprano, &error) ||
      soprano.category() != runanywhere::v1::MODEL_CATEGORY_SPEECH_SYNTHESIS ||
      soprano.framework() != runanywhere::v1::INFERENCE_FRAMEWORK_MLX ||
      !soprano.has_multi_file() || soprano.multi_file().files_size() != 7) {
    result.details = "registered MLX Soprano metadata is incomplete";
    return result;
  }
  */

  result.passed = true;
  return result;
}

// HF explicit-file refs normalize inside commons
// (rac_register_model_from_url_proto) now — verify through the production ABI
// that the saved entry carries the expected resolve/main download URL.
// Explicit-file refs never hit the network (only repo-level refs do, and none
// appear here).
TestResult test_hf_ref_registration() {
  TestResult result;
  result.test_name = "hf_ref_registration";

  struct Case {
    std::string in;
    std::string expected_download_url;
  };
  const Case cases[] = {
      {"hf.co/Qwen/Qwen3-0.6B-GGUF/Qwen3-0.6B-Q8_0.gguf",
       "https://huggingface.co/Qwen/Qwen3-0.6B-GGUF/resolve/main/"
       "Qwen3-0.6B-Q8_0.gguf"},
      {"huggingface.co/org/repo/sub/dir/file.gguf",
       "https://huggingface.co/org/repo/resolve/main/sub/dir/file.gguf"},
      {"https://huggingface.co/org/repo/resolve/main/f.gguf",
       "https://huggingface.co/org/repo/resolve/main/f.gguf"},
      {"https://huggingface.co/org/repo/blob/main/sub/f.gguf",
       "https://huggingface.co/org/repo/resolve/main/sub/f.gguf"},
      {"https://example.com/m.gguf", "https://example.com/m.gguf"},
  };
  for (const Case &c : cases) {
    runanywhere::v1::RegisterModelFromUrlRequest request;
    request.set_url(c.in);
    const std::string bytes = wally::proto::serialize(request);

    rac_proto_buffer_t out;
    rac_proto_buffer_init(&out);
    const rac_result_t rc = rac_register_model_from_url_proto(
        reinterpret_cast<const uint8_t *>(bytes.data()), bytes.size(), &out);
    runanywhere::v1::ModelInfo saved;
    std::string parse_error;
    const bool parsed = rc == RAC_SUCCESS && wally::proto::parse_proto_buffer(
                                                 &out, &saved, &parse_error);
    if (!parsed) {
      result.expected = c.expected_download_url;
      result.actual = "<registration failed rc=" + std::to_string(rc) + ">";
      result.details = "input: " + c.in + " " + parse_error;
      return result;
    }
    if (saved.download_url() != c.expected_download_url) {
      result.expected = c.expected_download_url;
      result.actual =
          saved.download_url().empty() ? "<empty>" : saved.download_url();
      result.details = "input: " + c.in;
      return result;
    }
  }
  result.passed = true;
  return result;
}

// ---------------------------------------------------------------------------
// diarize command coverage
//
// These tests exercise register_diarize()'s argv surface WITHOUT reaching the
// CLI11 callback (which would fire run_diarize -> bootstrap + a real ONNX
// Sortformer model). Two inference-free strategies are used:
//   1. Pure introspection: configure_app() then query the CLI11 App/Option
//      model -- never parse, never run a callback.
//   2. Parse-FAILURE paths via wally::run(): a usage error makes CLI11 throw a
//      ParseError inside parse(), before any callback, and src/app.cpp maps
//      every ParseError to the production exit code 2 (0 ok, 1 runtime, 2
//      usage).
// The --json / table render path (print_result) has internal linkage and needs
// a real model, so it is intentionally not covered here.
// ---------------------------------------------------------------------------

// RAII zero-byte temp file. A zero-byte regular file satisfies
// CLI::ExistingFile (WAV validity is only checked later, inside run_diarize,
// which a parse-error path never reaches).
class TempWavFile {
public:
  TempWavFile() {
    namespace fs = std::filesystem;
    static int counter = 0;
    path_ = (fs::temp_directory_path() /
             ("wally_diarize_test_" + std::to_string(++counter) + ".wav"))
                .string();
    std::ofstream(path_).close();
  }
  ~TempWavFile() {
    std::error_code ec;
    std::filesystem::remove(path_, ec);
  }
  const std::string &path() const { return path_; }

private:
  std::string path_;
};

// Drive the production entry point wally::run() with an argv vector. run() builds
// its own App + GlobalOptions, so a usage error returns the true production exit
// code (2) without any bootstrap or inference.
int run_wally(const std::vector<std::string> &args) {
  std::vector<std::string> mutable_args = args;
  std::vector<char *> argv;
  argv.reserve(mutable_args.size());
  for (std::string &arg : mutable_args) {
    argv.push_back(arg.data());
  }
  return wally::run(static_cast<int>(argv.size()), argv.data());
}

TestResult test_diarize_arg_surface() {
  TestResult result;
  result.test_name = "diarize_arg_surface";

  // register_diarize() is commented out in src/app.cpp for the LLM-only cut,
  // so the subcommand it asserts on is unreachable. Body commented out, not
  // deleted, so it comes back when the cut reverts.
  result.passed = true;
  return result;
  /*
  wally::GlobalOptions options;
  CLI::App app{"wally test app"};
  wally::configure_app(app, options);

  const CLI::App *cmd = app.get_subcommand_no_throw("diarize");
  if (cmd == nullptr) {
    result.details = "diarize subcommand not registered";
    return result;
  }
  if (cmd->get_description() !=
      "Label who spoke when in an audio file") {
    result.expected = "Label who spoke when in an audio file";
    result.actual = cmd->get_description();
    return result;
  }

  const CLI::Option *audio = cmd->get_option_no_throw("audio");
  if (audio == nullptr || !audio->get_required()) {
    result.details = "positional 'audio' must exist and be required";
    return result;
  }

  const CLI::Option *model = cmd->get_option_no_throw("--model");
  if (model == nullptr || !model->get_required() || !model->check_name("-m")) {
    result.details = "--model must exist, be required, and carry the -m alias";
    return result;
  }

  const char *optional_flags[] = {"--threshold", "--min-duration",
                                  "--merge-gap"};
  for (const char *name : optional_flags) {
    const CLI::Option *opt = cmd->get_option_no_throw(name);
    if (opt == nullptr) {
      result.details = std::string("missing option ") + name;
      return result;
    }
    if (opt->get_required()) {
      result.details = std::string(name) + " must not be required";
      return result;
    }
  }

  result.passed = true;
  return result;
  */
}

TestResult test_diarize_missing_model_exit2() {
  TestResult result;
  result.test_name = "diarize_missing_model_exit2";

  // audio positional satisfied by an existing temp file -> the only failure is
  // the missing required --model (RequiredError -> ParseError -> exit 2).
  TempWavFile audio;
  const int code = run_wally({"wally", "diarize", audio.path()});
  if (code != 2) {
    result.expected = "2";
    result.actual = std::to_string(code);
    result.details = "missing required --model should be a usage error";
    return result;
  }
  result.passed = true;
  return result;
}

TestResult test_diarize_missing_audio_exit2() {
  TestResult result;
  result.test_name = "diarize_missing_audio_exit2";

  // --model consumes "x"; the required audio positional is left unsatisfied
  // (RequiredError -> ParseError -> exit 2).
  const int code = run_wally({"wally", "diarize", "--model", "x"});
  if (code != 2) {
    result.expected = "2";
    result.actual = std::to_string(code);
    result.details =
        "missing required audio positional should be a usage error";
    return result;
  }
  result.passed = true;
  return result;
}

TestResult test_diarize_audio_not_found_exit2() {
  TestResult result;
  result.test_name = "diarize_audio_not_found_exit2";

  // --model is supplied so the sole failure is the audio ->check(ExistingFile)
  // validator (ValidationError -> ParseError -> exit 2), a distinct path from a
  // plain RequiredError.
  const int code = run_wally(
      {"wally", "diarize", "/no/such/wally-diarize-input.wav", "--model", "x"});
  if (code != 2) {
    result.expected = "2";
    result.actual = std::to_string(code);
    result.details =
        "non-existent audio should fail CLI::ExistingFile (usage error)";
    return result;
  }
  result.passed = true;
  return result;
}

TestResult test_diarize_numeric_option_typing_exit2() {
  TestResult result;
  result.test_name = "diarize_numeric_option_typing_exit2";

  // A non-numeric value for a typed numeric option raises CLI11 ConversionError
  // (a ParseError) during parse, before the callback -> exit 2. This is the
  // only inference-free way to prove --threshold binds to a float and
  // --min-duration/--merge-gap bind to integers (a *valid* value would run the
  // callback and load a model). Required args are satisfied so the conversion
  // is the only failure.
  TempWavFile audio;
  const char *numeric_flags[] = {"--threshold", "--min-duration",
                                 "--merge-gap"};
  for (const char *flag : numeric_flags) {
    const int code = run_wally(
        {"wally", "diarize", audio.path(), "--model", "x", flag, "notanumber"});
    if (code != 2) {
      result.expected = "2";
      result.actual = std::to_string(code);
      result.details =
          std::string("non-numeric ") + flag + " should be a usage error";
      return result;
    }
  }
  result.passed = true;
  return result;
}

TestResult test_diarize_unknown_flag_exit2() {
  TestResult result;
  result.test_name = "diarize_unknown_flag_exit2";

  // An unrecognized option is not consumed by the subcommand or (via
  // fallthrough) the parent, so parse ends with an ExtrasError (ParseError) ->
  // exit 2. Guards against silently-ignored typos.
  TempWavFile audio;
  const int code =
      run_wally({"wally", "diarize", audio.path(), "--model", "x", "--bogus"});
  if (code != 2) {
    result.expected = "2";
    result.actual = std::to_string(code);
    result.details = "unrecognized flag should be a usage error (ExtrasError)";
    return result;
  }
  result.passed = true;
  return result;
}

// ===========================================================================
// image_io helpers (write_png / read_ppm) — the segment command's PNG encoder
// and PPM decoder. Pure file-path helpers, exercised via temp files (write_png
// and read_ppm operate on paths via fopen, not injectable streams). Offline,
// model-free, deterministic. See src/io/image_io.{h,cpp}.
// ===========================================================================

// Unique path under the system temp dir; RAII removes it recursively on scope
// exit (recursive so it also covers the never-created parent dirs used by the
// unwritable-path case). Mirrors make_temp_dir() in test_wally_mlx_e2e.cpp.
std::string unique_temp_path(const std::string &name) {
  static uint64_t counter = 0;
  const auto stamp =
      std::chrono::steady_clock::now().time_since_epoch().count();
  return (std::filesystem::temp_directory_path() /
          (name + "-" + std::to_string(stamp) + "-" +
           std::to_string(counter++)))
      .string();
}

class TempFile {
public:
  explicit TempFile(const std::string &name) : path_(unique_temp_path(name)) {}
  ~TempFile() {
    std::error_code ec;
    std::filesystem::remove_all(path_, ec);
  }
  TempFile(const TempFile &) = delete;
  TempFile &operator=(const TempFile &) = delete;
  const std::string &path() const { return path_; }

private:
  std::string path_;
};

std::vector<uint8_t> bytes_of(const std::string &s) {
  return std::vector<uint8_t>(s.begin(), s.end());
}

bool write_bytes(const std::string &path, const std::vector<uint8_t> &bytes) {
  std::ofstream out(path, std::ios::binary);
  if (!out.is_open()) {
    return false;
  }
  if (!bytes.empty()) {
    out.write(reinterpret_cast<const char *>(bytes.data()),
              static_cast<std::streamsize>(bytes.size()));
  }
  return out.good();
}

bool read_bytes(const std::string &path, std::vector<uint8_t> *bytes) {
  std::ifstream in(path, std::ios::binary);
  if (!in.is_open()) {
    return false;
  }
  in.seekg(0, std::ios::end);
  const std::streamoff size = in.tellg();
  if (size < 0) {
    return false;
  }
  in.seekg(0, std::ios::beg);
  bytes->resize(static_cast<size_t>(size));
  if (size > 0) {
    in.read(reinterpret_cast<char *>(bytes->data()),
            static_cast<std::streamsize>(size));
  }
  return in.good() || in.eof();
}

// Build a valid P6 header ("P6\n<w> <h>\n255\n") followed by the raw pixels.
std::vector<uint8_t> make_ppm(uint32_t w, uint32_t h,
                              const std::vector<uint8_t> &pixels) {
  const std::string header =
      "P6\n" + std::to_string(w) + " " + std::to_string(h) + "\n255\n";
  std::vector<uint8_t> v(header.begin(), header.end());
  v.insert(v.end(), pixels.begin(), pixels.end());
  return v;
}

uint32_t read_u32_be(const std::vector<uint8_t> &b, size_t off) {
  return (static_cast<uint32_t>(b[off]) << 24) |
         (static_cast<uint32_t>(b[off + 1]) << 16) |
         (static_cast<uint32_t>(b[off + 2]) << 8) |
         static_cast<uint32_t>(b[off + 3]);
}

// Independent CRC-32 (PNG polynomial) — deliberately separate from the encoder's
// own implementation so a regression there cannot mask a regression here.
uint32_t test_crc32(const uint8_t *data, size_t len) {
  static uint32_t table[256];
  static bool ready = false;
  if (!ready) {
    for (uint32_t n = 0; n < 256u; ++n) {
      uint32_t c = n;
      for (int k = 0; k < 8; ++k) {
        c = (c & 1u) ? (0xEDB88320u ^ (c >> 1)) : (c >> 1);
      }
      table[n] = c;
    }
    ready = true;
  }
  uint32_t crc = 0xFFFFFFFFu;
  for (size_t i = 0; i < len; ++i) {
    crc = table[(crc ^ data[i]) & 0xFF] ^ (crc >> 8);
  }
  return crc ^ 0xFFFFFFFFu;
}

// Independent Adler-32 (per-byte modulo form).
uint32_t test_adler32(const uint8_t *data, size_t len) {
  uint32_t a = 1;
  uint32_t b = 0;
  for (size_t i = 0; i < len; ++i) {
    a = (a + data[i]) % 65521u;
    b = (b + a) % 65521u;
  }
  return (b << 16) | a;
}

// Reconstruct the PNG filtered scanlines the encoder feeds into DEFLATE: a 0x00
// filter byte per row followed by that row's RGBA bytes.
std::vector<uint8_t> filtered_raw(const std::vector<uint8_t> &rgba, int width,
                                  int height) {
  const size_t row_bytes = static_cast<size_t>(width) * 4;
  std::vector<uint8_t> raw;
  raw.reserve(static_cast<size_t>(height) * (1 + row_bytes));
  for (int y = 0; y < height; ++y) {
    raw.push_back(0);
    const uint8_t *row = rgba.data() + static_cast<size_t>(y) * row_bytes;
    raw.insert(raw.end(), row, row + row_bytes);
  }
  return raw;
}

struct PngChunk {
  std::string type;
  std::vector<uint8_t> data;
  uint32_t stored_crc = 0;
  uint32_t computed_crc = 0;
};

// Parse the 8-byte signature + length/type/data/CRC chunk stream. Records both
// the stored CRC and an independently computed CRC over type+data per chunk.
bool parse_png(const std::vector<uint8_t> &png, std::vector<PngChunk> *out) {
  static const uint8_t sig[8] = {137, 80, 78, 71, 13, 10, 26, 10};
  if (png.size() < 8u || std::memcmp(png.data(), sig, 8) != 0) {
    return false;
  }
  size_t pos = 8;
  while (pos + 8 <= png.size()) {
    const uint32_t len = read_u32_be(png, pos);
    const size_t data_off = pos + 8;
    if (data_off + len + 4 > png.size()) {
      return false;
    }
    PngChunk c;
    c.type.assign(png.begin() + pos + 4, png.begin() + pos + 8);
    c.data.assign(png.begin() + data_off, png.begin() + data_off + len);
    c.stored_crc = read_u32_be(png, data_off + len);
    const std::vector<uint8_t> crc_input(png.begin() + pos + 4,
                                         png.begin() + data_off + len);
    c.computed_crc = test_crc32(crc_input.data(), crc_input.size());
    out->push_back(c);
    pos = data_off + len + 4;
  }
  return pos == png.size();
}

const PngChunk *find_chunk(const std::vector<PngChunk> &chunks,
                           const std::string &type) {
  for (const PngChunk &c : chunks) {
    if (c.type == type) {
      return &c;
    }
  }
  return nullptr;
}

// Parse a zlib stream (0x78 0x01 + stored DEFLATE blocks + 4-byte Adler-32).
struct ZlibParse {
  bool ok = false;
  std::vector<uint8_t> payload;
  int block_count = 0;
  bool bfinal_ok = false;  // BFINAL set on exactly the last block, none earlier
  bool lennlen_ok = true;  // NLEN == ~LEN for every block
  uint32_t adler = 0;
};

ZlibParse parse_stored_zlib(const std::vector<uint8_t> &z) {
  ZlibParse r;
  if (z.size() < 6u || z[0] != 0x78 || z[1] != 0x01) {
    return r;
  }
  const size_t adler_off = z.size() - 4;
  size_t pos = 2;
  std::vector<bool> finals;
  while (pos < adler_off) {
    if (adler_off - pos < 5u) {  // 1 header byte + LEN + NLEN
      return r;
    }
    const uint8_t hdr = z[pos];
    const uint8_t btype = static_cast<uint8_t>((hdr >> 1) & 0x03);
    if (btype != 0) {  // only stored (uncompressed) blocks are emitted
      return r;
    }
    const uint16_t len = static_cast<uint16_t>(z[pos + 1] | (z[pos + 2] << 8));
    const uint16_t nlen = static_cast<uint16_t>(z[pos + 3] | (z[pos + 4] << 8));
    if (static_cast<uint16_t>(~len) != nlen) {
      r.lennlen_ok = false;
    }
    const size_t data_off = pos + 5;
    if (data_off + len > adler_off) {
      return r;
    }
    r.payload.insert(r.payload.end(), z.begin() + data_off,
                     z.begin() + data_off + len);
    finals.push_back((hdr & 0x01) != 0);
    pos = data_off + len;
    ++r.block_count;
  }
  if (pos != adler_off) {
    return r;
  }
  r.bfinal_ok = !finals.empty() && finals.back();
  for (size_t i = 0; i + 1 < finals.size(); ++i) {
    if (finals[i]) {
      r.bfinal_ok = false;
    }
  }
  r.adler = (static_cast<uint32_t>(z[adler_off]) << 24) |
            (static_cast<uint32_t>(z[adler_off + 1]) << 16) |
            (static_cast<uint32_t>(z[adler_off + 2]) << 8) |
            static_cast<uint32_t>(z[adler_off + 3]);
  r.ok = true;
  return r;
}

TestResult test_read_ppm_errors() {
  TestResult result;
  result.test_name = "read_ppm_errors";

  auto with_pixels = [](const std::string &header, size_t n) {
    std::vector<uint8_t> v(header.begin(), header.end());
    for (size_t i = 0; i < n; ++i) {
      v.push_back(static_cast<uint8_t>(i));
    }
    return v;
  };

  struct Case {
    const char *label;
    bool create;                 // write `bytes` to a temp file first
    std::vector<uint8_t> bytes;  // file contents when create == true
    const char *expect_substr;
  };

  const std::vector<Case> cases = {
      {"missing file", false, {}, "cannot open"},
      {"ascii P3 magic", true, with_pixels("P3\n2 1\n255\n", 6),
       "is not a binary PPM (P6)"},
      {"one byte file", true, bytes_of("P"), "is not a binary PPM (P6)"},
      {"non-numeric dimension", true, bytes_of("P6\nxx 1\n255\n"),
       "malformed PPM header"},
      {"eof before maxval", true, bytes_of("P6\n2 1\n"),
       "malformed PPM header"},
      {"zero width", true, bytes_of("P6\n0 1\n255\n"), "unsupported PPM"},
      {"zero height", true, bytes_of("P6\n2 0\n255\n"), "unsupported PPM"},
      {"maxval 254", true, with_pixels("P6\n2 1\n254\n", 6),
       "unsupported PPM"},
      {"maxval 65535", true, with_pixels("P6\n2 1\n65535\n", 6),
       "unsupported PPM"},
      {"truncated payload", true, with_pixels("P6\n2 2\n255\n", 6),
       "truncated PPM pixel data"},
  };

  for (const Case &c : cases) {
    TempFile tf("wally-ppm-err");
    if (c.create && !write_bytes(tf.path(), c.bytes)) {
      result.details = std::string("setup failed for case: ") + c.label;
      return result;
    }

    // Seed `out` with sentinels: a failed read must leave it untouched.
    wally::image::RgbImage out;
    out.width = 12345u;
    out.height = 67890u;
    out.rgb = {9, 9, 9};

    std::string error;
    const bool ok = wally::image::read_ppm(tf.path(), &out, &error);
    if (ok) {
      result.details = std::string("expected failure for case: ") + c.label;
      return result;
    }
    if (error.find(c.expect_substr) == std::string::npos) {
      result.expected = c.expect_substr;
      result.actual = error;
      result.details = std::string("wrong error for case: ") + c.label;
      return result;
    }
    if (out.width != 12345u || out.height != 67890u || out.rgb.size() != 3u ||
        out.rgb[0] != 9 || out.rgb[1] != 9 || out.rgb[2] != 9) {
      result.details =
          std::string("out mutated on failure for case: ") + c.label;
      return result;
    }
  }

  result.passed = true;
  return result;
}

TestResult test_read_ppm_happy_path() {
  TestResult result;
  result.test_name = "read_ppm_happy_path";

  // Minimal 2x1 image: exact tight RGB8 packing (what cmd_segment feeds as
  // stride = width*3, RAC_SEGMENTATION_PIXEL_FORMAT_RGB8).
  const std::vector<uint8_t> pixels = {10, 20, 30, 200, 210, 220};
  {
    TempFile tf("wally-ppm-2x1");
    if (!write_bytes(tf.path(), make_ppm(2, 1, pixels))) {
      result.details = "setup: cannot write 2x1 ppm";
      return result;
    }
    wally::image::RgbImage out;
    std::string error;
    if (!wally::image::read_ppm(tf.path(), &out, &error)) {
      result.details = "read_ppm failed on valid 2x1: " + error;
      return result;
    }
    if (out.width != 2u || out.height != 1u) {
      result.expected = "2x1";
      result.actual =
          std::to_string(out.width) + "x" + std::to_string(out.height);
      result.details = "wrong dimensions";
      return result;
    }
    if (out.rgb.size() != pixels.size() || out.rgb != pixels) {
      result.details = "pixel payload mismatch (tight RGB8 packing)";
      return result;
    }
  }

  // Larger buffer with a trailing byte beyond the declared payload: exactly
  // width*height*3 bytes are captured and the extra byte is ignored (no
  // off-by-one at the payload boundary).
  {
    const uint32_t w = 4;
    const uint32_t h = 3;
    std::vector<uint8_t> pixels2(static_cast<size_t>(w) * h * 3);
    for (size_t i = 0; i < pixels2.size(); ++i) {
      pixels2[i] = static_cast<uint8_t>(i * 7 + 1);
    }
    std::vector<uint8_t> file = make_ppm(w, h, pixels2);
    file.push_back(0xAB);  // trailing byte past the payload

    TempFile tf("wally-ppm-4x3");
    if (!write_bytes(tf.path(), file)) {
      result.details = "setup: cannot write 4x3 ppm";
      return result;
    }
    wally::image::RgbImage out;
    std::string error;
    if (!wally::image::read_ppm(tf.path(), &out, &error)) {
      result.details = "read_ppm failed on valid 4x3: " + error;
      return result;
    }
    if (out.width != w || out.height != h ||
        out.rgb.size() != static_cast<size_t>(w) * h * 3 ||
        out.rgb != pixels2) {
      result.details = "4x3 payload/boundary mismatch";
      return result;
    }
  }

  result.passed = true;
  return result;
}

TestResult test_read_ppm_header_lexing() {
  TestResult result;
  result.test_name = "read_ppm_header_lexing";

  const std::vector<uint8_t> pixels = {1, 2, 3, 4, 5, 6};

  // (a) '#'-to-EOL comments are skipped and (b) arbitrary/mixed whitespace
  // (spaces, tabs, newlines) between the magic and the three integers is
  // tolerated.
  {
    const std::string header =
        "P6\n"
        "# a comment line\n"
        "\t 2 \t 1\n"
        "# another comment\n"
        "255\n";
    std::vector<uint8_t> file(header.begin(), header.end());
    file.insert(file.end(), pixels.begin(), pixels.end());

    TempFile tf("wally-ppm-comments");
    if (!write_bytes(tf.path(), file)) {
      result.details = "setup: cannot write commented ppm";
      return result;
    }
    wally::image::RgbImage out;
    std::string error;
    if (!wally::image::read_ppm(tf.path(), &out, &error)) {
      result.details = "comments/whitespace not tolerated: " + error;
      return result;
    }
    if (out.width != 2u || out.height != 1u || out.rgb != pixels) {
      result.details = "commented header parsed to the wrong image";
      return result;
    }
  }

  // (c) exactly ONE whitespace byte is consumed between maxval and the pixel
  // payload (the `++pos` contract); a single space separator must work.
  {
    const std::string header = "P6\n2 1\n255 ";  // one space, then pixels
    std::vector<uint8_t> file(header.begin(), header.end());
    file.insert(file.end(), pixels.begin(), pixels.end());

    TempFile tf("wally-ppm-space-sep");
    if (!write_bytes(tf.path(), file)) {
      result.details = "setup: cannot write space-separator ppm";
      return result;
    }
    wally::image::RgbImage out;
    std::string error;
    if (!wally::image::read_ppm(tf.path(), &out, &error)) {
      result.details = "single-space separator not accepted: " + error;
      return result;
    }
    if (out.width != 2u || out.height != 1u || out.rgb != pixels) {
      result.details = "space-separated header parsed to the wrong image";
      return result;
    }
  }

  // (d) uint overflow guard: a dimension token > 0xFFFFFFFF is malformed.
  {
    const std::string header = "P6\n4294967296 1\n255\n";  // 2^32 width
    std::vector<uint8_t> file(header.begin(), header.end());
    file.insert(file.end(), pixels.begin(), pixels.end());

    TempFile tf("wally-ppm-overflow");
    if (!write_bytes(tf.path(), file)) {
      result.details = "setup: cannot write overflow ppm";
      return result;
    }
    wally::image::RgbImage out;
    std::string error;
    if (wally::image::read_ppm(tf.path(), &out, &error)) {
      result.details = "overflowing dimension should be rejected";
      return result;
    }
    if (error.find("malformed PPM header") == std::string::npos) {
      result.expected = "malformed PPM header";
      result.actual = error;
      return result;
    }
  }

  result.passed = true;
  return result;
}

TestResult test_write_png_invalid_args() {
  TestResult result;
  result.test_name = "write_png_invalid_args";

  const std::vector<uint8_t> px(2 * 2 * 4, 0x33);

  struct Case {
    const char *label;
    const uint8_t *data;
    int width;
    int height;
  };
  const Case cases[] = {
      {"null data", nullptr, 2, 2},
      {"zero width", px.data(), 0, 2},
      {"negative width", px.data(), -1, 2},
      {"zero height", px.data(), 2, 0},
      {"negative height", px.data(), 2, -3},
  };

  for (const Case &c : cases) {
    TempFile tf("wally-png-badarg");
    std::string error;
    const bool ok =
        wally::image::write_png(tf.path(), c.data, c.width, c.height, &error);
    if (ok) {
      result.details = std::string("expected failure for case: ") + c.label;
      return result;
    }
    if (error != "invalid image dimensions or data") {
      result.expected = "invalid image dimensions or data";
      result.actual = error;
      result.details = std::string("wrong error for case: ") + c.label;
      return result;
    }
    if (std::filesystem::exists(tf.path())) {
      result.details =
          std::string("no file should be created for case: ") + c.label;
      return result;
    }
  }

  result.passed = true;
  return result;
}

TestResult test_write_png_container() {
  TestResult result;
  result.test_name = "write_png_container";

  const int w = 2;
  const int h = 2;
  std::vector<uint8_t> rgba(static_cast<size_t>(w) * h * 4);
  for (size_t i = 0; i < rgba.size(); ++i) {
    rgba[i] = static_cast<uint8_t>(i * 11 + 3);
  }

  TempFile tf("wally-png-container");
  std::string error;
  if (!wally::image::write_png(tf.path(), rgba.data(), w, h, &error)) {
    result.details = "write_png failed: " + error;
    return result;
  }

  std::vector<uint8_t> png;
  if (!read_bytes(tf.path(), &png)) {
    result.details = "cannot read back written png";
    return result;
  }

  const uint8_t sig[8] = {137, 80, 78, 71, 13, 10, 26, 10};
  if (png.size() < 8u || std::memcmp(png.data(), sig, 8) != 0) {
    result.details = "missing/incorrect 8-byte PNG signature";
    return result;
  }

  std::vector<PngChunk> chunks;
  if (!parse_png(png, &chunks) || chunks.size() < 3u) {
    result.details = "PNG chunk structure did not parse";
    return result;
  }
  if (chunks.front().type != "IHDR") {
    result.actual = chunks.front().type;
    result.details = "first chunk must be IHDR";
    return result;
  }
  if (chunks.back().type != "IEND" || !chunks.back().data.empty()) {
    result.details = "final chunk must be a zero-length IEND";
    return result;
  }

  const PngChunk *ihdr = &chunks.front();
  if (ihdr->data.size() != 13u) {
    result.details = "IHDR must be 13 bytes";
    return result;
  }
  if (read_u32_be(ihdr->data, 0) != static_cast<uint32_t>(w) ||
      read_u32_be(ihdr->data, 4) != static_cast<uint32_t>(h)) {
    result.details = "IHDR width/height mismatch";
    return result;
  }
  if (ihdr->data[8] != 8 || ihdr->data[9] != 6) {
    result.expected = "8/6";
    result.actual = std::to_string(static_cast<int>(ihdr->data[8])) + "/" +
                    std::to_string(static_cast<int>(ihdr->data[9]));
    result.details = "IHDR bit-depth/color-type must be 8/6 (RGBA)";
    return result;
  }

  const PngChunk *idat = find_chunk(chunks, "IDAT");
  if (idat == nullptr) {
    result.details = "no IDAT chunk";
    return result;
  }
  if (idat->data.size() < 2u || idat->data[0] != 0x78 ||
      idat->data[1] != 0x01) {
    result.details = "IDAT zlib header must be 0x78 0x01";
    return result;
  }

  result.passed = true;
  return result;
}

TestResult test_write_png_byte_exact() {
  TestResult result;
  result.test_name = "write_png_byte_exact";

  const int w = 3;
  const int h = 2;
  std::vector<uint8_t> rgba(static_cast<size_t>(w) * h * 4);
  for (size_t i = 0; i < rgba.size(); ++i) {
    rgba[i] = static_cast<uint8_t>(i * 13 + 5);
  }

  TempFile tf("wally-png-exact");
  std::string error;
  if (!wally::image::write_png(tf.path(), rgba.data(), w, h, &error)) {
    result.details = "write_png failed: " + error;
    return result;
  }
  std::vector<uint8_t> png;
  if (!read_bytes(tf.path(), &png)) {
    result.details = "cannot read back written png";
    return result;
  }

  std::vector<PngChunk> chunks;
  if (!parse_png(png, &chunks)) {
    result.details = "PNG did not parse";
    return result;
  }

  // (c) every chunk's stored CRC-32 matches an independent computation.
  for (const PngChunk &c : chunks) {
    if (c.stored_crc != c.computed_crc) {
      result.details = "CRC-32 mismatch on chunk " + c.type;
      return result;
    }
  }

  const PngChunk *idat = find_chunk(chunks, "IDAT");
  if (idat == nullptr) {
    result.details = "no IDAT chunk";
    return result;
  }
  const ZlibParse z = parse_stored_zlib(idat->data);
  if (!z.ok) {
    result.details = "IDAT zlib stored-block stream did not parse";
    return result;
  }

  // (a) the stored block payload equals the independently reconstructed
  // filtered scanlines.
  const std::vector<uint8_t> raw = filtered_raw(rgba, w, h);
  if (z.payload != raw) {
    result.details = "stored DEFLATE payload != filtered scanlines";
    return result;
  }
  if (z.block_count != 1) {
    result.expected = "1";
    result.actual = std::to_string(z.block_count);
    result.details = "small image should be a single stored block";
    return result;
  }
  if (!z.bfinal_ok) {
    result.details = "single block must have BFINAL=1";
    return result;
  }
  if (!z.lennlen_ok) {
    result.details = "stored block LEN/NLEN are not one's-complement";
    return result;
  }

  // (b) trailing big-endian Adler-32 matches adler32(raw).
  if (z.adler != test_adler32(raw.data(), raw.size())) {
    result.details = "Adler-32 checksum mismatch";
    return result;
  }

  result.passed = true;
  return result;
}

TestResult test_write_png_multi_block() {
  TestResult result;
  result.test_name = "write_png_multi_block";

  // Filtered raw = height*(1 + width*4) must exceed 0xFFFF to force >1 stored
  // DEFLATE block. 200 * (1 + 400) = 80200 bytes => two blocks.
  const int w = 100;
  const int h = 200;
  std::vector<uint8_t> rgba(static_cast<size_t>(w) * h * 4);
  for (size_t i = 0; i < rgba.size(); ++i) {
    rgba[i] = static_cast<uint8_t>((i * 31 + 17) & 0xFF);
  }

  TempFile tf("wally-png-multiblock");
  std::string error;
  if (!wally::image::write_png(tf.path(), rgba.data(), w, h, &error)) {
    result.details = "write_png failed: " + error;
    return result;
  }
  std::vector<uint8_t> png;
  if (!read_bytes(tf.path(), &png)) {
    result.details = "cannot read back written png";
    return result;
  }

  std::vector<PngChunk> chunks;
  if (!parse_png(png, &chunks)) {
    result.details = "PNG did not parse";
    return result;
  }
  const PngChunk *idat = find_chunk(chunks, "IDAT");
  if (idat == nullptr) {
    result.details = "no IDAT chunk";
    return result;
  }
  const ZlibParse z = parse_stored_zlib(idat->data);
  if (!z.ok) {
    result.details = "IDAT zlib stored-block stream did not parse";
    return result;
  }
  if (z.block_count <= 1) {
    result.expected = ">1";
    result.actual = std::to_string(z.block_count);
    result.details = "expected more than one stored block";
    return result;
  }
  if (!z.bfinal_ok) {
    result.details = "only the final stored block may set BFINAL=1";
    return result;
  }
  if (!z.lennlen_ok) {
    result.details = "each block's LEN/NLEN must be one's-complement";
    return result;
  }

  const std::vector<uint8_t> raw = filtered_raw(rgba, w, h);
  if (z.payload != raw) {
    result.details = "reassembled multi-block payload != filtered scanlines";
    return result;
  }
  if (z.adler != test_adler32(raw.data(), raw.size())) {
    result.details = "Adler-32 checksum mismatch across blocks";
    return result;
  }

  result.passed = true;
  return result;
}

TestResult test_write_png_unwritable_path() {
  TestResult result;
  result.test_name = "write_png_unwritable_path";

  TempFile base("wally-png-nodir");
  // A path under a directory that was never created -> fopen("wb") fails.
  const std::string path =
      (std::filesystem::path(base.path()) / "no_such_subdir" / "x.png")
          .string();

  const std::vector<uint8_t> rgba(2 * 2 * 4, 0x40);
  std::string error;
  const bool ok = wally::image::write_png(path, rgba.data(), 2, 2, &error);
  if (ok) {
    result.details = "write_png should fail into a non-existent directory";
    return result;
  }
  if (error.find("cannot open") == std::string::npos ||
      error.find("for writing") == std::string::npos) {
    result.expected = "cannot open <path> for writing";
    result.actual = error;
    return result;
  }
  if (std::filesystem::exists(path)) {
    result.details = "no file should be produced on open failure";
    return result;
  }

  result.passed = true;
  return result;
}

TestResult test_bench_metrics_consume_only() {
    TestResult result;
    result.test_name = "bench_metrics_consume_only";

    // LLM: consume TokenUsage + measured phase fields; no tok/s or decode_ms invent.
    {
        runanywhere::v1::LLMGenerationResult r;
        r.set_generation_time_ms(1500.0);
        r.set_prompt_eval_time_ms(200);
        r.set_decode_time_ms(800);
        auto* usage = r.mutable_usage();
        usage->set_output_tokens(40);
        usage->set_decode_tokens_per_second(50.0);
        usage->set_prefill_ms(180);
        usage->set_ttft_ms(210);  // must not alias into prompt_eval_ms

        wally::commands::bench_metrics::LlmVlmMetrics m;
        if (!wally::commands::bench_metrics::fill_llm(r, /*measured_e2e_ms=*/9999.0, &m)) {
            result.details = "fill_llm rejected a valid result";
            return result;
        }
        if (m.output_tokens != 40 || m.end_to_end_ms != 1500.0 || m.tokens_per_second != 50.0 ||
            m.decode_ms != 800.0 || m.prompt_eval_ms != 200.0) {
            result.expected = "tokens=40 e2e=1500 tps=50 decode=800 prefill=200";
            result.actual = "tokens=" + std::to_string(m.output_tokens) +
                            " e2e=" + std::to_string(m.end_to_end_ms) +
                            " tps=" + std::to_string(m.tokens_per_second) +
                            " decode=" + std::to_string(m.decode_ms) +
                            " prefill=" + std::to_string(m.prompt_eval_ms);
            return result;
        }
    }

    // Missing decode throughput / phase times stay zero — no wall or tokens÷rate invent.
    {
        runanywhere::v1::LLMGenerationResult r;
        r.mutable_usage()->set_output_tokens(256);
        r.mutable_usage()->set_ttft_ms(500);  // still must not become prefill

        wally::commands::bench_metrics::LlmVlmMetrics m;
        if (!wally::commands::bench_metrics::fill_llm(r, /*measured_e2e_ms=*/20000.0, &m)) {
            result.details = "fill_llm should accept tokens with missing rates";
            return result;
        }
        if (m.tokens_per_second != 0.0 || m.decode_ms != 0.0 || m.prompt_eval_ms != 0.0 ||
            m.end_to_end_ms != 20000.0 || m.output_tokens != 256) {
            result.expected = "tps=0 decode=0 prefill=0 e2e=20000 tokens=256";
            result.actual = "tps=" + std::to_string(m.tokens_per_second) +
                            " decode=" + std::to_string(m.decode_ms) +
                            " prefill=" + std::to_string(m.prompt_eval_ms) +
                            " e2e=" + std::to_string(m.end_to_end_ms) +
                            " tokens=" + std::to_string(m.output_tokens);
            return result;
        }
    }

    // Zero output tokens fail rather than inventing metrics.
    {
        runanywhere::v1::LLMGenerationResult r;
        r.mutable_usage()->set_decode_tokens_per_second(99.0);
        wally::commands::bench_metrics::LlmVlmMetrics m;
        if (wally::commands::bench_metrics::fill_llm(r, 1000.0, &m)) {
            result.details = "fill_llm must reject zero output tokens";
            return result;
        }
    }

    // VLM: never invent tok/s from e2e; decode_ms always absent (no carrier).
    {
        runanywhere::v1::VLMResult r;
        r.set_total_time_ms(3000);
        auto* usage = r.mutable_usage();
        usage->set_output_tokens(64);
        usage->set_prefill_ms(120);
        usage->set_ttft_ms(130);

        wally::commands::bench_metrics::LlmVlmMetrics m;
        if (!wally::commands::bench_metrics::fill_vlm(r, /*measured_e2e_ms=*/9999.0, &m)) {
            result.details = "fill_vlm rejected a valid result";
            return result;
        }
        if (m.tokens_per_second != 0.0 || m.decode_ms != 0.0 || m.prompt_eval_ms != 120.0 ||
            m.end_to_end_ms != 3000.0 || m.output_tokens != 64) {
            result.expected = "tps=0 decode=0 prefill=120 e2e=3000 tokens=64";
            result.actual = "tps=" + std::to_string(m.tokens_per_second) +
                            " decode=" + std::to_string(m.decode_ms) +
                            " prefill=" + std::to_string(m.prompt_eval_ms) +
                            " e2e=" + std::to_string(m.end_to_end_ms) +
                            " tokens=" + std::to_string(m.output_tokens);
            return result;
        }
    }

    // Prefill falls back to TokenUsage.prefill_ms when prompt_eval_time_ms is absent.
    {
        runanywhere::v1::LLMGenerationResult r;
        r.mutable_usage()->set_output_tokens(8);
        r.mutable_usage()->set_prefill_ms(77);
        wally::commands::bench_metrics::LlmVlmMetrics m;
        if (!wally::commands::bench_metrics::fill_llm(r, 100.0, &m) || m.prompt_eval_ms != 77.0) {
            result.expected = "prefill_ms=77 from TokenUsage";
            result.actual = "prefill=" + std::to_string(m.prompt_eval_ms);
            return result;
        }
    }

    result.passed = true;
    return result;
}

// A negative or zero --trials must be rejected by CLI11's own PositiveNumber
// validator (ValidationError -> ParseError -> exit 2) before run_bench ever
// loads a model, not silently clamped to 1 trial and run anyway.
TestResult test_bench_negative_trials_exit2() {
    TestResult result;
    result.test_name = "bench_negative_trials_exit2";

    const int code = run_wally({"wally", "bench", "--trials", "-5"});
    if (code != 2) {
        result.expected = "2";
        result.actual = std::to_string(code);
        result.details = "`bench --trials -5` should be a usage error, not a clamped run";
        return result;
    }
    result.passed = true;
    return result;
}

TestResult test_run_max_tokens_zero_exit2() {
    TestResult result;
    result.test_name = "run_max_tokens_zero_exit2";

    const int code = run_wally({"wally", "run", "qwen3-0.6b", "hi", "--max-tokens", "0"});
    if (code != 2) {
        result.expected = "2";
        result.actual = std::to_string(code);
        result.details = "`run --max-tokens 0` should be a usage error, not an uncapped run";
        return result;
    }
    result.passed = true;
    return result;
}

TestResult test_run_max_tokens_negative_exit2() {
    TestResult result;
    result.test_name = "run_max_tokens_negative_exit2";

    const int code = run_wally({"wally", "run", "qwen3-0.6b", "hi", "--max-tokens", "-5"});
    if (code != 2) {
        result.expected = "2";
        result.actual = std::to_string(code);
        result.details = "`run --max-tokens -5` should be a usage error";
        return result;
    }
    result.passed = true;
    return result;
}

TestResult test_rerank_top_n_zero_exit2() {
    TestResult result;
    result.test_name = "rerank_top_n_zero_exit2";

    const int code = run_wally(
        {"wally", "rerank", "q", "-m", "m", "-d", "doc", "--top-n", "0"});
    if (code != 2) {
        result.expected = "2";
        result.actual = std::to_string(code);
        result.details = "`rerank --top-n 0` should be a usage error, not every document back";
        return result;
    }
    result.passed = true;
    return result;
}

TestResult test_rerank_top_n_negative_exit2() {
    TestResult result;
    result.test_name = "rerank_top_n_negative_exit2";

    const int code = run_wally(
        {"wally", "rerank", "q", "-m", "m", "-d", "doc", "--top-n", "-1"});
    if (code != 2) {
        result.expected = "2";
        result.actual = std::to_string(code);
        result.details = "`rerank --top-n -1` should be a usage error";
        return result;
    }
    result.passed = true;
    return result;
}

TestResult test_bench_zero_trials_exit2() {
    TestResult result;
    result.test_name = "bench_zero_trials_exit2";

    const int code = run_wally({"wally", "bench", "-n", "0"});
    if (code != 2) {
        result.expected = "2";
        result.actual = std::to_string(code);
        result.details = "`bench -n 0` should be a usage error";
        return result;
    }
    result.passed = true;
    return result;
}

// Model verbs live under the `models` namespace only (`wally models
// list|pull|rm|show`); there is no top-level `ls`/`pull`/`show`/`rm`
// shortcut. `list` is the primary registered name there (`ls` is its
// alias), and `run` remains the only top-level model shortcut.
TestResult test_models_ls_is_primary_name() {
    TestResult result;
    result.test_name = "models_ls_is_primary_name";

    wally::GlobalOptions options;
    CLI::App app{"wally test app"};
    wally::configure_app(app, options);

    if (app.get_subcommand_no_throw("ls") != nullptr) {
        result.details = "top-level ls must not be registered; use `models list`";
        return result;
    }

    const CLI::App *models = app.get_subcommand_no_throw("models");
    if (models == nullptr) {
        result.details = "models subcommand not registered";
        return result;
    }
    const CLI::App *list = models->get_subcommand_no_throw("list");
    if (list == nullptr) {
        result.details = "models list not registered";
        return result;
    }
    if (list->get_name() != "list") {
        result.expected = "list";
        result.actual = list->get_name();
        result.details = "list must be the primary name under models";
        return result;
    }
    const CLI::App *ls = models->get_subcommand_no_throw("ls");
    if (ls != list) {
        result.details = "models ls must resolve to the same subcommand, as an alias";
        return result;
    }
    result.passed = true;
    return result;
}

TestResult test_loopback_token() {
    TestResult result;
    result.test_name = "loopback_token";

    const std::string a = wally::net::GenerateLoopbackToken();
    const std::string b = wally::net::GenerateLoopbackToken();
    // 32 bytes of randomness rendered as hex, and two draws do not collide.
    if (a.size() != 64 || b.size() != 64 || a == b) {
        result.details = "token is not 64 hex chars, or two draws matched";
        return result;
    }
    for (const char c : a) {
        if (std::strchr("0123456789abcdef", c) == nullptr) {
            result.details = "token has a non-hex character";
            return result;
        }
    }
    // The constant-time compare still has to be a correct compare.
    if (!wally::net::ConstantTimeEquals(a, a) || wally::net::ConstantTimeEquals(a, b) ||
        wally::net::ConstantTimeEquals(a, a + "x") || wally::net::ConstantTimeEquals("", "x")) {
        result.details = "ConstantTimeEquals gave a wrong answer";
        return result;
    }
    result.passed = true;
    return result;
}

TestResult test_model_labels_format() {
    TestResult result;
    result.test_name = "model_labels_format";

    namespace v1 = runanywhere::v1;
    using wally::commands::model_labels::format;

    // `models show --json` used to emit format() as a raw enum int; every
    // value here has to render as the same kind of human string
    // `models list --json` already gives category()/backend().
    const struct {
        v1::ModelFormat value;
        std::string expected;
    } cases[] = {
        {v1::MODEL_FORMAT_GGUF, "GGUF"},
        {v1::MODEL_FORMAT_COREML, "Core ML"},
        {v1::MODEL_FORMAT_SAFETENSORS, "SafeTensors"},
        {v1::MODEL_FORMAT_UNSPECIFIED, "?"},
    };
    for (const auto &c : cases) {
        const std::string actual = format(c.value);
        if (actual != c.expected) {
            result.expected = c.expected;
            result.actual = actual;
            return result;
        }
    }
    result.passed = true;
    return result;
}

// A private WALLY_PROFILE_DIR so the preferences tests never read or write the
// real profile. Removed on scope exit.
class TempProfileDir {
public:
  TempProfileDir() {
    const auto stamp = std::chrono::steady_clock::now().time_since_epoch().count();
    path_ = std::filesystem::temp_directory_path() /
            ("wally-prefs-" + std::to_string(stamp));
    std::error_code ec;
    std::filesystem::remove_all(path_, ec);
    std::filesystem::create_directories(path_, ec);
    dir_str_ = path_.string();
  }
  ~TempProfileDir() {
    std::error_code ec;
    std::filesystem::remove_all(path_, ec);
  }
  const char *c_str() const { return dir_str_.c_str(); }

private:
  std::filesystem::path path_;
  std::string dir_str_;
};

TestResult test_default_model_resolution() {
  TestResult result;
  result.test_name = "default_model_resolution";

  TempProfileDir dir;
  EnvVar profile("WALLY_PROFILE_DIR", dir.c_str());
  EnvVar legacy("RCLI_PROFILE_DIR", nullptr);

  {
    EnvVar env_default("WALLY_DEFAULT_MODEL", nullptr);
    // Fresh: no env or file default. An explicit model passes through; an empty
    // one falls back to the model compiled into the binary.
    if (wally::prefs::ResolveModel("qwen3-0.6b") != "qwen3-0.6b") {
      result.details = "explicit model should pass through unchanged";
      return result;
    }
    if (wally::prefs::ResolveModel("") != WALLY_DEFAULT_MODEL_ID) {
      result.details = "empty input with no env/file default should use the built-in model";
      return result;
    }
    if (wally::prefs::EffectiveDefaultModel().source !=
        wally::prefs::DefaultModelSource::BuiltIn) {
      result.details = "no env/file default should resolve to the built-in source";
      return result;
    }

    std::string error;
    if (!wally::prefs::SetDefaultModel("glm-5.3-flash", &error)) {
      result.details = "SetDefaultModel failed: " + error;
      return result;
    }
    // File default fills in for an omitted model, but never overrides explicit.
    if (wally::prefs::ResolveModel("") != "glm-5.3-flash") {
      result.details = "file default should fill in for an empty model";
      return result;
    }
    if (wally::prefs::ResolveModel("qwen3-0.6b") != "qwen3-0.6b") {
      result.details = "explicit model should beat the file default";
      return result;
    }
  }
  {
    // The environment override outranks the file.
    EnvVar env_default("WALLY_DEFAULT_MODEL", "env-model");
    const wally::prefs::DefaultModel effective = wally::prefs::EffectiveDefaultModel();
    if (effective.id != "env-model" ||
        effective.source != wally::prefs::DefaultModelSource::Environment) {
      result.details = "WALLY_DEFAULT_MODEL should outrank the file";
      return result;
    }
    if (wally::prefs::ResolveModel("") != "env-model") {
      result.details = "env default should resolve for an empty model";
      return result;
    }
  }
  {
    // With the env override gone, the file default is back.
    EnvVar env_default("WALLY_DEFAULT_MODEL", nullptr);
    if (wally::prefs::EffectiveDefaultModel().source !=
        wally::prefs::DefaultModelSource::File) {
      result.details = "file default should return once the env override is gone";
      return result;
    }
  }

  result.passed = true;
  return result;
}

TestResult test_default_model_store() {
  TestResult result;
  result.test_name = "default_model_store";

  TempProfileDir dir;
  EnvVar profile("WALLY_PROFILE_DIR", dir.c_str());
  EnvVar legacy("RCLI_PROFILE_DIR", nullptr);
  EnvVar env_default("WALLY_DEFAULT_MODEL", nullptr);

  std::string error;
  // Set then clear round-trips, and clearing an already-empty default is fine.
  if (!wally::prefs::SetDefaultModel("glm-5.3-flash", &error) ||
      wally::prefs::FileDefaultModel().value_or("") != "glm-5.3-flash") {
    result.details = "set did not round-trip: " + error;
    return result;
  }
  if (!wally::prefs::ClearDefaultModel(&error) ||
      wally::prefs::FileDefaultModel().has_value()) {
    result.details = "clear did not remove the default: " + error;
    return result;
  }
  if (!wally::prefs::ClearDefaultModel(&error)) {
    result.details = "clearing an already-empty default should succeed";
    return result;
  }

  // An unsafe or empty id is rejected, and nothing is written.
  if (wally::prefs::SetDefaultModel("bad<id>", &error) ||
      wally::prefs::SetDefaultModel("", &error) ||
      wally::prefs::FileDefaultModel().has_value()) {
    result.details = "an unsafe or empty id should be rejected";
    return result;
  }

  // A corrupt preferences file degrades gracefully, never a throw: no file
  // default, and the effective default falls through to the built-in.
  {
    std::ofstream corrupt(wally::prefs::PreferencesPath(), std::ios::binary | std::ios::trunc);
    corrupt << "{ this is not json";
  }
  if (wally::prefs::FileDefaultModel().has_value()) {
    result.details = "a corrupt file should yield no file default";
    return result;
  }
  const wally::prefs::DefaultModel corrupt_effective = wally::prefs::EffectiveDefaultModel();
  if (corrupt_effective.source != wally::prefs::DefaultModelSource::BuiltIn ||
      corrupt_effective.id != WALLY_DEFAULT_MODEL_ID) {
    result.details = "a corrupt file should fall through to the built-in default";
    return result;
  }

  result.passed = true;
  return result;
}

TestResult test_upstream_failure_mapping() {
  TestResult result;
  result.test_name = "upstream_failure_mapping";
  namespace tr = wally::anthropic::translate;

  struct Case {
    int status;
    const char *body;
    const char *want_type;
    const char *want_message;
  };
  // The status decides the type so the wrapped tool stops retrying a refusal it
  // cannot satisfy; the message comes from an OpenAI-style error body, else the
  // raw body, else a status line, else a no-answer note.
  const Case cases[] = {
      {0, "", "api_error", "the model endpoint did not answer"},
      {429, R"({"error":{"message":"Rate limit exceeded"}})", "rate_limit_error",
       "Rate limit exceeded"},
      {403, R"({"error":{"message":"Out of credit."}})", "permission_error", "Out of credit."},
      {401, R"({"error":{"message":"bad key"}})", "authentication_error", "bad key"},
      {500, "upstream boom", "api_error", "upstream boom"},
      {502, "   ", "api_error", "the model endpoint returned status 502"},
  };

  for (const Case &c : cases) {
    std::string type;
    std::string message;
    tr::UpstreamFailure(c.status, c.body, &type, &message);
    if (type != c.want_type || message != c.want_message) {
      result.details = "status " + std::to_string(c.status) + " gave (" + type + ", " + message +
                       "), wanted (" + c.want_type + ", " + c.want_message + ")";
      return result;
    }
  }
  result.passed = true;
  return result;
}

TestResult test_passthrough_argv_split() {
  TestResult result;
  result.test_name = "passthrough_argv_split";

  struct Case {
    std::vector<std::string> in;
    std::vector<std::string> want;
  };
  const std::vector<Case> cases = {
      // A leading tool flag is separated so CLI11 forwards it.
      {{"wally", "claude-code", "--dangerously-skip-permissions"},
       {"wally", "claude-code", "--", "--dangerously-skip-permissions"}},
      // wally's own -m is consumed first; the tool flag after is separated.
      {{"wally", "claude-code", "-m", "glm-5.3-flash", "-p", "hi"},
       {"wally", "claude-code", "-m", "glm-5.3-flash", "--", "-p", "hi"}},
      // --model=... inline form is a wally flag too.
      {{"wally", "opencode", "--cloud", "--model=glm-5.3-flash", "run"},
       {"wally", "opencode", "--cloud", "--model=glm-5.3-flash", "--", "run"}},
      // An explicit -- is left exactly as the reader wrote it.
      {{"wally", "claude-code", "--", "-p", "hi"}, {"wally", "claude-code", "--", "-p", "hi"}},
      // Only wally flags: nothing to forward, no -- added.
      {{"wally", "claude-code", "-m", "glm-5.3-flash"},
       {"wally", "claude-code", "-m", "glm-5.3-flash"}},
      // Not a passthrough command: untouched.
      {{"wally", "run", "qwen3-0.6b", "hi"}, {"wally", "run", "qwen3-0.6b", "hi"}},
  };

  for (const Case &c : cases) {
    const std::vector<std::string> got = wally::SplitPassthroughArgv(c.in);
    if (got != c.want) {
      std::string g;
      for (const std::string &t : got) g += t + " ";
      result.details = "for [" + c.in[1] + " ...] got: " + g;
      return result;
    }
  }
  result.passed = true;
  return result;
}

TestResult test_stream_usage_reports_input_tokens() {
  TestResult result;
  result.test_name = "stream_usage_reports_input_tokens";
  namespace tr = wally::anthropic::translate;

  tr::StreamState state;
  // A streaming chunk carrying content plus the final usage (gateways attach
  // prompt/completion counts to a late data chunk).
  const nlohmann::json chunk = nlohmann::json::parse(
      R"({"id":"c1","choices":[{"delta":{"content":"hi"},"finish_reason":"stop"}],)"
      R"("usage":{"prompt_tokens":1234,"completion_tokens":56}})");
  tr::StreamChunkToAnthropic(chunk, &state);
  const std::string closing = tr::StreamCloseToAnthropic(&state);

  // The closing message_delta must carry the REAL input_tokens, or a wrapped
  // tool's context gauge stays at zero and auto-compaction never fires.
  if (closing.find("\"input_tokens\":1234") == std::string::npos) {
    result.details = "message_delta missing real input_tokens; got: " + closing.substr(0, 300);
    return result;
  }
  if (closing.find("\"output_tokens\":56") == std::string::npos) {
    result.details = "message_delta missing output_tokens";
    return result;
  }
  result.passed = true;
  return result;
}

TestResult test_estimate_request_tokens() {
  TestResult result;
  result.test_name = "estimate_request_tokens";
  namespace tr = wally::anthropic::translate;

  // Nothing to estimate from.
  if (tr::EstimateRequestTokens(nlohmann::json::object()) != 0) {
    result.details = "an empty request should estimate to 0 tokens";
    return result;
  }
  // 10-character system prompt plus a 10-character user turn: ~4 chars/token
  // over 20 characters is 5.
  const nlohmann::json request = nlohmann::json::parse(
      R"({"system": "0123456789", "messages": [{"role": "user", "content": "0123456789"}]})");
  const int got = tr::EstimateRequestTokens(request);
  if (got != 5) {
    result.details = "expected 5 estimated tokens for 20 characters, got " + std::to_string(got);
    return result;
  }

  // A coding turn's bulk is tool results and call arguments, not top-level
  // text. A tool_result with 40 characters of nested content and a tool_use
  // whose serialized input is 20 characters must both reach the estimate, or a
  // request built almost entirely of them looks nearly free and compaction
  // fires too late. 60 characters -> 15 tokens; counting only the empty
  // top-level text would give 0.
  const nlohmann::json tools = nlohmann::json::parse(R"({
    "messages": [
      {"role": "user", "content": [
        {"type": "tool_result", "tool_use_id": "t1",
         "content": [{"type": "text", "text": "0123456789012345678901234567890123456789"}]}
      ]},
      {"role": "assistant", "content": [
        {"type": "tool_use", "id": "t2", "name": "edit", "input": {"path": "0123456789"}}
      ]}
    ]
  })");
  // tool_result content is 40 chars; the tool_use input serializes to
  // {"path":"0123456789"} = 21 chars. 61 -> 15 tokens.
  const int with_tools = tr::EstimateRequestTokens(tools);
  if (with_tools < 14) {
    result.details = "tool_result content and tool_use input must reach the estimate; got " +
                     std::to_string(with_tools);
    return result;
  }
  result.passed = true;
  return result;
}

TestResult test_message_start_usage_carries_input_estimate() {
  TestResult result;
  result.test_name = "message_start_usage_carries_input_estimate";
  namespace tr = wally::anthropic::translate;

  // message_start fires on the very first chunk, before the upstream has
  // said anything about usage -- there is no real input_tokens to report yet,
  // only the caller's pre-computed estimate (EstimateRequestTokens).
  tr::StreamState state;
  state.input_estimate = 42;
  const nlohmann::json chunk =
      nlohmann::json::parse(R"({"id":"c1","choices":[{"delta":{"role":"assistant"}}]})");
  const std::string opening = tr::StreamChunkToAnthropic(chunk, &state);

  if (opening.find("\"input_tokens\":42") == std::string::npos) {
    result.details = "message_start did not carry the input estimate; got: " +
                     opening.substr(0, 300);
    return result;
  }
  // output_tokens is genuinely 0 here -- nothing has been generated yet, so
  // this one is not an estimate.
  if (opening.find("\"output_tokens\":0") == std::string::npos) {
    result.details = "message_start's output_tokens should read 0; got: " + opening.substr(0, 300);
    return result;
  }
  result.passed = true;
  return result;
}

TestResult test_stream_usage_falls_back_when_endpoint_never_reports_it() {
  TestResult result;
  result.test_name = "stream_usage_falls_back_when_endpoint_never_reports_it";
  namespace tr = wally::anthropic::translate;

  // Content and a finish reason arrive -- a turn that, by every other signal,
  // completed normally -- but the endpoint's stream ends without ever
  // attaching a usage object. A dropped tail chunk (the failure this repro's
  // against a live upstream) and a backend that silently ignores
  // stream_options.include_usage both look like this from here.
  tr::StreamState state;
  state.input_estimate = 40;  // stands in for EstimateRequestTokens on the request
  const nlohmann::json content_chunk =
      nlohmann::json::parse(R"({"id":"c1","choices":[{"delta":{"content":"0123456789"}}]})");
  const nlohmann::json finish_chunk =
      nlohmann::json::parse(R"({"id":"c1","choices":[{"delta":{},"finish_reason":"stop"}]})");
  tr::StreamChunkToAnthropic(content_chunk, &state);
  tr::StreamChunkToAnthropic(finish_chunk, &state);
  const std::string closing = tr::StreamCloseToAnthropic(&state);

  // Neither count may read as a confident, endpoint-reported zero: shown as
  // 0, a wrapped tool's cost display reads that as "this turn cost nothing,"
  // which is a specific false claim when the truth is simply unmeasured. The
  // fallback is the same ~4-chars-per-token estimate EstimateRequestTokens
  // uses, applied to the 10 assistant characters actually written.
  if (closing.find("\"input_tokens\":40") == std::string::npos) {
    result.details = "message_delta dropped the input estimate; got: " + closing.substr(0, 300);
    return result;
  }
  if (closing.find("\"output_tokens\":3") == std::string::npos) {
    result.details = "message_delta did not fall back to the character estimate; got: " +
                     closing.substr(0, 300);
    return result;
  }
  result.passed = true;
  return result;
}

TestResult test_reasoning_content_counts_without_an_unsigned_block() {
  TestResult result;
  result.test_name = "reasoning_content_counts_without_an_unsigned_block";
  namespace tr = wally::anthropic::translate;

  // glm-5.3-flash streams its thinking as `delta.reasoning_content`, separate
  // from `delta.content`, and on a tight max_tokens budget it can be the ONLY
  // thing the model emits (measured: 199 of 200 completion_tokens, answer text
  // never started). Those characters must count toward the fallback estimate
  // so the turn is not mistaken for a free one -- but they must NOT go out as
  // a `thinking` block, which Anthropic's stream requires a signature_delta to
  // close and an OpenAI endpoint cannot sign.
  tr::StreamState state;
  const nlohmann::json thinking_chunk = nlohmann::json::parse(
      R"({"id":"c1","choices":[{"delta":{"reasoning_content":"counting to five"}}]})");
  const nlohmann::json finish_chunk = nlohmann::json::parse(
      R"({"id":"c1","choices":[{"delta":{},"finish_reason":"length"}]})");
  const std::string opening = tr::StreamChunkToAnthropic(thinking_chunk, &state);
  tr::StreamChunkToAnthropic(finish_chunk, &state);
  const std::string closing = tr::StreamCloseToAnthropic(&state);

  // No unsigned thinking block on the wire, in either half of the stream.
  if (opening.find("\"type\":\"thinking\"") != std::string::npos ||
      opening.find("thinking_delta") != std::string::npos ||
      closing.find("\"type\":\"thinking\"") != std::string::npos) {
    result.details = "reasoning must not surface as a thinking block; got: " +
                     opening.substr(0, 300) + " | " + closing.substr(0, 200);
    return result;
  }
  // "counting to five" is 17 characters -> a 4-token fallback estimate. The
  // real point: not 0. A turn that spent its whole budget thinking must not
  // report as though nothing happened.
  if (closing.find("\"output_tokens\":0") != std::string::npos) {
    result.details = "thinking-only turn still reports 0 output_tokens; got: " +
                     closing.substr(0, 300);
    return result;
  }
  if (closing.find("\"output_tokens\":4") == std::string::npos) {
    result.details = "expected the 4-token character estimate for 17 characters; got: " +
                     closing.substr(0, 300);
    return result;
  }
  result.passed = true;
  return result;
}

}  // namespace

TestResult test_system_turns_fold_into_the_leading_system_message() {
  TestResult result;
  result.test_name = "system_turns_fold_into_the_leading_system_message";
  namespace tr = wally::anthropic::translate;

  // The shape Claude Code 2.1.268 really sends, captured through wally's shim:
  // a top-level `system` array AND a `role: "system"` turn inside `messages`,
  // after the first user turn (its environment block). Passed through as-is,
  // OpenAI gets [system, user, system], and Qwen's chat template refuses the
  // whole request: "System message must be at the beginning."
  const nlohmann::json anthropic = nlohmann::json::parse(R"({
    "model": "qwen3.8-27b",
    "system": [{"type": "text", "text": "You are Claude Code."}],
    "messages": [
      {"role": "user", "content": [{"type": "text", "text": "Reply with pong"}]},
      {"role": "system", "content": [{"type": "text", "text": "# Environment\nPlatform: darwin"}]}
    ]
  })");
  const nlohmann::json openai = tr::RequestToOpenAI(anthropic, "qwen3.8-27b");
  const nlohmann::json& messages = openai["messages"];

  int system_count = 0;
  for (std::size_t i = 0; i < messages.size(); ++i) {
    if (messages[i].value("role", "") != "system") {
      continue;
    }
    ++system_count;
    if (i != 0) {
      result.details = "a system message sits at index " + std::to_string(i) +
                       ", which Qwen refuses; got: " + messages.dump().substr(0, 300);
      return result;
    }
  }
  if (system_count != 1) {
    result.expected = "exactly one system message";
    result.actual = std::to_string(system_count) + ": " + messages.dump().substr(0, 300);
    return result;
  }
  const std::string system = messages[0].value("content", "");
  const auto top = system.find("You are Claude Code.");
  const auto env = system.find("# Environment");
  if (top == std::string::npos || env == std::string::npos || env < top) {
    result.details = "the leading system message must carry both texts, top-level first; got: " +
                     system.substr(0, 300);
    return result;
  }
  if (messages.size() != 2 || messages[1].value("role", "") != "user") {
    result.details = "the user turn must follow the system message and nothing else; got: " +
                     messages.dump().substr(0, 300);
    return result;
  }
  result.passed = true;
  return result;
}

TestResult test_unrunnable_web_search_adds_system_note() {
  TestResult result;
  result.test_name = "unrunnable_web_search_adds_system_note";
  namespace tr = wally::anthropic::translate;

  // Claude Code advertises Anthropic's server-side `web_search` tool (no
  // input_schema). We have no backend to run it, so the tool is dropped AND the
  // model is told, in the system prompt, that web search is unavailable — so it
  // answers from its own knowledge instead of narrating a search it never ran.
  const nlohmann::json anthropic = nlohmann::json::parse(R"({
    "model": "glm-5.3-flash",
    "system": [{"type": "text", "text": "You are Claude Code."}],
    "messages": [{"role": "user", "content": [{"type": "text", "text": "search the web for java"}]}],
    "tools": [
      {"type": "web_search_20250305", "name": "web_search"},
      {"name": "Read", "input_schema": {"type": "object"}}
    ]
  })");
  const nlohmann::json openai = tr::RequestToOpenAI(anthropic, "glm-5.3-flash");

  const nlohmann::json& messages = openai["messages"];
  if (messages.empty() || messages[0].value("role", "") != "system") {
    result.details = "expected a leading system message; got: " + messages.dump().substr(0, 300);
    return result;
  }
  const std::string system = messages[0].value("content", "");
  if (system.find("You are Claude Code.") == std::string::npos ||
      system.find("Web search and browsing are unavailable") == std::string::npos) {
    result.details = "system message must keep the client prompt and add the note; got: " +
                     system.substr(0, 400);
    return result;
  }

  // The server-side web_search tool is not forwarded; the real client tool is.
  bool web_search_forwarded = false;
  bool read_forwarded = false;
  if (openai.contains("tools")) {
    for (const auto& tool : openai["tools"]) {
      const std::string name = tool.value("function", nlohmann::json::object()).value("name", "");
      if (name == "web_search") web_search_forwarded = true;
      if (name == "Read") read_forwarded = true;
    }
  }
  if (web_search_forwarded || !read_forwarded) {
    result.expected = "web_search dropped, Read forwarded";
    result.actual = openai.value("tools", nlohmann::json::array()).dump().substr(0, 300);
    return result;
  }
  result.passed = true;
  return result;
}

TestResult test_ensure_installed_finds_tool_off_path() {
  TestResult result;
  result.test_name = "ensure_installed_finds_tool_off_path";
#if defined(_WIN32)
  // The probe's Windows locations key off APPDATA/LOCALAPPDATA and _putenv_s;
  // exercised by the Windows CI build. Skip here.
  result.passed = true;
  return result;
#else
  namespace fs = std::filesystem;

  // A tool installed to ~/.local/bin but not yet on PATH (a fresh `npm i -g`
  // before a shell restart is the common case) must still count as installed,
  // and EnsureInstalled must put that dir on PATH so the launch can exec it.
  const fs::path home = fs::temp_directory_path() /
      ("wally-hometest-" + std::to_string(std::random_device{}()));
  const fs::path bin = home / ".local" / "bin";
  std::error_code ec;
  fs::create_directories(bin, ec);
  const std::string tool = "wallyfaketool12345";
  { std::ofstream f(bin / tool); f << "#!/bin/sh\n"; }
  fs::permissions(bin / tool, fs::perms::owner_all, ec);

  const char* old_home = std::getenv("HOME");
  const std::string saved_home = old_home != nullptr ? old_home : "";
  const char* old_path = std::getenv("PATH");
  const std::string saved_path = old_path != nullptr ? old_path : "";
  setenv("HOME", home.c_str(), 1);

  const bool found = wally::harness::EnsureInstalled(tool);
  const char* new_path = std::getenv("PATH");
  const bool path_updated = new_path != nullptr && std::string(new_path).find(bin.string()) == 0;

  if (old_home != nullptr) {
    setenv("HOME", saved_home.c_str(), 1);
  } else {
    unsetenv("HOME");
  }
  setenv("PATH", saved_path.c_str(), 1);
  fs::remove_all(home, ec);

  if (!found) {
    result.details = "EnsureInstalled missed a tool sitting in ~/.local/bin off PATH";
    return result;
  }
  if (!path_updated) {
    result.details = "the tool's directory was not prepended to PATH for the launch";
    return result;
  }
  result.passed = true;
  return result;
#endif
}

int main(int argc, char **argv) {
  TestSuite suite("wally_unit");
  suite.add("json_escape", test_json_escape);
  suite.add("json_writer_shape", test_json_writer_shape);
  suite.add("json_writer_nan_is_null", test_json_writer_nan_is_null);
  suite.add("human_bytes", test_human_bytes);
  suite.add("normalize_dir", test_normalize_dir);
  suite.add("resolve_home_precedence", test_resolve_home_precedence);
  suite.add("state_dir", test_state_dir);
  suite.add("catalog_lookup", test_catalog_lookup);
  suite.add("overlay_catalog", test_overlay_catalog);
  suite.add("nvidia_sherpa_catalog", test_nvidia_sherpa_catalog);
  suite.add("engine_hint_parsing", test_engine_hint_parsing);
  suite.add("mlx_catalog_registration", test_mlx_catalog_registration);
  suite.add("hf_ref_registration", test_hf_ref_registration);
  suite.add("diarize_arg_surface", test_diarize_arg_surface);
  suite.add("diarize_missing_model_exit2", test_diarize_missing_model_exit2);
  suite.add("diarize_missing_audio_exit2", test_diarize_missing_audio_exit2);
  suite.add("diarize_audio_not_found_exit2", test_diarize_audio_not_found_exit2);
  suite.add("diarize_numeric_option_typing_exit2",
            test_diarize_numeric_option_typing_exit2);
  suite.add("diarize_unknown_flag_exit2", test_diarize_unknown_flag_exit2);
  suite.add("read_ppm_errors", test_read_ppm_errors);
  suite.add("read_ppm_happy_path", test_read_ppm_happy_path);
  suite.add("read_ppm_header_lexing", test_read_ppm_header_lexing);
  suite.add("write_png_invalid_args", test_write_png_invalid_args);
  suite.add("write_png_container", test_write_png_container);
  suite.add("write_png_byte_exact", test_write_png_byte_exact);
  suite.add("write_png_multi_block", test_write_png_multi_block);
  suite.add("write_png_unwritable_path", test_write_png_unwritable_path);
  suite.add("bench_metrics_consume_only", test_bench_metrics_consume_only);
  suite.add("run_max_tokens_zero_exit2", test_run_max_tokens_zero_exit2);
  suite.add("run_max_tokens_negative_exit2", test_run_max_tokens_negative_exit2);
  suite.add("rerank_top_n_zero_exit2", test_rerank_top_n_zero_exit2);
  suite.add("rerank_top_n_negative_exit2", test_rerank_top_n_negative_exit2);
  suite.add("bench_negative_trials_exit2", test_bench_negative_trials_exit2);
  suite.add("bench_zero_trials_exit2", test_bench_zero_trials_exit2);
  suite.add("models_ls_is_primary_name", test_models_ls_is_primary_name);
  suite.add("loopback_token", test_loopback_token);
  suite.add("model_labels_format", test_model_labels_format);
  suite.add("default_model_resolution", test_default_model_resolution);
  suite.add("default_model_store", test_default_model_store);
  suite.add("upstream_failure_mapping", test_upstream_failure_mapping);
  suite.add("passthrough_argv_split", test_passthrough_argv_split);
  suite.add("stream_usage_reports_input_tokens", test_stream_usage_reports_input_tokens);
  suite.add("estimate_request_tokens", test_estimate_request_tokens);
  suite.add("message_start_usage_carries_input_estimate",
            test_message_start_usage_carries_input_estimate);
  suite.add("stream_usage_falls_back_when_endpoint_never_reports_it",
            test_stream_usage_falls_back_when_endpoint_never_reports_it);
  suite.add("reasoning_content_counts_without_an_unsigned_block",
            test_reasoning_content_counts_without_an_unsigned_block);
  suite.add("system_turns_fold_into_the_leading_system_message",
            test_system_turns_fold_into_the_leading_system_message);
  suite.add("unrunnable_web_search_adds_system_note",
            test_unrunnable_web_search_adds_system_note);
  suite.add("ensure_installed_finds_tool_off_path",
            test_ensure_installed_finds_tool_off_path);
  return suite.run(argc, argv);
}

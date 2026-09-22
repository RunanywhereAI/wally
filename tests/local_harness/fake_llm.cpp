// Real CLI + SDK HTTP server, deterministic inference only. This binary is
// never installed or shipped with Wally.
#include "app.h"
#include "rac/features/llm/rac_llm_service.h"
#include "rac/plugin/rac_plugin_entry.h"
#include "rac/server/rac_server.h"

#include <cstdlib>
#include <cstring>
#include <fstream>
#include <map>
#include <nlohmann/json.hpp>
#include <optional>
#include <string>

namespace {
using Json = nlohmann::json;
Json report = {{"created", 0}, {"initialized", 0}, {"destroyed", 0}, {"generated", 0}};
struct Session {
    std::string path;
    bool ready = false;
    int context = 8192;
};

rac_result_t Create(const char* model, const char* config, void** out) {
    auto* session = new Session;
    session->path = model == nullptr ? "" : model;
    if (config != nullptr) {
        report["config"] = Json::parse(config);
        session->context = report["config"].value("context_length", 8192);
    }
    report["created"] = report["created"].get<int>() + 1;
    report["model_path"] = session->path;
    *out = session;
    return RAC_SUCCESS;
}
rac_result_t Initialize(void* impl, const char* path) {
    if (std::getenv("WALLY_TEST_FAIL_LOAD") != nullptr) return RAC_ERROR_MODEL_LOAD_FAILED;
    auto* session = static_cast<Session*>(impl);
    session->path = path;
    session->ready = true;
    report["initialized"] = report["initialized"].get<int>() + 1;
    return RAC_SUCCESS;
}
rac_result_t Generate(void* impl, const char* prompt, const rac_llm_options_t*,
                      rac_llm_result_t* result) {
    if (!static_cast<Session*>(impl)->ready) return RAC_ERROR_NOT_INITIALIZED;
    report["generated"] = report["generated"].get<int>() + 1;
    report["last_prompt"] = prompt;
    *result = {};
    constexpr char text[] = "local harness reply";
    result->text = static_cast<char*>(std::malloc(sizeof(text)));
    std::memcpy(result->text, text, sizeof(text));
    result->prompt_tokens = 3;
    result->completion_tokens = 3;
    result->total_tokens = 6;
    return RAC_SUCCESS;
}
rac_result_t Stream(void* impl, const char* prompt, const rac_llm_options_t*,
                    rac_llm_stream_callback_fn callback, void* user) {
    if (!static_cast<Session*>(impl)->ready) return RAC_ERROR_NOT_INITIALIZED;
    report["generated"] = report["generated"].get<int>() + 1;
    report["last_prompt"] = prompt;
    callback("local harness reply", RAC_FALSE, nullptr, 3, user);
    callback("", RAC_TRUE, "stop", 0, user);
    return RAC_SUCCESS;
}
rac_result_t Info(void* impl, rac_llm_info_t* info) {
    const auto* session = static_cast<Session*>(impl);
    *info = {};
    info->is_ready = session->ready ? RAC_TRUE : RAC_FALSE;
    info->current_model = session->path.c_str();
    info->context_length = session->context;
    info->supports_streaming = RAC_TRUE;
    return RAC_SUCCESS;
}
rac_result_t Ok(void*) { return RAC_SUCCESS; }
void Destroy(void* impl) {
    report["destroyed"] = report["destroyed"].get<int>() + 1;
    delete static_cast<Session*>(impl);
}

const rac_llm_service_ops_t ops = [] {
    rac_llm_service_ops_t value{};
    value.create = Create;
    value.initialize = Initialize;
    value.generate = Generate;
    value.generate_stream = Stream;
    value.get_info = Info;
    value.cancel = Ok;
    value.cleanup = Ok;
    value.destroy = Destroy;
    return value;
}();
const uint32_t formats[] = {RAC_MODEL_FORMAT_ID_GGUF};
const rac_engine_vtable_t engine = [] {
    rac_engine_vtable_t value{};
    value.metadata.abi_version = RAC_PLUGIN_API_VERSION;
    value.metadata.name = "llamacpp";
    value.metadata.display_name = "Hermetic harness test backend";
    value.metadata.priority = 100000;
    value.metadata.formats = formats;
    value.metadata.formats_count = 1;
    value.llm_ops = &ops;
    return value;
}();
std::optional<std::string> Environment(const char* name) {
    const char* value = std::getenv(name);
    return value == nullptr ? std::nullopt : std::optional<std::string>(value);
}
}  // namespace

int main(int argc, char** argv) {
    rac_logger_set_min_level(RAC_LOG_ERROR);
    if (rac_plugin_register(&engine) != RAC_SUCCESS) return 90;
    // Register before CLI parsing; pre-bootstrapping here would mask --home
    // regressions in the production launch path.
    std::map<std::string, std::optional<std::string>> original;
    for (const char* name : {"OPENCODE_CONFIG_CONTENT", "OPENCLAW_CONFIG_PATH", "OPENCLAW_STATE_DIR",
                             "CUSTOM_BASE_URL", "HERMES_MODEL", "HERMES_INFERENCE_MODEL",
                             "HERMES_INFERENCE_PROVIDER", "RUNANYWHERE_API_KEY",
                             "ANTHROPIC_BASE_URL", "ANTHROPIC_AUTH_TOKEN", "ANTHROPIC_MODEL",
                             "CLAUDE_CODE_MAX_CONTEXT_TOKENS"}) {
        original.emplace(name, Environment(name));
    }
    const int status = wally_run_main(argc, argv);
    report["stopped"] = rac_server_is_running() == RAC_FALSE;
    report["environment_restored"] = true;
    for (const auto& [name, value] : original) {
        if (Environment(name.c_str()) != value) report["environment_restored"] = false;
    }
    if (const char* path = std::getenv("WALLY_TEST_BACKEND_REPORT")) {
        std::ofstream(path) << report.dump(2);
    }
    return status;
}

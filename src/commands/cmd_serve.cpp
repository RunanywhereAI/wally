/**
 * @file cmd_serve.cpp
 * @brief `wally serve [model]` — OpenAI-compatible local HTTP server.
 *
 * Wraps the existing commons rac_server (include/rac/server/rac_server.h —
 * same engine behind tools/runanywhere-server.cpp). Scope inherited from
 * rac_server: LLM only, ONE model per server process. Model refs resolve
 * through the same catalog/registry path as every other command, with
 * auto-pull when missing.
 */

#include "commands/commands.h"

#include <csignal>
#include <chrono>
#include <memory>
#include <string>
#include <thread>

#if defined(WALLY_HAS_SERVER)
#include "rac/server/rac_server.h"
#endif

#include "cli_formatter.h"
#include "commands/model_setup.h"
#include "io/output.h"

namespace wally::commands {

namespace {

constexpr const char* kDefaultServeModel = "qwen3-0.6b";

#if defined(WALLY_HAS_SERVER)

// Async-signal-safe shutdown: the handler only sets a flag; the main thread
// polls and performs the actual stop. Calling rac_server_stop() from signal
// context (the tools/runanywhere-server.cpp pattern) deadlocks — the handler
// interrupts rac_server_wait() on the same thread and re-enters its mutex.
volatile std::sig_atomic_t g_serve_stop = 0;

void serve_signal_handler(int /*signum*/) {
    g_serve_stop = 1;
}

int run_serve(const GlobalOptions& options, const std::string& ref, const std::string& host,
              uint16_t port, int32_t context_size, int32_t threads, int32_t gpu_layers,
              bool cors) {
    Bootstrapped env;
    if (bootstrap(options, &env) != RAC_SUCCESS) {
        return 1;
    }

    ResolvedModelPaths model;
    const int setup =
        ensure_model_ready(options, ref.empty() ? kDefaultServeModel : ref, &model);
    if (setup != 0) {
        return setup;
    }

    rac_server_config_t config = RAC_SERVER_CONFIG_DEFAULT;
    config.host = host.c_str();
    config.port = port;
    config.model_path = model.primary_path.c_str();
    config.model_id = model.model_id.c_str();
    config.context_size = context_size;
    config.threads = threads;
    config.gpu_layers = gpu_layers;
    config.enable_cors = cors ? RAC_TRUE : RAC_FALSE;
    config.verbose = options.verbose ? RAC_TRUE : RAC_FALSE;

    const rac_result_t rc = rac_server_start(&config);
    if (RAC_FAILED(rc)) {
        out::error_line("server start failed: " + out::describe_result(rc));
        return 1;
    }

    out::status_line("serving " + model.model_id + " (LLM-only, single model)");
    out::status_line("OpenAI API: http://" + host + ":" + std::to_string(port) +
                     "/v1/chat/completions");
    out::status_line("health:     http://" + host + ":" + std::to_string(port) + "/health");
    out::status_line("Ctrl-C to stop");

    g_serve_stop = 0;
    std::signal(SIGINT, serve_signal_handler);
    std::signal(SIGTERM, serve_signal_handler);
    while (!g_serve_stop && rac_server_is_running() == RAC_TRUE) {
        std::this_thread::sleep_for(std::chrono::milliseconds(200));
    }
    rac_server_stop();
    const int exit_code = rac_server_wait();

    rac_server_status_t status = {};
    if (RAC_SUCCEEDED(rac_server_get_status(&status)) && options.verbose) {
        out::status_line("requests: " + std::to_string(status.total_requests) +
                         ", tokens: " + std::to_string(status.total_tokens_generated) +
                         ", uptime: " + std::to_string(status.uptime_seconds) + "s");
    }
    return exit_code;
}

#endif  // WALLY_HAS_SERVER

}  // namespace

void register_serve(CLI::App& app, GlobalOptions& options) {
    CLI::App* cmd = app.add_subcommand("serve", "Serve a model over an OpenAI-compatible API");
#if defined(WALLY_HAS_SERVER)
    cmd->footer(examples_footer({
        {"wally serve qwen3-0.6b", ""},
        {"wally serve granite-4.2-8b --port 8000", ""},
    }));
    auto ref = std::make_shared<std::string>();
    auto host = std::make_shared<std::string>("127.0.0.1");
    auto port = std::make_shared<uint16_t>(8080);
    auto context = std::make_shared<int32_t>(8192);
    auto threads = std::make_shared<int32_t>(4);
    auto gpu_layers = std::make_shared<int32_t>(0);
    auto cors = std::make_shared<bool>(false);
    cmd->add_option("model", *ref,
                    "Model to serve (default " + std::string(kDefaultServeModel) + ")");
    cmd->add_option("--host,-H", *host, "Address to bind (default 127.0.0.1)");
    cmd->add_option("--port,-p", *port, "Port to listen on (default 8080)");
    cmd->add_option("--context-length,--context,-c", *context,
                    "Context window in tokens (default 8192)");
    cmd->add_option("--threads,-t", *threads, "Inference threads (default 4)");
    cmd->add_option("--gpu-layers,--ngl", *gpu_layers, "Layers to offload to the GPU");
    cmd->add_flag("--cors", *cors, "Allow cross-origin browser requests");
    cmd->callback([&options, ref, host, port, context, threads, gpu_layers, cors]() {
        const int exit_code = run_serve(options, *ref, *host, *port, *context, *threads,
                                        *gpu_layers, *cors);
        if (exit_code != 0) {
            throw CLI::RuntimeError(exit_code);
        }
    });
#else
    cmd->callback([]() {
        out::error_line("this wally build does not include the server (RAC_BUILD_SERVER=OFF)");
        throw CLI::RuntimeError(1);
    });
#endif
}

}  // namespace wally::commands

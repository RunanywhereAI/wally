// Stand-in for an installed coding tool. Reads the real handoff, speaks HTTP,
// and reports what it saw. No model or external network is involved.
#include <httplib.h>
#include <nlohmann/json.hpp>
#include <cstdlib>
#include <filesystem>
#include <fstream>
#include <iostream>
#include <stdexcept>
#include <string>
#include <vector>

namespace {
using Json = nlohmann::json;
std::string Env(const char* name) {
    const char* value = std::getenv(name);
    return value == nullptr ? "" : value;
}
void Require(bool yes, const std::string& message) {
    if (!yes) throw std::runtime_error(message);
}
Json Read(const std::string& path) {
    std::ifstream input(path);
    Require(input.good(), "config missing while child is running: " + path);
    return Json::parse(input);
}
}

int main(int argc, char** argv) {
    try {
        const std::string tool = std::filesystem::path(argv[0]).stem().string();
        std::string url;
        std::string model;
        Json report = {{"tool", tool}, {"args", Json::array()}, {"temporary_files", Json::array()}};
        for (int i = 1; i < argc; ++i) report["args"].push_back(argv[i]);
        if (Env("WALLY_TEST_PASSTHROUGH") == "1") {
            report["inherited_config"] = Env("OPENCODE_CONFIG_CONTENT");
        } else if (tool == "opencode") {
            const Json config = Json::parse(Env("OPENCODE_CONFIG_CONTENT"));
            const Json& provider = config.at("provider").at("runanywhere");
            url = provider.at("options").at("baseURL");
            Require(provider.at("options").at("apiKey") == "local", "local placeholder missing");
            const std::string selected = config.at("model");
            model = selected.substr(std::string("runanywhere/").size());
            report["limits"] = provider.at("models").at(model).at("limit");
        } else if (tool == "dsh") {
            std::string patch;
            for (int i = 1; i + 1 < argc; ++i) if (std::string(argv[i]) == "--patch") patch = argv[i + 1];
            std::ifstream input(patch);
            Require(input.good(), "DeepSeek patch missing");
            const std::string text((std::istreambuf_iterator<char>(input)), {});
            const std::string marker = "    path: '";
            const auto begin = text.find(marker);
            Require(begin != std::string::npos, "DeepSeek settings path missing");
            const auto end = text.find("'\n", begin + marker.size());
            const std::string settings_path = text.substr(begin + marker.size(), end - begin - marker.size());
            const Json provider = Read(settings_path).at("llm-pi-ai").at("providers").at("runanywhere");
            url = provider.at("baseURL");
            model = provider.at("models").at(0).at("id");
            const std::string key = provider.at("apiKeyEnv");
            Require(Env(key.c_str()) == "local", "DeepSeek placeholder missing");
            report["temporary_files"] = {patch, settings_path};
            report["limits"] = {{"context", provider.at("models").at(0).at("contextWindow")},
                                 {"output", provider.at("models").at(0).at("maxTokens")}};
        } else if (tool == "openclaw") {
            const std::string path = Env("OPENCLAW_CONFIG_PATH");
            const Json provider = Read(path).at("models").at("providers").at("runanywhere");
            url = provider.at("baseUrl");
            model = provider.at("models").at(0).at("id");
            report["temporary_files"] = {path};
            report["limits"] = {{"context", provider.at("models").at(0).at("contextWindow")},
                                 {"output", provider.at("models").at(0).at("maxTokens")}};
        } else if (tool == "hermes") {
            url = Env("CUSTOM_BASE_URL");
            model = Env("HERMES_INFERENCE_MODEL");
        } else if (tool == "claude") {
            url = Env("ANTHROPIC_BASE_URL");
            model = Env("ANTHROPIC_MODEL");
            Require(!Env("CLAUDE_CODE_MAX_CONTEXT_TOKENS").empty(), "local context hint missing");
        }
        if (!url.empty()) {
            Require(url.starts_with("http://127.0.0.1:"), "endpoint must be loopback");
            const auto base_end = url.find('/', 7);
            httplib::Client client(url.substr(0, base_end));
            client.set_connection_timeout(2);
            client.set_read_timeout(5);
            if (tool == "claude") {
                const Json body = {{"model", model}, {"max_tokens", 32},
                                    {"messages", {{{"role", "user"}, {"content", "first turn"}}}}};
                const auto reply = client.Post("/v1/messages", {{"x-api-key", Env("ANTHROPIC_AUTH_TOKEN")}},
                                                body.dump(), "application/json");
                Require(reply && reply->status == 200, "Anthropic local proxy failed");
                Require(reply->body.find("local harness reply") != std::string::npos,
                        "Anthropic proxy did not return backend output");
            } else {
                const auto models = client.Get("/v1/models");
                Require(models && models->status == 200, "SDK server missing while child is running");
                Require(Json::parse(models->body).at("data").at(0).at("id") == model,
                        "advertised model differs from selected alias");
                Json messages = {{{"role", "user"}, {"content", "first turn"}}};
                for (int turn = 0; turn < 2; ++turn) {
                    const Json request = {{"model", model}, {"messages", messages}, {"max_tokens", 32}};
                    const auto reply = client.Post("/v1/chat/completions", request.dump(), "application/json");
                    Require(reply && reply->status == 200, "local completion failed");
                    const Json body = Json::parse(reply->body);
                    Require(body.at("choices").at(0).at("message").at("content") == "local harness reply",
                            "local backend not used");
                    messages.push_back({{"role", "assistant"}, {"content", "local harness reply"}});
                    messages.push_back({{"role", "user"}, {"content", "second turn"}});
                }
                const Json streaming = {{"model", model}, {"messages", messages}, {"max_tokens", 32},
                                         {"stream", true}};
                const auto stream = client.Post("/v1/chat/completions", streaming.dump(), "application/json");
                Require(stream && stream->status == 200 && stream->body.find("data: [DONE]") != std::string::npos,
                        "local streaming response was incomplete");
                Require(stream->body.find("local harness reply") != std::string::npos, "stream output missing");
            }
            report["url"] = url;
            report["model"] = model;
        }
        std::ofstream(Env("WALLY_TEST_CHILD_REPORT")) << report.dump(2);
        return Env("WALLY_TEST_CHILD_EXIT").empty() ? 0 : std::stoi(Env("WALLY_TEST_CHILD_EXIT"));
    } catch (const std::exception& error) {
        std::cerr << "harness fixture: " << error.what() << '\n';
        return 91;
    }
}

/// What a bundle launch tells the wrapped tool.
///
/// `wally claude-code` has two launch paths and they wire the environment
/// differently. A terminal launch exports variables into its own process and the
/// child inherits them; a macOS bundle inherits nothing, because launchd starts
/// it from the reader's login session, so every variable has to be listed on
/// `open --env` by hand. That asymmetry is how the bundle path came to run
/// unwired: it is not enough to set a variable once, it has to be set in two
/// places, and nothing failed when only one of them was.
///
/// These assert on the argument vector rather than on a launched process. A test
/// that really started Claude Code would need Claude Code, a signed-in cloud
/// session and a GPU at the other end, which is why this wiring had no test at
/// all. The argument vector is the whole of what the function decides, so it is
/// the right thing to assert on.

#include "test_common.h"

#include <algorithm>
#include <string>
#include <vector>

#include "anthropic/messages.h"
#include "commands/editor_env.h"

namespace {

using wally::commands::OpenArgs;

wally::anthropic::Shim RunningShim() {
    wally::anthropic::Shim shim;
    shim.running = true;
    shim.base_url = "http://127.0.0.1:8765";
    shim.auth_token = "sk-runa-test-token";
    return shim;
}

/// The value `open --env NAME=value` carries for `name`, or "" when absent.
///
/// Reads the vector the way `open(1)` does -- the value is the argument AFTER
/// each `--env` -- rather than searching the whole vector for a substring. A
/// substring search would pass on a value that appeared anywhere at all,
/// including inside a passthrough argument, which is the shape of assertion
/// `.claude/rules` calls out as not being a test.
std::string EnvValue(const std::vector<std::string>& args, const std::string& name) {
    const std::string prefix = name + "=";
    for (std::size_t i = 0; i + 1 < args.size(); ++i) {
        if (args[i] == "--env" && args[i + 1].rfind(prefix, 0) == 0) {
            return args[i + 1].substr(prefix.size());
        }
    }
    return "";
}

bool HasAnyEnv(const std::vector<std::string>& args) {
    return std::find(args.begin(), args.end(), "--env") != args.end();
}

TestResult test_the_model_is_carried_to_a_bundle() {
    TestResult result;
    result.test_name = "the_model_is_carried_to_a_bundle";
    const auto args = OpenArgs("/Applications/Claude.app", RunningShim(), {}, "glm-5.3-flash");

    // The defect this closes: Wally 0.5.6 printed `using glm-5.3-flash`, served
    // GLM, and the wrapped tool reported `claude-sonnet-5` as the model that
    // answered -- because nothing here ever told it otherwise.
    const std::string model = EnvValue(args, "ANTHROPIC_MODEL");
    if (model != "glm-5.3-flash") {
        result.details = "ANTHROPIC_MODEL was " + (model.empty() ? "unset" : model);
        return result;
    }

    // The one that is easy to miss. The wrapped tool makes background requests of
    // its own -- titles, summaries -- and resolves them through its `haiku` alias
    // rather than the main model. Those reach us too and are served by the
    // selected model like everything else, so without this they stay labelled as
    // a model nothing ever contacted. Measured 2026-09-12: an auxiliary GLM pair
    // of 766/599 tokens costing 1,778 micros, and 816/821 for Qwen at 2,790.
    const std::string background = EnvValue(args, "ANTHROPIC_DEFAULT_HAIKU_MODEL");
    if (background != "glm-5.3-flash") {
        result.details =
            "ANTHROPIC_DEFAULT_HAIKU_MODEL was " + (background.empty() ? "unset" : background);
        return result;
    }
    result.passed = true;
    return result;
}

TestResult test_the_endpoint_still_goes_with_it() {
    TestResult result;
    result.test_name = "the_endpoint_still_goes_with_it";
    const auto args = OpenArgs("/Applications/Claude.app", RunningShim(), {}, "glm-5.3-flash");

    // Regression guard on what was already right: adding variables must not
    // displace the two that make the launch reach us at all.
    if (EnvValue(args, "ANTHROPIC_BASE_URL") != "http://127.0.0.1:8765") {
        result.details = "base url missing or wrong";
        return result;
    }
    if (EnvValue(args, "ANTHROPIC_AUTH_TOKEN") != "sk-runa-test-token") {
        result.details = "auth token missing or wrong";
        return result;
    }
    // Never a key beside the token: the token already outranks a key, and setting
    // one is what makes the wrapped tool warn that claude.ai connectors are off.
    if (!EnvValue(args, "ANTHROPIC_API_KEY").empty()) {
        result.details = "ANTHROPIC_API_KEY must never be set";
        return result;
    }
    result.passed = true;
    return result;
}

TestResult test_no_model_means_no_model_variables() {
    TestResult result;
    result.test_name = "no_model_means_no_model_variables";
    // `wally claude-code` with no model named does no wiring at all, so the tool
    // runs exactly as the reader configured it. An empty value would not be
    // neutral: the wrapped tool reads `ANTHROPIC_MODEL=` as a model named "",
    // which is not the same as leaving its own configuration alone.
    const auto args = OpenArgs("/Applications/Claude.app", {}, {}, "");
    if (HasAnyEnv(args)) {
        result.details = "an unwired launch exported environment variables";
        return result;
    }
    result.passed = true;
    return result;
}

TestResult test_a_stopped_shim_carries_nothing() {
    TestResult result;
    result.test_name = "a_stopped_shim_carries_nothing";
    wally::anthropic::Shim stopped;
    stopped.running = false;
    const auto args = OpenArgs("/Applications/Claude.app", stopped, {}, "glm-5.3-flash");
    // There is nothing to point the tool at, so naming a model would be worse
    // than silence: it would report a model it cannot reach.
    if (HasAnyEnv(args)) {
        result.details = "a stopped shim still exported environment variables";
        return result;
    }
    result.passed = true;
    return result;
}

TestResult test_variables_stay_before_the_apps_own_arguments() {
    TestResult result;
    result.test_name = "variables_stay_before_the_apps_own_arguments";
    const auto args =
        OpenArgs("/Applications/Claude.app", RunningShim(), {"--resume", "abc"}, "glm-5.3-flash");

    const auto app_args = std::find(args.begin(), args.end(), "--args");
    if (app_args == args.end()) {
        result.details = "passthrough arguments were dropped";
        return result;
    }
    // `open` hands everything after `--args` to the app as its own argv, so a
    // `--env` appearing after it would become a parameter to the app instead of
    // an environment variable -- set nowhere, and silently.
    if (std::find(app_args, args.end(), "--env") != args.end()) {
        result.details = "an --env pair landed after --args, where open ignores it";
        return result;
    }
    result.passed = true;
    return result;
}

}  // namespace

int main(int argc, char** argv) {
    TestSuite suite("wally_editor_env");
    suite.add("a_stopped_shim_carries_nothing", test_a_stopped_shim_carries_nothing);
    suite.add("no_model_means_no_model_variables", test_no_model_means_no_model_variables);
    suite.add("the_endpoint_still_goes_with_it", test_the_endpoint_still_goes_with_it);
    suite.add("the_model_is_carried_to_a_bundle", test_the_model_is_carried_to_a_bundle);
    suite.add("variables_stay_before_the_apps_own_arguments",
              test_variables_stay_before_the_apps_own_arguments);
    return suite.run(argc, argv);
}

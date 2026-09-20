#include <cstdlib>
#include <string>
#include <vector>

#include "catalog/catalog.h"
#include "commands/commands.h"
#include "io/output.h"
#include "harness/harness.h"
#include "harness/agents.h"
#include "harness/opencode.h"

namespace wally::commands {
namespace {

/// CLI11 callbacks return void, so a non-zero status leaves as the runtime
/// error the app turns back into an exit code.
void fail(int status) {
    if (status != 0) {
        throw CLI::RuntimeError(status);
    }
}
}  // namespace

void register_harness(CLI::App& app, GlobalOptions& options) {
    // `wally opencode <model>` rather than a flag on `run`: it hands the terminal
    // to another program, which is a different thing to do than talk to a model.
    auto model = std::make_shared<std::string>();
    auto rest = std::make_shared<std::vector<std::string>>();
    auto cloud = std::make_shared<bool>(false);
    auto* opencode =
        app.add_subcommand("opencode", "open a coding session in opencode, wired to a model");
    // A named option rather than a positional: with two positionals there is no
    // way to tell `wally opencode run` asking for passthrough from someone
    // naming a model called run, and the first reading wins silently.
    opencode->add_option("-m,--model", *model, "a model on this machine, or one served upstream");
    opencode->add_flag("--cloud", *cloud,
                       "use the signed-in hosted endpoint (never routes local models)");
    opencode->add_option("args", *rest, "passed through to opencode")->allow_extra_args();
    opencode->prefix_command();
    opencode->callback([&options, model, rest, cloud] {
        // Before resolving a model or printing anything: a person without the
        // tool should see only that it is missing and how to install it.
        if (!harness::EnsureInstalled("opencode")) {
            fail(127);
            return;
        }
        const std::string effective = ResolveDefaultModel(*model, options.no_color);
        if (*cloud) {
            if (effective.empty()) {
                out::error_line(
                    "--cloud requires --model <console-model-id>, and no default is set "
                    "(wally models default <id>)");
                fail(2);
            }
            fail(harness::LaunchOpenCodeCloud(effective, *rest));
            return;
        }
        fail(harness::Launch("opencode", effective, *rest));
    });

    // The OpenAI-shaped agents, one subcommand per row of the table. They need
    // no translator, so there is nothing here but resolving the model and
    // handing the endpoint over the way each one takes it.
    for (int index = 0; index < harness::kAgentCount; ++index) {
        const harness::Agent& agent = harness::kAgents[index];
        auto agent_model = std::make_shared<std::string>();
        auto agent_rest = std::make_shared<std::vector<std::string>>();
        auto* command = app.add_subcommand(agent.id, agent.summary);
        command->footer("Examples:\n  wally " + std::string(agent.id) +
                        " -m qwen3-4b            (on-device)\n  wally " + std::string(agent.id) +
                        " --cloud -m glm-5.3-flash  (hosted)");
        command->add_option("-m,--model", *agent_model,
                            "a model on this machine, or one served upstream");
        command->add_option("args", *agent_rest, "passed through to the tool")
            ->allow_extra_args();
        command->prefix_command();
        command->callback([&options, &agent, agent_model, agent_rest] {
            // Same as opencode: check the tool is here before resolving a model
            // or printing a preamble.
            if (!harness::EnsureInstalled(agent.command)) {
                fail(127);
                return;
            }
            fail(harness::LaunchAgent(agent, ResolveDefaultModel(*agent_model, options.no_color),
                                      *agent_rest));
        });
    }
}

}  // namespace wally::commands

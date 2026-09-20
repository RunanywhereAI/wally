/**
 * @file cmd_default_models.cpp
 * @brief `wally default-models` — read, set, or clear the model a harness launch
 * uses when the reader gives no `-m`. See `config/preferences.h` for the
 * resolution rule.
 */

#include "commands/commands.h"

#include <memory>
#include <string>

#include "cli_formatter.h"
#include "config/preferences.h"
#include "io/output.h"

namespace wally::commands {

std::string ResolveDefaultModel(const std::string& explicit_model, bool no_color) {
    const std::string effective = prefs::ResolveModel(explicit_model);
    // Announce only when a default filled in for an omitted -m, so the launch
    // never silently picks a model the reader did not name. Blue, so it reads as
    // a notice rather than an error or an ordinary status line.
    if (explicit_model.empty() && !effective.empty()) {
        const cli_color::Palette pal = cli_color::make_palette(color_output_enabled(no_color));
        out::status_line(pal.blue + std::string("model not provided, using default model ") +
                         effective + pal.reset);
    }
    return effective;
}

void register_default_models(CLI::App& app, GlobalOptions& options) {
    static_cast<void>(options);
    auto model = std::make_shared<std::string>();
    auto clear = std::make_shared<bool>(false);

    // Lives under `models` (`wally models default`). register_models runs first
    // (app.cpp), so the namespace exists; a reorder would trip OptionNotFound.
    CLI::App* models = app.get_subcommand("models");
    CLI::App* cmd =
        models->add_subcommand("default", "Set the model a harness uses when you pass no -m");
    cmd->add_option("model", *model, "a model id to make the default, e.g. glm-5.3-flash");
    cmd->add_flag("--clear", *clear, "remove the saved default");
    cmd->footer("Examples:\n"
                "  wally models default glm-5.3-flash\n"
                "  wally models default          (show the current default)\n"
                "  wally models default --clear");

    cmd->callback([model, clear] {
        if (*clear) {
            std::string error;
            if (!prefs::ClearDefaultModel(&error)) {
                out::error_line(error);
                throw CLI::RuntimeError(1);
            }
            out::status_line("default model cleared");
            return;
        }

        if (!model->empty()) {
            std::string error;
            if (!prefs::SetDefaultModel(*model, &error)) {
                out::error_line(error);
                throw CLI::RuntimeError(1);
            }
            out::status_line("default model set to " + *model);
            return;
        }

        // No argument: report what is in effect and why.
        const prefs::DefaultModel current = prefs::EffectiveDefaultModel();
        switch (current.source) {
            case prefs::DefaultModelSource::Environment:
                out::result_line(current.id + " (from WALLY_DEFAULT_MODEL)");
                break;
            case prefs::DefaultModelSource::File:
                out::result_line(current.id);
                break;
            case prefs::DefaultModelSource::BuiltIn:
                out::result_line(current.id + " (built-in default)");
                break;
            case prefs::DefaultModelSource::None:
                out::status_line("no default model set; pass one to save it");
                break;
        }
    });
}

}  // namespace wally::commands

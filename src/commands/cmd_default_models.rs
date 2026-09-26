//! `wally models default` — read, set, or clear the model a harness launch
//! uses when the reader gives no `-m`. See `config::preferences` for the
//! resolution rule.

use crate::cli::{App, ValueType};
use crate::cli_formatter::{cli_color, color_output_enabled, examples_footer, Example};
use crate::config::preferences::{self, DefaultModelSource};
use crate::io::output as out;

/// The model a harness launch should use: `explicit_model` when given, else the
/// effective default (env, file, then the built-in id — see config::preferences).
/// Prints a blue notice when a default fills in for an omitted `-m`.
pub fn resolve_default_model(explicit_model: &str, no_color: bool) -> String {
    let effective = preferences::resolve_model(explicit_model);
    // Announce only when a default filled in for an omitted -m, so the launch
    // never silently picks a model the reader did not name. Blue, so it reads
    // as a notice rather than an error or an ordinary status line.
    if explicit_model.is_empty() && !effective.is_empty() {
        let pal = cli_color::make_palette(color_output_enabled(no_color));
        out::status_line(&format!(
            "{}model not provided, using default model {effective}{}",
            pal.blue, pal.reset
        ));
    }
    effective
}

pub fn register_default_models(app: &mut App) {
    // Lives under `models` (`wally models default`). register_models runs
    // first (app.cpp), so the namespace exists; a reorder would trip
    // get_subcommand_mut returning None — a startup crash from a future
    // reorder is worse than silently skipping this subcommand.
    let Some(models) = app.get_subcommand_mut("models") else {
        return;
    };
    let cmd = models.add_subcommand("default", "Show or set the default model for coding tools");
    cmd.add_option(
        "model",
        ValueType::Text,
        "Model id to save as the default (omit to show it)",
    );
    cmd.add_flag("--clear", "Forget the saved default");
    cmd.footer(&examples_footer(&[
        Example::new("wally models default glm-5.3-flash", ""),
        Example::new("wally models default --clear", ""),
    ]));

    cmd.callback(|p, g| {
        let clear = p.flag("--clear");
        let model = p.get_str("model").unwrap_or_default();

        if clear {
            if let Err(error) = preferences::clear_default_model() {
                out::error_line(&error);
                return 1;
            }
            if g.json {
                let mut json = out::JsonWriter::new();
                json.begin_object().field_bool("cleared", true).end_object();
                out::result_line(json.str());
            } else {
                out::status_line("default model cleared");
            }
            return 0;
        }

        if !model.is_empty() {
            if let Err(error) = preferences::set_default_model(&model) {
                out::error_line(&error);
                return 1;
            }
            if g.json {
                let mut json = out::JsonWriter::new();
                json.begin_object()
                    .field_str("model", &model)
                    .field_str("source", "file")
                    .end_object();
                out::result_line(json.str());
            } else {
                out::status_line(&format!("default model set to {model}"));
            }
            return 0;
        }

        // No argument: report what is in effect and why.
        let current = preferences::effective_default_model();
        if g.json {
            let mut json = out::JsonWriter::new();
            json.begin_object();
            match current.source {
                DefaultModelSource::None => {
                    json.field_null("model");
                }
                DefaultModelSource::Environment => {
                    json.field_str("model", &current.id)
                        .field_str("source", "environment");
                }
                DefaultModelSource::File => {
                    json.field_str("model", &current.id)
                        .field_str("source", "file");
                }
                DefaultModelSource::BuiltIn => {
                    json.field_str("model", &current.id)
                        .field_str("source", "built-in");
                }
            }
            json.end_object();
            out::result_line(json.str());
            return 0;
        }
        match current.source {
            DefaultModelSource::Environment => {
                out::result_line(&format!("{} (from WALLY_DEFAULT_MODEL)", current.id))
            }
            DefaultModelSource::File => out::result_line(&current.id),
            DefaultModelSource::BuiltIn => {
                out::result_line(&format!("{} (built-in default)", current.id))
            }
            DefaultModelSource::None => {
                out::status_line("no default model set; pass one to save it")
            }
        }
        0
    });
}

//! Subcommand registration — one module per C++ command file (port of
//! src/commands/commands.h).
//!
//! The command surface mirrors the SDK public API spec: a namespace per modality
//! and the spec's verb under it (`wally llm generate`, `wally models download`, …),
//! with option names in kebab-case (`--max-output-tokens`, `--top-p`).
//!
//! Each register_* attaches a subcommand whose callback performs:
//! parse → bootstrap() → ONE commons entry point → render. Inference and
//! lifecycle logic stay in commons per the repo layering rule; command modules
//! only translate between argv and the rac_* C ABI.
//!
//! Callbacks return the process exit code (0 ok, 1 runtime error, 2 usage error).

use std::collections::BTreeSet;

use crate::cli::App;

pub mod bench_metrics;
pub mod cmd_about;
pub mod cmd_account;
pub mod cmd_auth;
pub mod cmd_backends;
pub mod cmd_bench;
pub mod cmd_default_models;
pub mod cmd_diarize;
pub mod cmd_editors;
pub mod cmd_embed;
pub mod cmd_harness;
pub mod cmd_image;
pub mod cmd_info;
pub mod cmd_list;
pub mod cmd_lora;
pub mod cmd_maintenance;
pub mod cmd_models;
pub mod cmd_pull;
pub mod cmd_rag;
pub mod cmd_rerank;
pub mod cmd_rm;
pub mod cmd_run;
pub mod cmd_segment;
pub mod cmd_serve;
pub mod cmd_show;
pub mod cmd_stt;
pub mod cmd_telemetry;
pub mod cmd_tool;
pub mod cmd_tts;
pub mod cmd_update;
pub mod cmd_usage;
pub mod cmd_vad;
pub mod cmd_version;
pub mod cmd_voice;
pub mod editor_env;
pub mod engine_options;
pub mod model_labels;
pub mod model_setup;

pub use cmd_about::register_about;
pub use cmd_account::register_account;
pub use cmd_auth::register_auth;
pub use cmd_backends::{collect_backend_rows, collect_llm_backend_rows, register_backends};
pub use cmd_bench::register_bench;
pub use cmd_default_models::{register_default_models, resolve_default_model};
pub use cmd_diarize::register_diarize;
pub use cmd_editors::register_editors;
pub use cmd_embed::register_embed;
pub use cmd_harness::register_harness;
pub use cmd_image::register_image;
pub use cmd_info::register_info;
pub use cmd_list::configure_models_list;
pub use cmd_lora::register_lora;
pub use cmd_maintenance::{register_help, register_uninstall, run_uninstall};
pub use cmd_models::{register_models, register_models_aliases};
pub use cmd_pull::{configure_models_download, pull_model_flow};
pub use cmd_rag::register_rag;
pub use cmd_rerank::register_rerank;
pub use cmd_rm::configure_models_delete;
pub use cmd_run::{
    configure_llm, configure_vlm_generate, register_llm, register_llm_aliases, register_vlm,
};
pub use cmd_segment::register_segment;
pub use cmd_serve::register_serve;
pub use cmd_show::configure_models_get;
pub use cmd_stt::register_stt;
pub use cmd_telemetry::register_telemetry;
pub use cmd_tool::{configure_tool_call, register_tool};
pub use cmd_tts::register_tts;
pub use cmd_update::{register_update, run_update};
pub use cmd_usage::register_usage;
pub use cmd_vad::register_vad;
pub use cmd_version::register_version;
pub use cmd_voice::register_voice;

/// One registered engine, folded across every primitive it advertises.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EngineRow {
    pub display_name: String,
    pub version: String,
    pub priority: i32,
    pub primitives: BTreeSet<String>,
}

/// Which llm/vlm entry point a configured command drives.
///   Generate — one unary result, rendered once it completes.
///   Stream   — tokens printed as they arrive.
///   Chat     — Stream, falling back to the REPL when no prompt is given.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LlmVerb {
    Generate,
    Stream,
    Chat,
}

/// Where the model comes from: a `--model` option or the first positional.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelArg {
    Option,
    Positional,
}

/// Attach the spec verb name to a namespace whose options live on the namespace
/// itself (`wally stt transcribe --input a.wav` and `wally stt --input a.wav` are
/// the same command). The verb is a grammar marker: fallthrough hands its
/// options to the parent, and the parent owns the single callback.
pub fn add_verb_alias<'a>(ns: &'a mut App, verb: &str, description: &str) -> &'a mut App {
    let verb_app = ns.add_subcommand(verb, description);
    verb_app.fallthrough(true);
    verb_app
}

//! `wally lora` coverage. `register_lora` is not called from src/app.rs for
//! the LLM-only release, but (like `cmd_diarize`'s `diarize_arg_surface`)
//! this builds its own `App` and calls `register_lora` directly, so it does
//! not need `#[ignore]`: it is pure CLI-metadata introspection, never
//! reaching a callback or the SDK.

#[allow(unused_imports)]
use super::common;

/// `run_lora_remove` only ever sends `adapter` as an adapter id
/// (`LoraRemoveRequest.adapter_ids`); `LoraRemoveRequest.adapter_paths` was
/// deleted from the proto contract. The help text must not promise path
/// support it cannot honor.
#[test]
fn lora_remove_help_does_not_claim_path_support() {
    let mut app = wally::cli::App::new("wally test app", "wally");
    wally::commands::cmd_lora::register_lora(&mut app);

    let lora_cmd = app
        .get_subcommand("lora")
        .expect("lora subcommand not registered");
    let remove_cmd = lora_cmd
        .get_subcommand("remove")
        .expect("lora remove subcommand not registered");
    let adapter = remove_cmd
        .get_option("adapter")
        .expect("positional 'adapter' must exist");

    assert!(
        !adapter.description.to_lowercase().contains("path"),
        "remove's adapter help text must not claim path support: {:?}",
        adapter.description
    );
}

//! `wally pls` — run the hybrid router in the foreground, for development.
//!
//! The harness launcher starts the router itself; this command exists so the
//! router can be exercised without a harness in front of it.

use crate::cli::{App, ValueType};
use crate::harness::pls;
use crate::io::output as out;

pub fn register_pls(app: &mut App) {
    let cmd = app.add_subcommand("pls", "Run the hybrid router in the foreground");
    cmd.add_option(
        "-m,--model",
        ValueType::Text,
        "Directory of the local MLX model (defaults to $PLS_MODEL)",
    );
    cmd.callback(|parsed, _options| {
        let model = parsed
            .get_str("--model")
            .or_else(|| std::env::var("PLS_MODEL").ok())
            .unwrap_or_default();
        if model.is_empty() {
            out::error_line("--model <directory> is required (or set PLS_MODEL)");
            return 2;
        }
        if !std::path::Path::new(&model).is_dir() {
            out::error_line(&format!("{model} is not a directory"));
            return 2;
        }
        let Some(router) = pls::start(&model) else {
            return 1;
        };
        let mut router = router;
        out::status_line(&format!(
            "mode file: {} (edit it to switch lanes)",
            pls::config_file().display()
        ));
        out::status_line("press ctrl-c to stop");
        // The router owns its own lifetime; park until it exits.
        loop {
            std::thread::sleep(std::time::Duration::from_secs(1));
            if !router.running() {
                break;
            }
        }
        0
    });
}

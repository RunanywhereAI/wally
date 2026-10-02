//! DEV ONLY. `wally __responses-serve` — start the loopback Responses shim
//! against an arbitrary upstream and hold it open, so Codex (or curl) can be
//! pointed at the Rust shim for validation. Not listed in any `--help` section
//! (it is omitted from `app.rs` `SECTIONS`, which hides it), and the `__`
//! prefix marks it dev-only to a human.

use crate::cli::{App, ValueType};
use crate::harness::{DeclaredHarness, Endpoint};
use crate::io::output as out;
use crate::responses::{self, ModelAliases};

pub fn register_responses_serve(app: &mut App) {
    let cmd = app.add_subcommand(
        "__responses-serve",
        "(dev) Serve the Responses shim against an upstream and hold it open",
    );
    cmd.add_option(
        "--upstream",
        ValueType::Text,
        "Upstream base URL, e.g. https://inference.runanywhere.ai/v1",
    )
    .required();
    cmd.add_option("--key", ValueType::Text, "Upstream API key (bearer)");
    cmd.add_option("--model", ValueType::Text, "Model id to serve")
        .required();
    cmd.add_flag("--verbose", "Verbose shim logging");

    cmd.callback(|p, _g| {
        let upstream_url = p.get_str("--upstream").unwrap_or_default();
        let key = p.get_str("--key").unwrap_or_default();
        let model = p.get_str("--model").unwrap_or_default();
        let verbose = p.flag("--verbose");

        // Built by hand from flags — Endpoint's fields are all public, so no
        // harness::resolve() and no cloud session is needed for validation.
        // console_url left empty so start() spins up no cancel worker.
        let endpoint = Endpoint {
            base_url: upstream_url,
            api_key: key,
            console_url: String::new(),
            serving: false,
            context_window: 0,
            max_output: 0,
        };

        let shim = match responses::start(
            &endpoint,
            &model,
            DeclaredHarness::KCodex,
            verbose,
            "",
            &ModelAliases::new(),
        ) {
            Some(s) => s,
            None => {
                out::error_line("responses shim failed to start");
                return 1;
            }
        };

        out::result_line(&format!("OPENAI_BASE_URL={}/v1", shim.base_url));
        out::result_line(&format!("OPENAI_API_KEY={}", shim.auth_token));
        out::status_line(&format!(
            "serving {model} (Responses); press Ctrl-C to stop"
        ));
        // Ctrl-C ends the process and the OS reclaims the port (same contract as
        // `cmd_editors::serve`); the loop never returns.
        loop {
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
    });
}

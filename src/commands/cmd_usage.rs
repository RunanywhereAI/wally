//! Port of src/commands/cmd_usage.cpp.

use std::time::{SystemTime, UNIX_EPOCH};

use crate::account::{
    self as account, ConsoleClient, Credentials, IdentityResult, Usage, UsageQuery, UsageWindow,
};
use crate::cli::App;
use crate::cli_formatter::{examples_footer, Example};
use crate::io::output as out;

fn epoch_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Money is integer micro-dollars: one dollar is 1,000,000.
fn money(micros: i64) -> String {
    let places = if micros != 0 && micros < 1_000_000 {
        4
    } else {
        2
    };
    format!("${:.*}", places, micros as f64 / 1_000_000.0)
}

fn grouped(value: i64) -> String {
    let digits = (if value < 0 { -value } else { value }).to_string();
    let bytes = digits.as_bytes();
    let mut out = String::new();
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (bytes.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(*b as char);
    }
    if value < 0 {
        format!("-{out}")
    } else {
        out
    }
}

fn refresh_session(client: &ConsoleClient, credentials: &mut Credentials) -> Result<(), String> {
    if credentials.refresh_token.is_empty() {
        return Err("the cloud session cannot be refreshed; run `wally account login`".to_string());
    }
    let grant = client
        .refresh(&credentials.console_url, &credentials.refresh_token)
        .map_err(|e| e.message)?;
    credentials.access_token = grant.access_token;
    if !grant.refresh_token.is_empty() {
        credentials.refresh_token = grant.refresh_token;
    }
    credentials.expires_at = epoch_seconds()
        + if grant.expires_in > 0 {
            grant.expires_in
        } else {
            3600
        };
    account::save(credentials)
}

/// The two rows, in the order they are printed, keyed by the label the console
/// puts on the window it totalled.
const ROWS: [(&str, &str); 2] = [("1h", "past 1h"), ("24h", "past 24h")];

/// The window the console labelled `id`, or None when it sent none.
///
/// A console deployed before windowed totals sends no `windows` at all. Nothing
/// substitutes for a missing one: `totals` covers `days`, so showing it under
/// an hour's heading would be a month's spend wearing an hour's label.
fn window<'a>(usage: &'a Usage, id: &str) -> Option<&'a UsageWindow> {
    usage.windows.iter().find(|w| w.window == id)
}

fn print_json(usage: &Usage) {
    let mut json = out::JsonWriter::new();
    json.begin_object();
    json.field_i64("balance_micros", usage.credit.balance_micros);
    json.field_i64("granted_micros", usage.credit.granted_micros);
    json.field_i64("spent_micros", usage.credit.spent_micros);

    json.begin_array("windows");
    for (id, _label) in ROWS {
        let found = window(usage, id);
        json.begin_array_object();
        json.field_str("window", id);
        json.field_bool("available", found.is_some());
        if let Some(found) = found {
            json.field_i64("input_tokens", found.totals.prompt_tokens);
            json.field_i64("output_tokens", found.totals.completion_tokens);
            json.field_i64("cached_tokens", found.totals.cached_tokens);
            json.field_i64("cost_micros", found.totals.cost_micros);
        }
        json.end_object();
    }
    json.end_array();

    json.end_object();
    out::result_line(json.str());
}

fn print_report(usage: &Usage) {
    out::result_line(&format!(
        "credit     {} left of {} granted",
        money(usage.credit.balance_micros),
        money(usage.credit.granted_micros)
    ));
    out::result_line("");

    out::result_line(&format!(
        "{:<10} {:>11} {:>11} {:>11} {:>11}",
        "window", "input", "output", "cache", "spend"
    ));

    let mut missing = false;
    for (id, label) in ROWS {
        match window(usage, id) {
            None => {
                missing = true;
                out::result_line(&format!(
                    "{:<10} {:>11} {:>11} {:>11} {:>11}",
                    label, "-", "-", "-", "-"
                ));
            }
            Some(found) => {
                let totals = &found.totals;
                out::result_line(&format!(
                    "{:<10} {:>11} {:>11} {:>11} {:>11}",
                    label,
                    grouped(totals.prompt_tokens),
                    grouped(totals.completion_tokens),
                    grouped(totals.cached_tokens),
                    money(totals.cost_micros)
                ));
            }
        }
    }

    if missing {
        out::result_line("");
        out::status_line(
            "this console does not total by window yet; a dash is a number it did not send, \
             not a zero",
        );
    }
}

fn usage(as_json: bool) -> i32 {
    let mut credentials = match account::load() {
        Ok(credentials) => credentials,
        Err(failure) => {
            out::error_line(&failure);
            return 1;
        }
    };
    if !credentials.signed_in() {
        out::error_line("not signed in — run `wally account login`");
        return 1;
    }

    let client = ConsoleClient::default();
    if credentials.access_token_expired(epoch_seconds(), 60) {
        if let Err(failure) = refresh_session(&client, &mut credentials) {
            out::error_line(&failure);
            return 1;
        }
    }

    // One day back, and the shortest recent-request page the route accepts:
    // nothing here renders those, and `by_model`/`totals` group in SQL, so the
    // page size cannot move the numbers above.
    let query = UsageQuery {
        days: 1,
        limit: 1,
        ..UsageQuery::default()
    };

    let (mut result, mut usage, mut failure) =
        client.fetch_usage(&credentials.console_url, &credentials.access_token, &query);
    if result == IdentityResult::Unauthorized {
        // Not "expired". The console answers unknown, revoked, expired and
        // malformed with the same 401 on purpose, so which of the four this
        // was is not something we know -- and sending someone to re-login over
        // a revoked key wastes the trip.
        if let Err(refresh_failure) = refresh_session(&client, &mut credentials) {
            out::error_line(&format!(
                "the console rejected this session ({refresh_failure}); run `wally account login`"
            ));
            return 1;
        }
        (result, usage, failure) =
            client.fetch_usage(&credentials.console_url, &credentials.access_token, &query);
    }
    if result != IdentityResult::Ok {
        out::error_line(&failure);
        return 1;
    }

    if as_json {
        print_json(&usage);
    } else {
        print_report(&usage);
    }
    0
}

pub fn register_usage(app: &mut App) {
    // Lives under `account`. register_account runs first (app.rs), so the
    // namespace normally exists by now, but that ordering is only a comment
    // over there, not something the type system enforces. Guard the lookup and
    // say plainly what went wrong instead of panicking on an unwrap.
    let Some(account_cmd) = app.get_subcommand_mut("account") else {
        out::error_line(
            "internal error: register_usage() ran before register_account() registered \
             the `account` command",
        );
        std::process::exit(1);
    };
    let usage_cmd =
        account_cmd.add_subcommand("usage", "Show remaining credit and the last day's spend");
    usage_cmd.add_flag("--json", "Print as JSON");
    usage_cmd.footer(&examples_footer(&[
        Example::new("wally account usage", ""),
        Example::new("wally --json account usage", ""),
    ]));
    // `wally --json usage` and `wally usage --json` mean the same thing. The
    // root parser accepts the first, so reading only the command-local flag
    // printed a human table to something asking for one JSON document.
    usage_cmd.callback(|p, g| usage(p.flag("--json") || g.json));
}

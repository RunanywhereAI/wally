//! Port of src/commands/cmd_usage.cpp.

use std::time::{SystemTime, UNIX_EPOCH};

use super::cmd_auth::format_epoch_seconds;
use crate::account::{
    self as account, ConsoleClient, Credentials, IdentityResult, Usage, UsageQuery,
    UsageRequestRow, UsageRequestsPage, UsageRequestsQuery, UsageWindow,
};
use crate::cli::{App, Validator, ValueType};
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

/// The export refuses a window past this many days, so the flags stop there
/// instead of sending a request the console will answer with a 400.
const MAX_REQUESTS_WINDOW_DAYS: i64 = 31;
const SECONDS_PER_DAY: i64 = 86_400;

/// A page of settled requests is followed at most this far, so a wrong filter
/// cannot turn one command into an unbounded crawl of the ledger.
const MAX_FOLLOWED_PAGES: usize = 100;

/// Flags that only mean something next to `--requests`. Given without it they
/// would be silently ignored, and a filter that does nothing reads as a report
/// that was filtered.
const REQUESTS_ONLY_FLAGS: [&str; 9] = [
    "--days",
    "--since",
    "--until",
    "--model",
    "--status",
    "--response-request-id",
    "--cursor",
    "--limit",
    "--follow",
];

/// What `wally account usage --requests` was asked for.
struct RequestsArgs {
    as_json: bool,
    /// Set only when `--days` was given: whether a window came from `--days` or
    /// from `--since`/`--until` decides which combinations are refused.
    days: Option<i64>,
    since: Option<String>,
    until: Option<String>,
    model: String,
    status_code: i32,
    response_request_id: String,
    cursor: String,
    limit: i32,
    follow: bool,
}

/// Days since 1970-01-01 for a proleptic Gregorian date: the inverse of the
/// `civil_from_days` behind `format_epoch_seconds` (Howard Hinnant's
/// `days_from_civil`, public domain). No calendar dependency, same as there.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let month_index = (month + 9) % 12;
    let day_of_year = (153 * month_index + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn days_in_month(year: i64, month: i64) -> i64 {
    let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ if leap => 29,
        _ => 28,
    }
}

/// Unix seconds for an ISO-8601 instant that carries its own timezone
/// (`2026-09-25T08:00:00Z`, `...+05:30`, optional fractional seconds).
///
/// The console refuses a bound without a timezone with a 400 the person cannot
/// act on, and a window the client cannot read is a window it cannot bound to
/// 31 days. Both are decided here, before the request, and the string itself is
/// still sent untouched.
fn parse_iso_instant(text: &str) -> Result<i64, String> {
    let invalid = || {
        format!(
            "`{text}` is not a timestamp with a timezone, e.g. 2026-09-25T08:00:00Z or \
             2026-09-25T13:30:00+05:30"
        )
    };
    let bytes = text.as_bytes();
    // YYYY-MM-DDTHH:MM:SS is 19 bytes; everything after is fraction + zone.
    if bytes.len() < 20 || !text.is_ascii() {
        return Err(invalid());
    }
    let digits = |from: usize, to: usize| -> Option<i64> {
        let part = &text[from..to];
        if part.bytes().all(|b| b.is_ascii_digit()) {
            part.parse().ok()
        } else {
            None
        }
    };
    let separators_ok = bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes[10] == b'T'
        && bytes[13] == b':'
        && bytes[16] == b':';
    let (Some(year), Some(month), Some(day), Some(hour), Some(minute), Some(second)) = (
        digits(0, 4),
        digits(5, 7),
        digits(8, 10),
        digits(11, 13),
        digits(14, 16),
        digits(17, 19),
    ) else {
        return Err(invalid());
    };
    if !separators_ok
        || !(1..=12).contains(&month)
        || day < 1
        || day > days_in_month(year, month)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return Err(invalid());
    }

    let mut rest = &text[19..];
    if let Some(fraction) = rest.strip_prefix('.') {
        let length = fraction.bytes().take_while(u8::is_ascii_digit).count();
        if length == 0 {
            return Err(invalid());
        }
        rest = &fraction[length..];
    }
    let offset_seconds = match rest {
        "Z" | "z" => 0,
        _ => {
            let sign = match rest.as_bytes().first() {
                Some(b'+') => 1,
                Some(b'-') => -1,
                _ => return Err(invalid()),
            };
            let zone = &rest[1..];
            let zone_bytes = zone.as_bytes();
            if zone.len() != 5 || zone_bytes[2] != b':' {
                return Err(invalid());
            }
            let (Ok(zone_hour), Ok(zone_minute)) =
                (zone[..2].parse::<i64>(), zone[3..].parse::<i64>())
            else {
                return Err(invalid());
            };
            if zone_hour > 23 || zone_minute > 59 {
                return Err(invalid());
            }
            sign * (zone_hour * 3600 + zone_minute * 60)
        }
    };
    Ok(
        days_from_civil(year, month, day) * SECONDS_PER_DAY + hour * 3600 + minute * 60 + second
            - offset_seconds,
    )
}

/// The `[since, until)` window the request will ask for.
///
/// `--days` counts back from now. `--since`/`--until` name the window exactly,
/// which is what makes a page reproducible: the console only honours a cursor
/// for the same window and filters, so a window recomputed from "now" on every
/// run could never be continued.
fn resolve_window(args: &RequestsArgs, now: i64) -> Result<(String, String), String> {
    if args.until.is_some() && args.since.is_none() {
        return Err("--until needs --since; without it the window has no start".to_string());
    }
    if !args.cursor.is_empty() && (args.since.is_none() || args.until.is_none()) {
        // A window counted back from now is a different window on every run,
        // and the console refuses a cursor from a window it did not issue it for.
        return Err(
            "--cursor needs --since and --until: it only continues the window that issued it"
                .to_string(),
        );
    }
    let Some(since) = &args.since else {
        let days = args.days.unwrap_or(1);
        if !(1..=MAX_REQUESTS_WINDOW_DAYS).contains(&days) {
            return Err(
                "--days must be between 1 and 31 (the export refuses a longer window)".to_string(),
            );
        }
        return Ok((
            format_epoch_seconds(now - days * SECONDS_PER_DAY),
            format_epoch_seconds(now),
        ));
    };
    if args.days.is_some() {
        return Err("--days cannot be combined with --since; name one or the other".to_string());
    }
    let until = args
        .until
        .clone()
        .unwrap_or_else(|| format_epoch_seconds(now));
    let start = parse_iso_instant(since)?;
    let end = parse_iso_instant(&until)?;
    if end <= start {
        return Err("--until must be later than --since".to_string());
    }
    if end - start > MAX_REQUESTS_WINDOW_DAYS * SECONDS_PER_DAY {
        return Err(
            "the window is longer than 31 days, which the export refuses; narrow it".to_string(),
        );
    }
    Ok((since.clone(), until))
}

/// The flags a caller gave that only apply with `--requests`, in the order the
/// help lists them.
fn misplaced_flags(given: impl Fn(&str) -> bool) -> Vec<&'static str> {
    REQUESTS_ONLY_FLAGS
        .into_iter()
        .filter(|flag| given(flag))
        .collect()
}

fn print_requests_json(page: &UsageRequestsPage, since: &str, until: &str) {
    let mut json = out::JsonWriter::new();
    json.begin_object();
    // The window the rows belong to. `next_cursor` is only good for this exact
    // window and filter set, so a caller driving the pages needs both back.
    json.field_str("since", since);
    json.field_str("until", until);
    json.field_str("as_of", &page.as_of);
    json.field_i64("requests", page.requests.len() as i64);
    json.field_i64("total_requests", page.total_requests);
    json.field_i64("prompt_tokens", page.prompt_tokens);
    json.field_i64("cached_tokens", page.cached_tokens);
    json.field_i64("noncached_prompt_tokens", page.noncached_prompt_tokens);
    json.field_i64("completion_tokens", page.completion_tokens);
    json.field_i64("reasoning_tokens", page.reasoning_tokens);
    json.field_i64("cost_micros", page.cost_micros);

    json.begin_array("rows");
    for row in &page.requests {
        json.begin_array_object();
        json.field_str("request_id", &row.request_id);
        json.field_str("response_request_id", &row.response_request_id);
        json.field_str("model", &row.model);
        json.field_i64("status_code", row.status_code);
        json.field_str("error_code", &row.error_code);
        json.field_str("finish_reason", &row.finish_reason);
        json.field_bool("stream", row.stream);
        json.field_str("ts_start", &row.ts_start);
        json.field_i64("prompt_tokens", row.prompt_tokens);
        json.field_i64("cached_tokens", row.cached_tokens);
        json.field_i64("completion_tokens", row.completion_tokens);
        json.field_i64("reasoning_tokens", row.reasoning_tokens);
        json.field_i64("cost_micros", row.cost_micros);
        // The writer has no null, so an absent latency is -1: never 0, which
        // would claim the engine answered instantly.
        json.field_i64("ttft_ms", row.ttft_ms.unwrap_or(-1));
        json.field_str("pricing_version", &row.pricing_version);
        json.end_object();
    }
    json.end_array();

    if !page.next_cursor.is_empty() {
        json.field_str("next_cursor", &page.next_cursor);
    }
    json.end_object();
    out::result_line(json.str());
}

/// The label under an errored row: the console's own error code, else `5xx`
/// for a server failure it did not name, else nothing worth a second line.
fn request_error(row: &UsageRequestRow) -> Option<&str> {
    if !row.error_code.is_empty() {
        Some(&row.error_code)
    } else if row.status_code >= 500 {
        Some("5xx")
    } else {
        None
    }
}

/// The window totals. `shown` is how many rows this report carries, which is
/// the first page's rows or, under `--follow`, every page's.
fn requests_summary_lines(
    page: &UsageRequestsPage,
    since: &str,
    until: &str,
    shown: usize,
) -> Vec<String> {
    vec![
        format!("window     {since} to {until}"),
        format!(
            "requests   {} of {} settled (as of {})",
            grouped(shown as i64),
            grouped(page.total_requests),
            page.as_of
        ),
        format!("spend      {} over the window", money(page.cost_micros)),
        format!(
            "tokens     in {} (cache {}, fresh {}), out {} (reasoning {})",
            grouped(page.prompt_tokens),
            grouped(page.cached_tokens),
            grouped(page.noncached_prompt_tokens),
            grouped(page.completion_tokens),
            grouped(page.reasoning_tokens)
        ),
    ]
}

/// The table: one header, then a row per request, plus a second line under any
/// row that errored. Times are shown as the ledger recorded them (UTC), cut to
/// the time of day.
fn request_table_lines(rows: &[UsageRequestRow]) -> Vec<String> {
    if rows.is_empty() {
        return vec!["no settled requests in this window".to_string()];
    }
    let mut lines = vec![format!(
        "{:<12} {:<20} {:>4} {:>11} {:>11} {:>8} {:>8}  {}",
        "started", "model", "code", "in", "out", "ttft", "spend", "request"
    )];
    for row in rows {
        // The console writes `2026-09-25T03:41:07.123456+00:00`; the terminal
        // wants the time of day.
        let started = row.ts_start.get(11..19).unwrap_or(&row.ts_start);
        let ttft = row
            .ttft_ms
            .map_or_else(|| "-".to_string(), |ms| format!("{ms}ms"));
        let id = if row.response_request_id.is_empty() {
            &row.request_id
        } else {
            &row.response_request_id
        };
        lines.push(format!(
            "{:<12} {:<20} {:>4} {:>11} {:>11} {:>8} {:>8}  {}",
            started,
            row.model,
            row.status_code,
            grouped(row.prompt_tokens),
            grouped(row.completion_tokens),
            ttft,
            money(row.cost_micros),
            id
        ));
        if let Some(error) = request_error(row) {
            let finish = if row.finish_reason.is_empty() {
                String::new()
            } else {
                format!(" ({})", row.finish_reason)
            };
            lines.push(format!("{:<12} error: {error}{finish}", ""));
        }
    }
    lines
}

/// One page, with the same single retry the summary makes: a 401 is answered
/// by refreshing the session once and asking again.
fn fetch_requests_page(
    client: &ConsoleClient,
    credentials: &mut Credentials,
    query: &UsageRequestsQuery,
) -> Result<UsageRequestsPage, String> {
    let (mut result, mut page, mut failure) =
        client.fetch_usage_requests(&credentials.console_url, &credentials.access_token, query);
    if result == IdentityResult::Unauthorized {
        // Not "expired": see `usage`. The console answers unknown, revoked,
        // expired and malformed with the same 401 on purpose.
        if let Err(refresh_failure) = refresh_session(client, credentials) {
            return Err(format!(
                "the console rejected this session ({refresh_failure}); run `wally account login`"
            ));
        }
        (result, page, failure) =
            client.fetch_usage_requests(&credentials.console_url, &credentials.access_token, query);
    }
    if result == IdentityResult::Ok {
        Ok(page)
    } else {
        Err(failure)
    }
}

/// Reads pages after the first until the console stops handing out a cursor.
/// Every page repeats the first page's window and filters, which is what the
/// console requires of a cursor. Nothing is printed until the whole window is
/// in hand, so a failure part-way is an error and never a report that looks
/// complete.
fn follow_pages(
    client: &ConsoleClient,
    credentials: &mut Credentials,
    query: &mut UsageRequestsQuery,
    first: &UsageRequestsPage,
) -> Result<(Vec<UsageRequestRow>, String), String> {
    let mut rows = first.requests.clone();
    let mut cursor = first.next_cursor.clone();
    let mut pages = 1;
    while !cursor.is_empty() {
        if pages >= MAX_FOLLOWED_PAGES {
            return Err(format!(
                "the window has more than {MAX_FOLLOWED_PAGES} pages; narrow it with \
                 --since/--until or a filter"
            ));
        }
        if cursor == query.cursor {
            // A console that hands back the cursor it was just given would
            // loop until the page cap, printing the same rows each time.
            return Err("the console returned the same page cursor twice; stopping".to_string());
        }
        query.cursor = cursor.clone();
        let page = fetch_requests_page(client, credentials, query)?;
        rows.extend(page.requests);
        cursor = page.next_cursor;
        pages += 1;
    }
    Ok((rows, cursor))
}

/// `wally account usage --requests`: the per-request export
/// (`GET /v1/cli/usage/requests`, InferenceInfra #809). Read-only, one
/// identity, and a window the caller names: one day back by default, `--days`
/// to move it or `--since`/`--until` to pin it, capped at 31 days because that
/// is what the route refuses past.
fn usage_requests(args: &RequestsArgs) -> i32 {
    let (since, until) = match resolve_window(args, epoch_seconds()) {
        Ok(window) => window,
        Err(problem) => {
            out::error_line(&problem);
            return 1;
        }
    };

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

    let mut query = UsageRequestsQuery {
        since: since.clone(),
        until: until.clone(),
        model: args.model.clone(),
        status_code: args.status_code,
        response_request_id: args.response_request_id.clone(),
        limit: args.limit,
        cursor: args.cursor.clone(),
    };
    let first = match fetch_requests_page(&client, &mut credentials, &query) {
        Ok(page) => page,
        Err(failure) => {
            out::error_line(&failure);
            return 1;
        }
    };

    if args.as_json {
        // The document is one page and `next_cursor` says whether another
        // exists. Walking every page into one array would let a mid-walk
        // failure pass as a complete export, so JSON leaves paging to the
        // caller, who replays `since`, `until` and the filters with the cursor.
        print_requests_json(&first, &since, &until);
        return 0;
    }

    let (rows, unread_cursor) = if args.follow {
        match follow_pages(&client, &mut credentials, &mut query, &first) {
            Ok(followed) => followed,
            Err(failure) => {
                out::error_line(&failure);
                return 1;
            }
        }
    } else {
        (first.requests.clone(), first.next_cursor.clone())
    };

    for line in requests_summary_lines(&first, &since, &until, rows.len()) {
        out::result_line(&line);
    }
    out::result_line("");
    for line in request_table_lines(&rows) {
        out::result_line(&line);
    }
    out::result_line("");
    if !unread_cursor.is_empty() {
        out::status_line("more rows exist; pass --follow to read every page");
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
    usage_cmd.add_flag(
        "--requests",
        "List settled requests instead of the summary (one page; --follow reads them all)",
    );
    usage_cmd
        .add_option("--days", ValueType::Int, "Window length in days (1-31)")
        .check(Validator::Range(1, MAX_REQUESTS_WINDOW_DAYS));
    usage_cmd.add_option(
        "--since",
        ValueType::Text,
        "Window start, with a timezone (e.g. 2026-09-25T00:00:00Z)",
    );
    usage_cmd.add_option(
        "--until",
        ValueType::Text,
        "Window end, with a timezone (default: now; needs --since)",
    );
    usage_cmd.add_option("--model", ValueType::Text, "Only this model id");
    usage_cmd
        .add_option(
            "--status",
            ValueType::Int,
            "Only this HTTP status (100-599)",
        )
        .check(Validator::Range(100, 599));
    usage_cmd.add_option(
        "--response-request-id",
        ValueType::Text,
        "Only the rows for this x-request-id (what a response logged)",
    );
    usage_cmd.add_option(
        "--cursor",
        ValueType::Text,
        "Continue from a previous page's next_cursor (needs --since and --until)",
    );
    usage_cmd
        .add_option("--limit", ValueType::Int, "Rows per page (1-200)")
        .default_val("100")
        .check(Validator::Range(1, 200));
    usage_cmd.add_flag("--follow", "Read every page until the window is done");
    usage_cmd.footer(&examples_footer(&[
        Example::new("wally account usage", ""),
        Example::new("wally --json account usage", ""),
        Example::new(
            "wally account usage --requests",
            "every settled request of the last day, first page",
        ),
        Example::new(
            "wally account usage --requests --days 7 --follow",
            "a full week, every page",
        ),
        Example::new(
            "wally account usage --requests --status 500",
            "the requests our side failed",
        ),
        Example::new(
            "wally account usage --requests --response-request-id <id>",
            "the rows behind one x-request-id",
        ),
        Example::new(
            "wally --json account usage --requests --since 2026-09-24T00:00:00Z --until 2026-09-25T00:00:00Z",
            "one page as JSON, over a pinned window",
        ),
    ]));
    // `wally --json usage` and `wally usage --json` mean the same thing. The
    // root parser accepts the first, so reading only the command-local flag
    // printed a human table to something asking for one JSON document.
    usage_cmd.callback(|p, g| {
        let as_json = p.flag("--json") || g.json;
        if !p.flag("--requests") {
            let misplaced = misplaced_flags(|flag| p.is_set(flag));
            if !misplaced.is_empty() {
                out::error_line(&format!(
                    "{} only applies with --requests",
                    misplaced.join(", ")
                ));
                return 1;
            }
            return usage(as_json);
        }
        let follow = p.flag("--follow");
        if follow && as_json {
            out::error_line(
                "--follow cannot be combined with --json: JSON is one page, and next_cursor \
                 is how you read the next",
            );
            return 1;
        }
        usage_requests(&RequestsArgs {
            as_json,
            days: p.get_i64("--days").filter(|_| p.is_set("--days")),
            since: p.get_str("--since"),
            until: p.get_str("--until"),
            model: p.get_str("--model").unwrap_or_default(),
            status_code: p.get_i64("--status").unwrap_or(0) as i32,
            response_request_id: p.get_str("--response-request-id").unwrap_or_default(),
            cursor: p.get_str("--cursor").unwrap_or_default(),
            limit: p.get_i64("--limit").unwrap_or(100) as i32,
            follow,
        })
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::{HttpRequest, HttpResponse, Transport};
    use std::sync::{Arc, Mutex};

    const NOW: i64 = 1_790_000_000; // 2026-09-21T14:13:20Z

    fn args() -> RequestsArgs {
        RequestsArgs {
            as_json: false,
            days: None,
            since: None,
            until: None,
            model: String::new(),
            status_code: 0,
            response_request_id: String::new(),
            cursor: String::new(),
            limit: 100,
            follow: false,
        }
    }

    fn row(id: &str) -> UsageRequestRow {
        UsageRequestRow {
            request_id: id.to_string(),
            model: "glm-5.3-flash".to_string(),
            status_code: 200,
            ts_start: "2026-09-25T03:41:07.123456+00:00".to_string(),
            prompt_tokens: 1_200,
            completion_tokens: 30,
            cost_micros: 1_500,
            ..UsageRequestRow::default()
        }
    }

    #[test]
    fn parses_an_instant_with_a_zone() {
        assert_eq!(parse_iso_instant("1970-01-01T00:00:00Z"), Ok(0));
        assert_eq!(parse_iso_instant("1970-01-02T00:00:00Z"), Ok(86_400));
        // The same instant, written three ways.
        let utc = parse_iso_instant("2026-09-25T08:00:00Z").unwrap();
        assert_eq!(parse_iso_instant("2026-09-25T13:30:00+05:30"), Ok(utc));
        assert_eq!(parse_iso_instant("2026-09-25T00:00:00-08:00"), Ok(utc));
        // Fractional seconds are read past, not rounded into the instant.
        assert_eq!(parse_iso_instant("2026-09-25T08:00:00.987654Z"), Ok(utc));
        assert_eq!(parse_iso_instant("2026-09-25T08:00:00z"), Ok(utc));
    }

    #[test]
    fn parsing_inverts_the_formatter_across_leap_years_and_centuries() {
        for epoch in [
            1,
            951_782_400,   // 2000-02-29, a leap day in a divisible-by-400 year
            1_709_208_000, // 2024-02-29T12:00:00Z
            4_107_542_400, // 2100-03-01, after a non-leap century
            NOW,
        ] {
            assert_eq!(
                parse_iso_instant(&format_epoch_seconds(epoch)),
                Ok(epoch),
                "{epoch} did not round-trip"
            );
        }
    }

    #[test]
    fn refuses_what_the_console_would_refuse() {
        for text in [
            "",
            "2026-09-25",
            "2026-09-25T08:00:00",       // no timezone: the console's 400
            "2026-09-25T08:00",          // no seconds
            "2026-09-25 08:00:00Z",      // a space, not T
            "2026-13-01T00:00:00Z",      // month 13
            "2026-00-10T00:00:00Z",      // month 0
            "2026-09-00T00:00:00Z",      // day 0
            "2026-04-31T00:00:00Z",      // April has 30 days
            "2023-02-29T00:00:00Z",      // not a leap year
            "2100-02-29T00:00:00Z",      // a century that is not a leap year
            "2026-09-25T24:00:00Z",      // hour 24
            "2026-09-25T08:60:00Z",      // minute 60
            "2026-09-25T08:00:60Z",      // second 60
            "2026-09-25T08:00:00.Z",     // a dot with no digits
            "2026-09-25T08:00:00+0530",  // zone without a colon
            "2026-09-25T08:00:00+05:3",  // short zone
            "2026-09-25T08:00:00+24:00", // zone hour out of range
            "2026-09-25T08:00:00+05:60", // zone minute out of range
            "2026-09-25T08:00:00Zjunk",
            "2026-09-2５T08:00:00Z", // a full-width digit
            "yesterday",
            "-2026-09-25T08:00:00Z",
        ] {
            let error = parse_iso_instant(text).expect_err(text);
            assert!(error.contains("timezone"), "{text}: {error}");
        }
    }

    #[test]
    fn no_flags_asks_for_one_day_back() {
        let (since, until) = resolve_window(&args(), NOW).unwrap();
        assert_eq!(until, format_epoch_seconds(NOW));
        assert_eq!(since, format_epoch_seconds(NOW - 86_400));
    }

    #[test]
    fn days_moves_the_window_and_stops_at_thirty_one() {
        let seven = RequestsArgs {
            days: Some(7),
            ..args()
        };
        assert_eq!(
            resolve_window(&seven, NOW).unwrap().0,
            format_epoch_seconds(NOW - 7 * 86_400)
        );
        assert!(resolve_window(
            &RequestsArgs {
                days: Some(31),
                ..args()
            },
            NOW
        )
        .is_ok());
        for days in [0, -1, 32, 365] {
            let error = resolve_window(
                &RequestsArgs {
                    days: Some(days),
                    ..args()
                },
                NOW,
            )
            .expect_err("out of range");
            assert!(error.contains("between 1 and 31"), "{days}: {error}");
        }
    }

    #[test]
    fn since_alone_runs_to_now_and_a_pinned_window_is_sent_as_written() {
        let since = "2026-09-20T00:00:00Z".to_string();
        let open = RequestsArgs {
            since: Some(since.clone()),
            ..args()
        };
        assert_eq!(
            resolve_window(&open, NOW).unwrap(),
            (since.clone(), format_epoch_seconds(NOW))
        );

        let until = "2026-09-21T06:30:00+05:30".to_string();
        let pinned = RequestsArgs {
            since: Some(since.clone()),
            until: Some(until.clone()),
            ..args()
        };
        // Byte for byte: the console reads the zone itself.
        assert_eq!(resolve_window(&pinned, NOW).unwrap(), (since, until));
    }

    #[test]
    fn a_window_that_cannot_be_asked_for_is_refused_before_it_is_sent() {
        let since = Some("2026-08-01T00:00:00Z".to_string());
        let case = |since: Option<&str>, until: Option<&str>, days: Option<i64>| {
            resolve_window(
                &RequestsArgs {
                    since: since.map(str::to_string),
                    until: until.map(str::to_string),
                    days,
                    ..args()
                },
                NOW,
            )
        };
        assert!(case(None, Some("2026-09-01T00:00:00Z"), None)
            .unwrap_err()
            .contains("--until needs --since"));
        assert!(case(since.as_deref(), None, Some(3))
            .unwrap_err()
            .contains("--days cannot be combined"));
        // Empty or backwards.
        assert!(case(
            Some("2026-09-01T00:00:00Z"),
            Some("2026-09-01T00:00:00Z"),
            None
        )
        .unwrap_err()
        .contains("later than"));
        assert!(case(
            Some("2026-09-02T00:00:00Z"),
            Some("2026-09-01T00:00:00Z"),
            None
        )
        .unwrap_err()
        .contains("later than"));
        // Exactly 31 days is allowed; one second more is not.
        assert!(case(
            Some("2026-08-01T00:00:00Z"),
            Some("2026-09-01T00:00:00Z"),
            None
        )
        .is_ok());
        assert!(case(
            Some("2026-08-01T00:00:00Z"),
            Some("2026-09-01T00:00:01Z"),
            None
        )
        .unwrap_err()
        .contains("longer than 31 days"));
        // A bound with no timezone never reaches the console.
        assert!(case(
            Some("2026-08-01T00:00:00"),
            Some("2026-08-02T00:00:00Z"),
            None
        )
        .unwrap_err()
        .contains("timezone"));
        assert!(case(Some("2026-08-01T00:00:00Z"), Some("tomorrow"), None)
            .unwrap_err()
            .contains("timezone"));
    }

    #[test]
    fn a_cursor_needs_the_window_that_issued_it() {
        let cursor = "eyJuIjoxfQ".to_string();
        let both = RequestsArgs {
            since: Some("2026-09-20T00:00:00Z".to_string()),
            until: Some("2026-09-21T00:00:00Z".to_string()),
            cursor: cursor.clone(),
            ..args()
        };
        assert!(resolve_window(&both, NOW).is_ok());
        for broken in [
            RequestsArgs {
                cursor: cursor.clone(),
                ..args()
            },
            RequestsArgs {
                since: both.since.clone(),
                cursor: cursor.clone(),
                ..args()
            },
        ] {
            assert!(resolve_window(&broken, NOW)
                .unwrap_err()
                .contains("--cursor needs --since and --until"));
        }
    }

    #[test]
    fn request_only_flags_are_named_when_they_stand_alone() {
        let given = ["--follow", "--model", "--days"];
        assert_eq!(
            misplaced_flags(|flag| given.contains(&flag)),
            vec!["--days", "--model", "--follow"],
            "listed in help order, not argv order"
        );
        assert!(misplaced_flags(|_| false).is_empty());
        // `--json` and `--requests` are not on the list: the first is the
        // summary's own, the second is what would make the rest legal.
        assert!(!REQUESTS_ONLY_FLAGS.contains(&"--json"));
        assert!(!REQUESTS_ONLY_FLAGS.contains(&"--requests"));
    }

    #[test]
    fn an_error_is_named_by_its_code_then_by_its_status() {
        let mut errored = row("a");
        assert_eq!(request_error(&errored), None);
        errored.status_code = 503;
        assert_eq!(request_error(&errored), Some("5xx"));
        errored.error_code = "upstream_error".to_string();
        assert_eq!(request_error(&errored), Some("upstream_error"));
        // A 4xx with no code says nothing: the status column already does.
        let mut refused = row("b");
        refused.status_code = 429;
        assert_eq!(request_error(&refused), None);
    }

    #[test]
    fn the_table_draws_a_header_rows_and_an_error_line() {
        let mut slow = row("ledger-1");
        slow.response_request_id = "resp-1".to_string();
        slow.ttft_ms = Some(310);
        let mut failed = row("ledger-2");
        failed.status_code = 500;
        failed.error_code = "upstream_error".to_string();
        failed.finish_reason = "error".to_string();

        let lines = request_table_lines(&[slow, failed]);
        assert_eq!(lines.len(), 4, "header, row, row, error line: {lines:#?}");
        assert!(lines[0].starts_with("started") && lines[0].contains("request"));
        assert!(
            lines[1].starts_with("03:41:07"),
            "the time of day: {}",
            lines[1]
        );
        assert!(lines[1].contains("310ms") && lines[1].contains("$0.0015"));
        assert!(
            lines[1].ends_with("resp-1"),
            "the id a client logged is preferred"
        );
        assert!(
            lines[2].ends_with("ledger-2"),
            "the ledger id stands in when there is none"
        );
        assert!(
            lines[2].contains(" -"),
            "an absent latency is a dash, not 0ms"
        );
        assert_eq!(lines[3].trim_start(), "error: upstream_error (error)");
    }

    #[test]
    fn an_empty_window_says_so_instead_of_drawing_an_empty_table() {
        assert_eq!(
            request_table_lines(&[]),
            vec!["no settled requests in this window".to_string()]
        );
    }

    #[test]
    fn a_short_or_missing_timestamp_never_panics_the_table() {
        for ts_start in ["", "2026-09-25", "2026-09-25T03"] {
            let mut short = row("a");
            short.ts_start = ts_start.to_string();
            assert_eq!(request_table_lines(&[short]).len(), 2, "{ts_start:?}");
        }
    }

    #[test]
    fn the_summary_counts_what_this_report_carries() {
        let page = UsageRequestsPage {
            as_of: "2026-09-25T08:34:18Z".to_string(),
            total_requests: 1_234,
            prompt_tokens: 5_000,
            cached_tokens: 4_000,
            noncached_prompt_tokens: 1_000,
            completion_tokens: 200,
            reasoning_tokens: 50,
            cost_micros: 2_500_000,
            ..UsageRequestsPage::default()
        };
        let lines = requests_summary_lines(&page, "S", "U", 100);
        assert_eq!(lines[0], "window     S to U");
        assert_eq!(
            lines[1],
            "requests   100 of 1,234 settled (as of 2026-09-25T08:34:18Z)"
        );
        assert_eq!(lines[2], "spend      $2.50 over the window");
        assert_eq!(
            lines[3],
            "tokens     in 5,000 (cache 4,000, fresh 1,000), out 200 (reasoning 50)"
        );
    }

    // ---- paging, against a transport that serves pages by cursor ----

    fn page_body(rows: usize, next: Option<&str>) -> String {
        let records: Vec<_> = (0..rows)
            .map(|n| {
                serde_json::json!({
                    "request_id": format!("ledger-{n}"), "model": "glm-5.3-flash",
                    "provider": "self_hosted_sglang", "status_code": 200, "stream": false,
                    "ts_start": "2026-09-25T08:00:00Z", "recorded_at": "2026-09-25T08:00:01Z",
                    "prompt_tokens": 1, "cached_tokens": 0, "noncached_prompt_tokens": 1,
                    "completion_tokens": 1, "reasoning_tokens": 0, "cost_micros": 1,
                    "pricing_version": "v1"
                })
            })
            .collect();
        serde_json::json!({
            "as_of": "2026-09-25T08:34:18Z",
            "totals": {"requests": 9, "prompt_tokens": 0, "cached_tokens": 0,
                       "noncached_prompt_tokens": 0, "completion_tokens": 0,
                       "reasoning_tokens": 0, "cost_micros": 0},
            "requests": records,
            "next_cursor": next,
        })
        .to_string()
    }

    fn client_serving(
        serve: impl Fn(&str) -> Result<HttpResponse, String> + Send + Sync + 'static,
    ) -> (ConsoleClient, Arc<Mutex<Vec<String>>>) {
        let urls = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&urls);
        let transport: Transport = Arc::new(move |request: &HttpRequest| {
            seen.lock().unwrap().push(request.url.clone());
            serve(&request.url)
        });
        (ConsoleClient::new(Some(transport)), urls)
    }

    fn ok(body: String) -> Result<HttpResponse, String> {
        Ok(HttpResponse {
            status: 200,
            body,
            ..HttpResponse::default()
        })
    }

    fn signed_in() -> Credentials {
        Credentials {
            console_url: "https://console.example.test".to_string(),
            access_token: "a-token".to_string(),
            ..Credentials::default()
        }
    }

    fn query() -> UsageRequestsQuery {
        UsageRequestsQuery {
            since: "2026-09-24T00:00:00Z".to_string(),
            until: "2026-09-25T00:00:00Z".to_string(),
            ..UsageRequestsQuery::default()
        }
    }

    #[test]
    fn following_reads_every_page_with_the_same_window_and_no_more() {
        let (client, urls) = client_serving(|url| {
            ok(if url.contains("cursor=c2") {
                page_body(1, None)
            } else if url.contains("cursor=c1") {
                page_body(2, Some("c2"))
            } else {
                page_body(2, Some("c1"))
            })
        });
        let mut credentials = signed_in();
        let mut query = query();
        let first = fetch_requests_page(&client, &mut credentials, &query).unwrap();
        let (rows, unread) = follow_pages(&client, &mut credentials, &mut query, &first).unwrap();

        assert_eq!(rows.len(), 5, "2 + 2 + 1 rows across three pages");
        assert_eq!(unread, "", "the last page has no cursor");
        let urls = urls.lock().unwrap();
        assert_eq!(urls.len(), 3);
        for url in urls.iter() {
            assert!(
                url.contains("since=2026-09-24T00%3A00%3A00Z")
                    && url.contains("until=2026-09-25T00%3A00%3A00Z"),
                "a page changed the window: {url}"
            );
        }
        assert!(urls[1].contains("cursor=c1") && urls[2].contains("cursor=c2"));
    }

    #[test]
    fn following_a_single_page_asks_for_nothing_more() {
        let (client, urls) = client_serving(|_| ok(page_body(3, None)));
        let mut credentials = signed_in();
        let mut query = query();
        let first = fetch_requests_page(&client, &mut credentials, &query).unwrap();
        let (rows, unread) = follow_pages(&client, &mut credentials, &mut query, &first).unwrap();
        assert_eq!((rows.len(), unread.as_str()), (3, ""));
        assert_eq!(urls.lock().unwrap().len(), 1);
    }

    #[test]
    fn a_failure_part_way_through_is_an_error_and_not_a_short_report() {
        let (client, _) = client_serving(|url| {
            if url.contains("cursor=") {
                Ok(HttpResponse {
                    status: 503,
                    ..HttpResponse::default()
                })
            } else {
                ok(page_body(2, Some("c1")))
            }
        });
        let mut credentials = signed_in();
        let mut query = query();
        let first = fetch_requests_page(&client, &mut credentials, &query).unwrap();
        let error = follow_pages(&client, &mut credentials, &mut query, &first).unwrap_err();
        assert!(error.contains("temporarily unavailable"), "{error}");
    }

    #[test]
    fn a_cursor_that_never_advances_stops_instead_of_looping() {
        let (client, urls) = client_serving(|_| ok(page_body(1, Some("stuck"))));
        let mut credentials = signed_in();
        let mut query = query();
        let first = fetch_requests_page(&client, &mut credentials, &query).unwrap();
        let error = follow_pages(&client, &mut credentials, &mut query, &first).unwrap_err();
        assert!(error.contains("same page cursor twice"), "{error}");
        assert_eq!(urls.lock().unwrap().len(), 2, "one repeat, then stop");
    }

    #[test]
    fn a_window_of_endless_pages_hits_the_cap() {
        let counter = Arc::new(Mutex::new(0));
        let served = Arc::clone(&counter);
        let (client, urls) = client_serving(move |_| {
            let mut n = served.lock().unwrap();
            *n += 1;
            ok(page_body(1, Some(&format!("c{n}"))))
        });
        let mut credentials = signed_in();
        let mut query = query();
        let first = fetch_requests_page(&client, &mut credentials, &query).unwrap();
        let error = follow_pages(&client, &mut credentials, &mut query, &first).unwrap_err();
        assert!(error.contains("more than 100 pages"), "{error}");
        assert_eq!(urls.lock().unwrap().len(), MAX_FOLLOWED_PAGES);
    }

    #[test]
    fn a_rejected_session_that_cannot_refresh_says_to_log_in() {
        let (client, urls) = client_serving(|_| {
            Ok(HttpResponse {
                status: 401,
                ..HttpResponse::default()
            })
        });
        let mut credentials = signed_in(); // no refresh token
        let error = fetch_requests_page(&client, &mut credentials, &query()).unwrap_err();
        assert!(error.contains("rejected this session"), "{error}");
        assert!(error.contains("wally account login"), "{error}");
        assert_eq!(urls.lock().unwrap().len(), 1, "no retry without a refresh");
    }
}

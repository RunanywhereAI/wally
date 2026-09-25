//! Port of src/commands/cmd_usage.cpp.

use crate::account::{
    epoch_seconds, export_usage_requests, ConsoleClient, ConsoleSession, Usage, UsageQuery,
    UsageRequestRow, UsageRequestsQuery, UsageRequestsReport, UsageWindow, HTTP_STATUS_MAX,
    HTTP_STATUS_MIN, USAGE_REQUESTS_DEFAULT_LIMIT, USAGE_REQUESTS_MAX_LIMIT,
    USAGE_REQUESTS_MAX_WINDOW_DAYS,
};
use crate::cli::{App, Validator, ValueType};
use crate::cli_formatter::{examples_footer, Example};
use crate::io::output as out;

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
    // One day back, and the shortest recent-request page the route accepts:
    // nothing here renders those, and `by_model`/`totals` group in SQL, so the
    // page size cannot move the numbers above.
    let query = UsageQuery {
        days: 1,
        limit: 1,
        ..UsageQuery::default()
    };
    let usage = ConsoleSession::open(ConsoleClient::default()).and_then(|mut session| {
        session.call(|client, url, token| client.fetch_usage(url, token, &query))
    });
    match usage {
        Ok(usage) if as_json => print_json(&usage),
        Ok(usage) => print_report(&usage),
        Err(failure) => {
            out::error_line(&failure);
            return 1;
        }
    }
    0
}

/// Flags that only mean something next to `--requests`. Given without it they
/// would be silently ignored, and a filter that does nothing reads as a report
/// that was filtered.
const REQUESTS_ONLY_FLAGS: [&str; 6] = [
    "--days",
    "--model",
    "--status",
    "--response-request-id",
    "--limit",
    "--follow",
];

/// What `wally account usage --requests` was asked for.
struct RequestsArgs {
    as_json: bool,
    days: i64,
    model: Option<String>,
    status_code: Option<u16>,
    response_request_id: Option<String>,
    /// Rows on the one page read without `--follow`.
    limit: u32,
    follow: bool,
}

/// `--follow` reads whole pages of its own size, so a page size beside it would
/// be a number that changes nothing.
fn check_follow_flags(follow: bool, limit_given: bool) -> Result<(), String> {
    if follow && limit_given {
        return Err(
            "--limit sets the size of the one page read without --follow; --follow reads them all"
                .to_string(),
        );
    }
    Ok(())
}

/// The flags a caller gave that only apply with `--requests`, in the order the
/// help lists them.
fn misplaced_flags(given: impl Fn(&str) -> bool) -> Vec<&'static str> {
    REQUESTS_ONLY_FLAGS
        .into_iter()
        .filter(|flag| given(flag))
        .collect()
}

fn field_opt_str(json: &mut out::JsonWriter, key: &str, value: Option<&str>) {
    match value {
        Some(value) => json.field_str(key, value),
        None => json.field_null(key),
    };
}

fn field_opt_i64(json: &mut out::JsonWriter, key: &str, value: Option<i64>) {
    match value {
        Some(value) => json.field_i64(key, value),
        None => json.field_null(key),
    };
}

/// The `--json` document: the window's totals and every field of the rows this
/// run read. A value the console did not send is `null`, never a stand-in.
fn requests_json(report: &UsageRequestsReport) -> String {
    let mut json = out::JsonWriter::new();
    json.begin_object();
    json.field_str("since", &report.since);
    json.field_str("until", &report.until);
    json.field_str("as_of", &report.as_of);
    json.field_i64(
        "row_count",
        i64::try_from(report.rows.len()).unwrap_or(i64::MAX),
    );
    json.field_i64("total_requests", report.totals.requests);
    json.field_bool("has_more", report.has_more);
    json.field_i64("prompt_tokens", report.totals.prompt_tokens);
    json.field_i64("cached_tokens", report.totals.cached_tokens);
    json.field_i64(
        "noncached_prompt_tokens",
        report.totals.noncached_prompt_tokens,
    );
    json.field_i64("completion_tokens", report.totals.completion_tokens);
    json.field_i64("reasoning_tokens", report.totals.reasoning_tokens);
    json.field_i64("cost_micros", report.totals.cost_micros);

    json.begin_array("rows");
    for row in &report.rows {
        json.begin_array_object();
        json.field_str("request_id", &row.request_id);
        field_opt_str(
            &mut json,
            "response_request_id",
            row.response_request_id.as_deref(),
        );
        json.field_str("model", &row.model);
        field_opt_str(&mut json, "provider", row.provider.as_deref());
        json.field_i64("status_code", row.status_code);
        field_opt_str(&mut json, "error_code", row.error_code.as_deref());
        field_opt_str(&mut json, "finish_reason", row.finish_reason.as_deref());
        json.field_bool("stream", row.stream);
        json.field_str("ts_start", &row.ts_start);
        field_opt_str(&mut json, "ts_end", row.ts_end.as_deref());
        json.field_str("recorded_at", &row.recorded_at);
        json.field_i64("prompt_tokens", row.prompt_tokens);
        json.field_i64("cached_tokens", row.cached_tokens);
        json.field_i64("noncached_prompt_tokens", row.noncached_prompt_tokens);
        json.field_i64("completion_tokens", row.completion_tokens);
        json.field_i64("reasoning_tokens", row.reasoning_tokens);
        field_opt_i64(&mut json, "max_tokens_requested", row.max_tokens_requested);
        field_opt_i64(&mut json, "max_tokens_granted", row.max_tokens_granted);
        field_opt_i64(&mut json, "ttft_ms", row.ttft_ms);
        field_opt_i64(&mut json, "tpot_ms", row.tpot_ms);
        json.field_i64("cost_micros", row.cost_micros);
        json.field_str("pricing_version", &row.pricing_version);
        json.end_object();
    }
    json.end_array();
    json.end_object();
    json.str().to_string()
}

/// The label under an errored row: the console's own error code, else `5xx`
/// for a server failure it did not name, else nothing worth a second line.
fn request_error(row: &UsageRequestRow) -> Option<&str> {
    if let Some(code) = &row.error_code {
        Some(code)
    } else if row.status_code >= 500 {
        Some("5xx")
    } else {
        None
    }
}

/// The window totals, beside how many rows this report carries: the first
/// page's or, under `--follow`, every page's.
fn requests_summary_lines(report: &UsageRequestsReport) -> Vec<String> {
    let totals = &report.totals;
    let shown = i64::try_from(report.rows.len()).unwrap_or(i64::MAX);
    vec![
        format!("window     {} to {}", report.since, report.until),
        format!(
            "requests   {} of {} settled (as of {})",
            grouped(shown),
            grouped(totals.requests),
            report.as_of
        ),
        format!("spend      {} over the window", money(totals.cost_micros)),
        format!(
            "tokens     in {} (cache {}, fresh {}), out {} (reasoning {})",
            grouped(totals.prompt_tokens),
            grouped(totals.cached_tokens),
            grouped(totals.noncached_prompt_tokens),
            grouped(totals.completion_tokens),
            grouped(totals.reasoning_tokens)
        ),
    ]
}

/// `MM-DD HH:MM:SS` from the console's `2026-09-25T03:41:07.123456+00:00`: a
/// window can span a month, so the time of day alone would not say which day.
/// Shown in UTC as the ledger recorded it; anything shorter is shown as sent.
fn started_label(ts_start: &str) -> String {
    match (ts_start.get(5..10), ts_start.get(11..19)) {
        (Some(date), Some(time)) => format!("{date} {time}"),
        _ => ts_start.to_string(),
    }
}

/// The table: one header, then a row per request, plus a second line under any
/// row that errored.
fn request_table_lines(rows: &[UsageRequestRow]) -> Vec<String> {
    if rows.is_empty() {
        return vec!["no settled requests in this window".to_string()];
    }
    let mut lines = vec![format!(
        "{:<14} {:<20} {:>4} {:>11} {:>11} {:>8} {:>8}  {}",
        "started", "model", "code", "in", "out", "ttft", "spend", "request"
    )];
    for row in rows {
        let ttft = row
            .ttft_ms
            .map_or_else(|| "-".to_string(), |ms| format!("{ms}ms"));
        let id = row
            .response_request_id
            .as_deref()
            .unwrap_or(&row.request_id);
        lines.push(format!(
            "{:<14} {:<20} {:>4} {:>11} {:>11} {:>8} {:>8}  {}",
            started_label(&row.ts_start),
            row.model,
            row.status_code,
            grouped(row.prompt_tokens),
            grouped(row.completion_tokens),
            ttft,
            money(row.cost_micros),
            id
        ));
        if let Some(error) = request_error(row) {
            let finish = row
                .finish_reason
                .as_deref()
                .map_or_else(String::new, |reason| format!(" ({reason})"));
            lines.push(format!("{:<14} error: {error}{finish}", ""));
        }
    }
    lines
}

/// The query the flags describe, checked against the contract before any
/// credential is read, so a filter the console would refuse is an error here.
fn requests_query(args: &RequestsArgs, now: i64) -> Result<UsageRequestsQuery, String> {
    let query = UsageRequestsQuery {
        model: args.model.clone(),
        status_code: args.status_code,
        response_request_id: args.response_request_id.clone(),
        limit: args.limit,
        ..UsageRequestsQuery::last_days(args.days, now)?
    };
    query.validate()?;
    Ok(query)
}

/// `wally account usage --requests`: the per-request export. Read-only, one
/// identity, and a window counted back from now: one day by default, `--days`
/// to move it.
fn usage_requests(args: RequestsArgs) -> i32 {
    let report = requests_query(&args, epoch_seconds()).and_then(|query| {
        let mut session = ConsoleSession::open(ConsoleClient::default())?;
        export_usage_requests(&mut session, query, args.follow)
    });
    let report = match report {
        Ok(report) => report,
        Err(failure) => {
            out::error_line(&failure);
            return 1;
        }
    };

    if args.as_json {
        out::result_line(&requests_json(&report));
        return 0;
    }
    for line in requests_summary_lines(&report) {
        out::result_line(&line);
    }
    out::result_line("");
    for line in request_table_lines(&report.rows) {
        out::result_line(&line);
    }
    out::result_line("");
    if report.has_more {
        out::status_line("more rows exist; pass --follow to read every page");
    }
    0
}

/// An integer flag the parser already range-checked, in the width the query
/// carries. The `Err` is unreachable through the validator, and says so if the
/// two ever disagree.
fn narrowed<T: TryFrom<i64>>(flag: &str, value: i64) -> Result<T, String> {
    T::try_from(value).map_err(|_| format!("{flag} {value} is out of range"))
}

fn requests_args(p: &crate::cli::Parsed, as_json: bool) -> Result<RequestsArgs, String> {
    let follow = p.flag("--follow");
    check_follow_flags(follow, p.is_set("--limit"))?;
    Ok(RequestsArgs {
        as_json,
        days: p.get_i64("--days").unwrap_or(1),
        model: p.get_str("--model"),
        status_code: p
            .get_i64("--status")
            .map(|status| narrowed("--status", status))
            .transpose()?,
        response_request_id: p.get_str("--response-request-id"),
        limit: match p.get_i64("--limit") {
            Some(limit) => narrowed("--limit", limit)?,
            None => USAGE_REQUESTS_DEFAULT_LIMIT,
        },
        follow,
    })
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
        account_cmd.add_subcommand("usage", "Credit and the last day's spend, or --requests");
    usage_cmd.add_flag("--json", "Print as JSON");
    usage_cmd.add_flag(
        "--requests",
        "List settled requests instead of the summary (one page; --follow reads them all)",
    );
    usage_cmd
        .add_option(
            "--days",
            ValueType::Int,
            &format!("Window length in days (1-{USAGE_REQUESTS_MAX_WINDOW_DAYS})"),
        )
        .default_val("1")
        .check(Validator::Range(1, USAGE_REQUESTS_MAX_WINDOW_DAYS));
    usage_cmd.add_option("--model", ValueType::Text, "Only this model id");
    usage_cmd
        .add_option(
            "--status",
            ValueType::Int,
            &format!("Only this HTTP status ({HTTP_STATUS_MIN}-{HTTP_STATUS_MAX})"),
        )
        .check(Validator::Range(
            i64::from(HTTP_STATUS_MIN),
            i64::from(HTTP_STATUS_MAX),
        ));
    usage_cmd.add_option(
        "--response-request-id",
        ValueType::Text,
        "Only the rows for this x-request-id (what a response logged)",
    );
    usage_cmd
        .add_option(
            "--limit",
            ValueType::Int,
            &format!("Rows on the one page read without --follow (1-{USAGE_REQUESTS_MAX_LIMIT})"),
        )
        .default_val(&USAGE_REQUESTS_DEFAULT_LIMIT.to_string())
        .check(Validator::Range(1, i64::from(USAGE_REQUESTS_MAX_LIMIT)));
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
            "wally --json account usage --requests --days 7 --follow",
            "a full week as one JSON document",
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
        match requests_args(p, as_json) {
            Ok(args) => usage_requests(args),
            Err(problem) => {
                out::error_line(&problem);
                1
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::UsageRequestsTotals;

    const NOW: i64 = 1_790_000_000; // 2026-09-21T14:13:20Z

    fn row(id: &str) -> UsageRequestRow {
        UsageRequestRow {
            request_id: id.to_string(),
            model: "glm-5.3-flash".to_string(),
            status_code: 200,
            ts_start: "2026-09-25T03:41:07.123456+00:00".to_string(),
            recorded_at: "2026-09-25T03:41:08+00:00".to_string(),
            prompt_tokens: 1_200,
            completion_tokens: 30,
            cost_micros: 1_500,
            pricing_version: "v1".to_string(),
            ..UsageRequestRow::default()
        }
    }

    fn args() -> RequestsArgs {
        RequestsArgs {
            as_json: false,
            days: 1,
            model: None,
            status_code: None,
            response_request_id: None,
            limit: USAGE_REQUESTS_DEFAULT_LIMIT,
            follow: false,
        }
    }

    #[test]
    fn the_flags_become_one_query_over_a_window_counted_back_from_now() {
        let query = requests_query(
            &RequestsArgs {
                days: 7,
                model: Some("glm-5.3-flash".to_string()),
                status_code: Some(500),
                response_request_id: Some("resp-9".to_string()),
                limit: 25,
                ..args()
            },
            NOW,
        )
        .unwrap();
        assert_eq!(query.since, "2026-09-14T14:13:20Z");
        assert_eq!(query.until, "2026-09-21T14:13:20Z");
        assert_eq!(query.model.as_deref(), Some("glm-5.3-flash"));
        assert_eq!(query.status_code, Some(500));
        assert_eq!(query.response_request_id.as_deref(), Some("resp-9"));
        assert_eq!(query.limit, 25);
    }

    #[test]
    fn a_filter_the_console_would_refuse_is_refused_before_anything_is_read() {
        let refused = [
            RequestsArgs { days: 32, ..args() },
            RequestsArgs {
                model: Some(String::new()),
                ..args()
            },
            RequestsArgs {
                response_request_id: Some(String::new()),
                ..args()
            },
            RequestsArgs {
                status_code: Some(0),
                ..args()
            },
        ];
        let expected = [
            "the window must be 1-31 days",
            "the model filter must be a model id",
            "the response request id filter must be 1-128 characters",
            "the status filter must be an HTTP status (100-599), not 0",
        ];
        for (args, expected) in refused.iter().zip(expected) {
            let error = requests_query(args, NOW).unwrap_err();
            assert!(error.starts_with(expected), "{error}");
        }
    }

    #[test]
    fn a_flag_value_that_does_not_fit_is_an_error_not_a_wrap() {
        assert_eq!(narrowed::<u16>("--status", 500), Ok(500));
        assert_eq!(
            narrowed::<u16>("--status", 70_000),
            Err("--status 70000 is out of range".to_string())
        );
        assert!(narrowed::<u32>("--limit", -1).is_err());
    }

    #[test]
    fn a_page_size_beside_follow_is_refused() {
        assert!(check_follow_flags(true, true)
            .unwrap_err()
            .contains("--limit sets the size of the one page"));
        assert!(check_follow_flags(true, false).is_ok());
        assert!(check_follow_flags(false, true).is_ok());
        assert!(check_follow_flags(false, false).is_ok());
    }

    fn report(rows: Vec<UsageRequestRow>, has_more: bool) -> UsageRequestsReport {
        UsageRequestsReport {
            since: "S".to_string(),
            until: "U".to_string(),
            as_of: "2026-09-25T08:34:18Z".to_string(),
            totals: UsageRequestsTotals {
                requests: 5,
                cost_micros: 3_000,
                ..UsageRequestsTotals::default()
            },
            rows,
            has_more,
        }
    }

    #[test]
    fn the_json_document_carries_every_field_and_says_whether_more_exist() {
        let mut full = row("a");
        full.response_request_id = Some("resp-a".to_string());
        full.provider = Some("self_hosted_sglang".to_string());
        full.error_code = Some("upstream_error".to_string());
        full.finish_reason = Some("stop".to_string());
        full.ts_end = Some("2026-09-25T03:41:09+00:00".to_string());
        full.noncached_prompt_tokens = 200;
        full.max_tokens_requested = Some(4096);
        full.max_tokens_granted = Some(2048);
        full.ttft_ms = Some(310);
        full.tpot_ms = Some(12);

        let document: serde_json::Value =
            serde_json::from_str(&requests_json(&report(vec![full, row("b")], true)))
                .expect("one JSON document");
        assert_eq!(document["since"], "S");
        assert_eq!(document["until"], "U");
        assert_eq!(document["row_count"], 2);
        assert_eq!(document["total_requests"], 5);
        assert_eq!(document["has_more"], true);
        assert!(document.get("requests").is_none(), "renamed to row_count");
        assert!(
            document.get("next_cursor").is_none(),
            "a cursor nobody can pass back is not part of the document"
        );

        let first = &document["rows"][0];
        assert_eq!(first["response_request_id"], "resp-a");
        assert_eq!(first["provider"], "self_hosted_sglang");
        assert_eq!(first["error_code"], "upstream_error");
        assert_eq!(first["finish_reason"], "stop");
        assert_eq!(first["ts_end"], "2026-09-25T03:41:09+00:00");
        assert_eq!(first["recorded_at"], "2026-09-25T03:41:08+00:00");
        assert_eq!(first["noncached_prompt_tokens"], 200);
        assert_eq!(first["max_tokens_requested"], 4096);
        assert_eq!(first["max_tokens_granted"], 2048);
        assert_eq!(first["ttft_ms"], 310);
        assert_eq!(first["tpot_ms"], 12);

        let second = &document["rows"][1];
        for absent in [
            "response_request_id",
            "provider",
            "error_code",
            "finish_reason",
            "ts_end",
            "max_tokens_requested",
            "max_tokens_granted",
            "ttft_ms",
            "tpot_ms",
        ] {
            assert!(
                second[absent].is_null(),
                "{absent} is {} instead of null",
                second[absent]
            );
        }

        let complete: serde_json::Value =
            serde_json::from_str(&requests_json(&report(vec![row("a")], false))).unwrap();
        assert_eq!(complete["has_more"], false);
    }

    #[test]
    fn an_empty_result_is_still_a_document() {
        let document: serde_json::Value =
            serde_json::from_str(&requests_json(&report(Vec::new(), false))).unwrap();
        assert_eq!(document["rows"].as_array().unwrap().len(), 0);
        assert_eq!(document["row_count"], 0);
    }

    #[test]
    fn request_only_flags_are_named_when_they_stand_alone() {
        let given = ["--follow", "--model", "--days", "--limit"];
        assert_eq!(
            misplaced_flags(|flag| given.contains(&flag)),
            vec!["--days", "--model", "--limit", "--follow"],
            "listed in help order, not argv order"
        );
        assert!(misplaced_flags(|_| false).is_empty());
        // `--json` and `--requests` are not on the list: the first is the
        // summary's own, the second is what would make the rest legal.
        assert!(!REQUESTS_ONLY_FLAGS.contains(&"--json"));
        assert!(!REQUESTS_ONLY_FLAGS.contains(&"--requests"));
        for removed in ["--since", "--until", "--cursor"] {
            assert!(!REQUESTS_ONLY_FLAGS.contains(&removed));
        }
    }

    #[test]
    fn an_error_is_named_by_its_code_then_by_its_status() {
        let mut errored = row("a");
        assert_eq!(request_error(&errored), None);
        errored.status_code = 503;
        assert_eq!(request_error(&errored), Some("5xx"));
        errored.error_code = Some("upstream_error".to_string());
        assert_eq!(request_error(&errored), Some("upstream_error"));
        // A 4xx with no code says nothing: the status column already does.
        let mut refused = row("b");
        refused.status_code = 429;
        assert_eq!(request_error(&refused), None);
    }

    #[test]
    fn the_table_draws_a_header_rows_and_an_error_line() {
        let mut slow = row("ledger-1");
        slow.response_request_id = Some("resp-1".to_string());
        slow.ttft_ms = Some(310);
        let mut failed = row("ledger-2");
        failed.status_code = 500;
        failed.error_code = Some("upstream_error".to_string());
        failed.finish_reason = Some("error".to_string());

        let lines = request_table_lines(&[slow, failed]);
        assert_eq!(lines.len(), 4, "header, row, row, error line: {lines:#?}");
        assert!(lines[0].starts_with("started") && lines[0].contains("request"));
        assert!(
            lines[1].starts_with("09-25 03:41:07 "),
            "the day and the time: {}",
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
            let lines = request_table_lines(&[short]);
            assert_eq!(lines.len(), 2, "{ts_start:?}");
            assert!(
                lines[1].starts_with(ts_start),
                "{ts_start:?} is shown as sent"
            );
        }
    }

    #[test]
    fn the_summary_counts_what_this_report_carries() {
        let summary = UsageRequestsReport {
            totals: UsageRequestsTotals {
                requests: 1_234,
                prompt_tokens: 5_000,
                cached_tokens: 4_000,
                noncached_prompt_tokens: 1_000,
                completion_tokens: 200,
                reasoning_tokens: 50,
                cost_micros: 2_500_000,
            },
            ..report(vec![row("a"); 100], true)
        };
        let lines = requests_summary_lines(&summary);
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
}

//! The per-request export behind `wally account usage --requests`
//! (`GET /v1/cli/usage/requests`, InferenceInfra #809): the query and its
//! contract bounds, the domain rows, and the walk across pages. The HTTP call
//! for one page is `ConsoleClient::fetch_usage_requests`.

use std::collections::HashSet;

use super::ConsoleSession;

/// The route refuses a window longer than this.
pub const USAGE_REQUESTS_MAX_WINDOW_DAYS: i64 = 31;
pub const USAGE_REQUESTS_DEFAULT_LIMIT: u32 = 100;
pub const USAGE_REQUESTS_MAX_LIMIT: u32 = 200;
/// A window is followed at most this many pages, so a wrong filter cannot turn
/// one command into an unbounded crawl of the ledger.
pub const USAGE_REQUESTS_MAX_PAGES: usize = 100;
/// The `status_code` filter's bounds in the contract.
pub const HTTP_STATUS_MIN: u16 = 100;
pub const HTTP_STATUS_MAX: u16 = 599;
/// `model` and `response_request_id` are both 1..=128 characters.
pub const USAGE_REQUESTS_ID_MAX_CHARS: usize = 128;
/// The contract's pattern for `model`. `model_id_is_valid` is the hand-written
/// check; `test_wally_contract` pins this text to the artifact and the check
/// to this text.
pub const MODEL_ID_PATTERN: &str = "^[A-Za-z0-9][A-Za-z0-9._:/-]*$";
pub const USAGE_REQUESTS_CURSOR_MAX_CHARS: usize = 2048;

const SECONDS_PER_DAY: i64 = 86_400;

/// Whether `value` is a model id the export's `model` filter accepts.
pub fn model_id_is_valid(value: &str) -> bool {
    let bytes = value.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= USAGE_REQUESTS_ID_MAX_CHARS
        && bytes[0].is_ascii_alphanumeric()
        && bytes[1..]
            .iter()
            .all(|&b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'/' | b'-'))
}

/// `seconds` since the epoch as RFC 3339 UTC (`2026-09-21T14:13:20Z`), the form
/// the route reads `since` and `until` in. A time at or before the epoch is an
/// error: there is no window to ask about, and a placeholder sent in its place
/// would be the console's 422 to explain. The value may be a window bound
/// rather than the clock itself, so the message names the value.
pub fn rfc3339_utc(seconds: i64) -> Result<String, String> {
    if seconds <= 0 {
        return Err(format!(
            "{seconds}s since 1970 is at or before the epoch, which is not a time the console can \
             be asked about"
        ));
    }
    Ok(crate::util::format_utc(seconds))
}

/// An RFC 3339 `date-time` with a timezone, as the instant it names: seconds
/// since the epoch and the nanoseconds past them, so two can be ordered and
/// subtracted. `None` for anything else, including a time with no offset and a
/// leap second, both of which the route refuses.
fn parse_rfc3339(value: &str) -> Option<(i64, u32)> {
    let b = value.as_bytes();
    let number = |part: &[u8]| -> Option<u32> {
        if part.is_empty() || part.len() > 9 || !part.iter().all(u8::is_ascii_digit) {
            return None;
        }
        Some(part.iter().fold(0, |n, d| n * 10 + u32::from(d - b'0')))
    };
    if b.len() < 20
        || b[4] != b'-'
        || b[7] != b'-'
        || !matches!(b[10], b'T' | b't')
        || b[13] != b':'
        || b[16] != b':'
    {
        return None;
    }
    let year = number(&b[0..4])?;
    let month = number(&b[5..7])?;
    let day = number(&b[8..10])?;
    let hour = number(&b[11..13])?;
    let minute = number(&b[14..16])?;
    let second = number(&b[17..19])?;

    let mut rest = &b[19..];
    let mut nanos = 0;
    if let Some((b'.', tail)) = rest.split_first() {
        let width = tail.iter().take_while(|d| d.is_ascii_digit()).count();
        nanos = number(&tail[..width])? * 10u32.pow(9 - width as u32);
        rest = &tail[width..];
    }
    let offset: i64 = match rest {
        [b'Z' | b'z'] => 0,
        [sign @ (b'+' | b'-'), h1, h2, b':', m1, m2] => {
            let hours = number(&[*h1, *h2])?;
            let minutes = number(&[*m1, *m2])?;
            if hours > 23 || minutes > 59 {
                return None;
            }
            let magnitude = i64::from(hours * 3600 + minutes * 60);
            if *sign == b'-' {
                -magnitude
            } else {
                magnitude
            }
        }
        _ => return None,
    };

    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let month_days = match month {
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => 31,
    };
    if year == 0
        || !(1..=12).contains(&month)
        || !(1..=month_days).contains(&day)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return None;
    }
    let days = crate::util::days_from_civil(year as i32, month, day);
    let local = days * SECONDS_PER_DAY + i64::from(hour * 3600 + minute * 60 + second);
    Some((local - offset, nanos))
}

/// How far `until` runs past `since`, in nanoseconds.
fn span_nanos(since: (i64, u32), until: (i64, u32)) -> i128 {
    let nanos =
        |(seconds, nanos): (i64, u32)| i128::from(seconds) * 1_000_000_000 + i128::from(nanos);
    nanos(until) - nanos(since)
}

/// One page's query. The window is the caller's to name: the route requires
/// `since` and `until` and refuses a span past 31 days, so nothing here picks a
/// default a reader could mistake for the server's own idea of "recent".
/// `cursor` carries the previous page's `next_cursor` unchanged; every other
/// field must repeat across pages or the console refuses the page rather than
/// reinterpret it. A filter that is `None` is not sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageRequestsQuery {
    /// RFC 3339 with a timezone, both ends required.
    pub since: String,
    pub until: String,
    pub model: Option<String>,
    pub status_code: Option<u16>,
    /// The `x-request-id` a response carried, the id a client logs.
    pub response_request_id: Option<String>,
    pub limit: u32,
    pub cursor: Option<String>,
}

impl Default for UsageRequestsQuery {
    fn default() -> Self {
        UsageRequestsQuery {
            since: String::new(),
            until: String::new(),
            model: None,
            status_code: None,
            response_request_id: None,
            limit: USAGE_REQUESTS_DEFAULT_LIMIT,
            cursor: None,
        }
    }
}

impl UsageRequestsQuery {
    /// The `[now - days, now)` window with no filters.
    pub fn last_days(days: i64, now: i64) -> Result<Self, String> {
        if !(1..=USAGE_REQUESTS_MAX_WINDOW_DAYS).contains(&days) {
            return Err(format!(
                "the window must be 1-{USAGE_REQUESTS_MAX_WINDOW_DAYS} days (the export refuses a longer one)"
            ));
        }
        Ok(UsageRequestsQuery {
            since: rfc3339_utc(now - days * SECONDS_PER_DAY)?,
            until: rfc3339_utc(now)?,
            ..UsageRequestsQuery::default()
        })
    }

    /// Refuses what the contract would refuse, so a filter the console cannot
    /// take is an error here and never a request with that filter left off.
    pub fn validate(&self) -> Result<(), String> {
        if self.since.is_empty() || self.until.is_empty() {
            return Err("a since and an until are both required".to_string());
        }
        let bound = |name: &str, value: &str| {
            parse_rfc3339(value).ok_or_else(|| {
                format!(
                    "{name} must be an RFC 3339 time with a timezone, for example \
                     2026-09-23T19:35:45Z"
                )
            })
        };
        let since = bound("since", &self.since)?;
        let until = bound("until", &self.until)?;
        let span = span_nanos(since, until);
        if span <= 0 {
            return Err("since must be earlier than until".to_string());
        }
        if span > i128::from(USAGE_REQUESTS_MAX_WINDOW_DAYS * SECONDS_PER_DAY) * 1_000_000_000 {
            return Err(format!(
                "the window may span at most {USAGE_REQUESTS_MAX_WINDOW_DAYS} days"
            ));
        }
        if !(1..=USAGE_REQUESTS_MAX_LIMIT).contains(&self.limit) {
            return Err(format!(
                "the page size must be 1-{USAGE_REQUESTS_MAX_LIMIT}, not {}",
                self.limit
            ));
        }
        if let Some(status) = self.status_code {
            if !(HTTP_STATUS_MIN..=HTTP_STATUS_MAX).contains(&status) {
                return Err(format!(
                    "the status filter must be an HTTP status ({HTTP_STATUS_MIN}-{HTTP_STATUS_MAX}), not {status}"
                ));
            }
        }
        if let Some(model) = &self.model {
            if !model_id_is_valid(model) {
                return Err(format!(
                    "the model filter must be a model id: 1-{USAGE_REQUESTS_ID_MAX_CHARS} characters, \
                     a letter or digit first, then letters, digits and . _ : / -"
                ));
            }
        }
        if let Some(id) = &self.response_request_id {
            if !(1..=USAGE_REQUESTS_ID_MAX_CHARS).contains(&id.chars().count()) {
                return Err(format!(
                    "the response request id filter must be 1-{USAGE_REQUESTS_ID_MAX_CHARS} characters"
                ));
            }
        }
        if let Some(cursor) = &self.cursor {
            if !(1..=USAGE_REQUESTS_CURSOR_MAX_CHARS).contains(&cursor.chars().count()) {
                return Err(format!(
                    "the page cursor must be 1-{USAGE_REQUESTS_CURSOR_MAX_CHARS} characters"
                ));
            }
        }
        Ok(())
    }
}

/// One settled request as the ledger recorded it. `response_request_id` is the
/// id the response's `x-request-id` header carried; `request_id` is the
/// ledger's own, unique id. No prompt or completion text is ever carried: the
/// route does not ship it and the CLI does not print it. Everything the
/// contract makes nullable stays `Option`, so an absent latency never reads as
/// an instant answer and an absent id never reads as an empty one.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UsageRequestRow {
    pub request_id: String,
    pub response_request_id: Option<String>,
    pub model: String,
    /// The contract's provider text, or whatever newer provider a console
    /// names that this build does not know yet.
    pub provider: Option<String>,
    pub status_code: i64,
    pub error_code: Option<String>,
    pub finish_reason: Option<String>,
    pub stream: bool,
    pub ts_start: String,
    pub ts_end: Option<String>,
    pub recorded_at: String,
    pub prompt_tokens: i64,
    pub cached_tokens: i64,
    pub noncached_prompt_tokens: i64,
    pub completion_tokens: i64,
    pub reasoning_tokens: i64,
    pub max_tokens_requested: Option<i64>,
    pub max_tokens_granted: Option<i64>,
    pub ttft_ms: Option<i64>,
    pub tpot_ms: Option<i64>,
    pub cost_micros: i64,
    pub pricing_version: String,
}

/// What the whole window totals, whichever page carried it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UsageRequestsTotals {
    pub requests: i64,
    pub prompt_tokens: i64,
    pub cached_tokens: i64,
    pub noncached_prompt_tokens: i64,
    pub completion_tokens: i64,
    pub reasoning_tokens: i64,
    pub cost_micros: i64,
}

/// One page: rows, what the window totals, and how to read the next page.
/// `as_of` is the snapshot the first page took; `next_cursor` is `None` on the
/// last page.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UsageRequestsPage {
    pub as_of: String,
    pub totals: UsageRequestsTotals,
    pub requests: Vec<UsageRequestRow>,
    pub next_cursor: Option<String>,
}

/// What one export read: the window, its totals, and the rows. `has_more` says
/// rows exist past them, which only a single page can leave behind: following
/// reads to the end or fails.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UsageRequestsReport {
    pub since: String,
    pub until: String,
    pub as_of: String,
    pub totals: UsageRequestsTotals,
    pub rows: Vec<UsageRequestRow>,
    pub has_more: bool,
}

/// Reads the export for `query`: its first page, or with `follow` every page.
///
/// Following asks for the route's largest page. The route has no rate limit of
/// its own and every page re-runs a totals aggregate over the whole window
/// (InferenceInfra `usage.request_history`), so the fewest pages is the
/// gentlest walk. Every page repeats the first page's window and filters, which
/// is what the console requires of a cursor. Nothing is returned until the
/// whole window is in hand, so a failure part-way is an error and never a
/// report that looks complete.
pub fn export_usage_requests(
    session: &mut ConsoleSession,
    mut query: UsageRequestsQuery,
    follow: bool,
) -> Result<UsageRequestsReport, String> {
    if follow {
        query.limit = USAGE_REQUESTS_MAX_LIMIT;
    }
    query.cursor = None;
    query.validate()?;

    let mut first =
        session.call(|client, url, token| client.fetch_usage_requests(url, token, &query))?;
    let mut rows = std::mem::take(&mut first.requests);
    let mut cursor = first.next_cursor.take();

    if follow {
        let mut seen = HashSet::new();
        let mut pages = 1;
        while let Some(next) = cursor {
            if pages >= USAGE_REQUESTS_MAX_PAGES {
                return Err(format!(
                    "the window has more than {USAGE_REQUESTS_MAX_PAGES} pages; narrow it with \
                     --days or a filter"
                ));
            }
            // A console that hands back a cursor it already gave out would
            // loop until the page cap, collecting the same rows each time.
            if !seen.insert(next.clone()) {
                return Err(
                    "the console returned a page cursor it had already given out; stopping"
                        .to_string(),
                );
            }
            query.cursor = Some(next);
            let page = session
                .call(|client, url, token| client.fetch_usage_requests(url, token, &query))?;
            rows.extend(page.requests);
            cursor = page.next_cursor;
            pages += 1;
        }
    }

    Ok(UsageRequestsReport {
        since: query.since,
        until: query.until,
        as_of: first.as_of,
        totals: first.totals,
        rows,
        has_more: cursor.is_some(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::{ConsoleClient, Credentials, HttpRequest, HttpResponse, Transport};
    use std::sync::{Arc, Mutex};

    const NOW: i64 = 1_790_000_000; // 2026-09-21T14:13:20Z

    #[test]
    fn the_wire_timestamp_is_rfc3339_utc() {
        assert_eq!(rfc3339_utc(NOW).unwrap(), "2026-09-21T14:13:20Z");
        assert_eq!(rfc3339_utc(1).unwrap(), "1970-01-01T00:00:01Z");
        for bad in [0, -1, i64::MIN] {
            assert!(rfc3339_utc(bad).is_err(), "{bad} became a timestamp");
        }
        // A window bound, not the clock: the clock here is fine.
        assert_eq!(
            UsageRequestsQuery::last_days(31, 60).unwrap_err(),
            "-2678340s since 1970 is at or before the epoch, which is not a time the console can \
             be asked about"
        );
    }

    #[test]
    fn the_window_runs_back_from_now() {
        let day = UsageRequestsQuery::last_days(1, NOW).unwrap();
        assert_eq!(day.since, "2026-09-20T14:13:20Z");
        assert_eq!(day.until, "2026-09-21T14:13:20Z");
        let week = UsageRequestsQuery::last_days(7, NOW).unwrap();
        assert_eq!(week.since, "2026-09-14T14:13:20Z");
        assert_eq!(week.until, "2026-09-21T14:13:20Z");
        assert_eq!(
            (
                week.model,
                week.status_code,
                week.response_request_id,
                week.cursor
            ),
            (None, None, None, None),
            "a window carries no filter of its own"
        );
    }

    #[test]
    fn a_window_of_thirty_one_days_is_the_longest_asked_for() {
        assert_eq!(
            UsageRequestsQuery::last_days(31, NOW).unwrap().since,
            "2026-08-21T14:13:20Z"
        );
        for days in [0, -1, 32, 365, i64::MAX, i64::MIN] {
            let error = UsageRequestsQuery::last_days(days, NOW).expect_err("out of range");
            assert_eq!(
                error, "the window must be 1-31 days (the export refuses a longer one)",
                "{days}"
            );
        }
        assert!(
            UsageRequestsQuery::last_days(1, 0).is_err(),
            "a clock at 1970"
        );
    }

    fn window() -> UsageRequestsQuery {
        UsageRequestsQuery::last_days(1, NOW).unwrap()
    }

    #[test]
    fn a_filter_the_contract_refuses_is_refused_not_dropped() {
        let cases: [(UsageRequestsQuery, &str); 9] = [
            (
                UsageRequestsQuery {
                    limit: 0,
                    ..window()
                },
                "the page size must be 1-200, not 0",
            ),
            (
                UsageRequestsQuery {
                    limit: 201,
                    ..window()
                },
                "the page size must be 1-200, not 201",
            ),
            (
                UsageRequestsQuery {
                    status_code: Some(99),
                    ..window()
                },
                "the status filter must be an HTTP status (100-599), not 99",
            ),
            (
                UsageRequestsQuery {
                    status_code: Some(600),
                    ..window()
                },
                "the status filter must be an HTTP status (100-599), not 600",
            ),
            (
                UsageRequestsQuery {
                    model: Some(String::new()),
                    ..window()
                },
                "the model filter must be a model id",
            ),
            (
                UsageRequestsQuery {
                    model: Some("a&limit=1 b".to_string()),
                    ..window()
                },
                "the model filter must be a model id",
            ),
            (
                UsageRequestsQuery {
                    model: Some("m".repeat(129)),
                    ..window()
                },
                "the model filter must be a model id",
            ),
            (
                UsageRequestsQuery {
                    response_request_id: Some(String::new()),
                    ..window()
                },
                "the response request id filter must be 1-128 characters",
            ),
            (
                UsageRequestsQuery {
                    response_request_id: Some("r".repeat(129)),
                    ..window()
                },
                "the response request id filter must be 1-128 characters",
            ),
        ];
        for (query, expected) in cases {
            let error = query.validate().expect_err("refused");
            assert!(error.starts_with(expected), "{query:?}: {error}");
        }

        let accepted = UsageRequestsQuery {
            model: Some("org/glm-5.3:fp8_v1".to_string()),
            status_code: Some(500),
            response_request_id: Some("r".repeat(128)),
            limit: 200,
            ..window()
        };
        assert_eq!(accepted.validate(), Ok(()));
    }

    #[test]
    fn a_timestamp_is_read_as_the_instant_it_names() {
        assert_eq!(parse_rfc3339("1970-01-01T00:00:00Z"), Some((0, 0)));
        assert_eq!(parse_rfc3339("2026-09-21T14:13:20Z"), Some((NOW, 0)));
        assert_eq!(parse_rfc3339("2026-09-21t14:13:20z"), Some((NOW, 0)));
        assert_eq!(parse_rfc3339("2026-09-21T19:43:20+05:30"), Some((NOW, 0)));
        assert_eq!(parse_rfc3339("2026-09-21T10:13:20-04:00"), Some((NOW, 0)));
        assert_eq!(
            parse_rfc3339("2026-09-21T14:13:20.5Z"),
            Some((NOW, 500_000_000))
        );
        assert_eq!(
            parse_rfc3339("2026-09-21T14:13:20.123456789Z"),
            Some((NOW, 123_456_789))
        );
        assert!(parse_rfc3339("2024-02-29T00:00:00Z").is_some());
        for bad in [
            "",
            "2026-09-21T14:13:20",
            "2026-09-21 14:13:20Z",
            "2026-09-21T14:13Z",
            "2026-9-21T14:13:20Z",
            "2026-09-21T14:13:20+0530",
            "2026-09-21T14:13:20+24:00",
            "2026-09-21T14:13:20.Z",
            "2026-09-21T14:13:20.1234567890Z",
            "2026-09-21T24:00:00Z",
            "2026-09-21T14:60:00Z",
            "2016-12-31T23:59:60Z",
            "2026-02-29T00:00:00Z",
            "2026-13-01T00:00:00Z",
            "2026-00-01T00:00:00Z",
            "0000-01-01T00:00:00Z",
            "2026-09-21T14:13:20Z ",
            "+026-09-21T14:13:20Z",
            "２026-09-21T14:13:20Z",
        ] {
            assert_eq!(parse_rfc3339(bad), None, "{bad:?} was read as a time");
        }
    }

    #[test]
    fn a_window_the_route_would_refuse_is_refused_here() {
        let between = |since: &str, until: &str| UsageRequestsQuery {
            since: since.to_string(),
            until: until.to_string(),
            ..UsageRequestsQuery::default()
        };
        let cases = [
            (
                between("2026-09-20T14:13:20", "2026-09-21T14:13:20Z"),
                "since must be an RFC 3339 time with a timezone, for example 2026-09-23T19:35:45Z",
            ),
            (
                between("2026-09-20T14:13:20Z", "yesterday"),
                "until must be an RFC 3339 time with a timezone, for example 2026-09-23T19:35:45Z",
            ),
            (
                between("2026-09-21T14:13:20Z", "2026-09-20T14:13:20Z"),
                "since must be earlier than until",
            ),
            (
                between("2026-09-21T14:13:20Z", "2026-09-21T19:43:20+05:30"),
                "since must be earlier than until",
            ),
            (
                between("2026-08-21T14:13:20Z", "2026-09-21T14:13:20.000000001Z"),
                "the window may span at most 31 days",
            ),
            (
                between("2026-08-21T14:13:20Z", "2026-09-21T14:13:21Z"),
                "the window may span at most 31 days",
            ),
        ];
        for (query, expected) in cases {
            assert_eq!(query.validate().unwrap_err(), expected, "{query:?}");
        }

        for (since, until) in [
            ("2026-08-21T14:13:20Z", "2026-09-21T14:13:20Z"),
            ("2026-09-21T14:13:20Z", "2026-09-21T14:13:20.000001Z"),
            ("2026-08-21T19:43:20+05:30", "2026-09-21T14:13:20Z"),
        ] {
            assert_eq!(between(since, until).validate(), Ok(()), "{since} {until}");
        }
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

    fn session_serving(
        serve: impl Fn(&str) -> Result<HttpResponse, String> + Send + Sync + 'static,
    ) -> (ConsoleSession, Arc<Mutex<Vec<String>>>) {
        let urls = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&urls);
        let transport: Transport = Arc::new(move |request: &HttpRequest| {
            seen.lock().unwrap().push(request.url.clone());
            serve(&request.url)
        });
        let credentials = Credentials {
            console_url: "https://console.example.test".to_string(),
            access_token: "a-token".to_string(),
            ..Credentials::default()
        };
        let session =
            ConsoleSession::resume(ConsoleClient::new(Some(transport)), credentials, NOW).unwrap();
        (session, urls)
    }

    fn ok(body: String) -> Result<HttpResponse, String> {
        Ok(HttpResponse {
            status: 200,
            body,
            ..HttpResponse::default()
        })
    }

    fn filtered() -> UsageRequestsQuery {
        UsageRequestsQuery {
            model: Some("glm-5.3-flash".to_string()),
            status_code: Some(200),
            response_request_id: Some("resp-9".to_string()),
            limit: 5,
            ..window()
        }
    }

    #[test]
    fn following_reads_every_page_with_the_same_window_and_filters() {
        let (mut session, urls) = session_serving(|url| {
            ok(if url.contains("cursor=c2") {
                page_body(1, None)
            } else if url.contains("cursor=c1") {
                page_body(2, Some("c2"))
            } else {
                page_body(2, Some("c1"))
            })
        });
        let report = export_usage_requests(&mut session, filtered(), true).unwrap();

        assert_eq!(report.rows.len(), 5, "2 + 2 + 1 rows across three pages");
        assert!(!report.has_more);
        assert_eq!(report.since, "2026-09-20T14:13:20Z");
        assert_eq!(report.totals.requests, 9);
        let urls = urls.lock().unwrap();
        assert_eq!(urls.len(), 3);
        for url in urls.iter() {
            for fragment in [
                "since=2026-09-20T14%3A13%3A20Z",
                "until=2026-09-21T14%3A13%3A20Z",
                "limit=200",
                "model=glm-5.3-flash",
                "status_code=200",
                "response_request_id=resp-9",
            ] {
                assert!(url.contains(fragment), "a page dropped {fragment}: {url}");
            }
        }
        assert!(!urls[0].contains("cursor="));
        assert!(urls[1].contains("cursor=c1") && urls[2].contains("cursor=c2"));
    }

    #[test]
    fn one_page_keeps_its_size_and_says_more_exist() {
        let (mut session, urls) = session_serving(|_| ok(page_body(2, Some("c1"))));
        let report = export_usage_requests(&mut session, filtered(), false).unwrap();
        assert_eq!(report.rows.len(), 2);
        assert!(report.has_more);
        let urls = urls.lock().unwrap();
        assert_eq!(urls.len(), 1);
        assert!(urls[0].contains("limit=5"), "{}", urls[0]);
    }

    #[test]
    fn following_a_single_page_asks_for_nothing_more() {
        let (mut session, urls) = session_serving(|_| ok(page_body(3, None)));
        let report = export_usage_requests(&mut session, window(), true).unwrap();
        assert_eq!(report.rows.len(), 3);
        assert!(!report.has_more);
        assert_eq!(urls.lock().unwrap().len(), 1);
    }

    #[test]
    fn a_failure_part_way_through_is_an_error_and_not_a_short_report() {
        let (mut session, _) = session_serving(|url| {
            if url.contains("cursor=") {
                Ok(HttpResponse {
                    status: 503,
                    ..HttpResponse::default()
                })
            } else {
                ok(page_body(2, Some("c1")))
            }
        });
        let error = export_usage_requests(&mut session, window(), true).unwrap_err();
        assert_eq!(
            error,
            "Wally Cloud is temporarily unavailable - try again shortly (HTTP 503)"
        );
    }

    #[test]
    fn a_cursor_that_never_advances_stops_instead_of_looping() {
        let (mut session, urls) = session_serving(|_| ok(page_body(1, Some("stuck"))));
        let error = export_usage_requests(&mut session, window(), true).unwrap_err();
        assert_eq!(
            error,
            "the console returned a page cursor it had already given out; stopping"
        );
        assert_eq!(urls.lock().unwrap().len(), 2, "one repeat, then stop");
    }

    #[test]
    fn a_cursor_that_cycles_back_is_caught_too() {
        let (mut session, urls) = session_serving(|url| {
            ok(if url.contains("cursor=b") {
                page_body(1, Some("a"))
            } else {
                page_body(1, Some("b"))
            })
        });
        let error = export_usage_requests(&mut session, window(), true).unwrap_err();
        assert!(error.contains("already given out"), "{error}");
        assert_eq!(urls.lock().unwrap().len(), 3, "first, a, b, then a again");
    }

    #[test]
    fn a_window_of_endless_pages_hits_the_cap() {
        let counter = Arc::new(Mutex::new(0));
        let served = Arc::clone(&counter);
        let (mut session, urls) = session_serving(move |_| {
            let mut n = served.lock().unwrap();
            *n += 1;
            ok(page_body(1, Some(&format!("c{n}"))))
        });
        let error = export_usage_requests(&mut session, window(), true).unwrap_err();
        assert_eq!(
            error,
            "the window has more than 100 pages; narrow it with --days or a filter"
        );
        assert_eq!(urls.lock().unwrap().len(), USAGE_REQUESTS_MAX_PAGES);
    }

    #[test]
    fn a_rejected_session_that_cannot_refresh_says_to_log_in() {
        let (mut session, urls) = session_serving(|_| {
            Ok(HttpResponse {
                status: 401,
                ..HttpResponse::default()
            })
        });
        let error = export_usage_requests(&mut session, window(), false).unwrap_err();
        assert_eq!(
            error,
            "the console rejected this session (the cloud session cannot be refreshed); run \
             `wally account login`"
        );
        assert_eq!(urls.lock().unwrap().len(), 1, "no retry without a refresh");
    }

    #[test]
    fn a_bad_query_never_reaches_the_network() {
        let (mut session, urls) = session_serving(|_| ok(page_body(1, None)));
        let query = UsageRequestsQuery {
            model: Some("bad model".to_string()),
            ..window()
        };
        assert!(export_usage_requests(&mut session, query, true).is_err());
        assert!(urls.lock().unwrap().is_empty());
    }
}

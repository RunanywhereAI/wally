//! `wally browser-use` end to end: the real binary, a real headless Chrome,
//! and a local site with a fake model gateway (tests/common/browser_site.rs).
//! The fake model always reaches for the most dangerous control, so these
//! prove the stops hold against a model that wants to pay.
//!
//! Skipped, with a printed reason, on a machine with no Chrome.

#[path = "common/browser_site.rs"]
mod browser_site;
mod common;

use std::sync::{Mutex, MutexGuard};

use browser_site::{chrome_binary, Chrome, Site};
use common::{run_wally_env, TempHome};

/// One Chrome at a time: each run drives a whole browser.
fn serial() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

struct Run {
    code: i32,
    stdout: String,
    stderr: String,
}

/// Runs browser-use against `site` from `path`, attached to `chrome`.
fn browse(site: &Site, chrome: &Chrome, path: &str, goal: &str, extra: &[&str]) -> Run {
    let home = TempHome::new();
    let start = format!("{}{path}", site.origin);
    let mut args = vec![
        "browser-use",
        "-m",
        "jev-latest",
        "--chrome",
        "attach",
        "--cdp-url",
        &chrome.origin,
        "--start-url",
        &start,
    ];
    args.extend_from_slice(extra);
    args.push(goal);
    let base = format!("{}/v1", site.origin);
    let mut env = vec![
        ("WALLY_BROWSER_API_BASE", base.clone()),
        ("WALLY_BROWSER_API_KEY", "test-key".to_string()),
    ];
    // run_wally_env starts from an empty environment, and Windows sockets do
    // not initialise without SystemRoot: every connection to the test Chrome
    // and the fake gateway would fail before it is made.
    if let Some(root) = std::env::var_os("SystemRoot") {
        env.push(("SystemRoot", root.to_string_lossy().into_owned()));
    }
    let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let (code, stdout, stderr) = run_wally_env(&home, &args, &env);
    Run {
        code,
        stdout,
        stderr,
    }
}

macro_rules! need_chrome {
    () => {
        match chrome_binary() {
            Some(binary) => binary,
            None => {
                eprintln!("skipped: no Chrome on this machine");
                return;
            }
        }
    };
}

#[test]
fn a_search_fills_its_fields_with_the_text_models_values() {
    let binary = need_chrome!();
    let _serial = serial();
    let site = Site::start();
    let chrome = Chrome::start(&binary);
    let run = browse(
        &site,
        &chrome,
        "/search",
        "Book a flight from Delhi to Bangalore",
        &[],
    );
    assert_eq!(
        site.beacons("searched"),
        ["Bangalore"],
        "{}\n{}",
        run.stdout,
        run.stderr
    );
    assert_eq!(
        site.beacons("passenger"),
        ["Asha|Rao|asha@example.com"],
        "{}",
        run.stdout
    );
    assert!(site.beacons("paid").is_empty());
    assert!(site.beacons("typed_card").is_empty());
}

#[test]
fn a_payment_page_blocks_before_any_card_field_is_typed() {
    let binary = need_chrome!();
    let _serial = serial();
    let site = Site::start();
    let chrome = Chrome::start(&binary);
    let run = browse(&site, &chrome, "/payment", "Pay for the flight", &[]);
    assert_eq!(run.code, 1, "{}\n{}", run.stdout, run.stderr);
    assert!(
        run.stdout.contains("Stopped. The page stays open."),
        "{}",
        run.stdout
    );
    assert!(site.beacons("typed_card").is_empty());
    assert!(site.beacons("typed_cvv").is_empty());
    assert!(site.beacons("paid").is_empty());
    assert!(
        !site.typed().iter().any(|value| value.contains("4242")),
        "{:?}",
        site.typed()
    );
}

#[test]
fn a_pay_button_with_no_card_fields_is_never_pressed() {
    let binary = need_chrome!();
    let _serial = serial();
    let site = Site::start();
    let chrome = Chrome::start(&binary);
    let run = browse(&site, &chrome, "/review-pay", "Finish the booking", &[]);
    assert_eq!(run.code, 1, "{}\n{}", run.stdout, run.stderr);
    assert!(site.beacons("paid").is_empty());
}

#[test]
fn a_confirm_and_pay_dialog_is_never_accepted() {
    let binary = need_chrome!();
    let _serial = serial();
    let site = Site::start();
    let chrome = Chrome::start(&binary);
    let run = browse(&site, &chrome, "/review", "Finish the booking", &[]);
    assert_ne!(run.code, 0, "{}\n{}", run.stdout, run.stderr);
    assert!(!site.beacons("confirm").contains(&"true".to_string()));
    assert!(site.beacons("paid").is_empty());
}

#[test]
fn an_opacity_zero_radio_is_seen_and_clicked() {
    let binary = need_chrome!();
    let _serial = serial();
    let site = Site::start();
    let chrome = Chrome::start(&binary);
    let run = browse(&site, &chrome, "/stops", "Show nonstop flights only", &[]);
    assert_eq!(
        site.beacons("stops"),
        ["nonstop"],
        "{}\n{}",
        run.stdout,
        run.stderr
    );
    assert_eq!(run.code, 0, "{}\n{}", run.stdout, run.stderr);
    assert!(run.stdout.contains("click  Nonstop only"), "{}", run.stdout);
}

#[test]
fn max_steps_stops_after_that_many_actions() {
    let binary = need_chrome!();
    let _serial = serial();
    let site = Site::start();
    let chrome = Chrome::start(&binary);
    let run = browse(
        &site,
        &chrome,
        "/search",
        "Book a flight to Bangalore",
        &["--max-steps", "1"],
    );
    assert_eq!(run.code, 1, "{}\n{}", run.stdout, run.stderr);
    assert!(run.stderr.contains("1-action budget"), "{}", run.stderr);
    assert!(site.beacons("searched").is_empty(), "the second action ran");
}

#[test]
fn attaching_leaves_the_persons_chrome_running() {
    let binary = need_chrome!();
    let _serial = serial();
    let site = Site::start();
    let mut chrome = Chrome::start(&binary);
    browse(&site, &chrome, "/stops", "Show nonstop flights only", &[]);
    assert!(chrome.running());
}

#[test]
fn a_gateway_503_ends_the_run_with_one_line_after_retrying() {
    let binary = need_chrome!();
    let _serial = serial();
    let site = Site::start();
    site.fail_decisions_with(503);
    let chrome = Chrome::start(&binary);
    let run = browse(&site, &chrome, "/search", "Book a flight", &[]);
    assert_eq!(run.code, 1);
    let lines: Vec<&str> = run
        .stderr
        .lines()
        .filter(|line| line.contains("browser-use stopped"))
        .collect();
    assert_eq!(lines.len(), 1, "{}", run.stderr);
    assert!(lines[0].contains("HTTP 503"), "{}", lines[0]);
    assert!(!run.stderr.contains("panicked"), "{}", run.stderr);
    assert_eq!(site.systemone_requests(), 3);
}

#[test]
fn without_a_start_url_it_is_a_usage_error() {
    let home = TempHome::new();
    let (code, _, stderr) = run_wally_env(
        &home,
        &["browser-use", "eve", "find a flight"],
        &[
            ("WALLY_BROWSER_API_BASE", "http://127.0.0.1:9/v1"),
            ("WALLY_BROWSER_API_KEY", "k"),
        ],
    );
    assert_eq!(code, 2);
    assert!(stderr.contains("--start-url is required"), "{stderr}");
}

#[test]
fn a_remote_cdp_url_is_a_usage_error() {
    let home = TempHome::new();
    let (code, _, stderr) = run_wally_env(
        &home,
        &[
            "browser-use",
            "--chrome",
            "attach",
            "--cdp-url",
            "http://example.com:9222",
            "--start-url",
            "https://example.com",
            "eve",
            "find a flight",
        ],
        &[],
    );
    assert_eq!(code, 2);
    assert!(stderr.contains("on this machine"), "{stderr}");
}

// Patterns. Each page builds a control the way many sites do; the fake model
// follows a fixed plan, and the page's beacons prove each step really ran.

use browser_site::Want;

fn want(op: &'static str, label: &'static str, beacon: &'static str) -> Want {
    Want { op, label, beacon }
}

/// Runs `plan` on `path`; returns the site to read beacons from.
fn pattern(path: &str, plan: &[Want], extra: &[&str]) -> Option<(Site, Run)> {
    let binary = chrome_binary()?;
    let site = Site::start();
    site.follow(plan);
    let chrome = Chrome::start(&binary);
    let run = browse(&site, &chrome, path, "Do the task on this page", extra);
    Some((site, run))
}

macro_rules! pattern {
    ($path:expr, $plan:expr) => {
        pattern!($path, $plan, &[])
    };
    ($path:expr, $plan:expr, $extra:expr) => {{
        let _serial = serial();
        match pattern($path, $plan, $extra) {
            Some(result) => result,
            None => {
                eprintln!("skipped: no Chrome on this machine");
                return;
            }
        }
    }};
}

fn done(run: &Run) {
    assert_eq!(run.code, 0, "{}\n{}", run.stdout, run.stderr);
    assert!(run.stdout.contains("Done."), "{}", run.stdout);
}

#[test]
fn pattern_a_skinned_checkbox_is_ticked() {
    let (site, run) = pattern!("/p/checkbox", &[want("CLICK", "Free cancellation", "free")]);
    done(&run);
    assert_eq!(site.beacons("free"), ["true"]);
    assert!(site.beacons("breakfast").is_empty());
}

#[test]
fn pattern_a_native_select_under_a_drawn_prompt_is_set() {
    let (site, run) = pattern!(
        "/p/select-skin",
        &[want("SELECT", "Price: Low to High", "sort")]
    );
    done(&run);
    assert_eq!(site.beacons("sort"), ["price-asc"]);
}

#[test]
fn pattern_a_custom_listbox_is_opened_then_chosen() {
    let (site, run) = pattern!(
        "/p/listbox",
        &[
            want("CLICK", "Cabin class", "opened"),
            want("CLICK", "Business", "cabin")
        ]
    );
    done(&run);
    assert_eq!(site.beacons("cabin"), ["business"]);
}

#[test]
fn pattern_an_autocomplete_suggestion_is_picked_after_typing() {
    let (site, run) = pattern!(
        "/p/autocomplete",
        &[
            want("TYPE_TEXT", "City", "typed"),
            want("CLICK", "Bengaluru, Karnataka", "city")
        ]
    );
    done(&run);
    assert_eq!(site.beacons("typed"), ["Bengaluru"]);
    assert_eq!(site.beacons("city"), ["blr"]);
}

#[test]
fn pattern_a_date_picker_day_is_chosen() {
    let (site, run) = pattern!(
        "/p/datepicker",
        &[
            want("CLICK", "Departure", "opened"),
            want("CLICK", "November 20", "date")
        ]
    );
    done(&run);
    assert_eq!(site.beacons("date"), ["2026-11-20"]);
}

#[test]
fn pattern_a_consent_modal_is_dismissed_first() {
    let (site, run) = pattern!(
        "/p/modal",
        &[
            want("CLICK", "Accept cookies", "consent"),
            want("CLICK", "Show results", "results")
        ]
    );
    done(&run);
    assert_eq!(site.beacons("results"), ["1"]);
}

#[test]
fn pattern_a_control_under_a_modal_is_never_clicked() {
    let (site, run) = pattern!(
        "/p/modal",
        &[want("CLICK", "Show results", "results")],
        &["--max-steps", "2"]
    );
    assert_ne!(run.code, 0, "{}\n{}", run.stdout, run.stderr);
    assert!(
        site.beacons("results").is_empty(),
        "a covered button was clicked"
    );
}

#[test]
fn pattern_a_control_below_the_fold_is_scrolled_to() {
    let (site, run) = pattern!("/p/below-fold", &[want("CLICK", "Load more", "more")]);
    done(&run);
    assert_eq!(site.beacons("more"), ["1"]);
    assert!(run.stdout.contains("scroll  Scroll down"), "{}", run.stdout);
}

#[test]
fn pattern_a_submit_enabled_by_typing_is_pressed() {
    let (site, run) = pattern!(
        "/p/disabled-submit",
        &[
            want("TYPE_TEXT", "Promo code", "typed"),
            want("CLICK", "Apply", "applied")
        ]
    );
    done(&run);
    assert_eq!(site.beacons("applied"), ["SAVE10"]);
}

#[test]
fn pattern_a_contenteditable_box_takes_text() {
    let (site, run) = pattern!(
        "/p/contenteditable",
        &[
            want("TYPE_TEXT", "Message", "typed"),
            want("CLICK", "Send", "sent")
        ]
    );
    done(&run);
    assert_eq!(site.beacons("sent"), ["Hello"]);
}

#[test]
fn pattern_a_button_inside_a_same_origin_iframe_is_clicked() {
    let (site, run) = pattern!("/p/iframe", &[want("CLICK", "Choose this room", "booked")]);
    done(&run);
    assert_eq!(site.beacons("booked"), ["deluxe"]);
}

#[test]
fn pattern_controls_inside_a_shadow_root_are_filled_and_clicked() {
    let (site, run) = pattern!(
        "/p/shadow",
        &[
            want("TYPE_TEXT", "Quantity", "qty"),
            want("CLICK", "Add to basket", "added")
        ]
    );
    done(&run);
    assert_eq!(site.beacons("added"), ["2"]);
}

#[test]
fn pattern_a_shadow_root_inside_an_iframe_is_reached() {
    let (site, run) = pattern!(
        "/p/shadow-in-iframe",
        &[
            want("TYPE_TEXT", "Quantity", "qty"),
            want("CLICK", "Add to basket", "added")
        ]
    );
    done(&run);
    assert_eq!(site.beacons("added"), ["2"]);
}

#[test]
fn pattern_plain_divs_that_act_as_buttons_are_clicked() {
    let (site, run) = pattern!(
        "/p/div-buttons",
        &[
            want("CLICK", "Free Wi-Fi", "filter"),
            want("CLICK", "Next page", "page")
        ]
    );
    done(&run);
    assert_eq!(site.beacons("filter"), ["wifi"]);
    assert_eq!(site.beacons("page"), ["2"]);
}

#[test]
fn pattern_a_card_field_in_a_cross_origin_iframe_is_out_of_reach() {
    let (site, run) = pattern!(
        "/p/cross-frame-card",
        &[want("TYPE_TEXT", "Card number", "typed_card")],
        &["--max-steps", "3"]
    );
    assert_ne!(run.code, 0, "{}\n{}", run.stdout, run.stderr);
    assert_eq!(
        site.beacons("frame"),
        ["loaded"],
        "the card iframe never loaded"
    );
    assert!(site.beacons("typed_card").is_empty());
    assert!(site.beacons("paid").is_empty());
}

#[test]
fn pattern_a_target_every_group_refused_is_found_in_the_final_round() {
    let binary = need_chrome!();
    let _serial = serial();
    let site = Site::start();
    site.follow(&[want("CLICK", "Save for later", "saved")]);
    site.shy_in_split_questions();
    let chrome = Chrome::start(&binary);
    let run = browse(&site, &chrome, "/p/many", "Save the product for later", &[]);
    done(&run);
    assert_eq!(site.beacons("saved"), ["1"]);
    assert!(site.beacons("other").is_empty());
}

// The review's probes: a model that wants to buy, on pages built to slip past
// the guards. None of them may move money or put text in a card field.

#[test]
fn probe_place_your_order_is_never_pressed() {
    let (site, run) = pattern!(
        "/probe/place-order",
        &[want("CLICK", "Place your order", "paid")]
    );
    assert_ne!(run.code, 0, "{}\n{}", run.stdout, run.stderr);
    assert!(site.beacons("paid").is_empty(), "{}", run.stdout);
}

#[test]
fn probe_buy_is_never_pressed_when_the_card_will_be_charged() {
    let (site, run) = pattern!("/probe/buy", &[want("CLICK", "Buy", "paid")]);
    assert_ne!(run.code, 0, "{}\n{}", run.stdout, run.stderr);
    assert!(site.beacons("paid").is_empty(), "{}", run.stdout);
}

#[test]
fn probe_book_now_is_never_pressed_against_a_saved_card() {
    let (site, run) = pattern!("/probe/book-now", &[want("CLICK", "Book now", "paid")]);
    assert_ne!(run.code, 0, "{}\n{}", run.stdout, run.stderr);
    assert!(site.beacons("paid").is_empty(), "{}", run.stdout);
}

#[test]
fn probe_text_never_follows_focus_into_a_card_field() {
    let (site, run) = pattern!(
        "/probe/focus-steal",
        &[want("TYPE_TEXT", "Full name", "typed_card")]
    );
    assert_ne!(run.code, 0, "{}\n{}", run.stdout, run.stderr);
    assert!(
        site.beacons("typed_card").is_empty(),
        "{:?}",
        site.beacons("typed_card")
    );
    assert!(
        run.stderr.contains("refused: focus moved"),
        "{}",
        run.stderr
    );
}

#[test]
fn probe_text_never_follows_focus_into_a_cross_origin_card_frame() {
    let (site, run) = pattern!(
        "/probe/focus-steal-frame",
        &[want("TYPE_TEXT", "Full name", "typed_card_frame")]
    );
    assert_eq!(
        site.beacons("frame"),
        ["loaded"],
        "the card iframe never loaded"
    );
    assert_ne!(run.code, 0, "{}\n{}", run.stdout, run.stderr);
    assert!(
        site.beacons("typed_card_frame").is_empty(),
        "{:?}",
        site.beacons("typed_card_frame")
    );
}

#[test]
fn pattern_a_link_that_opens_a_new_tab_is_followed() {
    let (site, run) = pattern!(
        "/p/new-tab",
        &[
            want("CLICK", "Open seat map", "tab"),
            want("CLICK", "Reserve seat 14C", "seat")
        ]
    );
    done(&run);
    assert_eq!(site.beacons("seat"), ["14C"]);
}

#[test]
fn pattern_a_noopener_link_that_opens_a_new_tab_is_followed() {
    let (site, run) = pattern!(
        "/p/new-tab-noopener",
        &[
            want("CLICK", "Open seat map", "tab"),
            want("CLICK", "Reserve seat 14C", "seat")
        ]
    );
    done(&run);
    assert_eq!(site.beacons("seat"), ["14C"]);
}

#[test]
fn pattern_a_tab_opened_by_script_is_followed() {
    let (site, run) = pattern!(
        "/p/window-open",
        &[
            want("CLICK", "Open seat map", "tab"),
            want("CLICK", "Reserve seat 14C", "seat")
        ]
    );
    done(&run);
    assert_eq!(site.beacons("seat"), ["14C"]);
}

#[test]
fn probe_a_keydown_handler_cannot_pull_text_into_a_card_field() {
    let (site, run) = pattern!(
        "/probe/keydown-steal",
        &[want("TYPE_TEXT", "Full name", "typed_card")],
        &["--max-steps", "4"]
    );
    assert!(
        site.beacons("typed_card").is_empty(),
        "{:?}\n{}\n{}",
        site.beacons("typed_card"),
        run.stdout,
        run.stderr
    );
}

#[test]
fn probe_a_keydown_handler_cannot_pull_text_into_a_cross_origin_card_frame() {
    let (site, run) = pattern!(
        "/probe/keydown-steal-frame",
        &[want("TYPE_TEXT", "Full name", "typed_card_frame")],
        &["--max-steps", "4"]
    );
    assert_eq!(
        site.beacons("frame"),
        ["loaded"],
        "the card iframe never loaded"
    );
    assert!(
        site.beacons("typed_card_frame").is_empty(),
        "{:?}\n{}\n{}",
        site.beacons("typed_card_frame"),
        run.stdout,
        run.stderr
    );
}

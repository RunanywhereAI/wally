//! Stops the model cannot overrule. A match means the action is not executed.

use super::page::{Action, ActionKind, Observation};
use super::words::{contains_any, contains_joined};

const PAY: &[&str] = &[
    "pay",
    "place order",
    "buy now",
    "purchase",
    "complete purchase",
    "complete booking",
    "confirm and pay",
    "confirm payment",
    "submit payment",
];
/// Text that says money is about to move: a checkout, review or payment page.
const PAGE_MONEY: &[&str] = &[
    "pay",
    "paying",
    "payment",
    "charge",
    "charged",
    "card",
    "card ending",
    "order total",
    "grand total",
    "amount due",
    "billing",
    "checkout",
    "visa",
    "mastercard",
    "amex",
    "upi",
    "wallet",
];
/// Words that commit an order, a booking, a subscription or a transfer. On a
/// page with money text, a control whose label says any of these is refused:
/// blocking a harmless "Book now" is the safe way to be wrong.
const COMMIT: &[&str] = &[
    "buy",
    "order now",
    "book now",
    "submit order",
    "complete order",
    "confirm order",
    "confirm booking",
    "checkout",
    "check out",
    "subscribe",
    "donate",
    "send",
    "transfer",
    "pay now",
    "reserve",
    "charge",
];
const SENSITIVE: &[&str] = &[
    "card",
    "cvv",
    "cvc",
    "otp",
    "upi",
    "password",
    "pin",
    "expiry",
    "expiration",
    "security code",
    "net banking",
    "valid thru",
    "valid through",
    "valid till",
];
const POSTAL: &[&str] = &["pincode", "postal"];
/// A bare label that commits whatever the page around it describes.
const SHORT_COMMIT: &[&str] = &[
    "yes", "confirm", "book", "reserve", "continue", "submit", "proceed", "order", "finish",
    "complete",
];
const SENSITIVE_AUTOCOMPLETE: &[&str] = &[
    "cc-number",
    "cc-csc",
    "cc-exp",
    "one-time-code",
    "current-password",
    "new-password",
];

/// A card, password, or OTP field. A postal PIN code is an address, not a credential.
pub fn sensitive_fill(action: &Action) -> bool {
    let label = action.label.as_str();
    let autocomplete = action.autocomplete.as_deref().unwrap_or("").to_lowercase();
    let tokens: Vec<&str> = autocomplete.split_whitespace().collect();
    if tokens.contains(&"postal-code")
        || contains_joined(label, "pin", "code")
        || contains_any(label, POSTAL)
    {
        return false;
    }
    if tokens
        .iter()
        .any(|token| token.starts_with("cc-") || SENSITIVE_AUTOCOMPLETE.contains(token))
    {
        return true;
    }
    contains_any(label, SENSITIVE)
}

/// The softer commit words judge button-length labels only. A product title
/// such as "boAt Airdopes 161 … Fast Charge …" or "USB data transfer cable"
/// names no action. Pay and place-order wording, and any submit button, are
/// judged at every length.
const MAX_CONTROL_WORDS: usize = 8;

/// A link, not a button: product titles and listings are links.
fn link_like(action: &Action) -> bool {
    action.role.as_deref() == Some("link") && action.submits != Some(true)
}

fn button_length(label: &str) -> bool {
    label.split_whitespace().count() <= MAX_CONTROL_WORDS
}

/// "Place order", "Place your order", "Place the order now".
fn places_an_order(label: &str) -> bool {
    contains_any(label, &["place"]) && contains_any(label, &["order", "orders"])
}

/// A label that commits something: a payment, an order, a booking, a send.
/// Judged without the page and at any length, for the rule that such a press
/// is never repeated and for pages where a card field is on screen.
pub fn commits(label: &str) -> bool {
    let bare = label.trim().trim_end_matches(['.', '!', '›', '>']).trim();
    contains_any(label, PAY)
        || places_an_order(label)
        || contains_any(label, COMMIT)
        || SHORT_COMMIT
            .iter()
            .any(|word| bare.eq_ignore_ascii_case(word))
}

/// Every stop for `action` on `page`. Besides the word rules, a page's own
/// structure says when money or a secret is about to be sent, in any
/// language: submitting a form that holds a card, one-time-code or password
/// field, or pressing a commit while such a field is on screen.
pub fn refuses_on(action: &Action, page: &Observation) -> bool {
    if refuses(action, &page.text) {
        return true;
    }
    // A date or month while a card field is on the page is its expiry.
    if action.kind == ActionKind::Fill && action.input_type.is_some() && page.payment_fields {
        return true;
    }
    if !matches!(action.kind, ActionKind::Click | ActionKind::Select) {
        return false;
    }
    (action.submits == Some(true) && action.form_has_card == Some(true))
        || (page.payment_fields && commits(&action.label))
}

/// A payment control, a sensitive field, or a commit sitting on money text.
pub fn refuses(action: &Action, page_text: &str) -> bool {
    let label = action.label.as_str();
    match action.kind {
        // Pay and place-order wording is judged at any length: "Pay ₹4,532 now
        // using your saved Visa card ending 4242" is still a pay button.
        ActionKind::Click | ActionKind::Select
            if contains_any(label, PAY) || places_an_order(label) =>
        {
            return true
        }
        // Softer commit words skip long labels on links, which are product
        // titles ("… Fast Charge …"). A real button is judged at any length.
        ActionKind::Click | ActionKind::Select if !button_length(label) && link_like(action) => {
            return false
        }
        ActionKind::Fill if sensitive_fill(action) => return true,
        // A date or month in a form that takes a card is its expiry.
        ActionKind::Fill if action.input_type.is_some() && action.form_has_card == Some(true) => {
            return true
        }
        ActionKind::Fill | ActionKind::Scroll | ActionKind::Wait => return false,
        _ => {}
    }
    if !contains_any(page_text, PAGE_MONEY) {
        return false;
    }
    let bare = label.trim().trim_end_matches(['.', '!', '›', '>']).trim();
    contains_any(label, COMMIT)
        || SHORT_COMMIT
            .iter()
            .any(|word| bare.eq_ignore_ascii_case(word))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn action(kind: ActionKind, label: &str, autocomplete: Option<&str>) -> Action {
        Action {
            kind,
            label: label.into(),
            autocomplete: autocomplete.map(str::to_string),
            ..Action::default()
        }
    }

    const PAYMENT_PAGE: &str = "Pay ₹4,532";

    #[test]
    fn a_pay_button_is_refused() {
        assert!(refuses(
            &action(ActionKind::Click, "Pay ₹4,532", None),
            PAYMENT_PAGE
        ));
    }

    #[test]
    fn a_card_number_field_is_refused_by_autocomplete() {
        let field = action(ActionKind::Fill, "Card number", Some("cc-number"));
        assert!(refuses(&field, PAYMENT_PAGE));
    }

    #[test]
    fn a_bare_yes_on_payment_text_is_refused() {
        let yes = action(ActionKind::Click, "Yes", None);
        assert!(refuses(&yes, "Pay from your saved card?"));
    }

    #[test]
    fn submit_payment_is_refused() {
        let submit = action(ActionKind::Click, "Submit payment", None);
        assert!(refuses(&submit, PAYMENT_PAGE));
    }

    #[test]
    fn a_bare_confirm_on_charge_text_is_refused() {
        let confirm = action(ActionKind::Click, "Confirm", None);
        assert!(refuses(&confirm, "Charge the saved card"));
    }

    #[test]
    fn an_expiry_month_field_is_refused() {
        let field = action(ActionKind::Fill, "Expiration month", Some("cc-exp-month"));
        assert!(refuses(&field, PAYMENT_PAGE));
    }

    #[test]
    fn a_billing_card_number_token_is_refused() {
        let field = action(ActionKind::Fill, "Number", Some("billing cc-number"));
        assert!(refuses(&field, PAYMENT_PAGE));
    }

    #[test]
    fn a_security_code_field_is_refused_by_label() {
        assert!(refuses(
            &action(ActionKind::Fill, "Security code", None),
            PAYMENT_PAGE
        ));
    }

    #[test]
    fn a_postal_pin_code_is_allowed() {
        let field = action(ActionKind::Fill, "PIN code", Some("postal-code"));
        assert!(!refuses(&field, PAYMENT_PAGE));
    }

    #[test]
    fn a_bare_pin_is_refused() {
        assert!(refuses(
            &action(ActionKind::Fill, "PIN", None),
            PAYMENT_PAGE
        ));
    }

    #[test]
    fn search_is_allowed_on_a_payment_page() {
        assert!(!refuses(
            &action(ActionKind::Click, "Search", None),
            PAYMENT_PAGE
        ));
    }

    #[test]
    fn place_your_order_is_refused_on_any_page() {
        let button = action(ActionKind::Click, "Place your order", None);
        assert!(refuses(
            &button,
            "Order total ₹4,532 · Paying with Visa ending 4242"
        ));
        assert!(refuses(&button, ""));
    }

    #[test]
    fn buy_is_refused_when_the_page_says_the_card_will_be_charged() {
        let buy = action(ActionKind::Click, "Buy", None);
        assert!(refuses(&buy, "Your card ending 4242 will be charged"));
    }

    #[test]
    fn book_now_is_refused_when_the_page_charges_a_saved_card() {
        let book = action(ActionKind::Click, "Book now", None);
        assert!(refuses(&book, "We will charge your saved card"));
    }

    #[test]
    fn continue_is_refused_on_a_review_step_with_an_order_total() {
        let next = action(ActionKind::Click, "Continue", None);
        assert!(refuses(&next, "Review your order. Order total ₹4,532"));
    }

    #[test]
    fn reserving_a_seat_is_refused_on_a_page_that_shows_payment() {
        let reserve = action(ActionKind::Click, "Reserve seat 14C", None);
        assert!(refuses(
            &reserve,
            "Seat fee ₹450 will be charged to your card"
        ));
        assert!(!refuses(&reserve, "Seat map"));
    }

    #[test]
    fn a_bare_finish_is_refused_on_a_checkout_overview() {
        let finish = action(ActionKind::Click, "Finish", None);
        assert!(refuses(
            &finish,
            "Checkout: Overview. Payment Information: SauceCard #31337. Total: $32.39"
        ));
    }

    fn page(text: &str, payment_fields: bool) -> Observation {
        Observation {
            url: String::new(),
            title: String::new(),
            text: text.into(),
            actions: Vec::new(),
            marker: serde_json::Value::Null,
            page_key: serde_json::Value::Null,
            guards: serde_json::Value::Null,
            fingerprint: String::new(),
            payment_fields,
        }
    }

    #[test]
    fn submitting_a_form_that_holds_card_fields_is_refused_in_any_language() {
        let mut order = action(ActionKind::Click, "Zahlungspflichtig bestellen", None);
        order.submits = Some(true);
        order.form_has_card = Some(true);
        assert!(refuses_on(&order, &page("Gesamtsumme 49,99 €", true)));
    }

    #[test]
    fn submitting_a_form_without_card_fields_is_allowed() {
        let mut search = action(ActionKind::Click, "Suchen", None);
        search.submits = Some(true);
        assert!(!refuses_on(&search, &page("Flüge suchen", false)));
    }

    #[test]
    fn a_commit_while_card_fields_are_on_screen_is_refused() {
        let book = action(ActionKind::Click, "Book now", None);
        assert!(refuses_on(&book, &page("Hotel Lisboa", true)));
        assert!(!refuses_on(&book, &page("Hotel Lisboa", false)));
    }

    #[test]
    fn commit_words_pass_where_no_money_is_shown() {
        assert!(!refuses(
            &action(ActionKind::Click, "Book now", None),
            "Hotels in Lisbon"
        ));
        assert!(!refuses(
            &action(ActionKind::Click, "Continue", None),
            "Choose your seat"
        ));
    }

    #[test]
    fn a_product_title_with_action_words_is_not_a_payment_control() {
        let money = "Bank offer: 10% off with HDFC credit card · ₹1,299";
        for title in [
            "boAt Airdopes 161, Adv 4Mic ENx, 50H Battery,Fast Charge,13mm Driver, Metallic Finish",
            "SanDisk USB 3.2 data transfer cable for phones and laptops, 1 m",
            "Wireless mouse with send to back buttons for gaming and office, black",
        ] {
            let mut link = action(ActionKind::Click, title, None);
            link.role = Some("link".into());
            assert!(!refuses(&link, money), "{title}");
        }
    }

    #[test]
    fn a_long_submit_in_a_card_form_is_still_refused() {
        let mut order = action(
            ActionKind::Click,
            "Confirm and complete your purchase of these items with the saved card now",
            None,
        );
        order.submits = Some(true);
        order.form_has_card = Some(true);
        assert!(refuses_on(&order, &page("", true)));
    }

    #[test]
    fn a_long_place_your_order_and_pay_button_is_refused() {
        let order = action(
            ActionKind::Click,
            "Place your order and pay with your saved card ending 4242",
            None,
        );
        assert!(refuses(&order, ""));
    }

    #[test]
    fn a_long_saved_card_pay_button_is_refused() {
        let pay = action(
            ActionKind::Click,
            "Pay ₹4,532 now using your saved Visa card ending 4242",
            None,
        );
        assert!(refuses(&pay, ""));
    }

    #[test]
    fn a_long_submit_button_with_commit_words_is_refused_on_a_money_page() {
        let mut order = action(
            ActionKind::Click,
            "Reserve this room for two nights with free cancellation and breakfast",
            None,
        );
        order.role = Some("button".into());
        assert!(refuses(&order, "Total ₹9,800 · Paying with Visa"));
    }

    #[test]
    fn long_commit_labels_still_count_for_the_no_repeat_rule() {
        assert!(commits(
            "Pay ₹4,532 now using your saved Visa card ending 4242"
        ));
        assert!(commits(
            "Place your order and pay with your saved card ending 4242"
        ));
    }

    #[test]
    fn an_expiry_date_in_a_card_form_is_refused() {
        let mut expiry = action(ActionKind::Fill, "Valid thru", None);
        expiry.input_type = Some("month".into());
        expiry.form_has_card = Some(true);
        assert!(refuses(&expiry, ""));
        let mut travel = action(ActionKind::Fill, "Travel month", None);
        travel.input_type = Some("month".into());
        assert!(!refuses(&travel, ""));
    }

    #[test]
    fn a_standalone_valid_thru_month_is_refused() {
        let mut expiry = action(ActionKind::Fill, "Valid thru", None);
        expiry.input_type = Some("month".into());
        assert!(refuses(&expiry, ""));
    }

    #[test]
    fn a_date_fill_while_card_fields_are_on_the_page_is_refused() {
        let mut date = action(ActionKind::Fill, "MM / YY", None);
        date.input_type = Some("month".into());
        assert!(refuses_on(&date, &page("", true)));
        assert!(!refuses_on(&date, &page("", false)));
    }

    #[test]
    fn add_to_cart_is_not_a_commit() {
        let add = action(ActionKind::Click, "Add to cart", None);
        assert!(!refuses(
            &add,
            "₹6,499 · Pay on delivery available · Visa EMI"
        ));
    }

    #[test]
    fn paypal_is_not_pay() {
        assert!(!refuses(
            &action(ActionKind::Click, "Paypal help", None),
            "Help centre"
        ));
    }

    /// The guard's refusals over a page captured from a real site. Inputs are
    /// fills; links and buttons are clicks; page text is left empty so only
    /// the action itself is judged.
    fn refused_on(capture: &str) -> Vec<String> {
        let page: serde_json::Value = serde_json::from_str(capture).unwrap();
        let mut refused = Vec::new();
        for element in page["elements"].as_array().unwrap() {
            let text = |key: &str| element[key].as_str().unwrap_or("").to_string();
            let kind = match (text("tag").as_str(), text("role").as_str()) {
                ("input" | "textarea", _)
                    if !matches!(
                        text("input_type").as_str(),
                        "button" | "submit" | "checkbox" | "radio"
                    ) =>
                {
                    ActionKind::Fill
                }
                ("a" | "button", _) | (_, "button" | "link") => ActionKind::Click,
                _ => continue,
            };
            let autocomplete = Some(text("autocomplete")).filter(|a| !a.is_empty());
            if refuses(&action(kind, &text("name"), autocomplete.as_deref()), "") {
                refused.push(text("name"));
            }
        }
        refused
    }

    #[test]
    fn on_a_real_stripe_checkout_every_card_field_and_pay_button_is_refused() {
        let refused = refused_on(include_str!(
            "../../tests/fixtures/browser/real/stripe-payments-demo.json"
        ));
        assert_eq!(
            refused,
            [
                "Credit or debit card expiry date",
                "Credit or debit card CVC/CVV",
                "Pay securely with Link",
                "Pay €35.95",
                "Credit or debit card number",
            ]
        );
    }

    #[test]
    fn on_real_pages_without_a_payment_step_nothing_is_refused() {
        for capture in [
            include_str!("../../tests/fixtures/browser/real/stripe-elements-examples.json"),
            include_str!("../../tests/fixtures/browser/real/stripe-checkout-preview.json"),
            include_str!("../../tests/fixtures/browser/real/braintree-hosted-fields.json"),
        ] {
            assert_eq!(refused_on(capture), Vec::<String>::new());
        }
    }
}

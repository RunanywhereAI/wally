//! Stops the model cannot overrule. A match means the action is not executed.

use super::page::{Action, ActionKind};
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
    "charge",
];
const PAGE_MONEY: &[&str] = &["pay", "payment", "charge", "card"];
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
];
const POSTAL: &[&str] = &["pincode", "postal"];
const SHORT_COMMIT: &[&str] = &["yes", "confirm", "book", "reserve"];
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

/// A payment control, a sensitive field, or a bare confirm sitting on payment text.
pub fn refuses(action: &Action, page_text: &str) -> bool {
    let label = action.label.as_str();
    match action.kind {
        ActionKind::Click | ActionKind::Select if contains_any(label, PAY) => return true,
        ActionKind::Fill if sensitive_fill(action) => return true,
        _ => {}
    }
    let bare = label.trim();
    SHORT_COMMIT
        .iter()
        .any(|word| bare.eq_ignore_ascii_case(word))
        && contains_any(page_text, PAGE_MONEY)
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

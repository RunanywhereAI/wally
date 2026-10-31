//! Word-bounded matching, the subset of Python `re` the guards and the goal
//! strings were written in. `\b` is Unicode-aware there: a word character is
//! a letter, a digit or `_`, in any script.

pub fn is_word(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_'
}

/// `\b` between `chars[at - 1]` and `chars[at]`.
pub fn boundary(chars: &[char], at: usize) -> bool {
    let before = at > 0 && is_word(chars[at - 1]);
    let after = at < chars.len() && is_word(chars[at]);
    before != after
}

/// `re.search(r"\b(phrase)\b", text, re.I)` for a literal ASCII phrase.
pub fn contains_phrase(text: &str, phrase: &str) -> bool {
    let hay: Vec<char> = text.to_lowercase().chars().collect();
    let needle: Vec<char> = phrase.to_lowercase().chars().collect();
    if needle.is_empty() || needle.len() > hay.len() {
        return false;
    }
    (0..=hay.len() - needle.len()).any(|start| {
        hay[start..start + needle.len()] == needle[..]
            && boundary(&hay, start)
            && boundary(&hay, start + needle.len())
    })
}

/// Any of the phrases, each word-bounded.
pub fn contains_any(text: &str, phrases: &[&str]) -> bool {
    phrases.iter().any(|phrase| contains_phrase(text, phrase))
}

/// `re.search(r"\bfirst\s*second\b", text, re.I)`.
pub fn contains_joined(text: &str, first: &str, second: &str) -> bool {
    let hay: Vec<char> = text.to_lowercase().chars().collect();
    let first: Vec<char> = first.chars().collect();
    let second: Vec<char> = second.chars().collect();
    (0..hay.len()).any(|start| {
        if !boundary(&hay, start) || !hay[start..].starts_with(&first) {
            return false;
        }
        let mut at = start + first.len();
        while at < hay.len() && hay[at].is_whitespace() {
            at += 1;
        }
        hay[at..].starts_with(&second) && boundary(&hay, at + second.len())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_phrase_matches_only_on_word_boundaries() {
        assert!(contains_phrase("Pay ₹4,532", "pay"));
        assert!(contains_phrase("Place Order now", "place order"));
        assert!(!contains_phrase("Paypal", "pay"));
        assert!(!contains_phrase("repay", "pay"));
        assert!(!contains_phrase("payé", "pay"));
    }

    #[test]
    fn joined_words_allow_any_whitespace_or_none_between() {
        assert!(contains_joined("PIN code", "pin", "code"));
        assert!(contains_joined("pincode", "pin", "code"));
        assert!(contains_joined("pin \t code", "pin", "code"));
        assert!(!contains_joined("pin codes", "pin", "code"));
        assert!(!contains_joined("spin code", "pin", "code"));
    }
}

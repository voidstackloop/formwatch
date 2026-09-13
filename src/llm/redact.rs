//! Best-effort PII redaction before page text leaves the machine.
//!
//! The LLM checks send text extracted from a form to a third-party
//! provider. Most of that text is labels and instructions, but a live page
//! can also surface an email address, a phone number, or an account
//! number. This module scrubs the obvious, high-signal patterns first. It
//! is a mitigation, not a guarantee — operators handling sensitive data
//! should keep the LLM checks off, or point them at a self-hosted model.

use std::sync::OnceLock;

use regex::Regex;

fn email_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)\b[A-Z0-9._%+\-]+@[A-Z0-9.\-]+\.[A-Z]{2,}\b").expect("email regex")
    })
}

fn phone_na_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    // North-American style: (555) 555-0123 / 555-555-0123 / +1 555 555 0123.
    RE.get_or_init(|| {
        Regex::new(r"\b(?:\+?1[-.\s]?)?\(?\d{3}\)?[-.\s]?\d{3}[-.\s]?\d{4}\b").expect("phone regex")
    })
}

fn phone_intl_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    // Explicit international prefix, to avoid matching plain dates.
    RE.get_or_init(|| Regex::new(r"\+\d[\d\s().\-]{7,}\d").expect("intl phone regex"))
}

fn long_number_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\b\d{7,}\b").expect("long number regex"))
}

/// Replaces emails, phone numbers, and long digit runs with typed
/// placeholders. Idempotent and safe to run on any UTF-8 text.
pub fn redact(text: &str) -> String {
    let step = email_re().replace_all(text, "[email]");
    let step = phone_intl_re().replace_all(&step, "[phone]");
    let step = phone_na_re().replace_all(&step, "[phone]");
    long_number_re().replace_all(&step, "[number]").into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_emails_phones_and_long_numbers() {
        let input =
            "Contact a.person@example.gov or 555-555-0123, ref 123456789, +44 20 7946 0958.";
        let out = redact(input);
        assert!(out.contains("[email]"), "{out}");
        assert!(out.contains("[phone]"), "{out}");
        assert!(out.contains("[number]"), "{out}");
        assert!(!out.contains("a.person@example.gov"), "{out}");
        assert!(!out.contains("555-555-0123"), "{out}");
        assert!(!out.contains("123456789"), "{out}");
    }

    #[test]
    fn leaves_ordinary_instructional_text_alone() {
        let input = "Upload a PDF copy of your most recent utility bill, dated within 60 days.";
        assert_eq!(redact(input), input);
    }

    #[test]
    fn does_not_treat_a_plain_date_as_a_phone_number() {
        assert_eq!(
            redact("Submitted on 2026-09-13."),
            "Submitted on 2026-09-13."
        );
    }

    #[test]
    fn is_idempotent() {
        let once = redact("email me at x@y.gov");
        assert_eq!(redact(&once), once);
    }
}

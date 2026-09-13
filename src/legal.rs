//! Authorized-use and legal notices.
//!
//! formwatch drives a real browser against public-service forms, and its
//! `--submit` mode performs a real POST with dummy data. Pointed at a
//! system the operator doesn't own or have permission to test, that is
//! indistinguishable from unauthorized automated access — which in many
//! jurisdictions is a criminal offence, and near-universally a breach of
//! the target site's terms of service.
//!
//! This module is the single source of truth for the notice shown to
//! users, the acknowledgement check that gates `--submit`, and the
//! environment variable that records that acknowledgement. The long-form
//! version lives in `docs/legal.md`; the text here is what a CLI user
//! actually sees.

/// Environment variable that records an operator's acknowledgement of
/// the authorized-use notice. Set it to any non-empty value (e.g. `1`)
/// to acknowledge without passing a flag — useful in CI and Docker,
/// where flags are awkward. Read by [`is_accepted`].
pub const ACCEPT_ENV: &str = "FORMWATCH_ACCEPT_TERMS";

/// The full authorized-use notice, printed by `formwatch legal` and
/// shown (once, abbreviated) the first time a user runs a `test` or
/// `monitor` command without a recorded acknowledgement.
pub const NOTICE: &str = "\
formwatch — authorized use notice

formwatch drives an automated browser against third-party websites and, with
--submit, sends a real HTTP POST containing dummy data. Running it against a
system you do not own or have explicit permission to test may be unlawful and
almost certainly violates that site's terms of service.

  * Only point formwatch at forms you own, operate, or have written
    authorization to test. \"It is a public website\" is not authorization.
  * Automated access, scanning, or probing without permission can violate
    computer-misuse and unauthorized-access laws (for example the U.S.
    Computer Fraud and Abuse Act, 18 U.S.C. 1030, the U.K. Computer Misuse
    Act 1990, and equivalents elsewhere), as well as civil trespass and
    contract claims.
  * Never use formwatch for denial-of-service, load testing, credential
    attacks, scraping, or any attempt to bypass authentication, CAPTCHAs,
    or rate limits.
  * Respect a site's robots.txt, terms of service, and rate limits. Use
    --delay-ms / --per-host-delay-ms when checking many forms on one host.
  * --submit sends data to a live system. Leave it off unless you are
    authorized to submit to that specific form.
  * Screenshots captured by a check can contain personal data visible on
    the page. Treat stored history and reports as potentially sensitive;
    disable capture with --no-screenshots where required, and comply with
    applicable privacy law (e.g. GDPR, CCPA).

You are solely responsible for how you use this tool. The authors provide it
as-is, without warranty, and do not authorize any use that would be unlawful.

Read the full notice at: docs/legal.md (or the project repository).";

/// A one-line reminder shown alongside results when the operator hasn't
/// recorded an acknowledgement.
pub const SHORT_REMINDER: &str =
    "Reminder: use formwatch only on forms you are authorized to test (see `formwatch legal`).";

/// The error message returned when `--submit` is used without an
/// acknowledgement.
pub const SUBMIT_DENIED: &str = "--submit sends a real POST to a live system, so it requires an explicit acknowledgement that you are authorized to test this form. Re-run with --accept-terms (or set FORMWATCH_ACCEPT_TERMS=1) if you own or have permission to test it.";

/// The full notice text.
pub fn notice() -> &'static str {
    NOTICE
}

/// Whether `FORMWATCH_ACCEPT_TERMS` is set to a non-empty value.
pub fn accepted_from_env() -> bool {
    std::env::var(ACCEPT_ENV)
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false)
}

/// Combines every way an operator can acknowledge the notice: the
/// `--accept-terms` flag, a `config.yml` `accept_terms: true`, or the
/// [`ACCEPT_ENV`] environment variable.
pub fn is_accepted(flag: bool, config: Option<bool>) -> bool {
    flag || config == Some(true) || accepted_from_env()
}

/// Guards a real submission. Returns `Ok(())` when the notice has been
/// acknowledged, or `Err(SUBMIT_DENIED)` (which already includes the
/// remedy) when it hasn't.
pub fn check_submit_authorized(accepted: bool) -> std::result::Result<(), &'static str> {
    if accepted { Ok(()) } else { Err(SUBMIT_DENIED) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepted_by_any_single_mechanism() {
        assert!(!is_accepted(false, None));
        assert!(is_accepted(true, None));
        assert!(is_accepted(false, Some(true)));
        assert!(!is_accepted(false, Some(false)));
    }

    #[test]
    fn submit_is_denied_until_acknowledged() {
        let err = check_submit_authorized(false).expect_err("should be denied");
        assert!(err.contains("--submit"), "got: {err}");
        assert!(err.contains("--accept-terms"), "got: {err}");
        assert!(check_submit_authorized(true).is_ok());
    }

    #[test]
    fn notice_mentions_the_key_risks() {
        // A quick tripwire so an accidental rewrite can't drop the
        // substance: the notice has to name authorization, `--submit`,
        // and the fact that misuse can be unlawful.
        for needle in ["authorized", "--submit", "unlawful", "terms of service"] {
            assert!(NOTICE.contains(needle), "notice is missing {needle:?}");
        }
    }
}

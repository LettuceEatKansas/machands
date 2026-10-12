//! The safety gate (spec section 5, minimal version). Pure functions over facts read from the page,
//! so every rule is unit-tested without a browser.
//!
//! Refused on any site: a click that would submit a form; a click on a button whose label
//! looks like send, submit, buy, pay, delete, accept, agree or install; typing into a password or
//! payment field; typing a newline (Enter submits); a CAPTCHA widget; a page that is not http(s). A
//! refusal tells the agent to ask the user in chat. Page text cannot lift it: the checks read the live
//! element, not what the page says.
//!
//! 2026-10-11: which of those the user can approve from a card is decided in one place,
//! `wh_core::approval::Kind::approvable`. A gate check that the user could approve returns
//! [`BrowserError::Approval`] unless the caller passes the key of an approval for exactly this
//! element (`approved`); the `check_*` functions without `_with` keep the old all-refuse behaviour.

use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;

use wh_core::approval::{Kind, Need, shown};

use crate::error::BrowserError;

/// Labels that mean "do something that is hard to undo or that commits the owner".
static RISKY_LABEL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(send|submit|buy|purchase|pay|checkout|check out|place (?:your )?order|order now|delete|remove account|accept|agree|i agree|install|confirm|donate|subscribe|post|publish|allow all|authorize|authorise|grant)\b",
    )
    .expect("RISKY_LABEL")
});

/// The cookie-banner choices to prefer over "accept".
static REJECT_LABEL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(reject|decline|deny|necessary only|essential only|only necessary|only essential|refuse|no thanks)\b")
        .expect("REJECT_LABEL")
});

static CAPTCHA_LABEL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(captcha|i'?m not a robot|verify you are (?:a )?human|are you human|turnstile|security check)")
        .expect("CAPTCHA_LABEL")
});

static SENSITIVE_FIELD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)(passw(?:or)?d|passwd|passcode|card.?num|cc-?num|cardnumber|cvv|cvc|csc\b|security.?code|expir|exp-?(?:month|year)|iban|routing.?num|account.?num|\bssn\b|social.?security|\bpin\b|one-?time|otp|2fa|verification.?code)",
    )
    .expect("SENSITIVE_FIELD")
});

/// What the page says about the element a ref points at (read live through `FACTS_JS`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Facts {
    /// Lowercase tag name.
    pub tag: String,
    /// `type` attribute of an input or button, lowercase.
    pub kind: String,
    /// `autocomplete` attribute.
    pub autocomplete: String,
    /// `name`, `id`, `aria-label`, `placeholder` joined by spaces, for the field check.
    pub identity: String,
    /// What the user would read on it: aria-label, text, value or title.
    pub label: String,
    /// Would activating it submit a form?
    pub submits: bool,
    /// Is it a text-entry target (input, textarea, contenteditable)?
    pub editable: bool,
    /// Is it inside a form?
    pub in_form: bool,
}

/// The function run on the element (`this`). One round trip, returns plain JSON.
pub const FACTS_JS: &str = r#"function(){
  const el=this;const tag=(el.tagName||'').toLowerCase();
  const t=(el.getAttribute&&el.getAttribute('type')||el.type||'').toLowerCase();
  const form=el.form||(el.closest&&el.closest('form'))||null;
  const submits=!!form&&((tag==='button'&&(t===''||t==='submit'))||(tag==='input'&&(t==='submit'||t==='image')));
  const g=n=>(el.getAttribute&&el.getAttribute(n))||'';
  return {tag:tag,kind:t,autocomplete:g('autocomplete'),
    identity:[g('name'),el.id||'',g('aria-label'),g('placeholder'),g('data-testid')].join(' '),
    label:(g('aria-label')||el.innerText||el.value||g('title')||'').toString().trim().slice(0,120),
    submits:submits,editable:tag==='input'||tag==='textarea'||!!el.isContentEditable,inForm:!!form};
}"#;

impl Facts {
    /// Read the object `FACTS_JS` returned.
    pub fn from_json(v: &Value) -> Self {
        let s = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_string();
        let b = |k: &str| v.get(k).and_then(Value::as_bool).unwrap_or(false);
        Self {
            tag: s("tag"),
            kind: s("kind"),
            autocomplete: s("autocomplete"),
            identity: s("identity"),
            label: s("label"),
            submits: b("submits"),
            editable: b("editable"),
            in_form: b("inForm"),
        }
    }
}

/// Does `label` look like a send/submit/buy/pay/delete/accept/agree/install button?
pub fn risky_label(label: &str) -> bool {
    RISKY_LABEL.is_match(label) && !REJECT_LABEL.is_match(label)
}

/// Is `label` a "reject non-essential" cookie choice?
pub fn is_reject_label(label: &str) -> bool {
    REJECT_LABEL.is_match(label)
}

/// Does the text look like part of a CAPTCHA?
pub fn is_captcha(text: &str) -> bool {
    CAPTCHA_LABEL.is_match(text)
}

/// Let a step through if the user approved exactly this (`approved` is the key of their approval), else stop
/// it: as a card they can answer, or as a plain refusal when the kind is not on the approvable list.
fn ask(approved: Option<&str>, need: Need) -> Result<(), BrowserError> {
    if !need.approvable() {
        return Err(BrowserError::Refused(need.reason));
    }
    if approved == Some(need.key().as_str()) {
        return Ok(());
    }
    Err(BrowserError::Approval(Box::new(need)))
}

/// May this click go ahead? `ax_name` is the accessible name the reader showed; `facts` come from
/// the live element. Every refusal is final here; see [`check_click_with`] for the card path.
pub fn check_click(ax_name: &str, facts: &Facts) -> Result<(), BrowserError> {
    check_click_with(ax_name, facts, None).map_err(BrowserError::hard)
}

/// [`check_click`] where a click the user could approve returns [`BrowserError::Approval`], unless
/// `approved` is the key of their approval for this very element.
pub fn check_click_with(
    ax_name: &str,
    facts: &Facts,
    approved: Option<&str>,
) -> Result<(), BrowserError> {
    let shown_name = if ax_name.is_empty() {
        &facts.label
    } else {
        ax_name
    };
    if is_captcha(ax_name) || is_captcha(&facts.label) {
        return ask(
            approved,
            Need::new(
                Kind::Captcha,
                shown_name,
                "a CAPTCHA",
                BrowserError::refused(
                    "this is a CAPTCHA or bot check. I never solve those; the user has to do it themselves",
                )
                .message(),
            ),
        );
    }
    let click = format!("Click \"{}\"", shown(shown_name));
    if facts.submits {
        return ask(
            approved,
            Need::new(
                Kind::BrowserFormSubmit,
                shown_name,
                "submits a form",
                BrowserError::refused(format!("clicking \"{shown_name}\" would submit a form"))
                    .message(),
            )
            .doing(click.clone(), click),
        );
    }
    for label in [ax_name, facts.label.as_str()] {
        if risky_label(label) {
            let hint = if label.to_lowercase().contains("accept")
                || label.to_lowercase().contains("agree")
            {
                " (for a cookie banner, pick a \"Reject\" or \"Necessary only\" button instead; for terms, the user must approve)"
            } else {
                ""
            };
            let click = format!("Click \"{}\"", shown(label));
            return ask(
                approved,
                Need::new(
                    Kind::BrowserRiskyButton,
                    label,
                    "could send, buy, delete or accept something",
                    BrowserError::refused(format!(
                        "the button \"{label}\" looks like a send, submit, buy, pay, delete, accept, agree or install action{hint}"
                    ))
                    .message(),
                )
                .doing(click.clone(), click),
            );
        }
    }
    Ok(())
}

/// May this text go into the element? Never into password or payment fields, never with a newline.
pub fn check_type(ax_name: &str, facts: &Facts, text: &str) -> Result<(), BrowserError> {
    check_type_with(ax_name, facts, text, None).map_err(BrowserError::hard)
}

/// [`check_type`] where a line break returns [`BrowserError::Approval`] unless `approved`. A password
/// or payment field is never approvable.
pub fn check_type_with(
    ax_name: &str,
    facts: &Facts,
    text: &str,
    approved: Option<&str>,
) -> Result<(), BrowserError> {
    if !facts.editable {
        return Err(BrowserError::invalid(format!(
            "\"{ax_name}\" is not a text field (it is a {})",
            if facts.tag.is_empty() {
                "?"
            } else {
                &facts.tag
            }
        )));
    }
    let field = format!("{ax_name} {} {}", facts.identity, facts.autocomplete);
    if facts.kind == "password"
        || facts.autocomplete.to_lowercase().starts_with("cc-")
        || facts.autocomplete.to_lowercase().contains("password")
        || facts.autocomplete == "one-time-code"
        || SENSITIVE_FIELD.is_match(&field)
    {
        return ask(
            approved,
            Need::new(
                Kind::PasswordOrPaymentField,
                ax_name,
                "a password or payment field",
                BrowserError::refused(
                    "this looks like a password or payment field, and I never enter those",
                )
                .message(),
            ),
        );
    }
    if text.contains('\n') || text.contains('\r') {
        return ask(
            approved,
            Need::new(
                Kind::BrowserEnterInText,
                ax_name,
                "the text has a line break, which presses Enter and can submit a form",
                BrowserError::refused(
                    "the text contains a line break, which presses Enter and can submit a form",
                )
                .message(),
            )
            .doing(
                format!("Type \"{}\" into \"{}\"", shown(text), shown(ax_name)),
                format!(
                    "Type {} characters into \"{}\"",
                    text.chars().count(),
                    shown(ax_name)
                ),
            ),
        );
    }
    Ok(())
}

/// May the browser go to `url`? Web pages only.
pub fn check_url(url: &str) -> Result<String, BrowserError> {
    // a non-web address was an ordinary "invalid" error before cards, and still is here
    check_url_with(url, None).map_err(|e| match e {
        BrowserError::Approval(need) => BrowserError::Invalid(need.reason),
        other => other,
    })
}

static HAS_SCHEME: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^[a-z][a-z0-9+.\-]*:").expect("HAS_SCHEME"));

/// [`check_url`] where a real non-web address (file:, chrome:, data: and the like) returns
/// [`BrowserError::Approval`] unless `approved`. `javascript:` is never approvable, and text that is
/// not an address at all stays an ordinary error.
pub fn check_url_with(url: &str, approved: Option<&str>) -> Result<String, BrowserError> {
    let u = url.trim();
    let lower = u.to_lowercase();
    if lower == "about:blank" {
        return Ok(u.to_string());
    }
    if lower.starts_with("http://") || lower.starts_with("https://") {
        return Ok(u.to_string());
    }
    if !lower.contains(':') && !lower.is_empty() && !lower.contains(' ') && lower.contains('.') {
        return Ok(format!("https://{u}"));
    }
    const ONLY_WEB: &str =
        "only http(s) URLs can be opened (no javascript:, file:, data: or chrome: pages)";
    if HAS_SCHEME.is_match(u)
        && !lower.starts_with("javascript:")
        && !u.contains(char::is_whitespace)
    {
        ask(
            approved,
            Need::new(Kind::BrowserNonWebUrl, u, "not a web page", ONLY_WEB).doing(
                format!("Open \"{}\"", shown(u)),
                format!("Open a {} address", lower.split(':').next().unwrap_or("")),
            ),
        )?;
        return Ok(u.to_string());
    }
    Err(BrowserError::invalid(ONLY_WEB))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(tag: &str, kind: &str) -> Facts {
        Facts {
            tag: tag.into(),
            kind: kind.into(),
            editable: tag == "input" || tag == "textarea",
            ..Facts::default()
        }
    }

    #[test]
    fn plain_buttons_and_links_pass() {
        for name in [
            "Next",
            "Show more",
            "Deals",
            "Reject all",
            "Necessary only",
            "Search",
        ] {
            assert!(
                check_click(name, &facts("button", "button")).is_ok(),
                "{name}"
            );
        }
    }

    #[test]
    fn risky_labels_are_refused() {
        for name in [
            "Send",
            "Submit",
            "Buy now",
            "Pay $20",
            "Delete account",
            "Accept all",
            "I agree",
            "Install",
            "Place order",
            "Checkout",
            "Send message",
            "Confirm purchase",
        ] {
            let e = check_click(name, &facts("button", "button")).unwrap_err();
            assert!(matches!(e, BrowserError::Refused(_)), "{name}");
            assert!(e.message().contains("Ask the user in chat"), "{name}");
        }
    }

    #[test]
    fn labels_match_whole_words_only() {
        assert!(check_click("Compost tips", &facts("a", "")).is_ok());
        assert!(check_click("Payment history", &facts("a", "")).is_ok());
        assert!(check_click("Postal codes", &facts("a", "")).is_ok());
    }

    #[test]
    fn a_form_submit_is_refused_whatever_its_label() {
        let mut f = facts("button", "submit");
        f.submits = true;
        let e = check_click("Go", &f).unwrap_err();
        assert!(e.message().contains("submit a form"), "{e}");
    }

    #[test]
    fn the_live_label_is_checked_too() {
        // the accessible name says "Continue", the element itself says "Buy"
        let mut f = facts("div", "");
        f.label = "Buy now".into();
        assert!(check_click("Continue", &f).is_err());
    }

    #[test]
    fn reject_wins_over_accept_in_one_label() {
        assert!(!risky_label("Reject all"));
        assert!(!risky_label("Decline and close"));
        assert!(is_reject_label("Necessary only"));
        assert!(!is_reject_label("Accept all"));
    }

    #[test]
    fn accept_refusal_points_at_the_reject_button() {
        let e = check_click("Accept all cookies", &facts("button", "button")).unwrap_err();
        assert!(e.message().contains("Reject"), "{e}");
    }

    #[test]
    fn captchas_are_refused() {
        for name in ["I'm not a robot", "reCAPTCHA", "Verify you are human"] {
            let e = check_click(name, &facts("div", "")).unwrap_err();
            assert!(e.message().contains("CAPTCHA"), "{name}");
        }
        assert!(is_captcha("Please complete the security check"));
    }

    #[test]
    fn password_and_payment_fields_are_refused() {
        let ok = facts("input", "text");
        assert!(check_type("Search", &ok, "shoes").is_ok());
        let pw = facts("input", "password");
        assert!(check_type("Password", &pw, "x").is_err());
        let mut cc = facts("input", "text");
        cc.autocomplete = "cc-number".into();
        assert!(check_type("Card", &cc, "4111").is_err());
        let mut by_name = facts("input", "text");
        by_name.identity = "cvv".into();
        assert!(check_type("Code", &by_name, "123").is_err());
        let mut by_label = facts("input", "text");
        by_label.identity = String::new();
        assert!(check_type("Card number", &by_label, "4111").is_err());
        let mut otp = facts("input", "text");
        otp.autocomplete = "one-time-code".into();
        assert!(check_type("Code", &otp, "1").is_err());
    }

    #[test]
    fn a_newline_is_refused_because_enter_submits() {
        let f = facts("input", "text");
        let e = check_type("Search", &f, "shoes\n").unwrap_err();
        assert!(matches!(e, BrowserError::Refused(_)));
    }

    #[test]
    fn typing_into_a_non_field_is_an_ordinary_error() {
        let f = facts("div", "");
        assert!(matches!(
            check_type("Banner", &f, "x").unwrap_err(),
            BrowserError::Invalid(_)
        ));
    }

    #[test]
    fn only_web_urls_open() {
        assert_eq!(check_url("https://a.test/x").unwrap(), "https://a.test/x");
        assert_eq!(check_url("example.com").unwrap(), "https://example.com");
        assert!(check_url("javascript:alert(1)").is_err());
        assert!(check_url("file:///etc/passwd").is_err());
        assert!(check_url("data:text/html,hi").is_err());
        assert!(check_url("chrome://settings").is_err());
        assert!(check_url("").is_err());
    }

    fn need_of(r: Result<(), BrowserError>) -> Box<Need> {
        match r.unwrap_err() {
            BrowserError::Approval(n) => n,
            other => panic!("expected a card, got {other:?}"),
        }
    }

    #[test]
    fn a_submit_or_risky_label_asks_and_the_matching_key_lets_it_through() {
        let mut f = facts("button", "submit");
        f.submits = true;
        let need = need_of(check_click_with("Search", &f, None));
        assert!(need.approvable());
        assert!(check_click_with("Search", &f, Some(&need.key())).is_ok());
        // a key for some other target does not
        assert!(matches!(
            check_click_with("Search", &f, Some("form_submit\nOther")),
            Err(BrowserError::Approval(_))
        ));
        let risky = need_of(check_click_with(
            "Delete account",
            &facts("button", ""),
            None,
        ));
        assert!(risky.approvable());
    }

    #[test]
    fn a_line_break_and_a_non_web_url_ask() {
        let f = facts("input", "text");
        let need = need_of(check_type_with("Search", &f, "shoes\n", None));
        assert!(check_type_with("Search", &f, "shoes\n", Some(&need.key())).is_ok());
        let need = match check_url_with("mailto:a@b.test", None) {
            Err(BrowserError::Approval(n)) => n,
            other => panic!("expected a card, got {other:?}"),
        };
        assert!(check_url_with("mailto:a@b.test", Some(&need.key())).is_ok());
        // script URLs are not a card, just invalid
        assert!(matches!(
            check_url_with("javascript:alert(1)", None),
            Err(BrowserError::Invalid(_))
        ));
    }

    #[test]
    fn passwords_payments_and_captchas_never_become_a_card() {
        let pw = facts("input", "password");
        assert!(matches!(
            check_type_with("Password", &pw, "x", None),
            Err(BrowserError::Refused(_))
        ));
        // even with a key that would match a card
        let mut cc = facts("input", "text");
        cc.autocomplete = "cc-number".into();
        assert!(matches!(
            check_type_with("Card", &cc, "4111\n", Some("anything")),
            Err(BrowserError::Refused(_))
        ));
        assert!(matches!(
            check_click_with("I'm not a robot", &facts("div", ""), Some("anything")),
            Err(BrowserError::Refused(_))
        ));
    }

    #[test]
    fn facts_parse_from_the_page_object() {
        let v: Value = serde_json::json!({"tag":"button","kind":"submit","submits":true,"inForm":true,"label":"Go"});
        let f = Facts::from_json(&v);
        assert!(f.submits && f.in_form);
        assert_eq!(f.label, "Go");
    }
}

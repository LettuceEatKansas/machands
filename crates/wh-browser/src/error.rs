//! The ways a browser step can end badly. The kind decides how the `computer` tool words it.

use std::fmt;

use wh_core::approval::Need;

/// Why a step did not produce a result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrowserError {
    /// The caller asked for something malformed (missing ref, bad URL, unknown action).
    Invalid(String),
    /// The safety gate refused. The message tells the agent to ask the owner in chat.
    Refused(String),
    /// Brave is not reachable in debug mode, or the browser answered with an error.
    Failed(String),
    /// The gate stopped the step, but the user could say yes to it from an approval card. Carries the
    /// whole refusal text ([`Need::reason`]) for the caller that cannot ask.
    Approval(Box<Need>),
}

impl BrowserError {
    /// A malformed request.
    pub fn invalid(msg: impl Into<String>) -> Self {
        Self::Invalid(msg.into())
    }

    /// A failure talking to the browser.
    pub fn failed(msg: impl Into<String>) -> Self {
        Self::Failed(msg.into())
    }

    /// A gate refusal. The text always ends with the same instruction, so no refusal can read as
    /// "try another way".
    pub fn refused(why: impl fmt::Display) -> Self {
        Self::Refused(format!(
            "REFUSED by the browser safety gate: {why}. Ask the user in chat and wait for a clear yes. \
             Do not retry or work around this."
        ))
    }

    /// The message.
    pub fn message(&self) -> &str {
        match self {
            Self::Invalid(m) | Self::Refused(m) | Self::Failed(m) => m,
            Self::Approval(need) => &need.reason,
        }
    }

    /// A refusal nobody can approve: a step that needed a card becomes the plain refusal.
    pub fn hard(self) -> Self {
        match self {
            Self::Approval(need) => Self::Refused(need.reason),
            other => other,
        }
    }
}

impl fmt::Display for BrowserError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message())
    }
}

impl std::error::Error for BrowserError {}

/// Shorthand.
pub type Result<T> = std::result::Result<T, BrowserError>;

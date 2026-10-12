//! The seam between the `computer` tool and the browser module. wh-core stays free of network and
//! async code: the `winhands` binary implements [`BrowserHook`] over the `wh-browser` crate and hands it
//! to [`crate::orchestrate::Computer::set_browser`]. Until it does, `browser_*` actions answer that the
//! module is not available.

use serde_json::{Map, Value};

use crate::approval::Need;

/// Does `action` belong to the browser module?
pub fn is_browser_action(action: &str) -> bool {
    action.starts_with("browser_")
}

/// What one browser step returns: text, and base64 JPEGs for a screenshot.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BrowserOut {
    /// The result text.
    pub text: String,
    /// Base64 JPEG pictures, only when the step asked for one.
    pub images: Vec<String>,
}

/// Why a browser step did not run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrowserFail {
    /// A malformed request.
    Invalid(String),
    /// The browser safety gate refused.
    Refused(String),
    /// Brave is not reachable or answered with an error.
    Failed(String),
    /// The gate stopped the step, but the user could say yes from an approval card.
    Approval(Box<Need>),
}

impl BrowserFail {
    /// The message.
    pub fn message(&self) -> &str {
        match self {
            Self::Invalid(m) | Self::Refused(m) | Self::Failed(m) => m,
            Self::Approval(need) => &need.reason,
        }
    }
}

/// Runs one `browser_*` step.
pub trait BrowserHook: Send + Sync {
    /// Run `step` (an object with `action` and its arguments). `approved` is the key of an approval
    /// the user gave for this very step (see [`crate::approval::Need::key`]): the hook lets through the one
    /// gate refusal whose key matches it, and asks again for anything else.
    fn run(
        &self,
        step: &Map<String, Value>,
        approved: Option<&str>,
    ) -> Result<BrowserOut, BrowserFail>;
}

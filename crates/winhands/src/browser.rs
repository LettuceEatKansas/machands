//! The `browser_*` actions of the `computer` tool: [`wh_browser::Browser`] behind the core's
//! [`BrowserHook`]. One session per daemon; it connects to Brave on the first browser call and costs
//! nothing before that.

use std::sync::Arc;

use serde_json::{Map, Value};
use wh_browser::{Browser, BrowserError};
use wh_core::browser::{BrowserFail, BrowserHook, BrowserOut};

struct Hook(Browser);

impl BrowserHook for Hook {
    fn run(
        &self,
        step: &Map<String, Value>,
        approved: Option<&str>,
    ) -> Result<BrowserOut, BrowserFail> {
        match self.0.run_approved(step, approved) {
            Ok(out) => Ok(BrowserOut {
                text: out.text,
                images: out.images,
            }),
            Err(BrowserError::Invalid(m)) => Err(BrowserFail::Invalid(m)),
            Err(BrowserError::Refused(m)) => Err(BrowserFail::Refused(m)),
            Err(BrowserError::Failed(m)) => Err(BrowserFail::Failed(m)),
            Err(BrowserError::Approval(need)) => Err(BrowserFail::Approval(need)),
        }
    }
}

/// The hook the daemon installs on its [`wh_core::orchestrate::Computer`].
pub fn hook() -> Arc<dyn BrowserHook> {
    Arc::new(Hook(Browser::new()))
}

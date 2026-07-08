//! The browser seam (M2).
//!
//! Core stays browser-free and deterministic: it owns only the `BrowserHandle`
//! *trait* plus test doubles. Real implementations live outside core
//! (`pacewright-browser` drives the `chrome-agent` CLI today; a native impl over
//! a forked `chrome_agent` library can replace it behind this same trait without
//! touching a single adapter — see `docs/specs/2026-07-07-chrome-agent-fork-lib.md`).
//!
//! Method names/shapes deliberately mirror the fork's planned `Page` API so the
//! swap is mechanical.

use crate::model::AdapterError;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::sync::Mutex;

/// What a navigation landed on. Adapters check this to detect auth walls
/// (LinkedIn bounces unauthenticated sessions to `/authwall` or `/login`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NavInfo {
    pub url: String,
    pub title: String,
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum BrowserError {
    /// No browser could be reached at all (binary missing, daemon dead).
    #[error("browser unavailable: {0}")]
    Unavailable(String),
    /// Navigation failed or timed out — usually worth another attempt.
    #[error("navigation failed: {0}")]
    Navigation(String),
    /// A JS evaluation failed or returned an unusable value.
    #[error("eval failed: {0}")]
    Eval(String),
    /// Transport/process trouble talking to the browser — usually transient.
    #[error("browser io: {0}")]
    Io(String),
}

/// Browser trouble folded into the adapter error contract the runner enforces.
/// Transient classes (navigation, io) become `Retryable` so the runner backs off
/// and requeues; a missing browser or a bad script is `Terminal` — retrying an
/// absent binary or a broken selector just burns attempts.
impl From<BrowserError> for AdapterError {
    fn from(e: BrowserError) -> Self {
        match e {
            BrowserError::Navigation(_) | BrowserError::Io(_) => AdapterError::Retryable(e.to_string()),
            BrowserError::Unavailable(_) | BrowserError::Eval(_) => AdapterError::Terminal(e.to_string()),
        }
    }
}

/// A live browser page an adapter can drive. Implementations are shared across
/// tasks (the daemon owns one long-lived session), hence `Send + Sync` and `&self`.
#[async_trait]
pub trait BrowserHandle: Send + Sync {
    /// Navigate and settle; returns where we actually landed.
    async fn goto(&self, url: &str) -> Result<NavInfo, BrowserError>;
    /// Evaluate a JS expression in the page, returning its JSON value.
    async fn eval(&self, js: &str) -> Result<serde_json::Value, BrowserError>;
    /// Capture the viewport as PNG bytes.
    async fn screenshot(&self) -> Result<Vec<u8>, BrowserError>;
}

/// The default handle: every call fails `Terminal`. Wired in wherever a browser
/// is structurally required but none is configured, so a browser-using adapter
/// fails loudly with a clear message instead of the daemon refusing to boot for
/// operators who only run browser-free adapters.
pub struct NullBrowser;

#[async_trait]
impl BrowserHandle for NullBrowser {
    async fn goto(&self, _url: &str) -> Result<NavInfo, BrowserError> {
        Err(BrowserError::Unavailable("no browser configured for this daemon".into()))
    }
    async fn eval(&self, _js: &str) -> Result<serde_json::Value, BrowserError> {
        Err(BrowserError::Unavailable("no browser configured for this daemon".into()))
    }
    async fn screenshot(&self) -> Result<Vec<u8>, BrowserError> {
        Err(BrowserError::Unavailable("no browser configured for this daemon".into()))
    }
}

/// A scriptable in-memory browser for deterministic tests: no process, no network.
/// `goto` returns the queued `NavInfo` (or echoes the URL), `eval` returns the
/// value registered for an exact JS string, and every call is recorded.
#[derive(Default)]
pub struct FakeBrowser {
    nav: Mutex<Option<NavInfo>>,
    evals: Mutex<Vec<(String, serde_json::Value)>>,
    pub calls: Mutex<Vec<String>>,
    fail_goto: Mutex<Option<BrowserError>>,
}

impl FakeBrowser {
    pub fn new() -> Self { Self::default() }

    /// Make the next `goto` land here (e.g. an `/authwall` URL).
    pub fn with_nav(self, url: &str, title: &str) -> Self {
        *self.nav.lock().unwrap() = Some(NavInfo { url: url.into(), title: title.into() });
        self
    }

    /// Register the value returned when `eval` is called with exactly `js`.
    pub fn with_eval(self, js: &str, value: serde_json::Value) -> Self {
        self.evals.lock().unwrap().push((js.to_string(), value));
        self
    }

    /// Make `goto` fail with the given error.
    pub fn failing_goto(self, e: BrowserError) -> Self {
        *self.fail_goto.lock().unwrap() = Some(e);
        self
    }

    pub fn calls(&self) -> Vec<String> { self.calls.lock().unwrap().clone() }
}

#[async_trait]
impl BrowserHandle for FakeBrowser {
    async fn goto(&self, url: &str) -> Result<NavInfo, BrowserError> {
        self.calls.lock().unwrap().push(format!("goto:{url}"));
        if let Some(e) = self.fail_goto.lock().unwrap().clone() { return Err(e); }
        Ok(self.nav.lock().unwrap().clone().unwrap_or(NavInfo { url: url.to_string(), title: String::new() }))
    }
    async fn eval(&self, js: &str) -> Result<serde_json::Value, BrowserError> {
        self.calls.lock().unwrap().push(format!("eval:{js}"));
        self.evals
            .lock()
            .unwrap()
            .iter()
            .find(|(k, _)| k == js)
            .map(|(_, v)| v.clone())
            .ok_or_else(|| BrowserError::Eval(format!("no scripted result for: {js}")))
    }
    async fn screenshot(&self) -> Result<Vec<u8>, BrowserError> {
        self.calls.lock().unwrap().push("screenshot".into());
        // 8-byte PNG magic, enough for callers that sniff the header.
        Ok(vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn null_browser_fails_terminal() {
        let b = NullBrowser;
        let err = b.goto("https://x.test").await.unwrap_err();
        assert!(matches!(AdapterError::from(err), AdapterError::Terminal(_)));
    }

    #[tokio::test]
    async fn fake_browser_scripts_nav_and_eval_and_records_calls() {
        let b = FakeBrowser::new()
            .with_nav("https://landed.test/authwall", "Sign In")
            .with_eval("document.title", serde_json::json!("Sign In"));
        let nav = b.goto("https://x.test/in/foo").await.unwrap();
        assert_eq!(nav.url, "https://landed.test/authwall");
        assert_eq!(b.eval("document.title").await.unwrap(), serde_json::json!("Sign In"));
        assert_eq!(b.calls(), vec!["goto:https://x.test/in/foo", "eval:document.title"]);
    }

    #[tokio::test]
    async fn fake_browser_unscripted_eval_is_an_error() {
        let b = FakeBrowser::new();
        assert!(b.eval("nope()").await.is_err());
    }

    #[test]
    fn transient_browser_errors_are_retryable_permanent_are_terminal() {
        assert!(matches!(AdapterError::from(BrowserError::Navigation("t".into())), AdapterError::Retryable(_)));
        assert!(matches!(AdapterError::from(BrowserError::Io("t".into())), AdapterError::Retryable(_)));
        assert!(matches!(AdapterError::from(BrowserError::Unavailable("t".into())), AdapterError::Terminal(_)));
        assert!(matches!(AdapterError::from(BrowserError::Eval("t".into())), AdapterError::Terminal(_)));
    }
}

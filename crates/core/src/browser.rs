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

/// Where the always-on Chrome listens for CDP. pacewright **attaches** to that Chrome (started by
/// launchd with `--remote-debugging-port`, never by chrome-agent) and separates sites by named
/// *tabs* rather than by browser profiles.
///
/// Launching is what breaks auth: a CDP-launched browser is a bot signal, so Acme walls the
/// profile and revokes `li_at`, and Google refuses sign-in outright. See
/// `docs/plans/2026-07-16-single-chrome-attach.md`.
///
/// Lives in core (which stays browser-free) purely as a shared constant: both callers of
/// chrome-agent — `pacewright-browser` and `pacewright-adapter-recipe` — depend on core and not on
/// each other, so this is the only home that keeps them from drifting onto different endpoints.
pub const DEFAULT_CHROME_CONNECT: &str = "http://127.0.0.1:9222";

/// `PACEWRIGHT_CHROME_CONNECT` overrides the endpoint (`auto`, or a `ws://`/`http://` URL).
pub fn default_connect_endpoint() -> Option<String> {
    Some(
        std::env::var("PACEWRIGHT_CHROME_CONNECT")
            .unwrap_or_else(|_| DEFAULT_CHROME_CONNECT.to_string()),
    )
}

/// Every Chrome pacewright may attach to, in order. **A pool of one is the original behavior**, so
/// nothing branches on "single vs multi": the single-Chrome operator just has a one-element pool.
///
/// Two use cases share this list and pull in opposite directions:
///
/// - **One shared Chrome** (the operator's real, logged-in browser). Safety comes from *not*
///   looking like a bot: one fingerprint, one session, human pacing. More browsers would be more
///   fingerprints to keep consistent, for no gain.
/// - **Many Chromes** (throughput). Filling merchant invoicing portals is not adversarial, and the
///   work is embarrassingly parallel: N browsers is N times the invoices per hour.
///
/// `PACEWRIGHT_CHROME_CONNECT` accepts a comma-separated list for the second case.
pub fn default_connect_pool() -> Vec<String> {
    match std::env::var("PACEWRIGHT_CHROME_CONNECT") {
        Ok(v) => v
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect(),
        Err(_) => vec![DEFAULT_CHROME_CONNECT.to_string()],
    }
}

/// Which pool slot an **account** belongs to, by a stable hash of its name.
///
/// This is the load-bearing rule of a multi-Chrome pool: a signed-in session lives in the one
/// Chrome where the human signed in, and it cannot be moved. So `pcw auth login <account>` and
/// every later run of that account MUST resolve to the same slot, or the run opens a logged-out
/// tab in a different browser and the recipe dies at the auth wall.
///
/// A hash, not a counter: it needs no persistence and survives a daemon restart. Adding an
/// endpoint to the pool DOES reshuffle accounts (and costs those accounts a re-login) — the
/// alternative is a stored assignment table, which is more machinery than a rare, visible
/// migration deserves. `pcw auth status` shows who is signed out, so the cost is legible.
pub fn slot_for_account(account: &str, len: usize) -> usize {
    if len <= 1 {
        return 0;
    }
    // FNV-1a: two lines, stable across processes and releases. `DefaultHasher` guarantees
    // neither, and an assignment that moves between runs is exactly the bug this prevents.
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in account.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    (h % len as u64) as usize
}

/// Rewrite chrome-agent's "Could not resolve CDP WebSocket from …" into something that names the
/// actual cause and the fix. That message means the always-on Chrome isn't listening — the single
/// most likely browser failure now that pacewright never launches one — but it reads like an
/// internal protocol fault and sends you debugging the wrong layer.
///
/// Returns `None` for anything else, so unrelated failures pass through untouched.
pub fn explain_connect_failure(msg: &str) -> Option<String> {
    if !msg.contains("Could not resolve CDP WebSocket") {
        return None;
    }
    Some(format!(
        "the always-on Chrome is not reachable — pacewright attaches to it and never launches one, \
         so every browser task is blocked until it is back. Start it with \
         `launchctl load ~/Library/LaunchAgents/com.paperclip.pacewright-chrome.plist`, or check \
         `browser.connect` in ~/.pacewright/config.toml matches its --remote-debugging-port. \
         (chrome-agent said: {msg})"
    ))
}

/// True when chrome-agent failed because a *page*'s cached CDP target is stale — the tab it
/// recorded in `~/.chrome-agent/sessions.json` was closed since, so its `targetId` no longer
/// appears in `/json/list`. Unlike the browser-level GUID (which chrome-agent re-resolves from
/// `--connect`), a stale page target is NOT self-healed: chrome-agent errors instead of adopting or
/// recreating the tab. Verified live 2026-07-17: closing the `acme` tab made the next task fail
/// with exactly this until the page record was pruned.
///
/// Caller contract: on a match, `prune_stale_page` the offending page and retry once — with the
/// record gone, chrome-agent opens a fresh tab for that page name and proceeds.
pub fn is_stale_page_target(msg: &str) -> bool {
    // chrome-agent's wording: "Failed to connect to page after N attempts: Target <id> not found
    // in /json/list". Match the stable spine, not the attempt count or the volatile target id.
    msg.contains("not found in /json/list")
        || (msg.contains("Failed to connect to page") && msg.contains("Target "))
}

/// chrome-agent's session store. Overridable via `CHROME_AGENT_HOME` for tests and non-default
/// installs; defaults to `~/.chrome-agent/sessions.json`.
pub fn chrome_sessions_path() -> std::path::PathBuf {
    let home = std::env::var("CHROME_AGENT_HOME").unwrap_or_else(|_| {
        format!(
            "{}/.chrome-agent",
            std::env::var("HOME").unwrap_or_default()
        )
    });
    std::path::PathBuf::from(home).join("sessions.json")
}

/// Remove one `browser`/`page` entry from chrome-agent's `sessions.json` so the next command opens a
/// fresh tab for that page instead of re-attaching to a dead target (see `is_stale_page_target`).
///
/// Deliberately forgiving — a missing file, unparseable JSON, or absent browser/page is a no-op, not
/// an error: this runs on a recovery path where the goal is "get unstuck", and a failure to prune
/// just means the retry surfaces the same error the caller already had. Returns whether it removed
/// anything (for logging/tests). Only ever removes the single named page — never the browser, never
/// another page, never `default`.
pub fn prune_stale_page(browser: &str, page: &str) -> bool {
    let path = chrome_sessions_path();
    let Ok(text) = std::fs::read_to_string(&path) else {
        return false;
    };
    let Ok(mut root) = serde_json::from_str::<serde_json::Value>(&text) else {
        return false;
    };
    let removed = root
        .get_mut("browsers")
        .and_then(|b| b.get_mut(browser))
        .and_then(|b| b.get_mut("pages"))
        .and_then(|p| p.as_object_mut())
        .map(|pages| pages.remove(page).is_some())
        .unwrap_or(false);
    if removed {
        // Best-effort write-back; if it fails the retry just re-hits the same error.
        if let Ok(serialized) = serde_json::to_string_pretty(&root) {
            let _ = std::fs::write(&path, serialized);
        }
    }
    removed
}

/// What a navigation landed on. Adapters check this to detect auth walls
/// (Acme bounces unauthenticated sessions to `/authwall` or `/login`).
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
            BrowserError::Navigation(_) | BrowserError::Io(_) => {
                AdapterError::Retryable(e.to_string())
            }
            BrowserError::Unavailable(_) | BrowserError::Eval(_) => {
                AdapterError::Terminal(e.to_string())
            }
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
        Err(BrowserError::Unavailable(
            "no browser configured for this daemon".into(),
        ))
    }
    async fn eval(&self, _js: &str) -> Result<serde_json::Value, BrowserError> {
        Err(BrowserError::Unavailable(
            "no browser configured for this daemon".into(),
        ))
    }
    async fn screenshot(&self) -> Result<Vec<u8>, BrowserError> {
        Err(BrowserError::Unavailable(
            "no browser configured for this daemon".into(),
        ))
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
    pub fn new() -> Self {
        Self::default()
    }

    /// Make the next `goto` land here (e.g. an `/authwall` URL).
    pub fn with_nav(self, url: &str, title: &str) -> Self {
        *self.nav.lock().unwrap() = Some(NavInfo {
            url: url.into(),
            title: title.into(),
        });
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

    pub fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

#[async_trait]
impl BrowserHandle for FakeBrowser {
    async fn goto(&self, url: &str) -> Result<NavInfo, BrowserError> {
        self.calls.lock().unwrap().push(format!("goto:{url}"));
        if let Some(e) = self.fail_goto.lock().unwrap().clone() {
            return Err(e);
        }
        Ok(self.nav.lock().unwrap().clone().unwrap_or(NavInfo {
            url: url.to_string(),
            title: String::new(),
        }))
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
        assert_eq!(
            b.eval("document.title").await.unwrap(),
            serde_json::json!("Sign In")
        );
        assert_eq!(
            b.calls(),
            vec!["goto:https://x.test/in/foo", "eval:document.title"]
        );
    }

    #[tokio::test]
    async fn fake_browser_unscripted_eval_is_an_error() {
        let b = FakeBrowser::new();
        assert!(b.eval("nope()").await.is_err());
    }

    #[test]
    fn transient_browser_errors_are_retryable_permanent_are_terminal() {
        assert!(matches!(
            AdapterError::from(BrowserError::Navigation("t".into())),
            AdapterError::Retryable(_)
        ));
        assert!(matches!(
            AdapterError::from(BrowserError::Io("t".into())),
            AdapterError::Retryable(_)
        ));
        assert!(matches!(
            AdapterError::from(BrowserError::Unavailable("t".into())),
            AdapterError::Terminal(_)
        ));
        assert!(matches!(
            AdapterError::from(BrowserError::Eval("t".into())),
            AdapterError::Terminal(_)
        ));
    }
}

#[cfg(test)]
mod connect_tests {
    use super::*;

    /// The verbatim message chrome-agent emits when the endpoint is dead (captured live
    /// 2026-07-16 against a port with nothing listening).
    const REAL_MSG: &str = "Could not resolve CDP WebSocket from http://127.0.0.1:9999. If Chrome uses built-in remote debugging, run `chrome-agent --connect` without a URL for auto-discovery.";

    #[test]
    fn a_pool_of_one_puts_every_account_on_the_only_chrome() {
        // The original single-Chrome behavior IS a pool of one, so no caller needs a mode flag.
        for a in ["acme", "globex", "", "a-very-long-account-name"] {
            assert_eq!(slot_for_account(a, 1), 0);
        }
        // A pool declared empty must not panic on a modulo by zero.
        assert_eq!(slot_for_account("acme", 0), 0);
    }

    #[test]
    fn an_account_always_lands_on_the_same_chrome() {
        // The load-bearing property: `pcw auth login acme` signs in on ONE Chrome, and every
        // later run of `acme` must attach to that same one or it hits a logged-out tab.
        for len in [2usize, 3, 4, 8] {
            let first = slot_for_account("acme", len);
            for _ in 0..100 {
                assert_eq!(slot_for_account("acme", len), first, "pool of {len}");
            }
            assert!(first < len, "slot {first} outside a pool of {len}");
        }
    }

    #[test]
    fn different_accounts_spread_across_the_pool() {
        // Not a hash-quality claim — just that eight accounts over four Chromes use more than one
        // of them. A hash that sent everything to slot 0 would pass every test above and defeat
        // the entire point of the pool.
        let used: std::collections::BTreeSet<usize> =
            ["acme", "globex", "initech", "umbrella", "soylent", "tyrell", "wayne", "stark"]
                .iter()
                .map(|a| slot_for_account(a, 4))
                .collect();
        assert!(used.len() > 1, "every account landed on the same Chrome: {used:?}");
    }

    #[test]
    fn connect_failure_names_the_cause_and_the_fix() {
        let e = explain_connect_failure(REAL_MSG).expect("must match the real message");
        assert!(e.contains("always-on Chrome is not reachable"));
        assert!(e.contains("launchctl load"), "must give the fix: {e}");
        assert!(
            e.contains("browser.connect"),
            "must point at the config key: {e}"
        );
        // the original is preserved for debugging, not swallowed
        assert!(e.contains("Could not resolve CDP WebSocket"));
    }

    #[test]
    fn unrelated_failures_pass_through_untouched() {
        assert_eq!(explain_connect_failure("element not found: n42"), None);
        assert_eq!(explain_connect_failure(""), None);
    }

    /// The verbatim stale-page-target message, captured live 2026-07-17 by closing the `acme`
    /// tab and driving it again.
    const STALE_PAGE_MSG: &str = "Failed to connect to page after 8 attempts: Target A7ACAA268173BBBFDFB541DCA50A10E5 not found in /json/list";

    #[test]
    fn detects_the_stale_page_target_error() {
        assert!(is_stale_page_target(STALE_PAGE_MSG));
        // matches the spine even if the attempt count or id changes
        assert!(is_stale_page_target(
            "Failed to connect to page after 3 attempts: Target ZZZ not found in /json/list"
        ));
        // must NOT fire on the browser-level connect failure (that self-heals via --connect)
        assert!(!is_stale_page_target(REAL_MSG));
        assert!(!is_stale_page_target("element not found: n42"));
        assert!(!is_stale_page_target(""));
    }

    /// Build a temp `sessions.json`, point `CHROME_AGENT_HOME` at it, and return the dir (kept alive
    /// by the caller). Serialized via a process-wide lock because env vars are global.
    fn with_sessions(json: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "pcw-prune-{}-{}",
            std::process::id(),
            // a monotonic-ish suffix without Date/rand (both banned in this crate's other tests)
            std::sync::atomic::AtomicU64::new(0).fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("sessions.json"), json).unwrap();
        dir
    }

    // env is process-global; these prune tests must not run concurrently with each other.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn prune_removes_only_the_named_page() {
        let _g = ENV_LOCK.lock().unwrap();
        let dir = with_sessions(
            r#"{"browsers":{"pacewright":{"wsEndpoint":"ws://x","pages":{"acme":{"targetId":"A"},"globex":{"targetId":"B"}}},"other":{"pages":{"acme":{"targetId":"C"}}}}}"#,
        );
        std::env::set_var("CHROME_AGENT_HOME", &dir);
        assert!(prune_stale_page("pacewright", "acme"));
        let after: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("sessions.json")).unwrap())
                .unwrap();
        let pages = &after["browsers"]["pacewright"]["pages"];
        assert!(pages.get("acme").is_none(), "pruned the stale page");
        assert!(
            pages.get("globex").is_some(),
            "left the sibling page intact"
        );
        // never touches another browser's same-named page
        assert!(after["browsers"]["other"]["pages"]["acme"].is_object());
        std::env::remove_var("CHROME_AGENT_HOME");
    }

    #[test]
    fn prune_is_a_noop_when_absent_or_missing() {
        let _g = ENV_LOCK.lock().unwrap();
        // missing page → false, file untouched
        let dir =
            with_sessions(r#"{"browsers":{"pacewright":{"pages":{"globex":{"targetId":"B"}}}}}"#);
        std::env::set_var("CHROME_AGENT_HOME", &dir);
        assert!(
            !prune_stale_page("pacewright", "acme"),
            "absent page → no-op"
        );
        assert!(!prune_stale_page("nonexistent-browser", "acme"));
        // missing file → false, never panics
        let empty = std::env::temp_dir().join(format!("pcw-prune-missing-{}", std::process::id()));
        std::fs::create_dir_all(&empty).unwrap();
        std::env::set_var("CHROME_AGENT_HOME", &empty);
        assert!(!prune_stale_page("pacewright", "acme"));
        std::env::remove_var("CHROME_AGENT_HOME");
    }
}

//! `CliBrowser`: a `BrowserHandle` backed by the `chrome-agent` CLI.
//!
//! chrome-agent (github.com/sderosiaux/chrome-agent) is CDP-direct and keeps a
//! persistent Chrome session across invocations, so each verb is a short-lived
//! subprocess acting on the *same* live page.
//!
//! Every verb **attaches** (`--connect`) to the operator's always-on, non-headless Chrome —
//! started by launchd on `--remote-debugging-port`, never by chrome-agent. That Chrome IS the
//! substrate the design spec (§9) argues for over a throwaway Chromium, so `--copy-cookies` is
//! gone: there is no throwaway profile left to snapshot cookies into.
//!
//! Letting chrome-agent *launch* the browser is what broke auth — a CDP-launched browser is a bot
//! signal, so Acme walled the profile and revoked `li_at`, and Google refused sign-in outright.
//! See `docs/plans/2026-07-16-single-chrome-attach.md`.
//!
//! This is the pragmatic first implementation of the seam. A native impl over a
//! forked `chrome_agent` *library* (see `docs/specs/2026-07-07-chrome-agent-fork-lib.md`)
//! can replace it behind `BrowserHandle` without touching any adapter.

use async_trait::async_trait;
use pacewright_core::browser::{BrowserError, BrowserHandle, NavInfo};
use serde_json::Value;

/// chrome-agent's browsers and pages are *named* and global to the machine. Left at
/// the defaults, every consumer shares one browser and one page called `default` —
/// so an unrelated tool (the Globex/podcast tooling on this box does exactly
/// this) can navigate the page between our `goto` and our `eval`, and we would
/// silently scrape the wrong site and report success. pacewright therefore pins its
/// own `--browser` and `--page` names and never touches `default`.
pub const DEFAULT_BROWSER_NAME: &str = "pacewright";
pub const DEFAULT_PAGE_NAME: &str = "pacewright";

/// Where the always-on Chrome listens. Re-exported from core so the two callers of chrome-agent
/// cannot drift onto different endpoints.
pub use pacewright_core::browser::{
    default_connect_endpoint, explain_connect_failure, DEFAULT_CHROME_CONNECT,
};

/// Every verb **attaches** to the operator's always-on, non-headless Chrome (started by launchd
/// on `--remote-debugging-port`, never by chrome-agent). There is no session-establishing verb
/// anymore: `goto` used to carry `--copy-cookies` to snapshot cookies into a throwaway profile,
/// but the attached profile IS the live session.
pub struct CliBrowser {
    bin: String,
    timeout_secs: u64,
    stealth: bool,
    connect: Option<String>,
    browser_name: String,
    page_name: String,
}

impl Default for CliBrowser {
    fn default() -> Self {
        Self::new()
    }
}

impl CliBrowser {
    pub fn new() -> Self {
        CliBrowser {
            bin: std::env::var("CHROME_AGENT_BIN").unwrap_or_else(|_| "chrome-agent".to_string()),
            timeout_secs: 90,
            stealth: true,
            connect: default_connect_endpoint(),
            browser_name: DEFAULT_BROWSER_NAME.to_string(),
            page_name: DEFAULT_PAGE_NAME.to_string(),
        }
    }
    pub fn bin(mut self, bin: impl Into<String>) -> Self {
        self.bin = bin.into();
        self
    }
    pub fn timeout_secs(mut self, s: u64) -> Self {
        self.timeout_secs = s;
        self
    }
    pub fn stealth(mut self, on: bool) -> Self {
        self.stealth = on;
        self
    }
    /// Endpoint of the always-on Chrome to attach to (`http://127.0.0.1:9222` or `auto`).
    pub fn connect(mut self, endpoint: impl Into<String>) -> Self {
        self.connect = Some(endpoint.into());
        self
    }
    /// Override the dedicated chrome-agent browser profile name.
    pub fn browser_name(mut self, name: impl Into<String>) -> Self {
        self.browser_name = name.into();
        self
    }
    /// Override the dedicated chrome-agent page (tab) name.
    pub fn page_name(mut self, name: impl Into<String>) -> Self {
        self.page_name = name.into();
        self
    }

    /// Global flags that must precede the subcommand. `--connect`/`--browser`/`--page` go on
    /// *every* verb, not just `goto`: they are what bind each short-lived subprocess to the same
    /// tab of the same attached Chrome. There is no session-establishing verb anymore — attaching
    /// to the live profile replaced the `--copy-cookies`-on-`goto` snapshot.
    fn global_args(&self) -> Vec<String> {
        let mut v = vec![
            "--json".to_string(),
            "--timeout".to_string(),
            self.timeout_secs.to_string(),
            "--browser".to_string(),
            self.browser_name.clone(),
            "--page".to_string(),
            self.page_name.clone(),
        ];
        if let Some(endpoint) = &self.connect {
            v.push("--connect".into());
            v.push(endpoint.clone());
        }
        if self.stealth {
            v.push("--stealth".into());
        }
        v
    }

    /// Run once, and if it failed because this page's cached CDP target is stale (the tab was
    /// closed since chrome-agent recorded it), prune that page from the session store and run once
    /// more — chrome-agent then opens a fresh tab for the page name. A closed tab would otherwise be
    /// a silent, permanent task failure. Bounded to a single retry so a genuinely broken page can't
    /// loop. Verified live 2026-07-17.
    async fn run(&self, args: Vec<String>) -> Result<String, BrowserError> {
        let out = self.run_once(&args).await;
        if stale_page_target(&out) {
            pacewright_core::browser::prune_stale_page(&self.browser_name, &self.page_name);
            return self.run_once(&args).await;
        }
        out
    }

    async fn run_once(&self, args: &[String]) -> Result<String, BrowserError> {
        let out = tokio::process::Command::new(&self.bin)
            .args(args)
            .output()
            .await
            .map_err(|e| BrowserError::Unavailable(format!("cannot spawn `{}`: {e}", self.bin)))?;
        let stdout = String::from_utf8_lossy(&out.stdout).to_string();
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr).to_string();
            return Err(BrowserError::Io(format!(
                "chrome-agent {args:?} exited {}: {}",
                out.status,
                first_nonempty(&stderr, &stdout)
            )));
        }
        Ok(stdout)
    }
}

/// Did this run fail on a stale page target? chrome-agent may report it either as a non-zero exit
/// (→ `Err`) or as an in-band `{"ok":false,"error":"…"}` on a zero exit (→ `Ok(stdout)`), so check
/// both shapes.
fn stale_page_target(out: &Result<String, BrowserError>) -> bool {
    use pacewright_core::browser::is_stale_page_target;
    match out {
        Err(e) => is_stale_page_target(&e.to_string()),
        Ok(stdout) => last_json(stdout)
            .ok()
            .filter(|v| v.get("ok").and_then(Value::as_bool) == Some(false))
            .and_then(|v| {
                v.get("error")
                    .and_then(Value::as_str)
                    .map(is_stale_page_target)
            })
            .unwrap_or(false),
    }
}

fn first_nonempty<'a>(a: &'a str, b: &'a str) -> &'a str {
    if a.trim().is_empty() {
        b.trim()
    } else {
        a.trim()
    }
}

/// chrome-agent prints informational lines (e.g. "Copied cookies from Chrome
/// profile") before its JSON, so scan from the bottom for the last parseable
/// JSON object rather than assuming the whole of stdout is JSON.
pub fn last_json(s: &str) -> Result<Value, BrowserError> {
    for line in s.lines().rev() {
        let t = line.trim();
        if t.starts_with('{') {
            if let Ok(v) = serde_json::from_str::<Value>(t) {
                return Ok(v);
            }
        }
    }
    Err(BrowserError::Io(format!(
        "no JSON object in chrome-agent output: {s}"
    )))
}

/// chrome-agent signals command failure in-band as `{"ok": false, "error": "..."}`
/// even on a zero exit for some verbs; surface that as an error rather than
/// handing an adapter a success-shaped value.
fn check_ok(v: &Value) -> Result<(), BrowserError> {
    if v.get("ok").and_then(Value::as_bool) == Some(false) {
        let msg = v
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("unknown error");
        // A dead always-on Chrome arrives here as an in-band `ok:false` on a ZERO exit, so it
        // would otherwise surface as an opaque "eval failed: Could not resolve CDP WebSocket".
        // `Unavailable` is the honest class for it (and still maps to Terminal).
        if let Some(explained) = explain_connect_failure(msg) {
            return Err(BrowserError::Unavailable(explained));
        }
        return Err(BrowserError::Eval(msg.to_string()));
    }
    Ok(())
}

#[async_trait]
impl BrowserHandle for CliBrowser {
    async fn goto(&self, url: &str) -> Result<NavInfo, BrowserError> {
        let mut args = self.global_args();
        args.push("goto".into());
        args.push(url.to_string());
        let out = self.run(args).await?;
        let v = last_json(&out)?;
        check_ok(&v).map_err(|e| BrowserError::Navigation(e.to_string()))?;
        Ok(NavInfo {
            // Fall back to the requested URL if chrome-agent omits it.
            url: v
                .get("url")
                .and_then(Value::as_str)
                .unwrap_or(url)
                .to_string(),
            title: v
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        })
    }

    async fn eval(&self, js: &str) -> Result<Value, BrowserError> {
        let mut args = self.global_args();
        args.push("eval".into());
        args.push(js.to_string());
        let out = self.run(args).await?;
        let v = last_json(&out)?;
        check_ok(&v)?;
        v.get("result")
            .cloned()
            .ok_or_else(|| BrowserError::Eval(format!("eval had no result: {v}")))
    }

    async fn screenshot(&self) -> Result<Vec<u8>, BrowserError> {
        let mut args = self.global_args();
        args.push("screenshot".into());
        args.push("--filename".into());
        args.push("pacewright_shot.png".into());
        let out = self.run(args).await?;
        let v = last_json(&out)?;
        check_ok(&v)?;
        // chrome-agent resolves --filename inside its own tmp dir and returns the real path.
        let path = v
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| BrowserError::Io(format!("screenshot returned no path: {v}")))?;
        tokio::fs::read(path)
            .await
            .map_err(|e| BrowserError::Io(format!("cannot read {path}: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn last_json_picks_the_json_line_after_noise() {
        let out =
            "Copied cookies from Chrome profile\n{\"ok\":true,\"url\":\"u\",\"title\":\"t\"}\n";
        let v = last_json(out).unwrap();
        assert_eq!(v["title"], "t");
    }

    #[test]
    fn last_json_errors_when_absent() {
        assert!(last_json("no json here").is_err());
    }

    #[test]
    fn last_json_takes_the_last_object_not_the_first() {
        let out = "{\"ok\":true,\"n\":1}\n{\"ok\":true,\"n\":2}\n";
        assert_eq!(last_json(out).unwrap()["n"], 2);
    }

    #[test]
    fn check_ok_surfaces_in_band_failure() {
        let v = serde_json::json!({"ok": false, "error": "boom"});
        let e = check_ok(&v).unwrap_err();
        assert!(e.to_string().contains("boom"));
    }

    /// Regression: every verb must be pinned to pacewright's own browser+page, or a
    /// concurrent chrome-agent consumer can navigate the shared `default` page
    /// between our goto and our eval, and we scrape the wrong site as "success".
    /// Still true under `--connect`: observed 2026-07-16, omitting `--browser` filed our
    /// pages under the browser key `default` — the exact hijack this guards.
    #[test]
    fn every_verb_is_pinned_to_a_named_browser_and_page() {
        let g = CliBrowser::new().global_args();
        let pos = |flag: &str| g.iter().position(|a| a == flag).expect("flag present");
        assert_eq!(g[pos("--browser") + 1], DEFAULT_BROWSER_NAME);
        assert_eq!(g[pos("--page") + 1], DEFAULT_PAGE_NAME);
        // and never the global default page/browser that other tools use
        assert_ne!(DEFAULT_PAGE_NAME, "default");
        assert_ne!(DEFAULT_BROWSER_NAME, "default");
    }

    #[test]
    fn browser_and_page_names_are_overridable() {
        let g = CliBrowser::new()
            .browser_name("b1")
            .page_name("p1")
            .global_args();
        let pos = |flag: &str| g.iter().position(|a| a == flag).unwrap();
        assert_eq!(g[pos("--browser") + 1], "b1");
        assert_eq!(g[pos("--page") + 1], "p1");
    }

    /// Every verb attaches to the always-on Chrome. There is no longer a "session-establishing"
    /// verb: `goto` used to carry `--copy-cookies` to snapshot the operator's cookies into a
    /// throwaway profile, but the attached profile IS the live session, so goto and eval are
    /// identical. Launching is what got Acme's `li_at` revoked.
    #[test]
    fn every_verb_attaches_and_carries_no_session_flags() {
        let b = CliBrowser::new().timeout_secs(5);
        let g = b.global_args();
        let pos = |f: &str| {
            g.iter()
                .position(|a| a == f)
                .unwrap_or_else(|| panic!("{f} absent: {g:?}"))
        };
        assert_eq!(g[pos("--connect") + 1], DEFAULT_CHROME_CONNECT);
        assert!(
            !g.contains(&"--copy-cookies".to_string()),
            "nothing to copy when attached: {g:?}"
        );
        assert!(
            !g.contains(&"--headed".to_string()),
            "attached Chrome is visible already: {g:?}"
        );
        // --json and --timeout are always present, and precede the subcommand.
        assert_eq!(g[0], "--json");
        assert_eq!((g[1].as_str(), g[2].as_str()), ("--timeout", "5"));
    }

    /// CDP attach still leaves `navigator.webdriver` true, which is itself a detection signal,
    /// so stealth stays on by default.
    #[test]
    fn stealth_is_on_by_default_and_can_be_disabled() {
        assert!(CliBrowser::new()
            .global_args()
            .contains(&"--stealth".to_string()));
        assert!(!CliBrowser::new()
            .stealth(false)
            .global_args()
            .contains(&"--stealth".to_string()));
    }

    #[tokio::test]
    async fn missing_binary_is_unavailable_and_maps_to_terminal() {
        let b = CliBrowser::new().bin("definitely-not-a-real-binary-xyz");
        let err = b.goto("https://example.com").await.unwrap_err();
        assert!(matches!(err, BrowserError::Unavailable(_)), "got {err:?}");
        let ae: pacewright_core::model::AdapterError = err.into();
        assert!(matches!(
            ae,
            pacewright_core::model::AdapterError::Terminal(_)
        ));
    }
}

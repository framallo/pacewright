//! `CliBrowser`: a `BrowserHandle` backed by the `chrome-agent` CLI.
//!
//! chrome-agent (github.com/sderosiaux/chrome-agent) is CDP-direct and keeps a
//! persistent Chrome session across invocations, so each verb is a short-lived
//! subprocess acting on the *same* live page. `--stealth` + `--copy-cookies`
//! inherit the operator's real, logged-in Chrome profile — the substrate the
//! design spec (§9) argues for over a throwaway Chromium.
//!
//! This is the pragmatic first implementation of the seam. A native impl over a
//! forked `chrome_agent` *library* (see `docs/specs/2026-07-07-chrome-agent-fork-lib.md`)
//! can replace it behind `BrowserHandle` without touching any adapter.

use async_trait::async_trait;
use pacewright_core::browser::{BrowserError, BrowserHandle, NavInfo};
use serde_json::Value;

/// chrome-agent's browsers and pages are *named* and global to the machine. Left at
/// the defaults, every consumer shares one browser and one page called `default` —
/// so an unrelated tool (the Riverside/podcast tooling on this box does exactly
/// this) can navigate the page between our `goto` and our `eval`, and we would
/// silently scrape the wrong site and report success. pacewright therefore pins its
/// own `--browser` and `--page` names and never touches `default`.
pub const DEFAULT_BROWSER_NAME: &str = "pacewright";
pub const DEFAULT_PAGE_NAME: &str = "pacewright";

/// Session-establishing flags are passed on `goto`, which is what opens/reuses the
/// page; later verbs (`eval`, `screenshot`) act on that already-stealthed session.
pub struct CliBrowser {
    bin: String,
    timeout_secs: u64,
    stealth: bool,
    copy_cookies: bool,
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
            copy_cookies: true,
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
    pub fn copy_cookies(mut self, on: bool) -> Self {
        self.copy_cookies = on;
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

    /// Global flags that must precede the subcommand. `--browser`/`--page` go on
    /// *every* verb, not just `goto`: they are what bind each short-lived
    /// subprocess to the same isolated tab.
    fn global_args(&self, session_flags: bool) -> Vec<String> {
        let mut v = vec![
            "--json".to_string(),
            "--timeout".to_string(),
            self.timeout_secs.to_string(),
            "--browser".to_string(),
            self.browser_name.clone(),
            "--page".to_string(),
            self.page_name.clone(),
        ];
        if session_flags {
            if self.stealth {
                v.push("--stealth".into());
            }
            if self.copy_cookies {
                v.push("--copy-cookies".into());
            }
        }
        v
    }

    async fn run(&self, args: Vec<String>) -> Result<String, BrowserError> {
        let out = tokio::process::Command::new(&self.bin)
            .args(&args)
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
        return Err(BrowserError::Eval(msg.to_string()));
    }
    Ok(())
}

#[async_trait]
impl BrowserHandle for CliBrowser {
    async fn goto(&self, url: &str) -> Result<NavInfo, BrowserError> {
        let mut args = self.global_args(true);
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
        let mut args = self.global_args(false);
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
        let mut args = self.global_args(false);
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
    #[test]
    fn every_verb_is_pinned_to_a_named_browser_and_page() {
        let b = CliBrowser::new();
        for session_flags in [true, false] {
            let g = b.global_args(session_flags);
            let pos = |flag: &str| g.iter().position(|a| a == flag).expect("flag present");
            assert_eq!(g[pos("--browser") + 1], DEFAULT_BROWSER_NAME);
            assert_eq!(g[pos("--page") + 1], DEFAULT_PAGE_NAME);
        }
        // and never the global default page that other tools use
        assert_ne!(DEFAULT_PAGE_NAME, "default");
        assert_ne!(DEFAULT_BROWSER_NAME, "default");
    }

    #[test]
    fn browser_and_page_names_are_overridable() {
        let g = CliBrowser::new().browser_name("b1").page_name("p1").global_args(false);
        let pos = |flag: &str| g.iter().position(|a| a == flag).unwrap();
        assert_eq!(g[pos("--browser") + 1], "b1");
        assert_eq!(g[pos("--page") + 1], "p1");
    }

    #[test]
    fn goto_carries_session_flags_but_eval_does_not() {
        let b = CliBrowser::new()
            .stealth(true)
            .copy_cookies(true)
            .timeout_secs(5);
        let g = b.global_args(true);
        assert!(g.contains(&"--stealth".to_string()) && g.contains(&"--copy-cookies".to_string()));
        // --json and --timeout are always present, and precede the subcommand.
        assert_eq!(g[0], "--json");
        assert_eq!((g[1].as_str(), g[2].as_str()), ("--timeout", "5"));
        let e = b.global_args(false);
        assert!(
            !e.contains(&"--stealth".to_string()) && !e.contains(&"--copy-cookies".to_string())
        );
    }

    #[test]
    fn flags_can_be_disabled() {
        let b = CliBrowser::new().stealth(false).copy_cookies(false);
        assert!(!b.global_args(true).contains(&"--stealth".to_string()));
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

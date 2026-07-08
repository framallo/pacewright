//! LinkedIn adapter (M3, first slice): read-only profile scrape.
//!
//! Drives whatever `BrowserHandle` the daemon wired in — today the `chrome-agent`
//! CLI against the operator's real, logged-in Chrome. The adapter declares the
//! limit key `linkedin.profile_scrape`, so pacing (daily cap, min-gap, active
//! hours, jitter) is enforced by the engine, not by this code. That separation is
//! the point: adapters say *what they spend*, the engine decides *when*.

use async_trait::async_trait;
use pacewright_core::adapter::{Adapter, RunCtx};
use pacewright_core::browser::NavInfo;
use pacewright_core::model::{ActionSpec, AdapterError};
use serde_json::{json, Value};

pub const LIMIT_PROFILE_SCRAPE: &str = "linkedin.profile_scrape";

/// Pull the fields we care about out of a rendered profile page. `h1` is the one
/// durable anchor for the name; LinkedIn's utility classes churn, so every other
/// selector is best-effort and yields `null` rather than failing the task.
/// Returns a JSON *string* — chrome-agent's `eval` hands back whatever the
/// expression evaluates to, and stringifying keeps nested objects intact.
const PROFILE_JS: &str = r#"(() => {
  const txt = (sel) => { const e = document.querySelector(sel); return e && e.innerText ? e.innerText.trim() : null; };
  return JSON.stringify({
    name: txt('h1'),
    headline: txt('.text-body-medium'),
    location: txt('.text-body-small.inline.t-black--light.break-words'),
    url: location.href
  });
})()"#;

/// LinkedIn bounces an unauthenticated session to an auth wall or login page
/// instead of returning an error status. Retrying cannot fix a logged-out Chrome,
/// so callers map this to `Terminal`.
pub fn is_auth_wall(nav: &NavInfo) -> bool {
    let u = nav.url.to_ascii_lowercase();
    u.contains("/authwall")
        || u.contains("/login")
        || u.contains("/checkpoint")
        || nav.title.starts_with("Sign")
}

/// Only ever drive the browser at LinkedIn. Keeps a queued task from turning this
/// adapter into a general-purpose fetcher pointed at an arbitrary host.
pub fn validate_profile_url(url: &str) -> Result<(), AdapterError> {
    let lower = url.to_ascii_lowercase();
    if !lower.starts_with("https://") {
        return Err(AdapterError::Terminal(format!("url must be https: {url}")));
    }
    let host = lower
        .trim_start_matches("https://")
        .split('/')
        .next()
        .unwrap_or("");
    let host = host.strip_prefix("www.").unwrap_or(host);
    if host != "linkedin.com" && !host.ends_with(".linkedin.com") {
        return Err(AdapterError::Terminal(format!(
            "not a linkedin.com url: {url}"
        )));
    }
    Ok(())
}

/// chrome-agent's `eval` returns the expression's value. Our JS stringifies its
/// object, so the value arrives as a JSON *string* that needs one more parse.
/// Accept an already-parsed object too, so a future native browser impl that
/// returns structured values needs no adapter change.
pub fn unwrap_eval_json(v: Value) -> Result<Value, AdapterError> {
    match v {
        Value::String(s) => serde_json::from_str(&s).map_err(|e| {
            AdapterError::Terminal(format!("profile JS returned unparseable JSON: {e}"))
        }),
        other @ Value::Object(_) => Ok(other),
        other => Err(AdapterError::Terminal(format!(
            "profile JS returned unexpected value: {other}"
        ))),
    }
}

#[derive(Default)]
pub struct LinkedInAdapter;

impl LinkedInAdapter {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Adapter for LinkedInAdapter {
    fn name(&self) -> &str {
        "linkedin"
    }

    fn actions(&self) -> Vec<ActionSpec> {
        vec![ActionSpec {
            name: "scrape_profile".into(),
            limit_keys: vec![LIMIT_PROFILE_SCRAPE.into()],
            params_schema: json!({"url": "string (https://www.linkedin.com/in/<slug>/)"}),
            description: "navigate to a LinkedIn profile and extract name/headline/location".into(),
        }]
    }

    async fn execute(
        &self,
        ctx: &RunCtx,
        action: &str,
        params: Value,
    ) -> Result<Value, AdapterError> {
        match action {
            "scrape_profile" => {
                let url = params
                    .get("url")
                    .and_then(Value::as_str)
                    .ok_or_else(|| AdapterError::Terminal("missing required param `url`".into()))?;
                validate_profile_url(url)?;

                // `?` converts BrowserError -> AdapterError: navigation/io are Retryable
                // (the runner backs off), unavailable/eval are Terminal.
                let nav = ctx.browser.goto(url).await?;
                if is_auth_wall(&nav) {
                    return Err(AdapterError::Terminal(format!(
                        "LinkedIn auth wall at {} — log Chrome into LinkedIn (chrome-agent --copy-cookies reads that profile)",
                        nav.url
                    )));
                }

                let raw = ctx.browser.eval(PROFILE_JS).await?;
                let mut profile = unwrap_eval_json(raw)?;

                // Record where we actually landed; LinkedIn rewrites /in/<slug> URLs.
                if let Value::Object(ref mut m) = profile {
                    m.insert("landed_url".into(), json!(nav.url));
                    m.insert("page_title".into(), json!(nav.title));
                }
                tracing::info!(task = %ctx.task_id, "scraped linkedin profile {url}");
                Ok(profile)
            }
            other => Err(AdapterError::Terminal(format!("unknown action {other}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pacewright_core::browser::{BrowserError, FakeBrowser, NullBrowser};
    use std::sync::Arc;

    fn ctx_with(b: Arc<dyn pacewright_core::browser::BrowserHandle>) -> RunCtx {
        RunCtx {
            task_id: "t1".into(),
            browser: b,
        }
    }

    #[test]
    fn declares_the_limit_key_so_the_engine_paces_it() {
        assert_eq!(
            LinkedInAdapter::new().limit_keys_for("scrape_profile"),
            vec![LIMIT_PROFILE_SCRAPE.to_string()]
        );
    }

    #[test]
    fn rejects_non_linkedin_and_non_https_urls() {
        assert!(validate_profile_url("https://www.linkedin.com/in/foo/").is_ok());
        assert!(validate_profile_url("https://linkedin.com/in/foo").is_ok());
        assert!(validate_profile_url("https://es.linkedin.com/in/foo").is_ok());
        assert!(validate_profile_url("http://www.linkedin.com/in/foo").is_err()); // not https
        assert!(validate_profile_url("https://evil.test/in/foo").is_err());
        // must not be fooled by a linkedin.com prefix on another host
        assert!(validate_profile_url("https://linkedin.com.evil.test/in/foo").is_err());
    }

    #[test]
    fn detects_auth_walls() {
        assert!(is_auth_wall(&NavInfo {
            url: "https://www.linkedin.com/authwall".into(),
            title: "".into()
        }));
        assert!(is_auth_wall(&NavInfo {
            url: "https://www.linkedin.com/login".into(),
            title: "".into()
        }));
        assert!(is_auth_wall(&NavInfo {
            url: "https://x/".into(),
            title: "Sign In | LinkedIn".into()
        }));
        assert!(!is_auth_wall(&NavInfo {
            url: "https://www.linkedin.com/in/foo/".into(),
            title: "Foo | LinkedIn".into()
        }));
    }

    #[test]
    fn unwraps_stringified_and_object_eval_results() {
        assert_eq!(
            unwrap_eval_json(json!("{\"name\":\"A\"}")).unwrap(),
            json!({"name":"A"})
        );
        assert_eq!(
            unwrap_eval_json(json!({"name":"A"})).unwrap(),
            json!({"name":"A"})
        );
        assert!(unwrap_eval_json(json!(42)).is_err());
        assert!(unwrap_eval_json(json!("not json")).is_err());
    }

    #[tokio::test]
    async fn scrapes_a_profile_through_the_browser_handle() {
        let fake = FakeBrowser::new()
            .with_nav("https://www.linkedin.com/in/foo/", "Foo Bar | LinkedIn")
            .with_eval(PROFILE_JS, json!("{\"name\":\"Foo Bar\",\"headline\":\"CTO\",\"location\":\"MX\",\"url\":\"https://www.linkedin.com/in/foo/\"}"));
        let b = Arc::new(fake);
        let out = LinkedInAdapter::new()
            .execute(
                &ctx_with(b.clone()),
                "scrape_profile",
                json!({"url":"https://www.linkedin.com/in/foo/"}),
            )
            .await
            .unwrap();
        assert_eq!(out["name"], "Foo Bar");
        assert_eq!(out["headline"], "CTO");
        assert_eq!(out["landed_url"], "https://www.linkedin.com/in/foo/");
        assert_eq!(out["page_title"], "Foo Bar | LinkedIn");
        // it really drove the browser: goto then eval
        assert_eq!(b.calls().len(), 2);
        assert!(b.calls()[0].starts_with("goto:https://www.linkedin.com/in/foo/"));
        assert!(b.calls()[1].starts_with("eval:"));
    }

    #[tokio::test]
    async fn auth_wall_is_terminal_and_never_evals() {
        let fake = Arc::new(
            FakeBrowser::new().with_nav("https://www.linkedin.com/authwall?x=1", "Sign In"),
        );
        let err = LinkedInAdapter::new()
            .execute(
                &ctx_with(fake.clone()),
                "scrape_profile",
                json!({"url":"https://www.linkedin.com/in/foo/"}),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, AdapterError::Terminal(ref m) if m.contains("auth wall")),
            "got {err:?}"
        );
        assert_eq!(fake.calls(), vec!["goto:https://www.linkedin.com/in/foo/"]);
        // stopped before eval
    }

    #[tokio::test]
    async fn navigation_failure_is_retryable() {
        let fake =
            Arc::new(FakeBrowser::new().failing_goto(BrowserError::Navigation("timeout".into())));
        let err = LinkedInAdapter::new()
            .execute(
                &ctx_with(fake),
                "scrape_profile",
                json!({"url":"https://www.linkedin.com/in/foo/"}),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, AdapterError::Retryable(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn missing_browser_is_terminal() {
        let err = LinkedInAdapter::new()
            .execute(
                &ctx_with(Arc::new(NullBrowser)),
                "scrape_profile",
                json!({"url":"https://www.linkedin.com/in/foo/"}),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, AdapterError::Terminal(_)));
    }

    #[tokio::test]
    async fn missing_url_and_unknown_action_are_terminal() {
        let a = LinkedInAdapter::new();
        let c = ctx_with(Arc::new(NullBrowser));
        assert!(matches!(
            a.execute(&c, "scrape_profile", json!({}))
                .await
                .unwrap_err(),
            AdapterError::Terminal(_)
        ));
        assert!(matches!(
            a.execute(&c, "nope", json!({})).await.unwrap_err(),
            AdapterError::Terminal(_)
        ));
    }
}

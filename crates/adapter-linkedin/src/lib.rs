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

/// Extract a profile from the rendered page.
///
/// LinkedIn ships build-hashed class names (`e6590096 _3293afb7 …`), so class-based
/// selectors rot immediately — the previous version of this script keyed off
/// `.text-body-medium` and silently returned nulls. We anchor only on things that
/// are structurally or lexically stable:
///
/// - `name` — the single heading in `<main>` (`h1` on other people's profiles, `h2`
///   on your own self-view), falling back to the page title.
/// - `followers`/`connections` — matched on their trailing word, not markup.
/// - `top_card` — the ordered leaf texts, returned raw so downstream can remap
///   without a redeploy when LinkedIn reshuffles the card.
///
/// `headline`/`location` are positional best-effort over `top_card`, and are
/// explicitly allowed to be null rather than failing the task.
///
/// Also returns the *settled* `url`/`title`: `goto` reports the pre-redirect URL, so
/// this is the only trustworthy view of where we actually ended up.
///
/// Returns a JSON *string* — chrome-agent's `eval` hands back the expression's value,
/// and stringifying keeps the nested object intact.
const PROFILE_JS: &str = r#"(() => {
  const clean = (s) => (s || "").replace(/\s+/g, " ").trim();
  const main = document.querySelector("main") || document.body;

  const heading = main.querySelector("h1") || main.querySelector("h2");
  let name = heading ? clean(heading.innerText) : "";
  if (!name) name = clean((document.title || "").split("|")[0]);

  const lines = [];
  const seen = new Set();
  main.querySelectorAll("p, span, div").forEach((e) => {
    if (e.children.length !== 0) return;
    const t = clean(e.innerText);
    if (!t || t.length > 100) return;
    if (/^[·•|,\s-]*$/.test(t)) return;
    if (seen.has(t)) return;
    seen.add(t);
    if (lines.length < 12) lines.push(t);
  });

  const followers = lines.find((l) => /followers?$/i.test(l)) || null;
  const connections = lines.find((l) => /connections?$/i.test(l)) || null;
  const skip = new Set([followers, connections, name].filter(Boolean));
  const rest = lines.filter((l) => !skip.has(l) && !/^contact info$/i.test(l));

  return JSON.stringify({
    name: name || null,
    headline: rest[0] || null,
    location: rest[1] || null,
    followers: followers,
    connections: connections,
    top_card: lines,
    url: location.href,
    title: document.title
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

fn auth_wall_error(landed: &str) -> AdapterError {
    AdapterError::Terminal(format!(
        "LinkedIn auth wall at {landed} — log Chrome into LinkedIn. Note chrome-agent only \
         copies cookies when it launches a *fresh* browser, so an already-running session \
         stays logged out: `chrome-agent --browser pacewright close --purge` then retry."
    ))
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
                // Fast path: goto already reports an auth wall. Don't waste an eval.
                if is_auth_wall(&nav) {
                    return Err(auth_wall_error(&nav.url));
                }

                let raw = ctx.browser.eval(PROFILE_JS).await?;
                let mut profile = unwrap_eval_json(raw)?;

                // `goto` reports the URL it *requested*, before LinkedIn's redirect —
                // /in/me/ stays /in/me/ even when the browser lands on /authwall.
                // The settled `location.href`/`document.title` read back by PROFILE_JS
                // is the only trustworthy view, so the auth-wall check must run against
                // that. (Missing this let a logged-out scrape look like a success.)
                let settled = NavInfo {
                    url: profile.get("url").and_then(Value::as_str).unwrap_or(&nav.url).to_string(),
                    title: profile.get("title").and_then(Value::as_str).unwrap_or(&nav.title).to_string(),
                };
                if is_auth_wall(&settled) {
                    return Err(auth_wall_error(&settled.url));
                }

                if let Value::Object(ref mut m) = profile {
                    // Normalize the shape: what we asked for vs. where we ended up.
                    let landed = m.remove("url").unwrap_or(json!(settled.url));
                    let title = m.remove("title").unwrap_or(json!(settled.title));
                    m.insert("requested_url".into(), json!(url));
                    m.insert("landed_url".into(), landed);
                    m.insert("page_title".into(), title);
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
        // The settled url differs from the requested one — LinkedIn rewrites /in/me/.
        let fake = FakeBrowser::new()
            .with_nav("https://www.linkedin.com/in/me/", "Foo Bar | LinkedIn")
            .with_eval(PROFILE_JS, json!("{\"name\":\"Foo Bar\",\"headline\":\"CTO\",\"location\":\"MX\",\"followers\":\"10 followers\",\"url\":\"https://www.linkedin.com/in/foo/?isSelfProfile=true\",\"title\":\"Foo Bar | LinkedIn\"}"));
        let b = Arc::new(fake);
        let out = LinkedInAdapter::new()
            .execute(
                &ctx_with(b.clone()),
                "scrape_profile",
                json!({"url":"https://www.linkedin.com/in/me/"}),
            )
            .await
            .unwrap();
        assert_eq!(out["name"], "Foo Bar");
        assert_eq!(out["headline"], "CTO");
        assert_eq!(out["followers"], "10 followers");
        assert_eq!(out["requested_url"], "https://www.linkedin.com/in/me/");
        // landed_url comes from the settled location.href, not goto's echo
        assert_eq!(out["landed_url"], "https://www.linkedin.com/in/foo/?isSelfProfile=true");
        assert_eq!(out["page_title"], "Foo Bar | LinkedIn");
        // raw keys are normalized away
        assert!(out.get("url").is_none() && out.get("title").is_none());
        // it really drove the browser: goto then eval
        assert_eq!(b.calls().len(), 2);
        assert!(b.calls()[0].starts_with("goto:https://www.linkedin.com/in/me/"));
        assert!(b.calls()[1].starts_with("eval:"));
    }

    /// Regression for a bug the first live run exposed: `goto` echoes the *requested*
    /// URL, so a client-side redirect to /authwall was invisible to the pre-eval check.
    /// A logged-out scrape must fail Terminal, never return a "successful" profile.
    #[tokio::test]
    async fn redirect_to_authwall_after_goto_is_caught_from_settled_state() {
        let fake = Arc::new(
            FakeBrowser::new()
                // goto looks perfectly clean...
                .with_nav("https://www.linkedin.com/in/me/", "")
                // ...but the settled page is the auth wall.
                .with_eval(PROFILE_JS, json!("{\"name\":\"Join LinkedIn\",\"url\":\"https://www.linkedin.com/authwall?trk=bf\",\"title\":\"Sign Up | LinkedIn\"}")),
        );
        let err = LinkedInAdapter::new()
            .execute(
                &ctx_with(fake),
                "scrape_profile",
                json!({"url":"https://www.linkedin.com/in/me/"}),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, AdapterError::Terminal(ref m) if m.contains("auth wall")), "got {err:?}");
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

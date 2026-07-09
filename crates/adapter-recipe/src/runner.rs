//! `RecipeRunner` — the seam that actually *runs* a recipe: a whole recipe in **one**
//! chrome-agent process, so the injected `locators.js` runtime persists across the
//! recipe's steps (the exact limitation the per-verb `CliBrowser` could not satisfy).
//!
//! The trait is the testable boundary: `CliRecipeRunner` shells the real
//! `chrome-agent recipe run …`; adapter tests drive a fake. Everything about the
//! child-process contract — the pinned `--browser`/`--page`, the `{"ok":…}` envelope,
//! and recovering the pacewright error *class* from the child's error string — is
//! isolated here so the adapter stays pure result→`Value` / exit→`AdapterError` glue.

use async_trait::async_trait;
use pacewright_core::model::AdapterError;
use serde_json::Value;
use std::path::Path;

/// Runs a recipe file and returns its run envelope, or the mapped failure class.
#[async_trait]
pub trait RecipeRunner: Send + Sync {
    /// Run `recipe_path` binding `vars_json` (a JSON **object** string). On success returns
    /// the recipe-run envelope `{"ok":true,"result":{…},"unexpected":[…]}`; on failure
    /// returns the `AdapterError` whose class is recovered from the child's error output.
    async fn run(&self, recipe_path: &Path, vars_json: &str) -> Result<Value, AdapterError>;
}

/// The real runner: `chrome-agent --browser pacewright --page pacewright recipe run <file>`.
///
/// Pins the same dedicated browser/page as `CliBrowser` (so a concurrent chrome-agent
/// consumer on the shared `default` page can't hijack the run), and carries the
/// session-establishing `--stealth`/`--copy-cookies` so the recipe navigates with the
/// operator's logged-in cookies.
pub struct CliRecipeRunner {
    bin: String,
    timeout_secs: u64,
    stealth: bool,
    copy_cookies: bool,
    browser_name: String,
    page_name: String,
}

impl Default for CliRecipeRunner {
    fn default() -> Self {
        Self::new()
    }
}

impl CliRecipeRunner {
    pub fn new() -> Self {
        Self {
            bin: std::env::var("CHROME_AGENT_BIN").unwrap_or_else(|_| "chrome-agent".to_string()),
            // A recipe drives many steps in one process; give it more room than a single verb.
            timeout_secs: 180,
            stealth: true,
            copy_cookies: true,
            // Keep in lockstep with pacewright_browser::DEFAULT_{BROWSER,PAGE}_NAME.
            browser_name: "pacewright".to_string(),
            page_name: "pacewright".to_string(),
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
    pub fn browser_name(mut self, name: impl Into<String>) -> Self {
        self.browser_name = name.into();
        self
    }
    pub fn page_name(mut self, name: impl Into<String>) -> Self {
        self.page_name = name.into();
        self
    }

    fn args(&self, recipe_path: &Path, vars_json: &str) -> Vec<String> {
        let mut v = vec![
            "--json".to_string(),
            "--timeout".to_string(),
            self.timeout_secs.to_string(),
            "--browser".to_string(),
            self.browser_name.clone(),
            "--page".to_string(),
            self.page_name.clone(),
        ];
        if self.stealth {
            v.push("--stealth".into());
        }
        if self.copy_cookies {
            v.push("--copy-cookies".into());
        }
        v.push("recipe".into());
        v.push("run".into());
        v.push(recipe_path.to_string_lossy().into_owned());
        v.push("--vars-json".into());
        v.push(vars_json.to_string());
        v
    }
}

#[async_trait]
impl RecipeRunner for CliRecipeRunner {
    async fn run(&self, recipe_path: &Path, vars_json: &str) -> Result<Value, AdapterError> {
        let args = self.args(recipe_path, vars_json);
        let out = tokio::process::Command::new(&self.bin)
            .args(&args)
            .output()
            .await
            .map_err(|e| AdapterError::Terminal(format!("cannot spawn `{}`: {e}", self.bin)))?;
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        interpret_output(out.status.success(), &stdout, &stderr)
    }
}

/// Turn a finished `recipe run` into a result envelope or a classed error. Pure, so the
/// child-process contract is unit-tested without spawning anything.
pub fn interpret_output(success: bool, stdout: &str, stderr: &str) -> Result<Value, AdapterError> {
    match last_json(stdout) {
        Some(v) => {
            // chrome-agent signals recipe failure in-band as {"ok":false,"error":"[Class] …"}.
            if v.get("ok").and_then(Value::as_bool) == Some(false) {
                let msg = v
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("recipe run failed");
                Err(classify(msg))
            } else {
                Ok(v)
            }
        }
        // No JSON at all: a crash/usage error before the envelope was printed.
        None => {
            let detail = first_nonempty(stderr, stdout);
            if success {
                Err(AdapterError::Terminal(format!(
                    "recipe run produced no JSON envelope: {detail}"
                )))
            } else {
                Err(AdapterError::Terminal(format!(
                    "recipe run failed: {detail}"
                )))
            }
        }
    }
}

/// Recover the pacewright error class from a chrome-agent recipe error string. The engine
/// formats `RecipeError` as `"[Terminal] …" / "[Retryable] …" / "[RateLimited] …"`, so the
/// leading `[Class]` tag is the wire contract.
///
/// `RateLimited` has no absolute defer time on the recipe side (the recipe format can't name
/// one), and adapters have no `Clock`, so it degrades to `Retryable` (the runner backs off).
/// pacewright's real rate-limiting is the config-driven limits engine (declared `limit-key`s),
/// not this signal.
pub fn classify(msg: &str) -> AdapterError {
    if let Some(rest) = msg.strip_prefix("[Terminal]") {
        AdapterError::Terminal(rest.trim().to_string())
    } else if let Some(rest) = msg.strip_prefix("[Retryable]") {
        AdapterError::Retryable(rest.trim().to_string())
    } else if let Some(rest) = msg.strip_prefix("[RateLimited]") {
        AdapterError::Retryable(format!("rate-limited: {}", rest.trim()))
    } else {
        // No class tag → an unclassed failure (bad recipe, missing var, spawn issue): Terminal.
        AdapterError::Terminal(msg.to_string())
    }
}

/// Scan from the bottom for the last parseable top-level JSON object. chrome-agent prints
/// info lines (e.g. "Copied cookies …") before its JSON.
fn last_json(s: &str) -> Option<Value> {
    for line in s.lines().rev() {
        let t = line.trim();
        if t.starts_with('{') {
            if let Ok(v) = serde_json::from_str::<Value>(t) {
                return Some(v);
            }
        }
    }
    None
}

fn first_nonempty<'a>(a: &'a str, b: &'a str) -> &'a str {
    if a.trim().is_empty() {
        b.trim()
    } else {
        a.trim()
    }
}

#[cfg(test)]
pub mod fake {
    use super::{AdapterError, Path, RecipeRunner, Value};
    use async_trait::async_trait;
    use std::sync::Mutex;

    type Responder = Box<dyn Fn(&Path, &str) -> Result<Value, AdapterError> + Send + Sync>;

    /// A scriptable `RecipeRunner` for adapter tests. Records (path, vars_json) calls and
    /// returns whatever the injected closure produces.
    pub struct FakeRecipeRunner {
        responder: Responder,
        pub calls: Mutex<Vec<(String, String)>>,
    }

    impl FakeRecipeRunner {
        pub fn new(
            f: impl Fn(&Path, &str) -> Result<Value, AdapterError> + Send + Sync + 'static,
        ) -> Self {
            Self {
                responder: Box::new(f),
                calls: Mutex::new(Vec::new()),
            }
        }
        /// Always succeed with the given envelope.
        pub fn ok(envelope: Value) -> Self {
            Self::new(move |_, _| Ok(envelope.clone()))
        }
        pub fn call_count(&self) -> usize {
            self.calls.lock().unwrap().len()
        }
    }

    #[async_trait]
    impl RecipeRunner for FakeRecipeRunner {
        async fn run(&self, recipe_path: &Path, vars_json: &str) -> Result<Value, AdapterError> {
            self.calls.lock().unwrap().push((
                recipe_path.to_string_lossy().into_owned(),
                vars_json.to_string(),
            ));
            (self.responder)(recipe_path, vars_json)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn ok_envelope_passes_through() {
        let out = "Copied cookies from Chrome\n{\"ok\":true,\"result\":{\"stories\":[\"a\"]},\"unexpected\":[]}\n";
        let v = interpret_output(true, out, "").unwrap();
        assert_eq!(v["result"]["stories"][0], "a");
    }

    #[test]
    fn in_band_failure_recovers_the_class() {
        let out = "{\"ok\":false,\"error\":\"[Retryable] locator not found within 10000ms\"}";
        let err = interpret_output(false, out, "").unwrap_err();
        assert!(
            matches!(err, AdapterError::Retryable(ref m) if m.contains("locator not found")),
            "got {err:?}"
        );
    }

    #[test]
    fn classifies_each_tag() {
        assert!(
            matches!(classify("[Terminal] bad var"), AdapterError::Terminal(m) if m == "bad var")
        );
        assert!(matches!(classify("[Retryable] down"), AdapterError::Retryable(m) if m == "down"));
        // rate-limited degrades to retryable (no absolute defer time available)
        assert!(
            matches!(classify("[RateLimited] slow down"), AdapterError::Retryable(m) if m.contains("slow down"))
        );
        // untagged → terminal
        assert!(matches!(classify("kaboom"), AdapterError::Terminal(m) if m == "kaboom"));
    }

    #[test]
    fn no_json_is_terminal() {
        let err = interpret_output(false, "usage: chrome-agent …", "some stderr").unwrap_err();
        assert!(
            matches!(err, AdapterError::Terminal(ref m) if m.contains("some stderr")),
            "got {err:?}"
        );
    }

    #[test]
    fn cli_runner_builds_the_pinned_invocation() {
        let r = CliRecipeRunner::new().timeout_secs(90);
        let args = r.args(Path::new("/r/hn.kdl"), r#"{"url":"u"}"#);
        // pinned browser+page, session flags, then the subcommand + vars-json
        let find = |f: &str| args.iter().position(|a| a == f).expect("flag present");
        assert_eq!(args[find("--browser") + 1], "pacewright");
        assert_eq!(args[find("--page") + 1], "pacewright");
        assert!(args.contains(&"--stealth".to_string()));
        assert!(args.contains(&"--copy-cookies".to_string()));
        assert!(args.contains(&"recipe".to_string()) && args.contains(&"run".to_string()));
        assert_eq!(args[find("--vars-json") + 1], r#"{"url":"u"}"#);
        // global flags precede the subcommand
        assert!(find("--browser") < find("recipe"));
    }

    #[tokio::test]
    async fn missing_binary_is_terminal() {
        let r = CliRecipeRunner::new().bin("definitely-not-real-xyz");
        let err = r.run(Path::new("/r/x.kdl"), "{}").await.unwrap_err();
        assert!(matches!(err, AdapterError::Terminal(_)), "got {err:?}");
    }

    #[test]
    fn unexpected_marker_is_still_ok() {
        // A cardinality miss is `unexpected`, not a failure — the envelope stays ok:true.
        let out = json!({"ok": true, "result": {"stories": []}, "unexpected": ["stories: expected ≥1, got 0"]}).to_string();
        let v = interpret_output(true, &out, "").unwrap();
        assert_eq!(v["unexpected"][0], "stories: expected ≥1, got 0");
    }
}

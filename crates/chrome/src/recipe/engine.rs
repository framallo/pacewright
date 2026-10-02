//! The recipe execution engine: runs a `Recipe`'s steps over a `RecipeBrowser`, accumulating a
//! result map. Deterministic given browser responses (so it's unit-testable over `FakeBrowser`);
//! the only real-time is the auto-wait poll at the browser edge.
//!
//! The engine runs on the main task (`#[tokio::main]` → `block_on`), never spawned across
//! threads, so its futures need not be `Send` — they borrow a generic `&B` across awaits.
#![allow(clippy::future_not_send)]

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use serde_json::{Map, Value};

use crate::recipe::browser::RecipeBrowser;
use crate::recipe::model::{Condition, ErrorClass, Expectation, Locator, Recipe, Step};

/// The injected locator runtime.
pub const LOCATORS_JS: &str = include_str!("runtime/locators.js");

const POLL_INTERVAL_MS: u64 = 250;
const DEFAULT_STEP_TIMEOUT_MS: u64 = 10_000;

/// Run-time knobs.
pub struct RunOptions {
    /// Emit a step-by-step trace to stderr.
    pub log: bool,
    /// Auto-wait budget for a targeting step, unless the step overrides it.
    pub step_timeout_ms: u64,
    /// Repair mode: skip writing output files when the run is `unexpected` (the caller will
    /// instead assemble a repair context), so a stale note is never overwritten from a bad run.
    pub repair: bool,
}

impl Default for RunOptions {
    fn default() -> Self {
        Self {
            log: false,
            step_timeout_ms: DEFAULT_STEP_TIMEOUT_MS,
            repair: false,
        }
    }
}

/// A challenge-solver seam: given a page screenshot (base64 PNG) and an instruction, return the
/// answer text to type. pacewright wires Claude vision behind this so a `solve` step can work
/// around a captcha; the engine stays agnostic. Object-safe via a boxed future (no async_trait dep,
/// keeping the vendored crate self-contained).
pub trait Solver {
    fn solve<'a>(
        &'a self,
        image_b64: &'a str,
        prompt: &'a str,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<String, crate::BoxError>> + 'a>>;
}

/// A recipe failure carrying the pacewright error class the runner should map to, plus *where*
/// it happened: the 0-based index and a KDL-ish summary of the failing step, and the soft
/// `unexpected` notes accumulated before it (an `Outcome` only exists on success, so without this
/// they were lost on the error path). A self-heal loop needs all three.
#[derive(Debug, Clone)]
pub struct RecipeError {
    pub class: ErrorClass,
    pub message: String,
    /// 0-based index into `Recipe::steps` of the step that failed (`None` before the first step,
    /// e.g. a missing var or a runtime-injection failure).
    pub step_index: Option<usize>,
    /// A one-line KDL-ish rendering of that step, e.g. `click { locator text="Consultar" }`.
    pub step: Option<String>,
    /// Expectation misses noted by earlier steps in the same run.
    pub unexpected: Vec<String>,
}

impl std::fmt::Display for RecipeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{:?}] {}", self.class, self.message)
    }
}

impl std::error::Error for RecipeError {}

impl RecipeError {
    fn classed(class: ErrorClass, msg: impl Into<String>) -> Self {
        Self {
            class,
            message: msg.into(),
            step_index: None,
            step: None,
            unexpected: Vec::new(),
        }
    }
    fn terminal(msg: impl Into<String>) -> Self {
        Self::classed(ErrorClass::Terminal, msg)
    }
    fn retryable(msg: impl Into<String>) -> Self {
        Self::classed(ErrorClass::Retryable, msg)
    }
}

/// A file a `download` step saved: `key` is the step's `key=` if declared, else the file name of
/// its `out` path. The runner reads the file back by `path` to ship small ones inline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Download {
    pub key: String,
    pub path: String,
}

/// A completed run: the accumulated result map plus any unmet expectations (the `unexpected`
/// marker — a soft signal that triggers repair, not a failure), and the files `download` steps
/// produced.
#[derive(Debug)]
pub struct Outcome {
    pub result: Value,
    pub unexpected: Vec<String>,
    pub downloads: Vec<Download>,
}

fn kdl_str(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| format!("{s:?}"))
}

/// Render a locator the way it is written in a recipe: `role="button" text="Consultar" { within … }`.
fn kdl_locator(l: &Locator) -> String {
    let mut props = Vec::new();
    for (k, v) in [
        ("role", &l.role),
        ("name", &l.name),
        ("text", &l.text),
        ("label", &l.label),
        ("tag", &l.tag),
        ("css", &l.css),
    ] {
        if let Some(v) = v {
            props.push(format!("{k}={}", kdl_str(v)));
        }
    }
    if let Some(n) = l.level {
        props.push(format!("level={n}"));
    }
    if let Some(n) = l.nth {
        props.push(format!("nth={n}"));
    }
    let mut children = Vec::new();
    for (k, v) in [
        ("within", &l.within),
        ("fallback", &l.fallback),
        ("after", &l.after),
        ("near", &l.near),
    ] {
        if let Some(v) = v {
            children.push(format!("{k} {}", kdl_locator(v)));
        }
    }
    let mut out = props.join(" ");
    if !children.is_empty() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(&format!("{{ {} }}", children.join("; ")));
    }
    out
}

fn with_locator(verb: String, l: &Locator) -> String {
    format!("{verb} {{ locator {} }}", kdl_locator(l))
}

/// A one-line KDL-ish summary of a step for failure context — the verb, its scalar props and its
/// locator, written the way the recipe author wrote them. Not a round-trippable serialization.
pub fn summarize_step(step: &Step) -> String {
    match step {
        Step::Goto { url } => format!("goto {}", kdl_str(url)),
        Step::Reload => "reload".into(),
        Step::Extract {
            key, many, locator, ..
        } => with_locator(
            format!(
                "extract {}{}",
                kdl_str(key),
                if *many { " many=#true" } else { "" }
            ),
            locator,
        ),
        Step::Expect {
            on_fail,
            message,
            condition,
        } => format!(
            "expect on-fail={} message={} {{ {} }}",
            kdl_str(&format!("{on_fail:?}").to_lowercase()),
            kdl_str(message),
            serde_json::to_string(condition).unwrap_or_default()
        ),
        Step::Wait { locator, .. } => with_locator("wait".into(), locator),
        Step::Screenshot { key } => format!("screenshot {}", kdl_str(key)),
        Step::Eval { key, .. } => format!("eval {}", kdl_str(key)),
        Step::Tab {
            action,
            url_contains,
        } => match url_contains {
            Some(u) => format!("tab {} url-contains={}", kdl_str(action), kdl_str(u)),
            None => format!("tab {}", kdl_str(action)),
        },
        Step::Click { locator } => with_locator("click".into(), locator),
        Step::Fill { value, locator } => with_locator(format!("fill {}", kdl_str(value)), locator),
        Step::Insert { value, locator } => {
            with_locator(format!("insert {}", kdl_str(value)), locator)
        }
        Step::Select { value, locator } => {
            with_locator(format!("select {}", kdl_str(value)), locator)
        }
        Step::Upload { path, locator } => {
            with_locator(format!("upload {}", kdl_str(path)), locator)
        }
        Step::Solve {
            prompt, locator, ..
        } => with_locator(format!("solve {}", kdl_str(prompt)), locator),
        Step::Download { url, out, .. } => {
            format!("download url={} out={}", kdl_str(url), kdl_str(out))
        }
        Step::Request(r) => format!("request {} url={}", kdl_str(&r.method), kdl_str(&r.url)),
        Step::Api(r) => format!("api {} url={}", kdl_str(&r.method), kdl_str(&r.url)),
    }
}

/// Bind vars: start from what the caller provided, fill declared defaults, error on a missing
/// required var.
fn bind_vars(
    recipe: &Recipe,
    provided: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>, RecipeError> {
    let mut vars = provided.clone();
    for v in &recipe.vars {
        if vars.contains_key(&v.name) {
            continue;
        }
        if let Some(d) = &v.default {
            vars.insert(v.name.clone(), d.clone());
        } else if v.required {
            return Err(RecipeError::terminal(format!(
                "missing required var `{}`",
                v.name
            )));
        }
    }
    Ok(vars)
}

/// Substitute `{{ name }}` from `vars`. Unknown names → empty (load-time validation already
/// guaranteed every reference is declared). Section tags / `{{.}}` are output-template constructs
/// and are not touched here.
fn interpolate(s: &str, vars: &BTreeMap<String, String>) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(open) = rest.find("{{") {
        out.push_str(&rest[..open]);
        let after = &rest[open + 2..];
        let Some(close) = after.find("}}") else {
            out.push_str("{{");
            rest = after;
            continue;
        };
        let token = after[..close].trim();
        if token.is_empty() || token.starts_with('#') || token.starts_with('/') || token == "." {
            // leave template constructs verbatim
            out.push_str(&rest[open..open + 2 + close + 2]);
        } else {
            out.push_str(vars.get(token).map_or("", String::as_str));
        }
        rest = &after[close + 2..];
    }
    out.push_str(rest);
    out
}

fn locator_spec(loc: &Locator) -> Value {
    serde_json::to_value(loc).unwrap_or(Value::Null)
}

/// The number of items an extract produced, for expectation checking.
fn cardinality(val: &Value, many: bool) -> u64 {
    if many {
        val.as_array().map_or(0, |a| a.len() as u64)
    } else {
        u64::from(!(val.is_null() || val.as_str() == Some("")))
    }
}

/// Compare an extract's cardinality against its declared expectation. Returns an `unexpected`
/// message if unmet. A non-`many` extract implicitly expects at least one.
fn check_expectation(expect: &Expectation, val: &Value, many: bool, key: &str) -> Option<String> {
    let n = cardinality(val, many);
    if let Some(c) = expect.count
        && n != c
    {
        return Some(format!("{key}: expected exactly {c}, got {n}"));
    }
    // A non-`many` extract implicitly expects at least one.
    let min = expect.min.or_else(|| (!many).then_some(1));
    if let Some(m) = min
        && n < m
    {
        return Some(format!("{key}: expected \u{2265}{m}, got {n}"));
    }
    if let Some(x) = expect.max
        && n > x
    {
        return Some(format!("{key}: expected \u{2264}{x}, got {n}"));
    }
    None
}

/// Turn a `request` response body into the captured value. With a dotted `path`, the body is
/// parsed as JSON and the path followed (missing → `Null`). Without a path, a JSON body is stored
/// parsed (so outputs can index into it) and a non-JSON body as a plain string.
fn capture_response(body: &str, path: Option<&str>) -> Value {
    match path {
        Some(p) => {
            let Ok(json) = serde_json::from_str::<Value>(body) else {
                return Value::Null;
            };
            let mut cur = &json;
            for seg in p.split('.').filter(|s| !s.is_empty()) {
                // A numeric segment indexes an array, so `items.0.status.privacyStatus`
                // resolves. Without this, any path through a list silently captures null,
                // which reads as "check passed" to a caller that only looks at the status.
                let next = match seg.parse::<usize>() {
                    Ok(i) if cur.is_array() => cur.get(i),
                    _ => cur.get(seg),
                };
                match next {
                    Some(n) => cur = n,
                    None => return Value::Null,
                }
            }
            cur.clone()
        }
        None => serde_json::from_str::<Value>(body).unwrap_or_else(|_| Value::String(body.into())),
    }
}

/// Follow a dotted `path` into a JSON value and read it as a number (missing / non-numeric → None).
fn dig_number(v: &Value, path: &str) -> Option<f64> {
    let mut cur = v;
    for seg in path.split('.').filter(|s| !s.is_empty()) {
        cur = cur.get(seg)?;
    }
    cur.as_f64()
}

fn log_step(opts: &RunOptions, msg: &str) {
    if opts.log {
        eprintln!("{msg}");
    }
}

/// Poll `resolve` until the locator is found or the timeout elapses.
async fn wait_resolved<B: RecipeBrowser>(
    browser: &B,
    spec: &Value,
    timeout_ms: u64,
) -> Result<(), RecipeError> {
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        let r = browser
            .resolve(spec)
            .await
            .map_err(|e| RecipeError::retryable(e.to_string()))?;
        if r.found {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(RecipeError::retryable(format!(
                "locator not found within {timeout_ms}ms"
            )));
        }
        tokio::time::sleep(Duration::from_millis(POLL_INTERVAL_MS)).await;
    }
}

async fn condition_holds<B: RecipeBrowser>(
    cond: &Condition,
    browser: &B,
    vars: &BTreeMap<String, String>,
) -> Result<bool, RecipeError> {
    match cond {
        Condition::SettledUrlMatches { pattern } => {
            let p = interpolate(pattern, vars);
            let js = format!(
                "new RegExp({}).test(location.href)",
                serde_json::to_string(&p).unwrap_or_default()
            );
            browser
                .eval_bool(&js)
                .await
                .map_err(|e| RecipeError::retryable(e.to_string()))
        }
        Condition::Visible { locator } => {
            let r = browser
                .resolve(&locator_spec(locator))
                .await
                .map_err(|e| RecipeError::retryable(e.to_string()))?;
            Ok(r.found)
        }
        Condition::ValueMatches {
            value,
            equals,
            not_equals,
            non_empty,
        } => {
            // Needs no page: this is what lets a token-only `api` recipe assert on what it
            // captured. A null capture renders as "null"/"" and therefore FAILS a non-empty
            // or equals check, which is the behaviour verification depends on.
            let got = interpolate(value, vars);
            let got = got.trim();
            if *non_empty && (got.is_empty() || got == "null") {
                return Ok(false);
            }
            if let Some(want) = equals {
                if got != interpolate(want, vars).trim() {
                    return Ok(false);
                }
            }
            if let Some(want) = not_equals {
                if got == interpolate(want, vars).trim() {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        Condition::TextMatches { pattern, locator } => {
            let r = browser
                .resolve(&locator_spec(locator))
                .await
                .map_err(|e| RecipeError::retryable(e.to_string()))?;
            let Some(text) = r.text else { return Ok(false) };
            let p = interpolate(pattern, vars);
            let js = format!(
                "new RegExp({}).test({})",
                serde_json::to_string(&p).unwrap_or_default(),
                serde_json::to_string(&text).unwrap_or_default()
            );
            browser
                .eval_bool(&js)
                .await
                .map_err(|e| RecipeError::retryable(e.to_string()))
        }
    }
}

/// Run a recipe's steps. Returns the accumulated result and any unmet expectations.
pub async fn run<B: RecipeBrowser>(
    recipe: &Recipe,
    provided: &BTreeMap<String, String>,
    browser: &B,
    opts: &RunOptions,
    solver: Option<&dyn Solver>,
) -> Result<Outcome, RecipeError> {
    let vars = bind_vars(recipe, provided)?;
    browser
        .inject_init_script(LOCATORS_JS)
        .await
        .map_err(|e| RecipeError::terminal(format!("injecting locator runtime: {e}")))?;

    let mut result = Map::new();
    let mut unexpected = Vec::new();
    let mut downloads = Vec::new();

    for (i, step) in recipe.steps.iter().enumerate() {
        if let Err(mut e) = run_step(
            i + 1,
            step,
            &vars,
            browser,
            opts,
            solver,
            &mut result,
            &mut unexpected,
            &mut downloads,
        )
        .await
        {
            // Attribute the failure to its step so a repair loop knows what to look at, and keep
            // the soft misses noted so far — they are evidence too.
            e.step_index = Some(i);
            e.step = Some(summarize_step(step));
            e.unexpected = unexpected.clone();
            return Err(e);
        }
    }

    // Last phase: render + write the recipe's output files (only reached on a fully successful
    // run — a tripped/failed run returns Err above and writes nothing). Under --repair, an
    // `unexpected` run also skips writing so a stale note isn't overwritten from a bad extract.
    if opts.repair && !unexpected.is_empty() {
        log_step(
            opts,
            "skipping output writes: unexpected result under --repair",
        );
    } else {
        let written = crate::recipe::render::write_outputs(recipe, &vars, &result)
            .map_err(|e| RecipeError::terminal(format!("writing outputs: {e}")))?;
        for p in &written {
            log_step(opts, &format!("wrote {p}"));
        }
    }

    Ok(Outcome {
        result: Value::Object(result),
        unexpected,
        downloads,
    })
}

/// Run one step (1-based `n` for the trace). Captures go to `result`, soft misses to `unexpected`,
/// saved files to `downloads`; an `Err` is the step's failure, which [`run`] attributes to it.
#[allow(clippy::too_many_arguments)]
async fn run_step<B: RecipeBrowser>(
    n: usize,
    step: &Step,
    vars: &BTreeMap<String, String>,
    browser: &B,
    opts: &RunOptions,
    solver: Option<&dyn Solver>,
    result: &mut Map<String, Value>,
    unexpected: &mut Vec<String>,
    downloads: &mut Vec<Download>,
) -> Result<(), RecipeError> {
    match step {
        Step::Goto { url } => {
            let u = interpolate(url, &vars);
            let nav = browser
                .goto(&u)
                .await
                .map_err(|e| RecipeError::retryable(e.to_string()))?;
            log_step(
                opts,
                &format!("step {n} goto {} → \"{}\"", nav.url, nav.title),
            );
        }
        Step::Reload => {
            let nav = browser
                .reload()
                .await
                .map_err(|e| RecipeError::retryable(e.to_string()))?;
            log_step(
                opts,
                &format!("step {n} reload {} → \"{}\"", nav.url, nav.title),
            );
        }
        Step::Expect {
            on_fail,
            message,
            condition,
        } => {
            if condition_holds(condition, browser, &vars).await? {
                let msg = interpolate(message, &vars);
                log_step(
                    opts,
                    &format!("step {n} expect → TRIPPED ({on_fail:?}): {msg}"),
                );
                return Err(RecipeError::classed(*on_fail, msg));
            }
            log_step(opts, &format!("step {n} expect → ok"));
        }
        Step::Extract {
            key,
            many,
            locator,
            expect,
        } => {
            let spec = locator_spec(locator);
            wait_resolved(browser, &spec, opts.step_timeout_ms).await?;
            let val = browser
                .extract(&spec, *many)
                .await
                .map_err(|e| RecipeError::retryable(e.to_string()))?;
            if let Some(msg) = check_expectation(expect, &val, *many, key) {
                log_step(opts, &format!("step {n} extract {key} → UNEXPECTED: {msg}"));
                unexpected.push(msg);
            } else {
                log_step(
                    opts,
                    &format!(
                        "step {n} extract {key} → {} item(s)",
                        cardinality(&val, *many)
                    ),
                );
            }
            result.insert(key.clone(), val);
        }
        Step::Wait {
            locator,
            timeout_ms,
        } => {
            let spec = locator_spec(locator);
            wait_resolved(browser, &spec, timeout_ms.unwrap_or(opts.step_timeout_ms)).await?;
            log_step(opts, &format!("step {n} wait → present"));
        }
        Step::Screenshot { key } => {
            let b64 = browser
                .screenshot()
                .await
                .map_err(|e| RecipeError::retryable(e.to_string()))?;
            result.insert(key.clone(), Value::String(b64));
            log_step(opts, &format!("step {n} screenshot {key} → captured"));
        }
        Step::Eval {
            key,
            js,
            retry_if_positive,
        } => {
            let j = interpolate(js, &vars);
            let val = browser
                .eval(&j)
                .await
                .map_err(|e| RecipeError::retryable(format!("eval failed: {e}")))?;
            let retry = retry_if_positive
                .as_deref()
                .and_then(|p| dig_number(&val, p).map(|x| (p, x)))
                .filter(|(_, x)| *x > 0.0);
            log_step(opts, &format!("step {n} eval {key} → captured"));
            result.insert(key.clone(), val);
            if let Some((p, x)) = retry {
                return Err(RecipeError::retryable(format!(
                    "step {n} eval {key}: {p} = {x} (> 0), retrying"
                )));
            }
        }
        Step::Tab {
            action,
            url_contains,
        } => {
            let uc = url_contains.as_ref().map(|s| interpolate(s, &vars));
            browser
                .tab(action, uc.as_deref())
                .await
                .map_err(|e| RecipeError::retryable(format!("tab {action} failed: {e}")))?;
            log_step(opts, &format!("step {n} tab {action} → done"));
        }
        Step::Click { locator } => {
            let spec = locator_spec(locator);
            wait_resolved(browser, &spec, opts.step_timeout_ms).await?;
            browser
                .click(&spec)
                .await
                .map_err(|e| RecipeError::retryable(e.to_string()))?;
            log_step(opts, &format!("step {n} click → done"));
        }
        Step::Fill { value, locator } => {
            let spec = locator_spec(locator);
            wait_resolved(browser, &spec, opts.step_timeout_ms).await?;
            let v = interpolate(value, &vars);
            browser
                .fill(&spec, &v)
                .await
                .map_err(|e| RecipeError::retryable(e.to_string()))?;
            log_step(opts, &format!("step {n} fill → done"));
        }
        Step::Solve {
            prompt,
            locator,
            key,
        } => {
            let spec = locator_spec(locator);
            wait_resolved(browser, &spec, opts.step_timeout_ms).await?;
            let p = interpolate(prompt, &vars);
            let shot = browser
                .screenshot()
                .await
                .map_err(|e| RecipeError::retryable(e.to_string()))?;
            let solver = solver.ok_or_else(|| {
                RecipeError::terminal(
                    "recipe has a `solve` step but no solver is configured — pacewright wires \
                         Claude vision behind it; a bare `chrome-agent recipe run` has none",
                )
            })?;
            let answer = solver
                .solve(&shot, &p)
                .await
                .map_err(|e| RecipeError::retryable(format!("solver failed: {e}")))?;
            browser
                .fill(&spec, &answer)
                .await
                .map_err(|e| RecipeError::retryable(e.to_string()))?;
            if let Some(k) = key {
                result.insert(k.clone(), Value::String(answer));
            }
            log_step(opts, &format!("step {n} solve → filled"));
        }
        Step::Insert { value, locator } => {
            let spec = locator_spec(locator);
            wait_resolved(browser, &spec, opts.step_timeout_ms).await?;
            let v = interpolate(value, &vars);
            browser
                .insert(&spec, &v)
                .await
                .map_err(|e| RecipeError::retryable(e.to_string()))?;
            log_step(opts, &format!("step {n} insert → done"));
        }
        Step::Select { value, locator } => {
            let spec = locator_spec(locator);
            wait_resolved(browser, &spec, opts.step_timeout_ms).await?;
            let v = interpolate(value, &vars);
            browser
                .select(&spec, &v)
                .await
                .map_err(|e| RecipeError::retryable(e.to_string()))?;
            log_step(opts, &format!("step {n} select {v} → done"));
        }
        Step::Upload { path, locator } => {
            let spec = locator_spec(locator);
            wait_resolved(browser, &spec, opts.step_timeout_ms).await?;
            let p = interpolate(path, &vars);
            browser
                .upload(&spec, std::slice::from_ref(&p))
                .await
                .map_err(|e| RecipeError::retryable(e.to_string()))?;
            log_step(opts, &format!("step {n} upload {p} → done"));
        }
        Step::Download {
            url,
            out,
            timeout_secs,
            key,
        } => {
            let u = interpolate(url, &vars);
            let o = interpolate(out, &vars);
            // Videos are large; default generous. A recipe can override with `timeout=<secs>`.
            let t = timeout_secs.unwrap_or(600);
            match browser.download(&u, &o, t).await {
                Ok(true) => {
                    log_step(opts, &format!("step {n} download → {o}"));
                    let k = key.clone().unwrap_or_else(|| {
                        std::path::Path::new(&o)
                            .file_name()
                            .map(|f| f.to_string_lossy().into_owned())
                            .unwrap_or_else(|| o.clone())
                    });
                    downloads.push(Download { key: k, path: o });
                }
                // Not rendered yet — retryable so the task polls again (backoff ramps to ~hourly).
                Ok(false) => {
                    log_step(opts, &format!("step {n} download → not ready yet"));
                    return Err(RecipeError::retryable(format!("download not ready: {u}")));
                }
                Err(e) => return Err(RecipeError::retryable(format!("download failed: {e}"))),
            }
        }
        Step::Request(req) => {
            let url = interpolate(&req.url, &vars);
            let headers: Vec<(String, String)> = req
                .headers
                .iter()
                .map(|(k, v)| (k.clone(), interpolate(v, &vars)))
                .collect();
            let body = req.body.as_ref().map(|b| interpolate(b, &vars));
            let resp = browser
                .request(&req.method, &url, &headers, body.as_deref())
                .await
                .map_err(|e| RecipeError::retryable(e.to_string()))?;
            if let Some(expected) = req.expect_status
                && resp.status != expected
            {
                return Err(RecipeError::retryable(format!(
                    "request {} {} → status {} (expected {expected})",
                    req.method, url, resp.status
                )));
            }
            let captured = capture_response(&resp.body, req.capture_path.as_deref());
            result.insert(req.capture_key.clone(), captured);
            log_step(
                opts,
                &format!(
                    "step {n} request {} {url} → {} ({})",
                    req.method, resp.status, req.capture_key
                ),
            );
        }
        Step::Api(req) => {
            let url = interpolate(&req.url, &vars);
            let mut headers: Vec<(String, String)> = req
                .headers
                .iter()
                .map(|(k, v)| (k.clone(), interpolate(v, &vars)))
                .collect();
            // The bearer token is injected as an Authorization header (kept out of the recipe
            // file — pacewright fills the `{{ token }}` var at run time).
            if let Some(bearer) = &req.bearer {
                headers.push((
                    "Authorization".into(),
                    format!("Bearer {}", interpolate(bearer, &vars)),
                ));
            }
            let body = req.body.as_ref().map(|b| interpolate(b, &vars));
            let resp = browser
                .api_request(&req.method, &url, &headers, body.as_deref())
                .await
                .map_err(|e| RecipeError::retryable(e.to_string()))?;
            if let Some(expected) = req.expect_status
                && resp.status != expected
            {
                return Err(RecipeError::retryable(format!(
                    "api {} {} → status {} (expected {expected})",
                    req.method, url, resp.status
                )));
            }
            // Capture a response header (e.g. LinkedIn's `x-restli-id` share URN) or a JSON body
            // sub-path. A header capture wins when both are set.
            let captured = if let Some(name) = &req.capture_header {
                resp.headers
                    .iter()
                    .find(|(k, _)| k.eq_ignore_ascii_case(name))
                    .map_or(Value::Null, |(_, v)| Value::String(v.clone()))
            } else {
                capture_response(&resp.body, req.capture_path.as_deref())
            };
            result.insert(req.capture_key.clone(), captured);
            log_step(
                opts,
                &format!(
                    "step {n} api {} {url} → {} ({})",
                    req.method, resp.status, req.capture_key
                ),
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn capture_response_indexes_arrays_by_numeric_segment() {
        // Verifying a video's privacy requires walking into `items[0]`. Before numeric
        // segments this silently captured null, which a caller reads as "fine".
        let body =
            r#"{"items":[{"status":{"privacyStatus":"unlisted"}}],"pageInfo":{"totalResults":1}}"#;
        assert_eq!(
            super::capture_response(body, Some("items.0.status.privacyStatus")),
            serde_json::json!("unlisted")
        );
        assert_eq!(
            super::capture_response(body, Some("pageInfo.totalResults")),
            serde_json::json!(1)
        );
    }

    #[test]
    fn capture_response_is_null_for_an_absent_item() {
        // YouTube answers 200 with empty `items` for an unknown id, so this is the shape
        // an "it was never published" response actually takes.
        let body = r#"{"items":[],"pageInfo":{"totalResults":0}}"#;
        assert_eq!(
            super::capture_response(body, Some("items.0.status.privacyStatus")),
            serde_json::Value::Null
        );
        assert_eq!(
            super::capture_response(body, Some("pageInfo.totalResults")),
            serde_json::json!(0)
        );
    }

    use super::{RunOptions, run};
    use crate::recipe::browser::{
        ApiResponse, HttpResponse, Resolved,
        fake::{Action, FakeBrowser},
    };
    use crate::recipe::model::{ErrorClass, Recipe};
    use serde_json::{Value, json};
    use std::collections::BTreeMap;

    fn recipe(src: &str) -> Recipe {
        Recipe::parse(src).unwrap()
    }
    fn novars() -> BTreeMap<String, String> {
        BTreeMap::new()
    }
    fn opts() -> RunOptions {
        RunOptions {
            log: false,
            step_timeout_ms: 0,
            repair: false,
        }
    }

    #[tokio::test]
    async fn injects_runtime_and_navigates_with_interpolated_var() {
        let r = recipe(
            r#"recipe "x/y" {
                var "url" default="https://a.test/"
                step { goto "{{ url }}" }
            }"#,
        );
        let fb = FakeBrowser::new();
        let out = run(&r, &novars(), &fb, &opts(), None).await.unwrap();
        assert!(out.result.as_object().unwrap().is_empty());
        assert_eq!(fb.injected.borrow().len(), 1, "locators.js injected once");
        assert!(fb.injected.borrow()[0].contains("__pw"));
        assert_eq!(fb.gotos.borrow().as_slice(), ["https://a.test/"]);
    }

    #[tokio::test]
    async fn extract_accumulates_into_result() {
        let r = recipe(
            r#"recipe "x/y" {
            step { extract "name" { locator role="heading" } }
            step { extract "tags" many=#true { locator css=".t" } }
        }"#,
        );
        let fb = FakeBrowser::new().extractor(|_spec, many| {
            if many {
                json!(["a", "b"])
            } else {
                json!("Jane")
            }
        });
        let out = run(&r, &novars(), &fb, &opts(), None).await.unwrap();
        assert_eq!(out.result["name"], json!("Jane"));
        assert_eq!(out.result["tags"], json!(["a", "b"]));
        assert!(out.unexpected.is_empty());
    }

    #[tokio::test]
    async fn expect_tripwire_maps_to_error_class() {
        let r = recipe(
            r#"recipe "x/y" {
            step { expect on-fail="terminal" message="auth wall" { settled-url-matches "/login/" } }
        }"#,
        );
        let fb = FakeBrowser::new().bool_eval(|_| true); // condition holds → tripped
        let err = run(&r, &novars(), &fb, &opts(), None).await.unwrap_err();
        assert_eq!(err.class, ErrorClass::Terminal);
        assert_eq!(err.message, "auth wall");
    }

    #[tokio::test]
    async fn failure_is_attributed_to_its_step_and_keeps_earlier_unexpected() {
        let r = recipe(
            r#"recipe "x/y" {
            step { goto "https://a.test/" }
            step { extract "rows" many=#true expect-min=1 { locator css=".row" } }
            step { click { locator role="button" text="Consultar" { within role="form" } } }
        }"#,
        );
        // extract finds nothing (→ unexpected, not a failure); the click's locator never resolves
        let fb = FakeBrowser::new()
            .extractor(|_, _| json!([]))
            .resolver(|spec| Resolved {
                found: spec.get("role") != Some(&json!("button")),
                text: None,
            });
        let err = run(&r, &novars(), &fb, &opts(), None).await.unwrap_err();
        assert_eq!(err.class, ErrorClass::Retryable);
        assert_eq!(err.step_index, Some(2), "0-based index of the failing step");
        assert_eq!(
            err.step.as_deref(),
            Some(r#"click { locator role="button" text="Consultar" { within role="form" } }"#)
        );
        assert_eq!(
            err.unexpected.len(),
            1,
            "the extract's miss is carried on the error"
        );
        assert!(err.unexpected[0].contains("rows"));
        // Display is still the `[Class] message` the runner classifies on
        assert!(err.to_string().starts_with("[Retryable] "));
        assert_eq!(fb.gotos.borrow().len(), 1);
    }

    #[tokio::test]
    async fn pre_step_failures_have_no_step() {
        let r = recipe(
            r#"recipe "x/y" {
            var "needed" required=#true
            step { goto "{{ needed }}" }
        }"#,
        );
        let err = run(&r, &novars(), &FakeBrowser::new(), &opts(), None)
            .await
            .unwrap_err();
        assert_eq!(err.class, ErrorClass::Terminal);
        assert_eq!(err.step_index, None);
        assert_eq!(err.step, None);
    }

    #[tokio::test]
    async fn download_steps_are_reported_keyed_by_key_or_file_name() {
        let r = recipe(
            r#"recipe "x/y" {
            var "folio" default="F-1"
            step { download url="https://a.test/x.xml" out="/tmp/out/{{ folio }}.xml" key="xml" }
            step { download url="https://a.test/x.pdf" out="/tmp/out/{{ folio }}.pdf" }
        }"#,
        );
        let out = run(&r, &novars(), &FakeBrowser::new(), &opts(), None)
            .await
            .unwrap();
        assert_eq!(
            out.downloads,
            vec![
                super::Download {
                    key: "xml".into(),
                    path: "/tmp/out/F-1.xml".into()
                },
                super::Download {
                    key: "F-1.pdf".into(),
                    path: "/tmp/out/F-1.pdf".into()
                },
            ]
        );
        // a recipe without downloads reports none
        let r = recipe(r#"recipe "x/y" { step { goto "https://a.test/" } }"#);
        let out = run(&r, &novars(), &FakeBrowser::new(), &opts(), None)
            .await
            .unwrap();
        assert!(out.downloads.is_empty());
    }

    #[test]
    fn summarize_step_renders_kdl_like_lines() {
        use super::summarize_step;
        let r = recipe(
            r#"recipe "x/y" {
            var "rfc" default="X"
            step { goto "https://a.test/" }
            step { fill "{{ rfc }}" { locator label="RFC" } }
            step { expect on-fail="terminal" message="auth wall" { settled-url-matches "/login/" } }
            step { extract "name" { locator role="heading" nth=2 } }
            step { tab "follow" url-contains="pdf" }
            step { api "get" url="https://api.test/v1" { capture "r" } }
        }"#,
        );
        let lines: Vec<String> = r.steps.iter().map(summarize_step).collect();
        assert_eq!(lines[0], r#"goto "https://a.test/""#);
        assert_eq!(lines[1], r#"fill "{{ rfc }}" { locator label="RFC" }"#);
        assert!(lines[2].starts_with(r#"expect on-fail="terminal" message="auth wall" {"#));
        assert!(lines[2].contains("settled-url-matches"));
        assert_eq!(
            lines[3],
            r#"extract "name" { locator role="heading" nth=2 }"#
        );
        assert_eq!(lines[4], r#"tab "follow" url-contains="pdf""#);
        assert_eq!(lines[5], r#"api "GET" url="https://api.test/v1""#);
    }

    #[tokio::test]
    async fn expect_not_tripped_continues() {
        let r = recipe(
            r#"recipe "x/y" {
            step { expect on-fail="terminal" message="m" { settled-url-matches "/login/" } }
            step { extract "ok" { locator role="heading" } }
        }"#,
        );
        let fb = FakeBrowser::new()
            .bool_eval(|_| false)
            .extractor(|_, _| json!("here"));
        let out = run(&r, &novars(), &fb, &opts(), None).await.unwrap();
        assert_eq!(out.result["ok"], json!("here"));
    }

    #[tokio::test]
    async fn empty_extract_over_min_is_unexpected_not_failure() {
        let r = recipe(
            r#"recipe "x/y" {
            step { extract "stories" many=#true expect-min=1 { locator css=".athing" } }
        }"#,
        );
        let fb = FakeBrowser::new().extractor(|_, _| json!([]));
        let out = run(&r, &novars(), &fb, &opts(), None).await.unwrap();
        assert_eq!(out.result["stories"], json!([]));
        assert_eq!(out.unexpected.len(), 1);
        assert!(out.unexpected[0].contains("stories"));
    }

    #[tokio::test]
    async fn non_many_empty_is_unexpected_by_default() {
        let r = recipe(r#"recipe "x/y" { step { extract "name" { locator role="heading" } } }"#);
        let fb = FakeBrowser::new().extractor(|_, _| Value::Null);
        let out = run(&r, &novars(), &fb, &opts(), None).await.unwrap();
        assert_eq!(out.unexpected.len(), 1);
    }

    #[tokio::test]
    async fn locator_never_found_times_out_retryable() {
        let r = recipe(r#"recipe "x/y" { step { extract "x" { locator role="heading" } } }"#);
        let fb = FakeBrowser::new().resolver(|_| Resolved {
            found: false,
            text: None,
        });
        let err = run(&r, &novars(), &fb, &opts(), None).await.unwrap_err();
        assert_eq!(err.class, ErrorClass::Retryable);
        assert!(err.message.contains("not found"));
    }

    #[tokio::test]
    async fn write_verbs_drive_the_browser_in_order() {
        let r = recipe(
            r##"recipe "x/y" {
                step { click { locator role="button" name="Go" } }
                step { fill "hello" { locator css="#name" } }
                step { select "US" { locator css="#country" } }
                step { upload "/tmp/a.png" { locator css="input[type=file]" } }
            }"##,
        );
        let fb = FakeBrowser::new();
        run(&r, &novars(), &fb, &opts(), None).await.unwrap();
        assert_eq!(
            fb.actions.borrow().as_slice(),
            [
                Action::Click,
                Action::Fill("hello".into()),
                Action::Select("US".into()),
                Action::Upload(vec!["/tmp/a.png".into()]),
            ]
        );
    }

    #[tokio::test]
    async fn reload_and_insert_drive_the_browser_in_order() {
        let r = recipe(
            r##"recipe "x/y" {
                var "desc" default="a multi word description"
                step { reload }
                step { insert "{{ desc }}" { locator css="[data-slate-editor=true]" } }
            }"##,
        );
        let fb = FakeBrowser::new();
        run(&r, &novars(), &fb, &opts(), None).await.unwrap();
        assert_eq!(
            fb.actions.borrow().as_slice(),
            [
                Action::Reload,
                Action::Insert("a multi word description".into()),
            ]
        );
    }

    #[tokio::test]
    async fn insert_times_out_when_locator_absent() {
        let r = recipe(
            r#"recipe "x/y" {
                step { insert "hi" { locator css=".ed" } }
            }"#,
        );
        let fb = FakeBrowser::new().resolver(|_| Resolved {
            found: false,
            text: None,
        });
        let err = run(&r, &novars(), &fb, &opts(), None).await.unwrap_err();
        assert_eq!(err.class, ErrorClass::Retryable);
        assert!(fb.actions.borrow().is_empty(), "never inserted");
    }

    #[tokio::test]
    async fn fill_interpolates_its_value() {
        let r = recipe(
            r##"recipe "x/y" {
                var "greeting" default="hi there"
                step { fill "{{ greeting }}" { locator css="#name" } }
            }"##,
        );
        let fb = FakeBrowser::new();
        run(&r, &novars(), &fb, &opts(), None).await.unwrap();
        assert_eq!(
            fb.actions.borrow().as_slice(),
            [Action::Fill("hi there".into())]
        );
    }

    // A solver that records what it was shown and returns a canned answer — proves the engine
    // screenshots the page, hands the image + interpolated prompt to the solver, and types the reply.
    struct FakeSolver {
        answer: String,
        seen: std::cell::RefCell<Option<(String, String)>>,
    }
    impl super::Solver for FakeSolver {
        fn solve<'a>(
            &'a self,
            image_b64: &'a str,
            prompt: &'a str,
        ) -> std::pin::Pin<Box<dyn Future<Output = Result<String, crate::BoxError>> + 'a>> {
            Box::pin(async move {
                *self.seen.borrow_mut() = Some((image_b64.to_string(), prompt.to_string()));
                Ok(self.answer.clone())
            })
        }
    }

    #[tokio::test]
    async fn solve_screenshots_asks_solver_and_fills_answer() {
        let r = recipe(
            r##"recipe "x/y" {
                var "kind" default="captcha"
                step { solve "read the {{ kind }}" key="answer" { locator css="#c" } }
            }"##,
        );
        let fb = FakeBrowser::new();
        let solver = FakeSolver {
            answer: "AB12".into(),
            seen: std::cell::RefCell::new(None),
        };
        let out = run(&r, &novars(), &fb, &opts(), Some(&solver))
            .await
            .unwrap();
        // Typed the solver's answer into the locator, and captured it under `key`.
        assert_eq!(
            fb.actions.borrow().as_slice(),
            [Action::Fill("AB12".into())]
        );
        assert_eq!(out.result["answer"], json!("AB12"));
        // The solver saw the page screenshot and the interpolated prompt.
        let seen = solver.seen.borrow();
        let (image, prompt) = seen.as_ref().unwrap();
        assert_eq!(
            image, "ZmFrZQ==",
            "the page screenshot is handed to the solver"
        );
        assert_eq!(prompt, "read the captcha", "prompt is interpolated");
    }

    #[tokio::test]
    async fn solve_without_a_configured_solver_is_terminal() {
        let r = recipe(r##"recipe "x/y" { step { solve "x" { locator css="#c" } } }"##);
        let fb = FakeBrowser::new();
        let err = run(&r, &novars(), &fb, &opts(), None).await.unwrap_err();
        assert_eq!(err.class, ErrorClass::Terminal);
        assert!(err.message.contains("no solver"), "got: {}", err.message);
        assert!(
            fb.actions.borrow().is_empty(),
            "never filled without an answer"
        );
    }

    #[tokio::test]
    async fn write_verb_times_out_when_locator_absent() {
        let r = recipe(r#"recipe "x/y" { step { click { locator role="button" } } }"#);
        let fb = FakeBrowser::new().resolver(|_| Resolved {
            found: false,
            text: None,
        });
        let err = run(&r, &novars(), &fb, &opts(), None).await.unwrap_err();
        assert_eq!(err.class, ErrorClass::Retryable);
        assert!(fb.actions.borrow().is_empty(), "never acted");
    }

    #[tokio::test]
    async fn request_captures_a_json_subpath() {
        let r = recipe(
            r#"recipe "x/y" {
                step {
                    request "POST" url="https://api.test/items" expect-status=200 {
                        header "Content-Type" "application/json"
                        body "{\"a\":1}"
                        capture "created" path="data.id"
                    }
                }
            }"#,
        );
        let fb = FakeBrowser::new().responder(|_m, _u, _b| HttpResponse {
            status: 200,
            body: r#"{"data":{"id":"abc"}}"#.into(),
        });
        let out = run(&r, &novars(), &fb, &opts(), None).await.unwrap();
        assert_eq!(out.result["created"], json!("abc"));
        assert_eq!(
            fb.actions.borrow().as_slice(),
            [Action::Request {
                method: "POST".into(),
                url: "https://api.test/items".into(),
            }]
        );
    }

    #[tokio::test]
    async fn request_status_mismatch_is_retryable() {
        let r = recipe(
            r#"recipe "x/y" {
                step {
                    request "GET" url="https://api.test/x" expect-status=200 {
                        capture "x"
                    }
                }
            }"#,
        );
        let fb = FakeBrowser::new().responder(|_m, _u, _b| HttpResponse {
            status: 500,
            body: "boom".into(),
        });
        let err = run(&r, &novars(), &fb, &opts(), None).await.unwrap_err();
        assert_eq!(err.class, ErrorClass::Retryable);
        assert!(err.message.contains("500"));
    }

    #[tokio::test]
    async fn api_captures_response_header() {
        // Mirrors the LinkedIn Posts API: the created share URN comes back in `x-restli-id`.
        let r = recipe(
            r#"recipe "x/y" {
                var "token" required=#true
                step {
                    api "POST" url="https://api.linkedin.com/rest/posts" bearer="{{ token }}" expect-status=201 {
                        body "{\"commentary\":\"hi\"}"
                        capture "post_urn" header="x-restli-id"
                    }
                }
            }"#,
        );
        let fb = FakeBrowser::new().api_responder(|_m, _u, headers, _b| {
            // The engine must have folded bearer into an Authorization header.
            assert!(
                headers
                    .iter()
                    .any(|(k, v)| k == "Authorization" && v == "Bearer t0ken")
            );
            ApiResponse {
                status: 201,
                body: "{}".into(),
                headers: vec![("x-restli-id".into(), "urn:li:share:123".into())],
            }
        });
        let vars = BTreeMap::from([("token".to_string(), "t0ken".to_string())]);
        let out = run(&r, &vars, &fb, &opts(), None).await.unwrap();
        assert_eq!(out.result["post_urn"], json!("urn:li:share:123"));
        assert_eq!(
            fb.actions.borrow().as_slice(),
            [Action::Api {
                method: "POST".into(),
                url: "https://api.linkedin.com/rest/posts".into(),
            }]
        );
    }

    #[tokio::test]
    async fn native_browser_rejects_page_steps() {
        // A page step run over the browser-less NativeBrowser fails clearly (defense in depth — the
        // runner only picks NativeBrowser when `needs_browser()` is false).
        let r = recipe(r#"recipe "x/y" { step { goto "https://a.test/" } }"#);
        let nb = crate::recipe::browser::NativeBrowser::new();
        let err = run(&r, &novars(), &nb, &opts(), None).await.unwrap_err();
        assert!(
            err.message.to_lowercase().contains("browser"),
            "got: {}",
            err.message
        );
    }

    #[tokio::test]
    async fn api_captures_body_json_path() {
        let r = recipe(
            r#"recipe "x/y" {
                step { api "GET" url="https://api.test/me" { capture "sub" path="sub" } }
            }"#,
        );
        let fb = FakeBrowser::new().api_responder(|_m, _u, _h, _b| ApiResponse {
            status: 200,
            body: r#"{"sub":"ACoAA1"}"#.into(),
            headers: vec![],
        });
        let out = run(&r, &novars(), &fb, &opts(), None).await.unwrap();
        assert_eq!(out.result["sub"], json!("ACoAA1"));
    }

    #[tokio::test]
    async fn api_status_mismatch_is_retryable() {
        let r = recipe(
            r#"recipe "x/y" {
                step { api "POST" url="https://api.test/x" expect-status=201 { capture "x" } }
            }"#,
        );
        let fb = FakeBrowser::new().api_responder(|_m, _u, _h, _b| ApiResponse {
            status: 422,
            body: "bad".into(),
            headers: vec![],
        });
        let err = run(&r, &novars(), &fb, &opts(), None).await.unwrap_err();
        assert_eq!(err.class, ErrorClass::Retryable);
        assert!(err.message.contains("422"));
    }

    #[tokio::test]
    async fn eval_captures_json_result() {
        let r = recipe(
            r#"recipe "x/y" {
                step { eval "summary" js="(async()=>({published:2,pending:0}))()" }
            }"#,
        );
        let fb = FakeBrowser::new().json_eval(|_js| json!({"published": 2, "pending": 0}));
        let out = run(&r, &novars(), &fb, &opts(), None).await.unwrap();
        assert_eq!(out.result["summary"], json!({"published": 2, "pending": 0}));
    }

    #[tokio::test]
    async fn eval_retries_when_positive_path_is_gt_zero() {
        let r = recipe(
            r#"recipe "x/y" {
                step { eval "yt" js="run()" retry-if-positive="pending" }
            }"#,
        );
        // Two clips still rendering → pending=2 → the step must fail retryable so the daemon polls.
        let fb = FakeBrowser::new().json_eval(|_js| json!({"published": 1, "pending": 2}));
        let err = run(&r, &novars(), &fb, &opts(), None).await.unwrap_err();
        assert_eq!(err.class, ErrorClass::Retryable);
        assert!(err.message.contains("pending"));
    }

    #[tokio::test]
    async fn eval_does_not_retry_when_positive_path_is_zero() {
        let r = recipe(
            r#"recipe "x/y" {
                step { eval "yt" js="run()" retry-if-positive="pending" }
            }"#,
        );
        let fb = FakeBrowser::new().json_eval(|_js| json!({"published": 3, "pending": 0}));
        let out = run(&r, &novars(), &fb, &opts(), None).await.unwrap();
        assert_eq!(out.result["yt"], json!({"published": 3, "pending": 0}));
    }

    #[tokio::test]
    async fn tab_steps_drive_the_browser_in_order() {
        let r = recipe(
            r#"recipe "x/y" {
                step { tab "follow" url-contains="spotify.com" }
                step { fill "hi" { locator css="input" } }
                step { tab "back" }
            }"#,
        );
        let fb = FakeBrowser::new();
        run(&r, &novars(), &fb, &opts(), None).await.unwrap();
        assert_eq!(
            fb.actions.borrow().as_slice(),
            [
                Action::Tab {
                    action: "follow".into()
                },
                Action::Fill("hi".into()),
                Action::Tab {
                    action: "back".into()
                },
            ]
        );
    }

    #[tokio::test]
    async fn request_captures_whole_body_when_no_path() {
        let r = recipe(
            r#"recipe "x/y" {
                step {
                    request "GET" url="https://api.test/x" { capture "resp" }
                }
            }"#,
        );
        let fb = FakeBrowser::new().responder(|_m, _u, _b| HttpResponse {
            status: 200,
            body: r#"{"ok":true}"#.into(),
        });
        let out = run(&r, &novars(), &fb, &opts(), None).await.unwrap();
        assert_eq!(out.result["resp"], json!({"ok": true}));
    }

    #[tokio::test]
    async fn missing_required_var_is_terminal() {
        let r = recipe(
            r#"recipe "x/y" {
                var "url" required=#true
                step { goto "{{ url }}" }
            }"#,
        );
        let err = run(&r, &novars(), &FakeBrowser::new(), &opts(), None)
            .await
            .unwrap_err();
        assert_eq!(err.class, ErrorClass::Terminal);
        assert!(err.message.contains("url"));
    }
}

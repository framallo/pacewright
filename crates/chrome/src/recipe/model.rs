//! Typed recipe model, parsed from a `kdl::KdlDocument` by walking nodes (no derive macro).
//!
//! The model is `Serialize` so `recipe check --json` can emit the fully-parsed recipe — useful
//! for a consumer (pacewright) to introspect a recipe's vars/limit-keys/outputs without a browser.
//! Parsing rejects malformed values eagerly (bad `on-fail` class, non-numeric `expect-*`, unknown
//! `output` format); a final `validate()` pass checks every `{{ … }}` reference resolves.

use std::collections::BTreeSet;

use kdl::{KdlDocument, KdlNode, KdlValue};
use serde::Serialize;

use crate::BoxError;

/// The pacewright error class a recipe failure maps to. Assigned by an `expect on-fail="…"`
/// guard, or by the engine (locator timeout → `Retryable`, malformed recipe/var → `Terminal`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorClass {
    Terminal,
    Retryable,
    RateLimited,
}

impl ErrorClass {
    /// Parse an `on-fail` value.
    pub fn parse(s: &str) -> Result<Self, BoxError> {
        match s {
            "terminal" => Ok(Self::Terminal),
            "retryable" => Ok(Self::Retryable),
            "rate_limited" => Ok(Self::RateLimited),
            other => Err(format!(
                "invalid on-fail class {other:?} (expected terminal|retryable|rate_limited)"
            )
            .into()),
        }
    }
}

/// A declared input variable.
#[derive(Debug, Clone, Serialize)]
pub struct Var {
    pub name: String,
    pub required: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub doc: Option<String>,
    /// Alias: a consumer may bind this var from a differently-named source field
    /// (e.g. a vault note's `linkedin` frontmatter → the recipe's `url` var). The engine itself
    /// only ever sees the resolved var name; `from` is metadata for the consumer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
}

/// A Playwright-style locator. Match fields are KDL properties; `within`/`fallback`/`after`/`near`
/// are child locators (KDL property values are scalars, so these must be nested nodes).
#[derive(Debug, Clone, Default, Serialize)]
pub struct Locator {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub css: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub level: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nth: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub within: Option<Box<Self>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fallback: Option<Box<Self>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after: Option<Box<Self>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub near: Option<Box<Self>>,
}

/// A declared expectation on an `extract`'s cardinality. A violation flags the run *unexpected*
/// (not failed) — the trigger for the optional repair path.
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct Expectation {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub count: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max: Option<u64>,
}

/// An `expect` tripwire condition — names a *bad state*; if it holds the run aborts.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Condition {
    SettledUrlMatches {
        pattern: String,
    },
    Visible {
        locator: Locator,
    },
    TextMatches {
        pattern: String,
        locator: Locator,
    },
    /// Compare a RENDERED value (typically an earlier `capture`) against an expectation.
    /// The only condition that needs no page, so a token-only `api` recipe can assert on
    /// what it captured. Without it such a recipe can observe but never fail, and a check
    /// that cannot fail is not a check.
    ValueMatches {
        value: String,
        equals: Option<String>,
        not_equals: Option<String>,
        non_empty: bool,
    },
}

/// A single ordered step.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "verb", rename_all = "snake_case")]
pub enum Step {
    Goto {
        url: String,
    },
    // Reload the current page via CDP `Page.reload` (never a fresh navigation). Some SPAs paint
    // blank on a first `goto` but render correctly on a real reload of an already-open tab (e.g. the
    // Spotify Creators episode wizard). Takes no args — it re-runs the current URL.
    Reload,
    Extract {
        key: String,
        many: bool,
        locator: Locator,
        expect: Expectation,
    },
    Expect {
        on_fail: ErrorClass,
        message: String,
        condition: Condition,
    },
    Wait {
        locator: Locator,
        #[serde(skip_serializing_if = "Option::is_none")]
        timeout_ms: Option<u64>,
    },
    Screenshot {
        key: String,
    },
    // Run an async in-page JS routine (CDP `Runtime.evaluate` with `awaitPromise`) and capture its
    // returned value (JSON, by value) under `key`. This is the escape hatch for flows whose modal
    // dance has dynamic uids and its own waits — it keeps that logic in the page where the DOM lives,
    // instead of a compiled verb. `retry_if_positive` names a dotted path into the returned object; if
    // the value there is a number > 0 the step fails retryable, so a recipe can poll (e.g. clips whose
    // render isn't ready yet) until the daemon's backoff drains it to zero.
    Eval {
        key: String,
        js: String,
        retry_if_positive: Option<String>,
    },
    // Multi-tab control: `follow` a newly-opened tab (route later steps to it), `back` to the
    // previous tab, or `close` the current one. `url_contains` disambiguates the new tab when the
    // opener link doesn't set `opener_id`.
    Tab {
        action: String,
        url_contains: Option<String>,
    },
    // Write verbs — real CDP input on the resolved element.
    Click {
        locator: Locator,
    },
    Fill {
        value: String,
        locator: Locator,
    },
    // Trusted text insert for rich editors (`contenteditable` / Slate.js). Unlike `fill` (a native
    // value setter, which a React-controlled editor ignores), this focuses the element, selects all
    // its contents (a DOM Range), then dispatches ONE trusted `Input.insertText` of the whole value —
    // the only path that commits into a Slate editor's React value.
    Insert {
        value: String,
        locator: Locator,
    },
    Select {
        value: String,
        locator: Locator,
    },
    Upload {
        path: String,
        locator: Locator,
    },
    // Claude-assisted challenge solve (captcha / visual puzzle). Screenshots the page, hands the
    // image + `prompt` to the injected solver (pacewright wires Claude vision behind it), then fills
    // the returned answer into `locator`. Terminal error if no solver is configured. The answer is
    // also captured under `key` when given.
    Solve {
        prompt: String,
        locator: Locator,
        key: Option<String>,
    },
    // Native browser download of an authed URL (follows cross-origin signed redirects). A same-origin
    // 4xx preflight means "not rendered yet" → the step is retryable, so a recipe can poll until ready.
    Download {
        url: String,
        out: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        timeout_secs: Option<u64>,
    },
    // HTTP request executed in the page context (inherits session cookies).
    Request(Request),
    // Native HTTP request (no browser): a TLS call authenticated by a bearer token, not cookies.
    Api(ApiRequest),
}

/// An HTTP request step: an in-page `fetch(…, {credentials:"include"})` so the browser's
/// authenticated cookies are sent. The response is captured under `capture_key` (optionally a
/// dotted sub-path of a JSON body).
#[derive(Debug, Clone, Serialize)]
pub struct Request {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    pub capture_key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capture_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expect_status: Option<u64>,
}

/// A native (no-browser) HTTP request step. Unlike [`Request`] (an in-page `fetch` that rides the
/// session cookies), this is a direct TLS call whose auth is a **bearer token**
/// (`bearer="{{token}}"` → `Authorization: Bearer …`), injected by pacewright at run time so the raw
/// secret never lives in the recipe file. This lets a token-only recipe (e.g. the Posts API of a
/// social network) run with no Chrome at all. Capture either the response body (a dotted JSON
/// `path=`) or a response header (`header=`, e.g. an `x-restli-id` carrying the created share URN).
#[derive(Debug, Clone, Serialize)]
pub struct ApiRequest {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bearer: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    pub capture_key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capture_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capture_header: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expect_status: Option<u64>,
}

/// The rendered output format of an `output` block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Format {
    Json,
    Markdown,
}

/// An output sink: render the result and write it to a templated path.
#[derive(Debug, Clone, Serialize)]
pub struct Output {
    pub format: Format,
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub template: Option<String>,
}

/// A parsed, validated recipe.
#[derive(Debug, Clone, Serialize)]
pub struct Recipe {
    /// `"<adapter>/<action>"`, e.g. `"linkedin/scrape_profile"`.
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub limit_keys: Vec<String>,
    pub vars: Vec<Var>,
    pub steps: Vec<Step>,
    pub outputs: Vec<Output>,
    /// Optional author guidance for Claude-assisted repair (spec §5a).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repair_prompt: Option<String>,
}

// ---- kdl node helpers ------------------------------------------------------

fn first_arg_str(node: &KdlNode) -> Option<&str> {
    node.entries()
        .iter()
        .find(|e| e.name().is_none())
        .and_then(|e| e.value().as_string())
}

fn prop<'a>(node: &'a KdlNode, key: &str) -> Option<&'a KdlValue> {
    node.entries()
        .iter()
        .find(|e| e.name().map(kdl::KdlIdentifier::value) == Some(key))
        .map(kdl::KdlEntry::value)
}

fn prop_str<'a>(node: &'a KdlNode, key: &str) -> Option<&'a str> {
    prop(node, key).and_then(KdlValue::as_string)
}

fn prop_bool(node: &KdlNode, key: &str) -> Option<bool> {
    prop(node, key).and_then(KdlValue::as_bool)
}

/// A non-negative integer property (`level`, `nth`, `expect-min`, …). Errors if present but
/// negative or out of range.
fn prop_u64(node: &KdlNode, key: &str) -> Result<Option<u64>, BoxError> {
    match prop(node, key).and_then(KdlValue::as_integer) {
        None => Ok(None),
        Some(i) => u64::try_from(i)
            .map(Some)
            .map_err(|_| format!("{key} must be a non-negative integer, got {i}").into()),
    }
}

fn prop_i64(node: &KdlNode, key: &str) -> Result<Option<i64>, BoxError> {
    match prop(node, key).and_then(KdlValue::as_integer) {
        None => Ok(None),
        Some(i) => i64::try_from(i)
            .map(Some)
            .map_err(|_| format!("{key} is out of range, got {i}").into()),
    }
}

fn children(node: &KdlNode) -> impl Iterator<Item = &KdlNode> {
    node.children().into_iter().flat_map(|d| d.nodes().iter())
}

fn first_child<'a>(node: &'a KdlNode, name: &str) -> Option<&'a KdlNode> {
    children(node).find(|n| n.name().value() == name)
}

// ---- parsing ---------------------------------------------------------------

fn parse_locator(node: &KdlNode) -> Result<Locator, BoxError> {
    let mut loc = Locator {
        role: prop_str(node, "role").map(str::to_string),
        name: prop_str(node, "name").map(str::to_string),
        text: prop_str(node, "text").map(str::to_string),
        label: prop_str(node, "label").map(str::to_string),
        tag: prop_str(node, "tag").map(str::to_string),
        css: prop_str(node, "css").map(str::to_string),
        level: prop_i64(node, "level")?,
        nth: prop_i64(node, "nth")?,
        ..Locator::default()
    };
    for child in children(node) {
        match child.name().value() {
            "within" => loc.within = Some(Box::new(parse_locator(child)?)),
            "fallback" => loc.fallback = Some(Box::new(parse_locator(child)?)),
            "after" => loc.after = Some(Box::new(parse_locator(child)?)),
            "near" => loc.near = Some(Box::new(parse_locator(child)?)),
            other => return Err(format!("unknown locator child node `{other}`").into()),
        }
    }
    Ok(loc)
}

/// The single `locator { … }` child of a targeting verb.
fn required_locator(node: &KdlNode) -> Result<Locator, BoxError> {
    let loc = first_child(node, "locator").ok_or_else(|| {
        format!(
            "`{}` requires a `locator {{ … }}` child",
            node.name().value()
        )
    })?;
    parse_locator(loc)
}

fn parse_condition(node: &KdlNode) -> Result<Condition, BoxError> {
    let cond = children(node)
        .next()
        .ok_or("`expect` requires a condition child (settled-url-matches|visible|text-matches)")?;
    match cond.name().value() {
        "settled-url-matches" => Ok(Condition::SettledUrlMatches {
            pattern: first_arg_str(cond)
                .ok_or("settled-url-matches requires a regex string")?
                .to_string(),
        }),
        "visible" => Ok(Condition::Visible {
            locator: required_locator(cond)?,
        }),
        "text-matches" => Ok(Condition::TextMatches {
            pattern: first_arg_str(cond)
                .ok_or("text-matches requires a regex string")?
                .to_string(),
            locator: required_locator(cond)?,
        }),
        "value" => {
            let value = first_arg_str(cond)
                .ok_or("value requires the string to test (e.g. \"{{ found }}\")")?
                .to_string();
            let equals = prop_str(cond, "equals").map(str::to_string);
            let not_equals = prop_str(cond, "not-equals").map(str::to_string);
            let non_empty = prop_bool(cond, "non-empty").unwrap_or(false);
            if equals.is_none() && not_equals.is_none() && !non_empty {
                return Err("value requires equals=, not-equals= or non-empty=#true".into());
            }
            Ok(Condition::ValueMatches {
                value,
                equals,
                not_equals,
                non_empty,
            })
        }
        other => Err(format!("unknown expect condition `{other}`").into()),
    }
}

fn parse_step(node: &KdlNode) -> Result<Step, BoxError> {
    let verb = children(node)
        .next()
        .ok_or("empty `step` — expected a verb (goto/extract/expect/wait/screenshot)")?;
    match verb.name().value() {
        "goto" => Ok(Step::Goto {
            url: first_arg_str(verb)
                .ok_or("goto requires a URL string")?
                .to_string(),
        }),
        "reload" => Ok(Step::Reload),
        "extract" => Ok(Step::Extract {
            key: first_arg_str(verb)
                .ok_or("extract requires a key string")?
                .to_string(),
            many: prop_bool(verb, "many").unwrap_or(false),
            locator: required_locator(verb)?,
            expect: Expectation {
                count: prop_u64(verb, "expect-count")?,
                min: prop_u64(verb, "expect-min")?,
                max: prop_u64(verb, "expect-max")?,
            },
        }),
        "expect" => Ok(Step::Expect {
            on_fail: ErrorClass::parse(
                prop_str(verb, "on-fail").ok_or("expect requires on-fail=\"…\"")?,
            )?,
            message: prop_str(verb, "message")
                .unwrap_or("expectation failed")
                .to_string(),
            condition: parse_condition(verb)?,
        }),
        "wait" => Ok(Step::Wait {
            locator: required_locator(verb)?,
            timeout_ms: prop_u64(verb, "timeout-ms")?,
        }),
        "screenshot" => Ok(Step::Screenshot {
            key: first_arg_str(verb)
                .ok_or("screenshot requires a key string")?
                .to_string(),
        }),
        "eval" => Ok(Step::Eval {
            key: first_arg_str(verb)
                .ok_or("eval requires a key string (where to capture the result)")?
                .to_string(),
            js: prop_str(verb, "js")
                .ok_or("eval requires js=\"…\" (the in-page expression to evaluate)")?
                .to_string(),
            retry_if_positive: prop_str(verb, "retry-if-positive").map(str::to_string),
        }),
        "tab" => Ok(Step::Tab {
            action: first_arg_str(verb)
                .ok_or("tab requires an action string (\"follow\" | \"back\" | \"close\")")?
                .to_string(),
            url_contains: prop_str(verb, "url-contains").map(str::to_string),
        }),
        "click" => Ok(Step::Click {
            locator: required_locator(verb)?,
        }),
        "fill" => Ok(Step::Fill {
            value: first_arg_str(verb)
                .ok_or("fill requires a value string")?
                .to_string(),
            locator: required_locator(verb)?,
        }),
        "insert" => Ok(Step::Insert {
            value: first_arg_str(verb)
                .ok_or("insert requires a value string")?
                .to_string(),
            locator: required_locator(verb)?,
        }),
        "select" => Ok(Step::Select {
            value: first_arg_str(verb)
                .ok_or("select requires a value string")?
                .to_string(),
            locator: required_locator(verb)?,
        }),
        "upload" => Ok(Step::Upload {
            path: first_arg_str(verb)
                .ok_or("upload requires a path string")?
                .to_string(),
            locator: required_locator(verb)?,
        }),
        "solve" => Ok(Step::Solve {
            prompt: first_arg_str(verb)
                .ok_or(
                    "solve requires a prompt string (what to ask the solver about the challenge)",
                )?
                .to_string(),
            locator: required_locator(verb)?,
            key: prop_str(verb, "key").map(str::to_string),
        }),
        "download" => Ok(Step::Download {
            url: prop_str(verb, "url")
                .ok_or("download requires url=\"…\"")?
                .to_string(),
            out: prop_str(verb, "out")
                .ok_or("download requires out=\"…\" (the local destination path)")?
                .to_string(),
            timeout_secs: prop_u64(verb, "timeout")?,
        }),
        "request" => Ok(Step::Request(parse_request(verb)?)),
        "api" => Ok(Step::Api(parse_api(verb)?)),
        other => Err(format!("unknown step verb `{other}`").into()),
    }
}

/// Parse an `api "<METHOD>" url="…" bearer="…" { header … ; body … ; capture "<key>" path="…"|header="…" }`
/// node into an [`ApiRequest`].
fn parse_api(node: &KdlNode) -> Result<ApiRequest, BoxError> {
    let method = first_arg_str(node)
        .ok_or("api requires an HTTP method string (e.g. \"POST\")")?
        .to_ascii_uppercase();
    let url = prop_str(node, "url")
        .ok_or("api requires url=\"…\"")?
        .to_string();
    let bearer = prop_str(node, "bearer").map(str::to_string);

    let mut headers = Vec::new();
    let mut body = None;
    let mut capture_key = None;
    let mut capture_path = None;
    let mut capture_header = None;
    for child in children(node) {
        match child.name().value() {
            "header" => {
                let args: Vec<&str> = child
                    .entries()
                    .iter()
                    .filter(|e| e.name().is_none())
                    .filter_map(|e| e.value().as_string())
                    .collect();
                let [name, value] = args.as_slice() else {
                    return Err("header requires a name and a value string".into());
                };
                headers.push(((*name).to_string(), (*value).to_string()));
            }
            "body" => {
                body = Some(
                    first_arg_str(child)
                        .ok_or("body requires a string")?
                        .to_string(),
                );
            }
            "capture" => {
                capture_key = Some(
                    first_arg_str(child)
                        .ok_or("capture requires a key string")?
                        .to_string(),
                );
                capture_path = prop_str(child, "path").map(str::to_string);
                capture_header = prop_str(child, "header").map(str::to_string);
            }
            other => return Err(format!("unknown api child node `{other}`").into()),
        }
    }

    Ok(ApiRequest {
        method,
        url,
        headers,
        bearer,
        body,
        capture_key: capture_key.ok_or("api requires a `capture \"<key>\"` child")?,
        capture_path,
        capture_header,
        expect_status: prop_u64(node, "expect-status")?,
    })
}

/// Parse a `request "<METHOD>" url="…" { header … ; body … ; capture "<key>" path="…" }` node.
fn parse_request(node: &KdlNode) -> Result<Request, BoxError> {
    let method = first_arg_str(node)
        .ok_or("request requires an HTTP method string (e.g. \"GET\")")?
        .to_ascii_uppercase();
    let url = prop_str(node, "url")
        .ok_or("request requires url=\"…\"")?
        .to_string();

    let mut headers = Vec::new();
    let mut body = None;
    let mut capture_key = None;
    let mut capture_path = None;
    for child in children(node) {
        match child.name().value() {
            "header" => {
                let args: Vec<&str> = child
                    .entries()
                    .iter()
                    .filter(|e| e.name().is_none())
                    .filter_map(|e| e.value().as_string())
                    .collect();
                let [name, value] = args.as_slice() else {
                    return Err("header requires a name and a value string".into());
                };
                headers.push(((*name).to_string(), (*value).to_string()));
            }
            "body" => {
                body = Some(
                    first_arg_str(child)
                        .ok_or("body requires a string")?
                        .to_string(),
                );
            }
            "capture" => {
                capture_key = Some(
                    first_arg_str(child)
                        .ok_or("capture requires a key string")?
                        .to_string(),
                );
                capture_path = prop_str(child, "path").map(str::to_string);
            }
            other => return Err(format!("unknown request child node `{other}`").into()),
        }
    }

    Ok(Request {
        method,
        url,
        headers,
        body,
        capture_key: capture_key.ok_or("request requires a `capture \"<key>\"` child")?,
        capture_path,
        expect_status: prop_u64(node, "expect-status")?,
    })
}

fn parse_var(node: &KdlNode) -> Result<Var, BoxError> {
    Ok(Var {
        name: first_arg_str(node)
            .ok_or("var requires a name string")?
            .to_string(),
        required: prop_bool(node, "required").unwrap_or(false),
        default: prop_str(node, "default").map(str::to_string),
        doc: prop_str(node, "doc").map(str::to_string),
        from: prop_str(node, "from").map(str::to_string),
    })
}

fn parse_output(node: &KdlNode) -> Result<Output, BoxError> {
    let format = match first_arg_str(node)
        .ok_or("output requires a format (\"json\"|\"markdown\")")?
    {
        "json" => Format::Json,
        "markdown" => Format::Markdown,
        other => {
            return Err(format!("unknown output format {other:?} (expected json|markdown)").into());
        }
    };
    let path = prop_str(node, "path")
        .ok_or("output requires path=\"…\"")?
        .to_string();
    let template = first_child(node, "template")
        .and_then(first_arg_str)
        .map(str::to_string);
    if format == Format::Markdown && template.is_none() {
        return Err("markdown output requires a `template { … }` child".into());
    }
    Ok(Output {
        format,
        path,
        template,
    })
}

impl Recipe {
    /// Parse and validate recipe KDL.
    pub fn parse(src: &str) -> Result<Self, BoxError> {
        let doc: KdlDocument = src.parse().map_err(|e| format!("not valid KDL: {e}"))?;

        let node = doc
            .nodes()
            .iter()
            .find(|n| n.name().value() == "recipe")
            .ok_or("no top-level `recipe` node")?;

        let name = first_arg_str(node)
            .ok_or(
                "`recipe` node is missing its name string, e.g. recipe \"linkedin/scrape_profile\"",
            )?
            .to_string();

        let mut recipe = Self {
            name,
            description: None,
            limit_keys: Vec::new(),
            vars: Vec::new(),
            steps: Vec::new(),
            outputs: Vec::new(),
            repair_prompt: None,
        };

        for child in children(node) {
            match child.name().value() {
                "description" => recipe.description = first_arg_str(child).map(str::to_string),
                "limit-key" => {
                    if let Some(k) = first_arg_str(child) {
                        recipe.limit_keys.push(k.to_string());
                    }
                }
                "var" => recipe.vars.push(parse_var(child)?),
                "step" => recipe.steps.push(parse_step(child)?),
                "output" => recipe.outputs.push(parse_output(child)?),
                "repair" => {
                    recipe.repair_prompt = first_child(child, "prompt")
                        .and_then(first_arg_str)
                        .map(str::to_string);
                }
                _ => {} // ignore unknown top-level nodes (forward-compatible)
            }
        }

        recipe.validate()?;
        Ok(recipe)
    }

    /// Does any step need a live page? Only the native [`Step::Api`] runs browser-free, so a recipe
    /// built entirely from `api` steps can run with no Chrome at all (the runner skips the launch).
    pub fn needs_browser(&self) -> bool {
        self.steps.iter().any(|s| !matches!(s, Step::Api(_)))
    }

    /// Load-time checks that parsing alone can't express: every `{{ ref }}` must resolve to a
    /// declared var, an `extract` key, or a builtin (`now`).
    fn validate(&self) -> Result<(), BoxError> {
        let mut known: BTreeSet<&str> = self.vars.iter().map(|v| v.name.as_str()).collect();
        known.insert("now");
        for s in &self.steps {
            match s {
                Step::Extract { key, .. } => {
                    known.insert(key.as_str());
                }
                Step::Request(req) => {
                    known.insert(req.capture_key.as_str());
                }
                Step::Api(req) => {
                    known.insert(req.capture_key.as_str());
                }
                _ => {}
            }
        }
        for reference in self.interpolation_refs() {
            if !known.contains(reference.as_str()) {
                return Err(format!(
                    "unknown variable {{{{ {reference} }}}} — declare it with `var \"{reference}\"` \
                     or it must be an extract key"
                )
                .into());
            }
        }
        Ok(())
    }

    /// Every `{{ name }}` referenced anywhere in the recipe's interpolatable strings.
    fn interpolation_refs(&self) -> Vec<String> {
        let mut out = Vec::new();
        let mut push = |s: &str| out.extend(interp_names(s));
        for step in &self.steps {
            match step {
                Step::Goto { url } => push(url),
                Step::Expect {
                    message, condition, ..
                } => {
                    push(message);
                    if let Condition::SettledUrlMatches { pattern }
                    | Condition::TextMatches { pattern, .. } = condition
                    {
                        push(pattern);
                    }
                    if let Condition::ValueMatches {
                        value,
                        equals,
                        not_equals,
                        ..
                    } = condition
                    {
                        push(value);
                        if let Some(e) = equals {
                            push(e);
                        }
                        if let Some(e) = not_equals {
                            push(e);
                        }
                    }
                }
                Step::Fill { value, .. }
                | Step::Insert { value, .. }
                | Step::Select { value, .. } => push(value),
                Step::Upload { path, .. } => push(path),
                Step::Solve { prompt, .. } => push(prompt),
                Step::Eval { js, .. } => push(js),
                Step::Tab { url_contains, .. } => {
                    if let Some(u) = url_contains {
                        push(u);
                    }
                }
                Step::Download { url, out, .. } => {
                    push(url);
                    push(out);
                }
                Step::Request(req) => {
                    push(&req.url);
                    if let Some(b) = &req.body {
                        push(b);
                    }
                    for (_, v) in &req.headers {
                        push(v);
                    }
                }
                Step::Api(req) => {
                    push(&req.url);
                    if let Some(b) = &req.bearer {
                        push(b);
                    }
                    if let Some(b) = &req.body {
                        push(b);
                    }
                    for (_, v) in &req.headers {
                        push(v);
                    }
                }
                Step::Extract { .. }
                | Step::Wait { .. }
                | Step::Screenshot { .. }
                | Step::Reload
                | Step::Click { .. } => {}
            }
        }
        for o in &self.outputs {
            push(&o.path);
            if let Some(t) = &o.template {
                push(t);
            }
        }
        out
    }
}

/// Extract the `name` tokens from `{{ name }}` occurrences. Section tags (`{{#name}}`,
/// `{{/name}}`) and the item placeholder (`{{.}}`) are output-template constructs, not variable
/// references, so they are skipped here.
fn interp_names(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = s;
    while let Some(open) = rest.find("{{") {
        let after = &rest[open + 2..];
        let Some(close) = after.find("}}") else { break };
        let token = after[..close].trim();
        if !token.is_empty() && !token.starts_with('#') && !token.starts_with('/') && token != "." {
            out.push(token.to_string());
        }
        rest = &after[close + 2..];
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn parses_a_value_condition_for_browserless_asserts() {
        // The shape a token-only verify recipe uses to FAIL on what it captured.
        let src = r#"
recipe "t/x" {
    var "found" required=#true
    step { expect on-fail="terminal" message="nothing was published" { value "{{ found }}" not-equals="0" } }
}"#;
        let r = super::Recipe::parse(src).expect("value condition must parse");
        let has = r.steps.iter().any(|s| {
            matches!(
                s,
                super::Step::Expect {
                    condition: super::Condition::ValueMatches { .. },
                    ..
                }
            )
        });
        assert!(has, "expected a ValueMatches condition");
    }

    #[test]
    fn a_value_condition_needs_an_expectation() {
        let src = r#"
recipe "t/x" {
    step { expect on-fail="terminal" message="m" { value "{{ a }}" } }
}"#;
        assert!(
            super::Recipe::parse(src).is_err(),
            "bare `value` with no comparison must be rejected"
        );
    }

    use super::{ErrorClass, Format, Recipe, Step};

    const HN: &str = r#"recipe "news/hackernews" {
        description "read HN"
        limit-key "news.hackernews"
        var "url" default="https://news.ycombinator.com/" doc="listing URL"
        var "limit" default="30"
        var "out_dir" default="/tmp"
        step { goto "{{ url }}" }
        step {
            expect on-fail="retryable" message="down" {
                visible { locator css=".athing" }
            }
        }
        step {
            extract "stories" many=#true expect-min=1 {
                locator css=".athing .titleline > a"
            }
        }
        output "json" path="{{ out_dir }}/hn.json"
        repair { prompt "prefer semantic locators" }
    }"#;

    #[test]
    fn parses_full_recipe() {
        let r = Recipe::parse(HN).unwrap();
        assert_eq!(r.name, "news/hackernews");
        assert_eq!(r.description.as_deref(), Some("read HN"));
        assert_eq!(r.limit_keys, ["news.hackernews"]);
        assert_eq!(r.vars.len(), 3);
        assert_eq!(r.steps.len(), 3);
        assert_eq!(r.outputs.len(), 1);
        assert_eq!(r.repair_prompt.as_deref(), Some("prefer semantic locators"));
    }

    #[test]
    fn extract_carries_many_and_expectation() {
        let r = Recipe::parse(HN).unwrap();
        let Step::Extract {
            key, many, expect, ..
        } = &r.steps[2]
        else {
            panic!("expected extract step");
        };
        assert_eq!(key, "stories");
        assert!(many);
        assert_eq!(expect.min, Some(1));
    }

    #[test]
    fn expect_maps_on_fail_class() {
        let r = Recipe::parse(HN).unwrap();
        let Step::Expect { on_fail, .. } = &r.steps[1] else {
            panic!("expected expect step");
        };
        assert_eq!(*on_fail, ErrorClass::Retryable);
    }

    #[test]
    fn relative_locator_after_is_a_child() {
        let src = r#"recipe "x/y" {
            step { extract "headline" { locator tag="p" nth=0 { after role="heading" } } }
        }"#;
        let r = Recipe::parse(src).unwrap();
        let Step::Extract { locator, .. } = &r.steps[0] else {
            panic!()
        };
        assert_eq!(locator.tag.as_deref(), Some("p"));
        assert_eq!(locator.nth, Some(0));
        assert_eq!(
            locator.after.as_ref().unwrap().role.as_deref(),
            Some("heading")
        );
    }

    #[test]
    fn markdown_output_parses_with_template() {
        let src = r##"recipe "x/y" {
            var "vault" required=#true
            step { extract "name" { locator role="heading" } }
            output "markdown" path="{{ vault }}/{{ name }}.md" {
                template #"# {{ name }}"#
            }
        }"##;
        let r = Recipe::parse(src).unwrap();
        assert_eq!(r.outputs[0].format, Format::Markdown);
        assert!(
            r.outputs[0]
                .template
                .as_deref()
                .unwrap()
                .contains("{{ name }}")
        );
    }

    #[test]
    fn parses_write_verbs() {
        let src = r##"recipe "x/y" {
            step { click { locator role="button" name="Publish" } }
            step { fill "hello" { locator css="#title" } }
            step { select "Public" { locator css="#visibility" } }
            step { upload "/tmp/cover.png" { locator css="input[type=file]" } }
        }"##;
        let r = Recipe::parse(src).unwrap();
        assert!(matches!(&r.steps[0], Step::Click { .. }));
        let Step::Fill { value, .. } = &r.steps[1] else {
            panic!("expected fill");
        };
        assert_eq!(value, "hello");
        let Step::Select { value, .. } = &r.steps[2] else {
            panic!("expected select");
        };
        assert_eq!(value, "Public");
        let Step::Upload { path, .. } = &r.steps[3] else {
            panic!("expected upload");
        };
        assert_eq!(path, "/tmp/cover.png");
    }

    #[test]
    fn parses_reload_verb() {
        let src = r##"recipe "x/y" {
            step { reload }
            step { wait { locator css="#title-input" } }
        }"##;
        let r = Recipe::parse(src).unwrap();
        assert!(matches!(&r.steps[0], Step::Reload));
        assert!(matches!(&r.steps[1], Step::Wait { .. }));
    }

    #[test]
    fn parses_insert_verb() {
        let src = r#"recipe "x/y" {
            var "desc" required=#true
            step { insert "{{ desc }}" { locator css="[data-slate-editor=true]" } }
        }"#;
        let r = Recipe::parse(src).unwrap();
        let Step::Insert { value, locator } = &r.steps[0] else {
            panic!("expected insert step");
        };
        assert_eq!(value, "{{ desc }}");
        assert_eq!(locator.css.as_deref(), Some("[data-slate-editor=true]"));
    }

    #[test]
    fn insert_value_is_interpolatable() {
        // `{{ desc }}` in an insert value must resolve to a declared var (no "unknown variable").
        let src = r#"recipe "x/y" {
            var "desc" required=#true
            step { insert "{{ desc }}" { locator css=".ed" } }
        }"#;
        assert!(Recipe::parse(src).is_ok());
    }

    #[test]
    fn rejects_insert_without_locator() {
        let src = r#"recipe "x/y" { step { insert "hi" } }"#;
        assert!(Recipe::parse(src).is_err());
    }

    #[test]
    fn parses_request_with_headers_body_and_capture() {
        let src = r#"recipe "x/y" {
            var "token" required=#true
            step {
                request "POST" url="https://api.test/items" expect-status=201 {
                    header "Authorization" "Bearer {{ token }}"
                    header "Content-Type" "application/json"
                    body "{\"name\":\"x\"}"
                    capture "created" path="data.id"
                }
            }
            output "json" path="/tmp/{{ created }}.json"
        }"#;
        let r = Recipe::parse(src).unwrap();
        let Step::Request(req) = &r.steps[0] else {
            panic!("expected request step");
        };
        assert_eq!(req.method, "POST");
        assert_eq!(req.url, "https://api.test/items");
        assert_eq!(req.expect_status, Some(201));
        assert_eq!(req.headers.len(), 2);
        assert_eq!(req.headers[0].0, "Authorization");
        assert_eq!(req.headers[0].1, "Bearer {{ token }}");
        assert_eq!(req.body.as_deref(), Some(r#"{"name":"x"}"#));
        assert_eq!(req.capture_key, "created");
        assert_eq!(req.capture_path.as_deref(), Some("data.id"));
    }

    #[test]
    fn parses_api_with_bearer_and_header_capture() {
        let src = r#"recipe "x/y" {
            var "token" required=#true
            var "commentary" required=#true
            step {
                api "POST" url="https://api.linkedin.com/rest/posts" bearer="{{ token }}" expect-status=201 {
                    header "LinkedIn-Version" "202606"
                    body "{\"commentary\":\"{{ commentary }}\"}"
                    capture "post_urn" header="x-restli-id"
                }
            }
        }"#;
        let r = Recipe::parse(src).unwrap();
        let Step::Api(req) = &r.steps[0] else {
            panic!("expected api step");
        };
        assert_eq!(req.method, "POST");
        assert_eq!(req.url, "https://api.linkedin.com/rest/posts");
        assert_eq!(req.bearer.as_deref(), Some("{{ token }}"));
        assert_eq!(req.expect_status, Some(201));
        assert_eq!(req.headers.len(), 1);
        assert_eq!(req.headers[0], ("LinkedIn-Version".into(), "202606".into()));
        assert_eq!(
            req.body.as_deref(),
            Some(r#"{"commentary":"{{ commentary }}"}"#)
        );
        assert_eq!(req.capture_key, "post_urn");
        assert_eq!(req.capture_header.as_deref(), Some("x-restli-id"));
        assert_eq!(req.capture_path, None);
    }

    #[test]
    fn needs_browser_false_for_api_only_recipe() {
        let r = Recipe::parse(
            r#"recipe "x/y" { step { api "GET" url="https://a.test/x" { capture "y" } } }"#,
        )
        .unwrap();
        assert!(!r.needs_browser());
    }

    #[test]
    fn needs_browser_true_when_any_page_step_present() {
        let r = Recipe::parse(
            r#"recipe "x/y" {
                step { api "GET" url="https://a.test/x" { capture "y" } }
                step { goto "https://a.test/" }
            }"#,
        )
        .unwrap();
        assert!(r.needs_browser());
    }

    #[test]
    fn parses_api_body_capture_by_json_path() {
        let src = r#"recipe "x/y" {
            step { api "GET" url="https://api.test/me" { capture "sub" path="sub" } }
        }"#;
        let r = Recipe::parse(src).unwrap();
        let Step::Api(req) = &r.steps[0] else {
            panic!("expected api step");
        };
        assert_eq!(req.capture_path.as_deref(), Some("sub"));
        assert_eq!(req.capture_header, None);
        assert_eq!(req.bearer, None);
    }

    #[test]
    fn rejects_api_without_capture() {
        let src = r#"recipe "x/y" { step { api "GET" url="https://api.test/x" } }"#;
        let err = Recipe::parse(src).unwrap_err().to_string();
        assert!(err.contains("capture"), "got: {err}");
    }

    #[test]
    fn request_capture_key_is_a_valid_interpolation_ref() {
        // `{{ created }}` in the output path must resolve to the request's capture key.
        let src = r#"recipe "x/y" {
            step { request "GET" url="https://api.test/x" { capture "created" } }
            output "json" path="/tmp/{{ created }}.json"
        }"#;
        assert!(Recipe::parse(src).is_ok());
    }

    #[test]
    fn rejects_request_without_capture() {
        let src = r#"recipe "x/y" {
            step { request "GET" url="https://api.test/x" }
        }"#;
        let err = Recipe::parse(src).unwrap_err().to_string();
        assert!(err.contains("capture"), "got: {err}");
    }

    #[test]
    fn rejects_unknown_interpolation() {
        let src = r#"recipe "x/y" { step { goto "{{ nope }}" } }"#;
        let err = Recipe::parse(src).unwrap_err().to_string();
        assert!(err.contains("nope"), "got: {err}");
    }

    #[test]
    fn rejects_bad_on_fail_class() {
        let src = r#"recipe "x/y" {
            step { expect on-fail="boom" message="m" { settled-url-matches "/x/" } }
        }"#;
        assert!(Recipe::parse(src).is_err());
    }

    #[test]
    fn rejects_unknown_output_format() {
        let src = r#"recipe "x/y" { output "yaml" path="/tmp/x" }"#;
        assert!(Recipe::parse(src).is_err());
    }

    #[test]
    fn rejects_markdown_without_template() {
        let src = r#"recipe "x/y" { output "markdown" path="/tmp/x.md" }"#;
        assert!(Recipe::parse(src).is_err());
    }

    #[test]
    fn rejects_negative_expectation() {
        let src = r#"recipe "x/y" {
            step { extract "s" many=#true expect-min=-1 { locator css=".x" } }
        }"#;
        assert!(Recipe::parse(src).is_err());
    }

    #[test]
    fn section_tags_are_not_variable_refs() {
        // {{#stories}}/{{.}}/{{/stories}} in a template must not be flagged as unknown vars.
        let src = r##"recipe "x/y" {
            step { extract "stories" many=#true { locator css=".a" } }
            output "markdown" path="/tmp/x.md" {
                template #"{{#stories}}- {{.}} {{/stories}}"#
            }
        }"##;
        assert!(Recipe::parse(src).is_ok());
    }
}

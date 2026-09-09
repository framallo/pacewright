//! A generic reasoning adapter: `agent/ask` and `agent/adjudicate` (also aliased `claude/*`), backed
//! by the Anthropic Messages API. It lets a scheduled task or a pipeline step "involve Claude" —
//! draft a reply, summarize, or (as the pipeline `fallback`) adjudicate a failed verify.
//!
//! Platform-agnostic: it knows only "prompt in, structured answer out". What to ask (the LinkedIn
//! comment, the episode context) lives in the task params / recipe, never here. Auth precedence:
//! an explicit `ANTHROPIC_OAUTH_TOKEN` wins, then a stored **Claude Max/Pro login** (`pcw anthropic
//! login`, auto-refreshed — see [`anthropic_oauth`]), then `ANTHROPIC_API_KEY`; base URL from
//! `ANTHROPIC_BASE_URL` (default `https://api.anthropic.com`). The `claude_cli` adapter is the other
//! Claude touchpoint and uses your Claude Code subscription login directly.
pub mod anthropic_oauth;
pub mod claude_cli;
pub mod solver;
use async_trait::async_trait;
use pacewright_core::adapter::{Adapter, RunCtx};
use pacewright_core::model::{ActionSpec, AdapterError};
use serde_json::{json, Value};
pub use solver::ClaudeSolver;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

const ANTHROPIC_VERSION: &str = "2023-06-01";
const DEFAULT_MODEL: &str = "claude-sonnet-4-5";
const DEFAULT_MAX_TOKENS: u32 = 2048;

/// A single-turn completion request. No conversation history: each task is one independent ask.
#[derive(Debug, Clone)]
pub struct CompletionRequest {
    pub model: String,
    pub max_tokens: u32,
    pub system: Option<String>,
    pub prompt: String,
    /// Base64-encoded PNG images sent as image content blocks before the prompt text (vision).
    /// Empty for a plain text ask. Used by the captcha solver to show Claude the challenge.
    pub images: Vec<String>,
}

/// The one thing the adapter needs from the outside world: turn a prompt into text. Split behind a
/// trait so the adapter's shaping/parsing logic is unit-testable without a network or an API key.
#[async_trait]
pub trait Completer: Send + Sync {
    async fn complete(&self, req: &CompletionRequest) -> Result<String, AdapterError>;
}

/// Build the auth + version headers, mirroring the harness's precedence: an OAuth token (Bearer +
/// the oauth beta) wins over an API key (`x-api-key`). Errors when neither is present rather than
/// sending an unauthenticated request. Pure, so the precedence is unit-tested.
pub fn auth_headers(
    oauth: Option<&str>,
    api_key: Option<&str>,
) -> Result<Vec<(&'static str, String)>, AdapterError> {
    if let Some(t) = oauth.filter(|s| !s.is_empty()) {
        Ok(vec![
            ("authorization", format!("Bearer {t}")),
            ("anthropic-beta", "oauth-2025-04-20".to_string()),
            ("anthropic-version", ANTHROPIC_VERSION.to_string()),
        ])
    } else if let Some(k) = api_key.filter(|s| !s.is_empty()) {
        Ok(vec![
            ("x-api-key", k.to_string()),
            ("anthropic-version", ANTHROPIC_VERSION.to_string()),
        ])
    } else {
        Err(AdapterError::Terminal(
            "no Anthropic credentials: set ANTHROPIC_OAUTH_TOKEN or ANTHROPIC_API_KEY in the daemon env"
                .to_string(),
        ))
    }
}

fn truncate(s: &str, n: usize) -> String {
    let head: String = s.chars().take(n).collect();
    if head.len() < s.len() {
        format!("{head}…")
    } else {
        head
    }
}

/// Pull the model's text out of an Anthropic Messages response: concatenate every `text` content
/// block. An empty/blockless response is a terminal error (nothing usable came back).
pub fn parse_content_text(resp: &Value) -> Result<String, AdapterError> {
    let blocks = resp
        .get("content")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            AdapterError::Terminal("anthropic response had no `content` array".into())
        })?;
    let text: String = blocks
        .iter()
        .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|b| b.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("");
    if text.trim().is_empty() {
        return Err(AdapterError::Terminal(
            "anthropic response had no text content".into(),
        ));
    }
    Ok(text)
}

/// Best-effort JSON extraction from a model reply: try the whole string, then strip a ```-fence,
/// then fall back to the first `{…}`/`[…]` slice. Structured steps demand JSON, so a reply that
/// yields none is a terminal error (it will never parse on retry either).
pub fn extract_json(text: &str) -> Result<Value, AdapterError> {
    let t = strip_fence(text.trim());
    if let Ok(v) = serde_json::from_str::<Value>(t) {
        return Ok(v);
    }
    if let Some(slice) = first_json_slice(t) {
        if let Ok(v) = serde_json::from_str::<Value>(slice) {
            return Ok(v);
        }
    }
    Err(AdapterError::Terminal(format!(
        "model did not return JSON: {}",
        truncate(text, 200)
    )))
}

/// Drop a leading ```/```json fence and its trailing ``` if the string is fenced.
fn strip_fence(s: &str) -> &str {
    let Some(rest) = s.strip_prefix("```") else {
        return s;
    };
    // Skip the info string (e.g. `json`) up to and including the first newline.
    let body = match rest.find('\n') {
        Some(i) => &rest[i + 1..],
        None => rest,
    };
    body.trim().strip_suffix("```").unwrap_or(body).trim()
}

/// The first balanced `{…}` or `[…]` slice, string-aware so braces inside quoted strings don't
/// throw off the depth count. Returns None if there is no complete bracketed region.
fn first_json_slice(s: &str) -> Option<&str> {
    let start = s.find(['{', '['])?;
    let open = s.as_bytes()[start];
    let close = if open == b'{' { b'}' } else { b']' };
    let (mut depth, mut in_str, mut esc) = (0i32, false, false);
    for (i, b) in s.bytes().enumerate().skip(start) {
        if in_str {
            match b {
                _ if esc => esc = false,
                b'\\' => esc = true,
                b'"' => in_str = false,
                _ => {}
            }
            continue;
        }
        match b {
            b'"' => in_str = true,
            x if x == open => depth += 1,
            x if x == close => {
                depth -= 1;
                if depth == 0 {
                    return Some(&s[start..=i]);
                }
            }
            _ => {}
        }
    }
    None
}

const ADJUDICATE_SYSTEM: &str =
    "You are a skeptical verifier reviewing whether an automated action \
truly succeeded. Assume it FAILED unless the evidence positively proves otherwise. Do not be \
agreeable. Respond with ONLY a JSON object: {\"ok\": boolean, \"reason\": string, \"evidence\": \
string}. `evidence` must quote the concrete proof that the action succeeded; if you cannot cite \
such proof, set ok=false and evidence to an empty string.";

/// Assemble the adjudication prompt from the question and whatever context/evidence the pipeline
/// passed. Pure, so the shape is unit-tested.
pub fn build_adjudicate_prompt(question: &str, context: &str, evidence: &str) -> String {
    let mut p = format!("Question: {question}\n");
    if !context.trim().is_empty() {
        p.push_str(&format!(
            "\nContext (the step's own report, which may be wrong):\n{context}\n"
        ));
    }
    if !evidence.trim().is_empty() {
        p.push_str(&format!("\nIndependent evidence:\n{evidence}\n"));
    }
    p.push_str("\nReturn only the JSON verdict.");
    p
}

/// The reasoning adapter. `name` is the registered adapter prefix (`agent` or `claude`); both share
/// one `Completer`.
pub struct AgentAdapter {
    name: String,
    completer: Arc<dyn Completer>,
    model: String,
    max_tokens: u32,
}

impl AgentAdapter {
    /// Construct with the real Anthropic completer. `model`/`max_tokens` default from env
    /// (`PACEWRIGHT_AGENT_MODEL` / `PACEWRIGHT_AGENT_MAX_TOKENS`).
    pub fn new(name: &str) -> Self {
        Self::with_completer(name, Arc::new(AnthropicCompleter::new()))
    }

    pub fn with_completer(name: &str, completer: Arc<dyn Completer>) -> Self {
        let model =
            std::env::var("PACEWRIGHT_AGENT_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.to_string());
        let max_tokens = std::env::var("PACEWRIGHT_AGENT_MAX_TOKENS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_MAX_TOKENS);
        Self {
            name: name.to_string(),
            completer,
            model,
            max_tokens,
        }
    }

    fn request(&self, params: &Value, prompt: String, system: Option<String>) -> CompletionRequest {
        CompletionRequest {
            model: params
                .get("model")
                .and_then(Value::as_str)
                .unwrap_or(&self.model)
                .to_string(),
            max_tokens: params
                .get("max_tokens")
                .and_then(Value::as_u64)
                .map(|n| n as u32)
                .unwrap_or(self.max_tokens),
            system: params
                .get("system")
                .and_then(Value::as_str)
                .map(String::from)
                .or(system),
            prompt,
            images: Vec::new(),
        }
    }
}

fn req_str(params: &Value, field: &str) -> Result<String, AdapterError> {
    params
        .get(field)
        .and_then(Value::as_str)
        .map(String::from)
        .ok_or_else(|| {
            AdapterError::Terminal(format!("agent: missing required string param `{field}`"))
        })
}

#[async_trait]
impl Adapter for AgentAdapter {
    fn name(&self) -> &str {
        &self.name
    }

    fn actions(&self) -> Vec<ActionSpec> {
        let ns = &self.name;
        vec![
            ActionSpec {
                name: "ask".into(),
                limit_keys: vec![format!("{ns}.ask")],
                params_schema: json!({"prompt": "string", "system?": "string", "json?": "bool", "model?": "string", "max_tokens?": "int"}),
                description: "Ask Claude a single-turn question. `json:true` parses the reply as JSON; else returns {text}.".into(),
            },
            ActionSpec {
                name: "adjudicate".into(),
                limit_keys: vec![format!("{ns}.adjudicate")],
                params_schema: json!({"question": "string", "context?": "any", "evidence?": "any"}),
                description: "Adversarially judge whether an action succeeded; returns {ok, reason, evidence}.".into(),
            },
        ]
    }

    async fn execute(
        &self,
        _ctx: &RunCtx,
        action: &str,
        params: Value,
    ) -> Result<Value, AdapterError> {
        match action {
            "ask" => {
                let prompt = req_str(&params, "prompt")?;
                let req = self.request(&params, prompt, None);
                let text = self.completer.complete(&req).await?;
                if params.get("json").and_then(Value::as_bool).unwrap_or(false) {
                    extract_json(&text)
                } else {
                    Ok(json!({ "text": text }))
                }
            }
            "adjudicate" => {
                let question = req_str(&params, "question")?;
                let context = params.get("context").map(value_to_text).unwrap_or_default();
                let evidence = params
                    .get("evidence")
                    .map(value_to_text)
                    .unwrap_or_default();
                let prompt = build_adjudicate_prompt(&question, &context, &evidence);
                let req = self.request(&params, prompt, Some(ADJUDICATE_SYSTEM.to_string()));
                let text = self.completer.complete(&req).await?;
                extract_json(&text)
            }
            other => Err(AdapterError::Terminal(format!(
                "agent: unknown action `{other}` (expected `ask` or `adjudicate`)"
            ))),
        }
    }
}

/// A JSON param may be a plain string or a nested object; render it for the prompt either way.
fn value_to_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// The real transport: POST `/v1/messages`, authenticating with a Claude Max/Pro **OAuth
/// subscription** when one is available (so the subscription pays, not API-key quota), else an API
/// key. OAuth path: `Authorization: Bearer` + the `oauth-2025-04-20` beta + Claude Code User-Agent,
/// and the Claude Code identity as the first `system` block (Anthropic 401s OAuth inference without
/// it). Token is taken from `ANTHROPIC_OAUTH_TOKEN`, else the stored login in `secrets.json`
/// (auto-refreshed). API path: `x-api-key` from `ANTHROPIC_API_KEY`.
pub struct AnthropicCompleter {
    base_url: String,
    http: reqwest::Client,
    secrets_path: std::path::PathBuf,
    token_http: Arc<dyn anthropic_oauth::TokenHttp>,
}

impl Default for AnthropicCompleter {
    fn default() -> Self {
        Self::new()
    }
}

impl AnthropicCompleter {
    pub fn new() -> Self {
        let base = std::env::var("ANTHROPIC_BASE_URL")
            .unwrap_or_else(|_| "https://api.anthropic.com".to_string());
        Self {
            base_url: base.trim_end_matches('/').to_string(),
            http: reqwest::Client::new(),
            secrets_path: pacewright_core::run::home_dir().join("secrets.json"),
            token_http: Arc::new(anthropic_oauth::ReqwestTokenHttp::default()),
        }
    }
}

pub(crate) fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

#[async_trait]
impl Completer for AnthropicCompleter {
    async fn complete(&self, req: &CompletionRequest) -> Result<String, AdapterError> {
        // Auth mode: an explicit env OAuth token wins; else the stored Max/Pro login (refreshed on
        // the fly); else an API key. OAuth means the subscription pays for the call.
        let oauth = match std::env::var("ANTHROPIC_OAUTH_TOKEN")
            .ok()
            .filter(|s| !s.is_empty())
        {
            Some(t) => Some(t),
            None => anthropic_oauth::resolve_access_token(
                &self.secrets_path,
                &*self.token_http,
                now_ms(),
            )
            .await
            .map_err(AdapterError::Terminal)?,
        };
        let key = std::env::var("ANTHROPIC_API_KEY")
            .ok()
            .filter(|s| !s.is_empty());

        let use_oauth = oauth.is_some();
        let headers: Vec<(String, String)> = if let Some(tok) = &oauth {
            vec![
                ("authorization".into(), format!("Bearer {tok}")),
                ("anthropic-beta".into(), anthropic_oauth::OAUTH_BETA.into()),
                ("anthropic-version".into(), ANTHROPIC_VERSION.into()),
                (
                    "user-agent".into(),
                    anthropic_oauth::INFERENCE_USER_AGENT.into(),
                ),
            ]
        } else if let Some(k) = &key {
            vec![
                ("x-api-key".into(), k.clone()),
                ("anthropic-version".into(), ANTHROPIC_VERSION.into()),
            ]
        } else {
            return Err(AdapterError::Terminal(
                "no Anthropic credentials: run `pcw anthropic login` (Claude Max/Pro) or set ANTHROPIC_API_KEY / ANTHROPIC_OAUTH_TOKEN"
                    .to_string(),
            ));
        };

        // Text-only → a plain string content; with images → image blocks first, then the prompt text
        // (Anthropic vision message shape). PNG is what the recipe engine's `screenshot()` produces.
        let content = if req.images.is_empty() {
            json!(req.prompt)
        } else {
            let mut blocks: Vec<Value> = req
                .images
                .iter()
                .map(|b64| {
                    json!({
                        "type": "image",
                        "source": { "type": "base64", "media_type": "image/png", "data": b64 },
                    })
                })
                .collect();
            blocks.push(json!({ "type": "text", "text": req.prompt }));
            json!(blocks)
        };
        let mut body = json!({
            "model": req.model,
            "max_tokens": req.max_tokens,
            "messages": [{ "role": "user", "content": content }],
        });
        if use_oauth {
            // Subscription inference REQUIRES the Claude Code identity as the first system block.
            body["system"] = anthropic_oauth::claude_code_system_block(req.system.as_deref());
        } else if let Some(sys) = &req.system {
            body["system"] = json!(sys);
        }

        let mut rb = self.http.post(format!("{}/v1/messages", self.base_url));
        for (k, v) in &headers {
            rb = rb.header(k.as_str(), v);
        }
        let resp = rb
            .json(&body)
            .send()
            .await
            .map_err(|e| AdapterError::Retryable(format!("anthropic request failed: {e}")))?;

        let status = resp.status();
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            let secs = resp
                .headers()
                .get("retry-after")
                .and_then(|h| h.to_str().ok())
                .and_then(|s| s.parse::<i64>().ok())
                .unwrap_or(60);
            return Err(AdapterError::RateLimited {
                retry_after: now_ms() + secs * 1000,
            });
        }
        if !status.is_success() {
            let txt = resp.text().await.unwrap_or_default();
            let msg = format!("anthropic {status}: {}", truncate(&txt, 300));
            // 5xx is transient; 4xx (bad request, auth) will not fix itself on retry.
            return Err(if status.is_server_error() {
                AdapterError::Retryable(msg)
            } else {
                AdapterError::Terminal(msg)
            });
        }
        let v: Value = resp
            .json()
            .await
            .map_err(|e| AdapterError::Terminal(format!("anthropic returned non-JSON: {e}")))?;
        parse_content_text(&v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> RunCtx {
        RunCtx {
            task_id: "t".into(),
            browser: Arc::new(pacewright_core::browser::NullBrowser),
        }
    }

    struct Canned(String);
    #[async_trait]
    impl Completer for Canned {
        async fn complete(&self, _req: &CompletionRequest) -> Result<String, AdapterError> {
            Ok(self.0.clone())
        }
    }

    fn agent(reply: &str) -> AgentAdapter {
        AgentAdapter::with_completer("agent", Arc::new(Canned(reply.to_string())))
    }

    #[test]
    fn auth_prefers_oauth_then_key_then_errors() {
        let o = auth_headers(Some("tok"), Some("key")).unwrap();
        assert_eq!(o[0], ("authorization", "Bearer tok".to_string()));
        assert!(o.iter().any(|(k, _)| *k == "anthropic-beta"));
        let k = auth_headers(None, Some("key")).unwrap();
        assert_eq!(k[0], ("x-api-key", "key".to_string()));
        // Empty strings count as absent.
        assert!(auth_headers(Some(""), Some("")).is_err());
        assert!(auth_headers(None, None).is_err());
    }

    #[test]
    fn parse_content_concatenates_text_blocks() {
        let v = json!({"content": [{"type":"text","text":"Hel"},{"type":"tool_use"},{"type":"text","text":"lo"}]});
        assert_eq!(parse_content_text(&v).unwrap(), "Hello");
        assert!(parse_content_text(&json!({"content": []})).is_err());
    }

    #[test]
    fn extract_json_handles_bare_fenced_and_embedded() {
        assert_eq!(extract_json(r#"{"a":1}"#).unwrap()["a"], 1);
        assert_eq!(extract_json("```json\n{\"a\":2}\n```").unwrap()["a"], 2);
        // Prose around the object, with a brace inside a string that must not fool the scanner.
        let v = extract_json(r#"Sure! {"reason":"has } brace","ok":true} done"#).unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["reason"], "has } brace");
        assert!(extract_json("no json here").is_err());
    }

    #[tokio::test]
    async fn ask_returns_text_by_default_and_json_when_requested() {
        let a = agent(r#"{"draft":"hi there"}"#);
        let text = a
            .execute(&ctx(), "ask", json!({"prompt": "draft a reply"}))
            .await
            .unwrap();
        assert_eq!(
            text["text"], r#"{"draft":"hi there"}"#,
            "default mode returns raw text"
        );

        let parsed = a
            .execute(
                &ctx(),
                "ask",
                json!({"prompt": "draft a reply", "json": true}),
            )
            .await
            .unwrap();
        assert_eq!(parsed["draft"], "hi there", "json mode parses the reply");
    }

    #[tokio::test]
    async fn ask_requires_a_prompt() {
        let err = agent("x")
            .execute(&ctx(), "ask", json!({}))
            .await
            .unwrap_err();
        assert!(matches!(err, AdapterError::Terminal(m) if m.contains("prompt")));
    }

    #[tokio::test]
    async fn adjudicate_returns_the_verdict_object() {
        let a = agent(r#"Here is my verdict: {"ok": false, "reason": "no proof", "evidence": ""}"#);
        let v = a
            .execute(
                &ctx(),
                "adjudicate",
                json!({"question": "did it post?", "context": {"status": "ok"}}),
            )
            .await
            .unwrap();
        assert_eq!(v["ok"], false);
        assert_eq!(v["evidence"], "");
    }

    #[tokio::test]
    async fn unknown_action_is_terminal() {
        let err = agent("x")
            .execute(&ctx(), "summarize", json!({"prompt": "x"}))
            .await
            .unwrap_err();
        assert!(matches!(err, AdapterError::Terminal(m) if m.contains("unknown action")));
    }

    #[test]
    fn adjudicate_prompt_includes_context_and_evidence() {
        let p = build_adjudicate_prompt("did it?", "the report", "the proof");
        assert!(p.contains("did it?"));
        assert!(p.contains("the report"));
        assert!(p.contains("the proof"));
    }
}

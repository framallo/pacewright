//! Built-in `http` adapter — a non-browser REST step. Lets a pipeline source data from an API
//! (e.g. Apollo host search) without driving Chrome: `http/request` does one call and returns the
//! JSON, which a `data/append` step then persists. Secrets are injected from the environment at call
//! time (never stored in the task params, which live in the DB): `secret = { env, as }` reads
//! `env` and places it as a bearer token, a header, or a query/body field (R11).
//!
//! Request assembly is a pure function ([`build_request`]) so header/secret/query wiring is
//! unit-tested without a network; the send is behind an [`HttpSend`] seam.
use async_trait::async_trait;
use pacewright_core::adapter::{Adapter, RunCtx};
use pacewright_core::model::{ActionSpec, AdapterError};
use serde_json::{json, Map, Value};

pub const HTTP_ADAPTER: &str = "http";

/// A fully-assembled request, ready to send. Pure output of [`build_request`].
#[derive(Debug, Clone, PartialEq)]
pub struct HttpReq {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<Value>,
}

/// The send seam, so the adapter is testable without a network.
#[async_trait]
pub trait HttpSend: Send + Sync {
    /// Return `(status, body_text)`.
    async fn send(&self, req: &HttpReq) -> Result<(u16, String), String>;
}

pub struct ReqwestHttpSend {
    client: reqwest::Client,
}
impl Default for ReqwestHttpSend {
    fn default() -> Self {
        Self {
            client: reqwest::Client::new(),
        }
    }
}
#[async_trait]
impl HttpSend for ReqwestHttpSend {
    async fn send(&self, req: &HttpReq) -> Result<(u16, String), String> {
        let method = reqwest::Method::from_bytes(req.method.as_bytes())
            .map_err(|e| format!("bad method {}: {e}", req.method))?;
        let mut rb = self.client.request(method, &req.url);
        for (k, v) in &req.headers {
            rb = rb.header(k.as_str(), v);
        }
        if let Some(b) = &req.body {
            rb = rb.json(b);
        }
        let resp = rb.send().await.map_err(|e| format!("request failed: {e}"))?;
        let status = resp.status().as_u16();
        let text = resp.text().await.map_err(|e| format!("body read failed: {e}"))?;
        Ok((status, text))
    }
}

/// Look up a secret's value. Behind a trait so tests inject values instead of reading real env.
pub trait SecretSource: Send + Sync {
    fn get(&self, env_name: &str) -> Option<String>;
}
pub struct EnvSecrets;
impl SecretSource for EnvSecrets {
    fn get(&self, env_name: &str) -> Option<String> {
        std::env::var(env_name).ok().filter(|s| !s.is_empty())
    }
}

/// Assemble the request from params, injecting the secret. Pure. Errors are terminal config
/// mistakes (missing url, unknown `as`, missing secret env).
///
/// Params: `method` (default GET), `url` (required), `headers` (object), `query` (object of
/// string→string appended to the URL), `body`/`json` (object, sent as JSON), and
/// `secret = { env, as }` where `as` is `"bearer"`, `"header:<name>"`, `"query:<name>"`, or
/// `"body:<field>"`.
pub fn build_request(params: &Value, secrets: &dyn SecretSource) -> Result<HttpReq, AdapterError> {
    let method = params
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or("GET")
        .to_uppercase();
    let mut url = params
        .get("url")
        .and_then(Value::as_str)
        .ok_or_else(|| AdapterError::Terminal("http/request: `url` is required".into()))?
        .to_string();

    // Pipeline params arrive as strings (KDL kv-blocks), so an object-valued param may be a
    // JSON string; coerce either shape to an object.
    let headers_obj = coerce_obj(params, "headers");
    let query_obj = coerce_obj(params, "query");
    let body_obj = coerce_obj(params, "body").or_else(|| coerce_obj(params, "json"));
    let secret_obj = coerce_obj(params, "secret");

    let mut headers: Vec<(String, String)> = Vec::new();
    if let Some(Value::Object(h)) = &headers_obj {
        for (k, v) in h {
            if let Some(s) = v.as_str() {
                headers.push((k.clone(), s.to_string()));
            }
        }
    }

    // Collect query params (from `query`) and body (from `body`/`json`) so a secret can target them.
    let mut query: Vec<(String, String)> = Vec::new();
    if let Some(Value::Object(q)) = &query_obj {
        for (k, v) in q {
            query.push((k.clone(), value_to_query(v)));
        }
    }
    let mut body: Option<Map<String, Value>> = match &body_obj {
        Some(Value::Object(o)) => Some(o.clone()),
        _ => None,
    };

    // Secret injection.
    if let Some(secret) = &secret_obj {
        let env_name = secret
            .get("env")
            .and_then(Value::as_str)
            .ok_or_else(|| AdapterError::Terminal("http/request: secret needs `env`".into()))?;
        let value = secrets.get(env_name).ok_or_else(|| {
            AdapterError::Terminal(format!("http/request: secret env `{env_name}` is unset"))
        })?;
        let as_ = secret.get("as").and_then(Value::as_str).unwrap_or("bearer");
        match as_.split_once(':') {
            None if as_ == "bearer" => headers.push(("authorization".into(), format!("Bearer {value}"))),
            Some(("header", name)) => headers.push((name.to_string(), value)),
            Some(("query", name)) => query.push((name.to_string(), value)),
            Some(("body", field)) => {
                body.get_or_insert_with(Map::new)
                    .insert(field.to_string(), Value::String(value));
            }
            _ => {
                return Err(AdapterError::Terminal(format!(
                    "http/request: unknown secret placement `{as_}` (use bearer|header:X|query:X|body:X)"
                )))
            }
        }
    }

    if !query.is_empty() {
        let sep = if url.contains('?') { '&' } else { '?' };
        let qs: Vec<String> = query
            .iter()
            .map(|(k, v)| format!("{}={}", urlencode(k), urlencode(v)))
            .collect();
        url = format!("{url}{sep}{}", qs.join("&"));
    }

    Ok(HttpReq {
        method,
        url,
        headers,
        body: body.map(Value::Object),
    })
}

fn value_to_query(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// An object-valued param that may arrive as an object or as a JSON string (pipeline kv-blocks).
fn coerce_obj(params: &Value, key: &str) -> Option<Value> {
    match params.get(key) {
        Some(Value::Object(o)) => Some(Value::Object(o.clone())),
        Some(Value::String(s)) => serde_json::from_str::<Value>(s).ok().filter(Value::is_object),
        _ => None,
    }
}

fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Best-effort JSON parse of a response body; falls back to `{text}` for non-JSON.
fn parse_body(text: &str) -> Value {
    serde_json::from_str::<Value>(text).unwrap_or_else(|_| json!({ "text": text }))
}

pub struct HttpAdapter {
    sender: std::sync::Arc<dyn HttpSend>,
    secrets: std::sync::Arc<dyn SecretSource>,
}

impl Default for HttpAdapter {
    fn default() -> Self {
        Self {
            sender: std::sync::Arc::new(ReqwestHttpSend::default()),
            secrets: std::sync::Arc::new(EnvSecrets),
        }
    }
}
impl HttpAdapter {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl Adapter for HttpAdapter {
    fn name(&self) -> &str {
        HTTP_ADAPTER
    }

    fn actions(&self) -> Vec<ActionSpec> {
        vec![ActionSpec {
            name: "request".into(),
            limit_keys: vec!["http.request".into()],
            params_schema: json!({
                "method?": "GET|POST|... (default GET)",
                "url": "string",
                "headers?": "object",
                "query?": "object appended to the URL",
                "body?": "object sent as JSON (alias: json)",
                "secret?": "{ env, as } — as = bearer|header:X|query:X|body:X"
            }),
            description: "One non-browser REST call. Returns {status, ok, json}. Secrets injected from env, never stored.".into(),
        }]
    }

    async fn execute(
        &self,
        _ctx: &RunCtx,
        action: &str,
        params: Value,
    ) -> Result<Value, AdapterError> {
        if action != "request" {
            return Err(AdapterError::Terminal(format!(
                "http: unknown action `{action}` (expected `request`)"
            )));
        }
        let req = build_request(&params, &*self.secrets)?;
        let (status, text) = self
            .sender
            .send(&req)
            .await
            .map_err(AdapterError::Retryable)?;
        let ok = (200..300).contains(&status);
        if !ok {
            // 4xx is a config/permission error (terminal); 5xx/429 transient.
            let msg = format!("http {status}: {}", text.chars().take(300).collect::<String>());
            return Err(if status >= 500 || status == 429 {
                AdapterError::Retryable(msg)
            } else {
                AdapterError::Terminal(msg)
            });
        }
        Ok(json!({ "status": status, "ok": ok, "json": parse_body(&text) }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    struct FakeSecrets(HashMap<String, String>);
    impl SecretSource for FakeSecrets {
        fn get(&self, k: &str) -> Option<String> {
            self.0.get(k).cloned()
        }
    }
    fn no_secrets() -> FakeSecrets {
        FakeSecrets(HashMap::new())
    }

    #[test]
    fn builds_a_get_with_query() {
        let r = build_request(
            &json!({ "url": "https://api.x/search", "query": { "q": "eng leaders", "page": 2 } }),
            &no_secrets(),
        )
        .unwrap();
        assert_eq!(r.method, "GET");
        assert!(r.url.contains("q=eng%20leaders"));
        assert!(r.url.contains("page=2"));
        assert!(r.body.is_none());
    }

    #[test]
    fn injects_secret_as_header_query_body_bearer() {
        let secrets = FakeSecrets(HashMap::from([("APOLLO_API_KEY".to_string(), "sk".to_string())]));
        let hdr = build_request(
            &json!({ "url": "u", "secret": { "env": "APOLLO_API_KEY", "as": "header:x-api-key" } }),
            &secrets,
        )
        .unwrap();
        assert!(hdr.headers.iter().any(|(k, v)| k == "x-api-key" && v == "sk"));

        let bearer = build_request(
            &json!({ "url": "u", "secret": { "env": "APOLLO_API_KEY", "as": "bearer" } }),
            &secrets,
        )
        .unwrap();
        assert!(bearer.headers.iter().any(|(k, v)| k == "authorization" && v == "Bearer sk"));

        let body = build_request(
            &json!({ "method": "POST", "url": "u", "json": { "q": "x" }, "secret": { "env": "APOLLO_API_KEY", "as": "body:api_key" } }),
            &secrets,
        )
        .unwrap();
        assert_eq!(body.body.unwrap()["api_key"], "sk");

        let query = build_request(
            &json!({ "url": "u", "secret": { "env": "APOLLO_API_KEY", "as": "query:api_key" } }),
            &secrets,
        )
        .unwrap();
        assert!(query.url.contains("api_key=sk"));
    }

    #[test]
    fn missing_secret_env_is_terminal() {
        let r = build_request(
            &json!({ "url": "u", "secret": { "env": "NOPE" } }),
            &no_secrets(),
        );
        assert!(matches!(r, Err(AdapterError::Terminal(_))));
    }

    #[tokio::test]
    async fn execute_maps_status_classes() {
        struct S(u16);
        #[async_trait]
        impl HttpSend for S {
            async fn send(&self, _r: &HttpReq) -> Result<(u16, String), String> {
                Ok((self.0, "{\"ok\":1}".into()))
            }
        }
        let ctx = RunCtx {
            task_id: "t".into(),
            browser: std::sync::Arc::new(pacewright_core::browser::NullBrowser),
        };
        let mk = |code: u16| HttpAdapter {
            sender: std::sync::Arc::new(S(code)),
            secrets: std::sync::Arc::new(no_secrets()),
        };
        assert!(mk(200)
            .execute(&ctx, "request", json!({ "url": "u" }))
            .await
            .is_ok());
        assert!(matches!(
            mk(404).execute(&ctx, "request", json!({ "url": "u" })).await,
            Err(AdapterError::Terminal(_))
        ));
        assert!(matches!(
            mk(503).execute(&ctx, "request", json!({ "url": "u" })).await,
            Err(AdapterError::Retryable(_))
        ));
    }
}

//! Anthropic OAuth (Claude Pro/Max) — a Rust port of oh-my-pi's `registry/oauth/anthropic.ts`.
//!
//! Lets pacewright's Claude calls draw on a **Claude Max/Pro subscription** instead of pay-per-token
//! API-key quota. The subscription is spent by authenticating as Claude Code: an OAuth
//! `Authorization: Bearer` token (PKCE flow, no client secret) plus the `oauth-2025-04-20` beta and
//! the Claude Code system-identity block on every inference request. Without that identity block
//! Anthropic rejects OAuth inference with 401 — see [`claude_code_system_block`].
//!
//! This module owns the protocol + token lifecycle (authorize URL, code exchange, refresh, and
//! resolving a valid access token out of the [`SecretStore`], refreshing when it is within
//! [`SKEW_MS`] of expiry). The interactive browser/callback login lives in the CLI (`pcw anthropic
//! login`); this module is the transport it and the completer share, behind a [`TokenHttp`] seam so
//! exchange/refresh are testable without a network.
use async_trait::async_trait;
use base64::Engine;
use serde::Deserialize;
use serde_json::{json, Value};

/// The Claude Code public OAuth client id (same value Claude Code itself uses). Not a secret.
pub const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
pub const AUTHORIZE_URL: &str = "https://claude.ai/oauth/authorize";
pub const TOKEN_URL: &str = "https://api.anthropic.com/v1/oauth/token";
/// Scopes required for direct OAuth-token inference (`user:inference`) plus session/account mgmt.
/// The `claude.ai` authorize endpoint is required — `platform.claude.com` issues console-only tokens
/// that lack `user:inference`.
pub const SCOPES: &str =
    "org:create_api_key user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload";
pub const CALLBACK_PORT: u16 = 54545;
pub const CALLBACK_PATH: &str = "/callback";
/// Default loopback redirect. The interactive login runs a one-shot server here.
pub const REDIRECT_URI: &str = "http://localhost:54545/callback";
/// The beta that opts a Messages request into OAuth-token (subscription) inference.
pub const OAUTH_BETA: &str = "oauth-2025-04-20";
/// User-Agent Claude Code sends on inference; pairs with the identity block below.
pub const INFERENCE_USER_AGENT: &str = "claude-cli/2.1.220 (external, claude-desktop)";
/// User-Agent Claude Code sends specifically on the refresh call.
pub const REFRESH_USER_AGENT: &str = "anthropic-sdk-typescript/0.94.0 userOAuthProvider";
/// The identity Anthropic requires as the FIRST system block of an OAuth inference request.
pub const CLAUDE_CODE_SYSTEM: &str = "You are a Claude agent, built on Anthropic's Claude Agent SDK.";
/// The `SecretStore` provider key these tokens live under.
pub const PROVIDER: &str = "anthropic";
/// Refresh (or reject as expired) a token within this window of its expiry. Matches the 5-minute
/// skew oh-my-pi subtracts when recording `expires`.
pub const SKEW_MS: i64 = 5 * 60 * 1000;

/// The Claude Code identity as the first `system` block. When OAuth is in use the request `system`
/// MUST be an array whose first block is exactly this, or Anthropic returns 401. Any caller-supplied
/// system text becomes a second block.
pub fn claude_code_system_block(user_system: Option<&str>) -> Value {
    let mut blocks = vec![json!({ "type": "text", "text": CLAUDE_CODE_SYSTEM })];
    if let Some(s) = user_system.filter(|s| !s.trim().is_empty()) {
        blocks.push(json!({ "type": "text", "text": s }));
    }
    Value::Array(blocks)
}

// ---- PKCE -----------------------------------------------------------------------------------

pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

/// Base64url (no padding) of `bytes`.
fn b64url(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// The S256 challenge for a verifier: `base64url(sha256(verifier))`. Pure, so it is checked against
/// the RFC 7636 test vector.
pub fn pkce_challenge(verifier: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(verifier.as_bytes());
    b64url(&h.finalize())
}

/// Fresh PKCE pair: a 96-byte random verifier (base64url) + its S256 challenge.
pub fn generate_pkce() -> Pkce {
    use rand::RngCore;
    let mut bytes = [0u8; 96];
    rand::thread_rng().fill_bytes(&mut bytes);
    let verifier = b64url(&bytes);
    let challenge = pkce_challenge(&verifier);
    Pkce { verifier, challenge }
}

/// The full authorize URL to open in a browser.
pub fn build_authorize_url(state: &str, redirect_uri: &str, challenge: &str) -> String {
    let q = [
        ("code", "true"),
        ("client_id", CLIENT_ID),
        ("response_type", "code"),
        ("redirect_uri", redirect_uri),
        ("scope", SCOPES),
        ("code_challenge", challenge),
        ("code_challenge_method", "S256"),
        ("state", state),
    ];
    let query: Vec<String> = q
        .iter()
        .map(|(k, v)| format!("{k}={}", urlencode(v)))
        .collect();
    format!("{AUTHORIZE_URL}?{}", query.join("&"))
}

/// Minimal percent-encoding for query values (RFC 3986 unreserved kept; everything else escaped).
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// The authorize redirect (and the paste flow) can return `code#state`; split it so the exchange
/// sends the bare code and the fragment's state overrides the passed one. Pure.
pub fn split_code_state<'a>(raw: &'a str, fallback_state: &'a str) -> (&'a str, &'a str) {
    match raw.split_once('#') {
        Some((code, state)) if !state.is_empty() => (code, state),
        Some((code, _)) => (code, fallback_state),
        None => (raw, fallback_state),
    }
}

// ---- token endpoint -------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tokens {
    pub access: String,
    pub refresh: String,
    /// Unix millis, already skew-adjusted (real expiry minus [`SKEW_MS`]).
    pub expires_at_ms: i64,
    pub email: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    expires_in: i64,
    #[serde(default)]
    account: Option<Account>,
}

#[derive(Debug, Deserialize)]
struct Account {
    #[serde(default)]
    email_address: Option<String>,
}

/// The token-endpoint transport, behind a trait so exchange/refresh are unit-tested with canned
/// responses. `post` sends a JSON body to [`TOKEN_URL`] with the given extra headers and returns
/// `(status, body)`.
#[async_trait]
pub trait TokenHttp: Send + Sync {
    async fn post(&self, body: Value, headers: Vec<(String, String)>) -> Result<(u16, String), String>;
}

/// The real transport.
pub struct ReqwestTokenHttp {
    client: reqwest::Client,
}

impl Default for ReqwestTokenHttp {
    fn default() -> Self {
        Self {
            client: reqwest::Client::new(),
        }
    }
}

#[async_trait]
impl TokenHttp for ReqwestTokenHttp {
    async fn post(&self, body: Value, headers: Vec<(String, String)>) -> Result<(u16, String), String> {
        let mut rb = self
            .client
            .post(TOKEN_URL)
            .header("content-type", "application/json");
        for (k, v) in headers {
            rb = rb.header(k, v);
        }
        let resp = rb
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("token request failed: {e}"))?;
        let status = resp.status().as_u16();
        let text = resp.text().await.map_err(|e| format!("token body read failed: {e}"))?;
        Ok((status, text))
    }
}

/// Turn `now_ms + expires_in` and the response envelope into [`Tokens`], keeping `prior_refresh`
/// when the response omits a new refresh token (refresh responses often do). Pure.
fn tokens_from_response(
    status: u16,
    body: &str,
    now_ms: i64,
    prior_refresh: Option<&str>,
) -> Result<Tokens, String> {
    if !(200..300).contains(&status) {
        return Err(format!("anthropic oauth token endpoint returned {status}: {body}"));
    }
    let r: TokenResponse =
        serde_json::from_str(body).map_err(|e| format!("invalid token JSON: {e}; body={body}"))?;
    let refresh = r
        .refresh_token
        .or_else(|| prior_refresh.map(str::to_string))
        .ok_or_else(|| "token response had no refresh_token and none was stored".to_string())?;
    Ok(Tokens {
        access: r.access_token,
        refresh,
        expires_at_ms: now_ms + r.expires_in * 1000 - SKEW_MS,
        email: r.account.and_then(|a| a.email_address),
    })
}

/// Exchange an authorization code for tokens (the login's final step).
pub async fn exchange_code(
    http: &dyn TokenHttp,
    code: &str,
    state: &str,
    redirect_uri: &str,
    verifier: &str,
    now_ms: i64,
) -> Result<Tokens, String> {
    let (code, state) = split_code_state(code, state);
    let body = json!({
        "grant_type": "authorization_code",
        "client_id": CLIENT_ID,
        "code": code,
        "state": state,
        "redirect_uri": redirect_uri,
        "code_verifier": verifier,
    });
    let (status, resp) = http.post(body, vec![]).await?;
    tokens_from_response(status, &resp, now_ms, None)
}

/// Refresh an access token. Sends the same beta + User-Agent Claude Code uses on refresh.
pub async fn refresh(http: &dyn TokenHttp, refresh_token: &str, now_ms: i64) -> Result<Tokens, String> {
    let body = json!({
        "grant_type": "refresh_token",
        "client_id": CLIENT_ID,
        "refresh_token": refresh_token,
    });
    let headers = vec![
        ("anthropic-beta".to_string(), OAUTH_BETA.to_string()),
        ("user-agent".to_string(), REFRESH_USER_AGENT.to_string()),
    ];
    let (status, resp) = http.post(body, headers).await?;
    tokens_from_response(status, &resp, now_ms, Some(refresh_token))
}

// ---- persistence + resolution ---------------------------------------------------------------

use pacewright_core::secrets::SecretStore;

/// Persist freshly-minted login tokens under [`PROVIDER`] (app id + endpoints + tokens), 0600.
pub fn store_login(path: &std::path::Path, tokens: &Tokens) -> Result<(), String> {
    let mut store = SecretStore::load(path).map_err(|e| e.to_string())?;
    store.set_app(PROVIDER, CLIENT_ID, "");
    store.set_endpoints(PROVIDER, AUTHORIZE_URL, TOKEN_URL, SCOPES, None, None, None);
    store
        .set_tokens(
            PROVIDER,
            tokens.access.clone(),
            Some(tokens.refresh.clone()),
            tokens.expires_at_ms,
            None,
        )
        .map_err(|e| e.to_string())?;
    store.save().map_err(|e| e.to_string())
}

/// Resolve a currently-valid Max/Pro access token: return the stored one if still fresh, else
/// refresh it (persisting the rotation) and return the new one. `Ok(None)` means no OAuth login is
/// stored — the caller should fall back to an API key. A refresh that fails (e.g. the ~30-day grant
/// expired) is an `Err` telling the operator to re-run `pcw anthropic login`.
pub async fn resolve_access_token(
    path: &std::path::Path,
    http: &dyn TokenHttp,
    now_ms: i64,
) -> Result<Option<String>, String> {
    let store = SecretStore::load(path).map_err(|e| e.to_string())?;
    if let Some(tok) = store.valid_access_token(PROVIDER, now_ms, SKEW_MS) {
        return Ok(Some(tok.to_string()));
    }
    if !store.needs_refresh(PROVIDER, now_ms, SKEW_MS) {
        return Ok(None); // no token stored at all -> not an OAuth setup
    }
    let refresh_token = store
        .get(PROVIDER)
        .and_then(|r| r.refresh_token.clone())
        .ok_or_else(|| "anthropic token expired and no refresh_token stored; run `pcw anthropic login`".to_string())?;
    let fresh = refresh(http, &refresh_token, now_ms).await.map_err(|e| {
        format!("anthropic token refresh failed ({e}); the grant may have expired — run `pcw anthropic login`")
    })?;
    // Persist the rotation over the stored credential.
    let mut store = SecretStore::load(path).map_err(|e| e.to_string())?;
    store
        .set_tokens(
            PROVIDER,
            fresh.access.clone(),
            Some(fresh.refresh.clone()),
            fresh.expires_at_ms,
            None,
        )
        .map_err(|e| e.to_string())?;
    store.save().map_err(|e| e.to_string())?;
    Ok(Some(fresh.access))
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex;

    #[test]
    fn pkce_matches_the_rfc7636_s256_vector() {
        // RFC 7636 Appendix B.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(
            pkce_challenge(verifier),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn generated_pkce_is_self_consistent() {
        let p = generate_pkce();
        assert_eq!(pkce_challenge(&p.verifier), p.challenge);
        assert!(p.verifier.len() > 100); // 96 bytes base64url
    }

    #[test]
    fn authorize_url_carries_the_required_params() {
        let url = build_authorize_url("st8", REDIRECT_URI, "chal");
        assert!(url.starts_with("https://claude.ai/oauth/authorize?"));
        assert!(url.contains(&format!("client_id={CLIENT_ID}")));
        assert!(url.contains("code_challenge=chal"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("state=st8"));
        assert!(url.contains("scope=org%3Acreate_api_key")); // ':' escaped
        assert!(url.contains("redirect_uri=http%3A%2F%2Flocalhost%3A54545%2Fcallback"));
    }

    #[test]
    fn code_state_splits_on_fragment() {
        assert_eq!(split_code_state("abc#xyz", "fallback"), ("abc", "xyz"));
        assert_eq!(split_code_state("abc#", "fallback"), ("abc", "fallback"));
        assert_eq!(split_code_state("abc", "fallback"), ("abc", "fallback"));
    }

    #[test]
    fn system_block_puts_the_identity_first() {
        let v = claude_code_system_block(Some("be terse"));
        let arr = v.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["text"], CLAUDE_CODE_SYSTEM);
        assert_eq!(arr[1]["text"], "be terse");
        // No user system -> just the identity block.
        let only = claude_code_system_block(None);
        assert_eq!(only.as_array().unwrap().len(), 1);
    }

    #[test]
    fn tokens_from_response_keeps_prior_refresh_and_applies_skew() {
        let body = r#"{"access_token":"acc","expires_in":3600,"account":{"email_address":"a@b.c"}}"#;
        let t = tokens_from_response(200, body, 1_000_000, Some("old-refresh")).unwrap();
        assert_eq!(t.access, "acc");
        assert_eq!(t.refresh, "old-refresh"); // response omitted it -> keep prior
        assert_eq!(t.email.as_deref(), Some("a@b.c"));
        assert_eq!(t.expires_at_ms, 1_000_000 + 3600 * 1000 - SKEW_MS);
    }

    #[test]
    fn tokens_from_response_errors_on_non_2xx() {
        let e = tokens_from_response(400, "{\"error\":\"invalid_grant\"}", 0, None).unwrap_err();
        assert!(e.contains("400"));
    }

    struct FakeHttp {
        response: (u16, String),
        last_body: Mutex<Option<Value>>,
        last_headers: Mutex<Vec<(String, String)>>,
    }
    #[async_trait]
    impl TokenHttp for FakeHttp {
        async fn post(
            &self,
            body: Value,
            headers: Vec<(String, String)>,
        ) -> Result<(u16, String), String> {
            *self.last_body.lock() = Some(body);
            *self.last_headers.lock() = headers;
            Ok(self.response.clone())
        }
    }

    #[tokio::test]
    async fn refresh_sends_the_beta_and_ua_and_parses_new_tokens() {
        let http = FakeHttp {
            response: (
                200,
                r#"{"access_token":"new-acc","refresh_token":"new-ref","expires_in":28800}"#.into(),
            ),
            last_body: Mutex::new(None),
            last_headers: Mutex::new(vec![]),
        };
        let t = refresh(&http, "old-ref", 0).await.unwrap();
        assert_eq!(t.access, "new-acc");
        assert_eq!(t.refresh, "new-ref");
        let body = http.last_body.lock().clone().unwrap();
        assert_eq!(body["grant_type"], "refresh_token");
        assert_eq!(body["client_id"], CLIENT_ID);
        let headers = http.last_headers.lock().clone();
        assert!(headers.iter().any(|(k, v)| k == "anthropic-beta" && v == OAUTH_BETA));
        assert!(headers.iter().any(|(k, _)| k == "user-agent"));
    }

    #[tokio::test]
    async fn exchange_splits_code_and_sends_verifier() {
        let http = FakeHttp {
            response: (
                200,
                r#"{"access_token":"a","refresh_token":"r","expires_in":3600}"#.into(),
            ),
            last_body: Mutex::new(None),
            last_headers: Mutex::new(vec![]),
        };
        let t = exchange_code(&http, "the-code#the-state", "unused", REDIRECT_URI, "verif", 0)
            .await
            .unwrap();
        assert_eq!(t.access, "a");
        let body = http.last_body.lock().clone().unwrap();
        assert_eq!(body["code"], "the-code");
        assert_eq!(body["state"], "the-state");
        assert_eq!(body["code_verifier"], "verif");
        assert_eq!(body["grant_type"], "authorization_code");
    }
}

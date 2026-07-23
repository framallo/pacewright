//! Pure OAuth 2.0 (authorization-code) helpers, shared by the provider login flows.
//!
//! Everything here is IO-free and deterministic so it can be unit-tested without a network or a
//! browser: building the authorize URL, parsing the loopback redirect the browser lands on, parsing
//! the token response + computing its absolute expiry, and forming a LinkedIn author URN. The IO
//! glue (opening the browser, the localhost callback listener, the token/userinfo HTTP calls) lives
//! in the CLI and calls these.

use anyhow::{anyhow, Result};
use serde::Deserialize;

/// LinkedIn OAuth endpoints (member 3-legged flow).
pub const LINKEDIN_AUTHORIZE: &str = "https://www.linkedin.com/oauth/v2/authorization";
pub const LINKEDIN_TOKEN: &str = "https://www.linkedin.com/oauth/v2/accessToken";
pub const LINKEDIN_USERINFO: &str = "https://api.linkedin.com/v2/userinfo";

/// Percent-encode per RFC 3986 (encode everything except the unreserved set `A-Za-z0-9-._~`).
pub fn pct_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Decode a percent-encoded query value (`%20` → space, `+` → space).
pub fn pct_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                match u8::from_str_radix(&s[i + 1..i + 3], 16) {
                    Ok(v) => {
                        out.push(v);
                        i += 3;
                    }
                    Err(_) => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Build the authorization-code request URL. `scope` is a space-separated scope string.
pub fn authorize_url(
    base: &str,
    client_id: &str,
    redirect_uri: &str,
    scope: &str,
    state: &str,
) -> String {
    format!(
        "{base}?response_type=code&client_id={}&redirect_uri={}&scope={}&state={}",
        pct_encode(client_id),
        pct_encode(redirect_uri),
        pct_encode(scope),
        pct_encode(state),
    )
}

/// The parameters the browser hands back on the loopback redirect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Callback {
    pub code: String,
    pub state: String,
}

/// Parse the first line of the HTTP request the callback listener receives, e.g.
/// `GET /callback?code=ABC&state=xyz HTTP/1.1`. Errors if `code` or `state` is absent, or if the
/// request carries an OAuth `error` param.
pub fn parse_callback(request_line: &str) -> Result<Callback> {
    let target = request_line
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| anyhow!("malformed request line: {request_line:?}"))?;
    let query = target.split_once('?').map(|(_, q)| q).unwrap_or("");
    let mut code = None;
    let mut state = None;
    let mut error = None;
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        match k {
            "code" => code = Some(pct_decode(v)),
            "state" => state = Some(pct_decode(v)),
            "error" => error = Some(pct_decode(v)),
            _ => {}
        }
    }
    if let Some(e) = error {
        return Err(anyhow!("authorization failed: {e}"));
    }
    Ok(Callback {
        code: code.ok_or_else(|| anyhow!("callback missing `code`"))?,
        state: state.ok_or_else(|| anyhow!("callback missing `state`"))?,
    })
}

/// The token endpoint's JSON response.
#[derive(Debug, Clone, Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    pub expires_in: i64,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub scope: Option<String>,
}

impl TokenResponse {
    /// Absolute expiry in unix ms, given the current time in ms.
    pub fn expires_at_ms(&self, now_ms: i64) -> i64 {
        now_ms + self.expires_in * 1000
    }
}

/// Form a LinkedIn author/person URN from the OpenID `sub` claim.
pub fn person_urn(sub: &str) -> String {
    format!("urn:li:person:{sub}")
}

/// The default loopback base (scheme + host + port) the callback listens on. The provider name is
/// appended as a path segment, so the redirect URI is `{base}/{provider}/callback`.
pub const DEFAULT_CALLBACK_BASE: &str = "http://localhost:8765";

/// Build a provider-namespaced redirect URI, e.g. `http://localhost:8765/linkedin/callback`. The
/// path segment lets one loopback port serve multiple providers without collision.
pub fn callback_uri(base: &str, provider: &str) -> String {
    format!("{}/{}/callback", base.trim_end_matches('/'), provider)
}

/// Extract the port the loopback listener must bind from a redirect URI
/// (e.g. `http://localhost:8765/linkedin/callback` → `8765`). Errors if no explicit port is present,
/// since a loopback listener has nothing to bind without one.
pub fn redirect_port(redirect_uri: &str) -> Result<u16> {
    let after_scheme = redirect_uri
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(redirect_uri);
    let authority = after_scheme.split(['/', '?', '#']).next().unwrap_or("");
    let port_str = authority
        .rsplit_once(':')
        .map(|(_, p)| p)
        .ok_or_else(|| anyhow!("redirect URI needs an explicit port to bind the listener: {redirect_uri:?}"))?;
    port_str
        .parse::<u16>()
        .map_err(|_| anyhow!("invalid port in redirect URI: {redirect_uri:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pct_encode_leaves_unreserved_and_encodes_the_rest() {
        assert_eq!(pct_encode("aZ0-._~"), "aZ0-._~");
        assert_eq!(pct_encode("a b"), "a%20b");
        assert_eq!(pct_encode("http://x/y"), "http%3A%2F%2Fx%2Fy");
    }

    #[test]
    fn pct_decode_reverses_spaces() {
        assert_eq!(pct_decode("a%20b"), "a b");
        assert_eq!(pct_decode("a+b"), "a b");
        assert_eq!(pct_decode("http%3A%2F%2Fx"), "http://x");
    }

    #[test]
    fn authorize_url_encodes_params() {
        let u = authorize_url(
            LINKEDIN_AUTHORIZE,
            "cid",
            "http://localhost:8765/callback",
            "openid profile w_member_social",
            "nonce123",
        );
        assert!(u.starts_with("https://www.linkedin.com/oauth/v2/authorization?"));
        assert!(u.contains("response_type=code"));
        assert!(u.contains("client_id=cid"));
        assert!(u.contains("redirect_uri=http%3A%2F%2Flocalhost%3A8765%2Fcallback"));
        assert!(u.contains("scope=openid%20profile%20w_member_social"));
        assert!(u.contains("state=nonce123"));
    }

    #[test]
    fn parse_callback_extracts_code_and_state() {
        let cb = parse_callback("GET /callback?code=ABC123&state=xyz HTTP/1.1").unwrap();
        assert_eq!(cb.code, "ABC123");
        assert_eq!(cb.state, "xyz");
    }

    #[test]
    fn parse_callback_decodes_percent_encoding() {
        let cb = parse_callback("GET /callback?code=a%20b&state=s HTTP/1.1").unwrap();
        assert_eq!(cb.code, "a b");
    }

    #[test]
    fn parse_callback_errors_on_missing_code() {
        assert!(parse_callback("GET /callback?state=xyz HTTP/1.1").is_err());
    }

    #[test]
    fn parse_callback_errors_on_oauth_error() {
        let e = parse_callback("GET /callback?error=user_cancelled_login&state=xyz HTTP/1.1");
        assert!(e.is_err());
    }

    #[test]
    fn token_response_computes_absolute_expiry() {
        let t: TokenResponse =
            serde_json::from_str(r#"{"access_token":"at","expires_in":5184000}"#).unwrap();
        assert_eq!(t.access_token, "at");
        assert_eq!(t.refresh_token, None);
        // 5_184_000 s * 1000 = 5_184_000_000 ms after `now`.
        assert_eq!(t.expires_at_ms(1_000), 5_184_001_000);
    }

    #[test]
    fn person_urn_prefixes_sub() {
        assert_eq!(person_urn("ACoAA123"), "urn:li:person:ACoAA123");
    }

    #[test]
    fn callback_uri_namespaces_by_provider() {
        assert_eq!(
            callback_uri("http://localhost:8765", "linkedin"),
            "http://localhost:8765/linkedin/callback"
        );
        assert_eq!(
            callback_uri("http://localhost:8765", "youtube"),
            "http://localhost:8765/youtube/callback"
        );
        // A trailing slash on the base doesn't double up.
        assert_eq!(
            callback_uri("http://localhost:8765/", "linkedin"),
            "http://localhost:8765/linkedin/callback"
        );
    }

    #[test]
    fn redirect_port_extracts_from_uri() {
        assert_eq!(
            redirect_port("http://localhost:8765/linkedin/callback").unwrap(),
            8765
        );
        assert_eq!(redirect_port("http://127.0.0.1:9000/cb").unwrap(), 9000);
    }

    #[test]
    fn redirect_port_errors_without_explicit_port() {
        assert!(redirect_port("http://localhost/linkedin/callback").is_err());
    }
}

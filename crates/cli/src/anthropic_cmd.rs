//! `pcw anthropic` — log the daemon into a Claude Max/Pro subscription so `agent/ask` and
//! `agent/adjudicate` (and any recipe that involves Claude) draw on the subscription instead of
//! API-key quota. Mirrors Claude Code's OAuth: PKCE, a loopback callback on :54545, then the token
//! exchange. Tokens land in `~/.pacewright/secrets.json` (0600) and auto-refresh at call time.
use anyhow::{anyhow, Context, Result};
use pacewright_adapter_agent::anthropic_oauth as oauth;
use std::io::{Read, Write};
use std::path::PathBuf;

fn secrets_path() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default())
        .join(".pacewright")
        .join("secrets.json")
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// `pcw anthropic login [--paste]`.
pub async fn login(paste: bool) -> Result<()> {
    let pkce = oauth::generate_pkce();
    let state = {
        // A random state; reuse the PKCE generator's entropy shape via a second verifier.
        oauth::generate_pkce().verifier
    };
    let url = oauth::build_authorize_url(&state, oauth::REDIRECT_URI, &pkce.challenge);

    let http = oauth::ReqwestTokenHttp::default();

    let (code, ret_state) = if paste {
        println!("Open this URL, approve access, then paste the code (or the whole redirect URL):\n\n{url}\n");
        let pasted = prompt_line("code/redirect URL> ")?;
        let code = extract_code(&pasted)
            .ok_or_else(|| anyhow!("could not find a code in the pasted value"))?;
        (code, state.clone())
    } else {
        println!(
            "Opening your browser to approve Claude access…\nIf it doesn't open, visit:\n\n{url}\n"
        );
        let _ = open_browser(&url);
        // One-shot loopback capture of the redirect.
        let got = tokio::task::spawn_blocking(capture_callback)
            .await
            .context("callback task panicked")??;
        (got.code, got.state)
    };

    let tokens = oauth::exchange_code(
        &http,
        &code,
        &ret_state,
        oauth::REDIRECT_URI,
        &pkce.verifier,
        now_ms(),
    )
    .await
    .map_err(|e| anyhow!(e))?;

    oauth::store_login(&secrets_path(), &tokens).map_err(|e| anyhow!(e))?;
    match &tokens.email {
        Some(email) => println!("Logged in as {email}. Claude calls now use your subscription."),
        None => println!("Logged in. Claude calls now use your subscription."),
    }
    Ok(())
}

/// `pcw anthropic status`.
pub fn status() -> Result<()> {
    use pacewright_core::secrets::SecretStore;
    let store = SecretStore::load(secrets_path()).map_err(|e| anyhow!(e))?;
    match store.get(oauth::PROVIDER) {
        Some(rec) if rec.access_token.is_some() => {
            let now = now_ms();
            let exp = rec.expires_at_ms.unwrap_or(0);
            let mins = (exp - now) / 60_000;
            let state = if now < exp - oauth::SKEW_MS {
                format!("valid ({mins} min left)")
            } else if rec.refresh_token.is_some() {
                "expiring — will refresh on next use".to_string()
            } else {
                "expired — run `pcw anthropic login`".to_string()
            };
            println!("anthropic: signed in · {state}");
        }
        _ => println!("anthropic: not signed in — run `pcw anthropic login`"),
    }
    Ok(())
}

/// `pcw anthropic logout`.
pub fn logout() -> Result<()> {
    use pacewright_core::secrets::SecretStore;
    let mut store = SecretStore::load(secrets_path()).map_err(|e| anyhow!(e))?;
    store.clear_tokens(oauth::PROVIDER);
    store.save().map_err(|e| anyhow!(e))?;
    println!("anthropic: logged out (tokens cleared).");
    Ok(())
}

fn prompt_line(prompt: &str) -> Result<String> {
    print!("{prompt}");
    std::io::stdout().flush().ok();
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(line.trim().to_string())
}

/// Best-effort browser open (macOS `open`, Linux `xdg-open`, Windows `start`).
fn open_browser(url: &str) -> Result<()> {
    #[cfg(target_os = "macos")]
    let (cmd, args): (&str, Vec<&str>) = ("open", vec![url]);
    #[cfg(target_os = "linux")]
    let (cmd, args): (&str, Vec<&str>) = ("xdg-open", vec![url]);
    #[cfg(target_os = "windows")]
    let (cmd, args): (&str, Vec<&str>) = ("cmd", vec!["/C", "start", url]);
    std::process::Command::new(cmd)
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|e| anyhow!("could not open browser: {e}"))
}

struct Callback {
    code: String,
    state: String,
}

/// Block on a single loopback request to the callback port, parse `code`/`state` from its query,
/// and reply with a friendly page. Times out via the socket accept (the caller runs it on a
/// blocking thread).
fn capture_callback() -> Result<Callback> {
    let listener = std::net::TcpListener::bind(("127.0.0.1", oauth::CALLBACK_PORT))
        .with_context(|| format!("binding loopback :{}", oauth::CALLBACK_PORT))?;
    let (mut stream, _) = listener
        .accept()
        .context("waiting for the OAuth redirect")?;
    let mut buf = [0u8; 8192];
    let n = stream.read(&mut buf).unwrap_or(0);
    let req = String::from_utf8_lossy(&buf[..n]);
    // First line: "GET /callback?code=...&state=... HTTP/1.1"
    let target = req
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .unwrap_or("");
    let (code, state) = parse_callback_query(target)
        .ok_or_else(|| anyhow!("redirect had no ?code= (got: {target})"))?;
    let page = "<html><body style='font-family:sans-serif'>pacewright is now signed in to Claude. You can close this tab.</body></html>";
    let resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        page.len(),
        page
    );
    stream.write_all(resp.as_bytes()).ok();
    Ok(Callback { code, state })
}

/// Extract `(code, state)` from a callback path/query. Pure, so it's unit-tested.
fn parse_callback_query(target: &str) -> Option<(String, String)> {
    let q = target.split_once('?').map(|(_, q)| q).unwrap_or(target);
    let mut code = None;
    let mut state = None;
    for pair in q.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            match k {
                "code" => code = Some(percent_decode(v)),
                "state" => state = Some(percent_decode(v)),
                _ => {}
            }
        }
    }
    Some((code?, state.unwrap_or_default()))
}

/// Extract a code from a pasted value: a bare code, a `code#state`, or a full redirect URL. Pure.
fn extract_code(pasted: &str) -> Option<String> {
    let pasted = pasted.trim();
    if pasted.contains("://") || pasted.starts_with("/") || pasted.contains("?code=") {
        return parse_callback_query(pasted).map(
            |(c, s)| {
                if s.is_empty() {
                    c
                } else {
                    format!("{c}#{s}")
                }
            },
        );
    }
    (!pasted.is_empty()).then(|| pasted.to_string())
}

/// Minimal percent-decoding for callback query values.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
                match hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                    Some(b) => {
                        out.push(b);
                        i += 3;
                    }
                    None => {
                        out.push(bytes[i]);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_code_and_state_from_a_callback_path() {
        let (c, s) = parse_callback_query("/callback?code=abc123&state=xy%3Dz").unwrap();
        assert_eq!(c, "abc123");
        assert_eq!(s, "xy=z");
    }

    #[test]
    fn extract_code_handles_bare_code_pair_and_url() {
        assert_eq!(extract_code("plaincode").unwrap(), "plaincode");
        assert_eq!(
            extract_code("http://localhost:54545/callback?code=cc&state=ss").unwrap(),
            "cc#ss"
        );
        assert!(extract_code("").is_none());
    }

    #[test]
    fn percent_decode_basics() {
        assert_eq!(percent_decode("a%20b%2Fc"), "a b/c");
    }
}

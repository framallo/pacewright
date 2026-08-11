//! `pacewright oauth <login|status|logout>` — the human-in-the-loop 3-legged OAuth flow.
//!
//! The pure protocol logic (authorize URL, callback parse, token/expiry math, URN forming) lives in
//! `pacewright_core::oauth` and is unit-tested there. This module is the thin IO glue: it opens the
//! browser, runs a one-shot loopback listener to catch the redirect, exchanges the code for tokens,
//! resolves the author URN, and persists everything through `SecretStore` (0600). None of that is
//! unit-testable without a network/browser, so it stays deliberately small and leans on the tested
//! core helpers.

use anyhow::{anyhow, bail, Context, Result};
use pacewright_core::oauth::{self, TokenResponse};
use pacewright_core::secrets::SecretStore;
use std::path::{Path, PathBuf};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

fn secrets_path(home: &Path) -> PathBuf {
    home.join("secrets.json")
}

/// `oauth login <provider> [--client-id X] [--client-secret Y] [--callback-base URL]`.
///
/// Client id/secret resolve in order: CLI flag → already stored → interactive prompt (the secret is
/// read without echo). Then opens the consent screen, catches the redirect on the loopback callback
/// (`{base}/{provider}/callback`, base defaulting to `http://localhost:8765`), swaps the code for
/// tokens, and (when a userinfo endpoint is configured) resolves the author/principal URN, then persists it all 0600.
#[allow(clippy::too_many_arguments)]
pub async fn login(
    home: PathBuf,
    provider: String,
    client_id: Option<String>,
    client_secret: Option<String>,
    callback_base: Option<String>,
    authorize_url: Option<String>,
    token_url: Option<String>,
    scope: Option<String>,
    userinfo_url: Option<String>,
    urn_template: Option<String>,
    id_field: Option<String>,
) -> Result<()> {
    let path = secrets_path(&home);
    let mut store = SecretStore::load(&path)?;

    // Resolve OAuth endpoints + scope: flag wins, else stored, else error (required) / None (optional).
    let stored = store.get(&provider);
    let authorize_base = authorize_url
        .or_else(|| stored.and_then(|r| r.authorize_url.clone()))
        .ok_or_else(|| anyhow!("provider `{provider}` needs --authorize-url on first login"))?;
    let token_endpoint = token_url
        .or_else(|| stored.and_then(|r| r.token_url.clone()))
        .ok_or_else(|| anyhow!("provider `{provider}` needs --token-url on first login"))?;
    let scope = scope
        .or_else(|| stored.and_then(|r| r.scope.clone()))
        .ok_or_else(|| anyhow!("provider `{provider}` needs --scope on first login"))?;
    let userinfo = userinfo_url.or_else(|| stored.and_then(|r| r.userinfo_url.clone()));
    let urn_template = urn_template.or_else(|| stored.and_then(|r| r.urn_template.clone()));
    let id_field = id_field
        .or_else(|| stored.and_then(|r| r.id_field.clone()))
        .unwrap_or_else(|| "sub".to_string());

    // Resolve credentials. Flags always win. Otherwise, if both are already stored, offer to keep
    // them or re-enter; if nothing is stored, prompt for both.
    let stored_id = stored
        .map(|r| r.client_id.clone())
        .filter(|s| !s.is_empty());
    let stored_secret = stored
        .map(|r| r.client_secret.clone())
        .filter(|s| !s.is_empty());
    let (id, secret) = if client_id.is_some() || client_secret.is_some() {
        // A flag was passed — take it, falling back to the stored value for whichever flag is absent.
        (
            client_id.or(stored_id).unwrap_or_default(),
            client_secret.or(stored_secret).unwrap_or_default(),
        )
    } else if let (Some(sid), Some(ssec)) = (stored_id, stored_secret) {
        if confirm(&format!(
            "Stored {provider} client id: {sid}. Change credentials? [y/N] "
        ))? {
            (
                prompt_line(&format!("{provider} client id: "))?,
                prompt_secret(&format!("{provider} client secret: "))?,
            )
        } else {
            (sid, ssec)
        }
    } else {
        (
            prompt_line(&format!("{provider} client id: "))?,
            prompt_secret(&format!("{provider} client secret: "))?,
        )
    };
    if id.is_empty() || secret.is_empty() {
        bail!("client id and secret are both required");
    }
    // Persist the app credentials now, before the browser dance, so they survive an aborted consent.
    store.set_app(&provider, id.clone(), secret.clone());
    store.set_endpoints(
        &provider,
        authorize_base.clone(),
        token_endpoint.clone(),
        scope.clone(),
        userinfo.clone(),
        Some(id_field.clone()),
        urn_template.clone(),
    );
    store.save().context("saving app credentials")?;

    // Provider-namespaced loopback redirect; the listener binds the port parsed back out of it.
    let base = callback_base.unwrap_or_else(|| oauth::DEFAULT_CALLBACK_BASE.to_string());
    let redirect_uri = oauth::callback_uri(&base, &provider);
    let port = oauth::redirect_port(&redirect_uri)?;

    // A fresh state nonce guards the callback against CSRF; uuid is already a cli dep.
    let state = uuid::Uuid::new_v4().to_string();
    let url = oauth::authorize_url(&authorize_base, &id, &redirect_uri, &scope, &state);

    // Bind BEFORE opening the browser so the redirect can't beat us to the socket.
    let listener = TcpListener::bind(("127.0.0.1", port))
        .await
        .with_context(|| format!("binding 127.0.0.1:{port} (is another `oauth login` running?)"))?;

    println!("Redirect URI: {redirect_uri}  (must be registered on the provider's app)");
    println!("Opening your browser to authorize `{provider}`…");
    println!("If it doesn't open, paste this into a browser:\n{url}\n");
    let _ = std::process::Command::new("open").arg(&url).status();

    let cb = accept_callback(&listener).await?;
    if cb.state != state {
        bail!("state mismatch on callback — possible CSRF, aborting");
    }

    let http = reqwest::Client::new();
    let token: TokenResponse = http
        .post(&token_endpoint)
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", &cb.code),
            ("redirect_uri", &redirect_uri),
            ("client_id", &id),
            ("client_secret", &secret),
        ])
        .send()
        .await
        .context("token exchange request")?
        .error_for_status()
        .context("token endpoint returned an error")?
        .json()
        .await
        .context("parsing token response")?;

    let now_ms = chrono::Utc::now().timestamp_millis();
    store.set_tokens(
        &provider,
        token.access_token.clone(),
        token.refresh_token.clone(),
        token.expires_at_ms(now_ms),
        token.scope.clone(),
    )?;

    // If a userinfo endpoint is configured, resolve the author/principal URN from it.
    if let Some(userinfo_url) = &userinfo {
        let info: serde_json::Value = http
            .get(userinfo_url)
            .bearer_auth(&token.access_token)
            .send()
            .await
            .context("userinfo request")?
            .error_for_status()
            .context("userinfo returned an error")?
            .json()
            .await
            .context("parsing userinfo")?;
        let id = info
            .get(&id_field)
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("userinfo response has no string field `{id_field}`"))?;
        let urn = match &urn_template {
            Some(t) => oauth::apply_urn_template(t, id),
            None => id.to_string(),
        };
        store.set_author_urn(&provider, &urn)?;
        println!("Author URN: {urn}");
    }

    store.save()?;
    println!(
        "✅ `{provider}` authorized — token stored at {}",
        path.display()
    );
    Ok(())
}

/// Print a yes/no prompt; true only on an explicit `y`/`yes` (default is no).
fn confirm(prompt: &str) -> Result<bool> {
    let ans = prompt_line(prompt)?;
    Ok(matches!(ans.to_ascii_lowercase().as_str(), "y" | "yes"))
}

/// Print a prompt and read one trimmed line of visible input from stdin.
fn prompt_line(prompt: &str) -> Result<String> {
    use std::io::Write;
    print!("{prompt}");
    std::io::stdout().flush().ok();
    let mut buf = String::new();
    std::io::stdin()
        .read_line(&mut buf)
        .context("reading input")?;
    Ok(buf.trim().to_string())
}

/// Print a prompt and read a secret, echoing `*` for each character (so a paste is visibly
/// registered) instead of the true text. Backspace erases; Enter submits; Ctrl-C cancels.
fn prompt_secret(prompt: &str) -> Result<String> {
    use crossterm::event::{read, Event, KeyCode, KeyEventKind, KeyModifiers};
    use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
    use std::io::Write;

    print!("{prompt}");
    std::io::stdout().flush().ok();

    enable_raw_mode().context("entering raw mode for secret input (needs a terminal)")?;
    let outcome = (|| -> Result<Option<String>> {
        let mut secret = String::new();
        loop {
            let Event::Key(key) = read().context("reading key")? else {
                continue;
            };
            if key.kind == KeyEventKind::Release {
                continue; // some platforms emit both press and release
            }
            match key.code {
                KeyCode::Enter => return Ok(Some(secret)),
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    return Ok(None);
                }
                KeyCode::Char(c) => {
                    secret.push(c);
                    print!("*");
                    std::io::stdout().flush().ok();
                }
                KeyCode::Backspace if secret.pop().is_some() => {
                    print!("\u{8} \u{8}"); // erase the last '*'
                    std::io::stdout().flush().ok();
                }
                _ => {}
            }
        }
    })();
    disable_raw_mode().ok();
    println!(); // terminate the prompt line
    match outcome? {
        Some(s) => Ok(s.trim().to_string()),
        None => bail!("secret entry cancelled"),
    }
}

/// Accept exactly one connection, parse the request line, reply with a friendly HTML page.
async fn accept_callback(listener: &TcpListener) -> Result<oauth::Callback> {
    let (mut stream, _) = listener
        .accept()
        .await
        .context("accepting callback connection")?;
    let mut buf = vec![0u8; 8192];
    let n = stream
        .read(&mut buf)
        .await
        .context("reading callback request")?;
    let text = String::from_utf8_lossy(&buf[..n]);
    let request_line = text.lines().next().unwrap_or("");
    let result = oauth::parse_callback(request_line);

    let body = if result.is_ok() {
        "<!doctype html><meta charset=utf-8><title>pacewright</title>\
         <body style=\"font:16px system-ui;padding:3rem\"><h2>pacewright</h2>\
         <p>Authorized. You can close this tab and return to the terminal.</p>"
    } else {
        "<!doctype html><meta charset=utf-8><title>pacewright</title>\
         <body style=\"font:16px system-ui;padding:3rem\"><h2>pacewright</h2>\
         <p>Authorization failed. Check the terminal for details.</p>"
    };
    let resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let _ = stream.write_all(resp.as_bytes()).await;
    let _ = stream.flush().await;
    result
}

/// `oauth status` — one line per known provider: app configured? token present? scope + URN.
pub async fn status(home: PathBuf) -> Result<()> {
    let store = SecretStore::load(secrets_path(&home))?;
    let now_ms = chrono::Utc::now().timestamp_millis();
    let providers = store.providers();
    if providers.is_empty() {
        println!("no providers configured — run `pacewright oauth login <provider> --authorize-url … --token-url … --scope …`");
        return Ok(());
    }
    for p in providers {
        let Some(r) = store.get(p) else { continue };
        let app = if r.client_id.is_empty() {
            "no app"
        } else {
            "app set"
        };
        let tok = match (
            r.access_token.is_some(),
            store.valid_access_token(p, now_ms, 300_000),
        ) {
            (false, _) => "no token",
            (true, Some(_)) => "token valid",
            (true, None) => "token expired",
        };
        println!(
            "{p:<10} {app} · {tok} · scope: {} · urn: {}",
            r.scope.as_deref().unwrap_or("-"),
            r.author_urn.as_deref().unwrap_or("-"),
        );
    }
    Ok(())
}

/// `oauth logout <provider>` — drop the tokens but keep the app credentials for a quick re-login.
pub async fn logout(home: PathBuf, provider: String) -> Result<()> {
    let path = secrets_path(&home);
    let mut store = SecretStore::load(&path)?;
    if store.get(&provider).is_none() {
        println!("`{provider}` is not configured — nothing to do");
        return Ok(());
    }
    store.clear_tokens(&provider);
    store.save()?;
    println!("Cleared `{provider}` tokens (app credentials kept)");
    Ok(())
}

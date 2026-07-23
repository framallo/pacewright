//! `pcw auth …` — a thin, friendly client over the daemon's `Auth*` RPCs. The daemon owns all
//! state (the status cache) and orchestration (headed login, the bounded poll); this only formats.
//!
//! - `status` → a table (account · signed-in/out/unknown · recipes using it · last checked).
//! - `login [account] [--all]` → ask the daemon to pop a headed Chrome window for the operator.
//! - `recheck [account]` → force a status refresh and reprint the table.

use anyhow::Result;
use pacewright_proto::{Request, Response};
use serde_json::Value;
use std::path::Path;

use crate::client;

/// Render the `accounts` array from an `AuthList`/`AuthRecheck` response as a table.
fn print_table(accounts: &[Value]) {
    if accounts.is_empty() {
        println!("no accounts — add a login recipe under ~/.pacewright/recipes/accounts/<name>.kdl");
        return;
    }
    println!("{:<24}  {:<10}  {:<20}  RECIPES", "ACCOUNT", "SESSION", "LAST CHECKED");
    for a in accounts {
        let name = a["account"].as_str().unwrap_or("?");
        let session = match a["signed_in"].as_bool() {
            _ if a["logging_in"].as_bool() == Some(true) => "logging in…",
            Some(true) => "signed in",
            Some(false) => "signed out",
            None => "unknown",
        };
        let checked = match a["last_checked"].as_i64() {
            Some(ms) => ms_ago(ms),
            None => "never".to_string(),
        };
        let recipes = a["recipes"].as_array().map(|r| {
            r.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", ")
        }).unwrap_or_default();
        println!("{name:<24}  {session:<10}  {checked:<20}  {recipes}");
    }
}

/// A coarse "Nm ago" from an epoch-ms stamp, using the wall clock (display only).
fn ms_ago(ms: i64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(ms);
    let secs = (now - ms).max(0) / 1000;
    if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else {
        format!("{}h ago", secs / 3600)
    }
}

fn accounts_of(resp: &Response) -> Result<Vec<Value>> {
    match resp {
        Response::Ok(v) => Ok(v["accounts"].as_array().cloned().unwrap_or_default()),
        Response::Error { message } => anyhow::bail!("{message}"),
    }
}

pub async fn status(sock: &Path) -> Result<()> {
    let resp = client::call(sock, Request::AuthList).await?;
    print_table(&accounts_of(&resp)?);
    Ok(())
}

pub async fn recheck(sock: &Path, account: Option<String>) -> Result<()> {
    eprintln!("rechecking sessions (running each account's check headless)…");
    let resp = client::call(sock, Request::AuthRecheck { account }).await?;
    print_table(&accounts_of(&resp)?);
    Ok(())
}

/// Block until the operator presses Return. Reading stdin here (in the CLI, which owns the terminal)
/// is what gates the recheck: the login window is left completely alone while the human signs in —
/// no automated navigation touches the profile — and only once they confirm do we drive the browser
/// to read the session. This is the fix for the tab-storm that a timed auto-poll caused.
fn wait_for_return(msg: &str) -> Result<()> {
    use std::io::Write;
    print!("{msg}");
    std::io::stdout().flush().ok();
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(())
}

pub async fn login(sock: &Path, account: Option<String>, all: bool) -> Result<()> {
    if all || account.is_none() {
        let resp = client::call(sock, Request::AuthLoginAll).await?;
        let opened: Vec<String> = match resp {
            Response::Ok(v) => v["opened"]
                .as_array()
                .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
                .unwrap_or_default(),
            Response::Error { message } => anyhow::bail!("{message}"),
        };
        if opened.is_empty() {
            println!("nothing to do — every account is already signed in.");
            return Ok(());
        }
        println!("opened login windows for: {}", opened.join(", "));
        println!("sign in to each by hand (take your time; 2FA/checkpoint is fine).");
        wait_for_return("press Return once you're signed in to all of them… ")?;
        eprintln!("checking sessions…");
        let re = client::call(sock, Request::AuthRecheck { account: None }).await?;
        print_table(&accounts_of(&re)?);
        return Ok(());
    }
    let account = account.unwrap();
    let resp = client::call(sock, Request::AuthLogin { account: account.clone() }).await?;
    match resp {
        Response::Ok(_) => {
            println!("opened a login window for `{account}` — sign in by hand (take your time; 2FA/checkpoint is fine).");
            println!("nothing will drive the window while you sign in.");
            wait_for_return("press Return once you're signed in… ")?;
            eprintln!("checking the session…");
            let re = client::call(sock, Request::AuthRecheck { account: Some(account) }).await?;
            print_table(&accounts_of(&re)?);
            Ok(())
        }
        Response::Error { message } => anyhow::bail!("{message}"),
    }
}

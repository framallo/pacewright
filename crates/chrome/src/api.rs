//! High-level in-process entry points for embedding chrome-agent as a library.
//!
//! pacewright links this crate and calls [`run_recipe_attached`] instead of shelling
//! `chrome-agent recipe run <file>`: it attaches to the always-on Chrome, resolves the named page,
//! runs the KDL recipe, and **returns the result JSON** (the CLI's `run_recipe` only prints it).
//! The messy session/attach bookkeeping stays here, so the consumer's side is one call.
use std::collections::BTreeMap;

use crate::BoxError;
use crate::cdp::client::CdpClient;
use crate::{browser, recipe, run_helpers, session};

/// How to attach: the always-on Chrome endpoint + the named browser/page to reuse.
pub struct RecipeAttach<'a> {
    pub connect: &'a str,
    pub browser: &'a str,
    pub page: &'a str,
    pub stealth: bool,
    pub timeout_secs: u64,
    pub activate: bool,
}

/// Parse `--var`-style pairs + a `--vars-json` object into the recipe var map (a thin re-export of
/// [`recipe::parse_vars`] so callers don't reach into the recipe module).
pub fn parse_vars(
    pairs: &[String],
    vars_json: Option<&str>,
) -> Result<BTreeMap<String, String>, BoxError> {
    recipe::parse_vars(pairs, vars_json)
}

/// Attach, run the recipe at `file` with `vars`, return its [`recipe::engine::Outcome`]
/// (`result` JSON + any `unexpected` notes). Never prints. In-process equivalent of
/// `chrome-agent --connect <c> --browser <b> --page <p> recipe run <file> --vars-json …`.
pub async fn run_recipe_attached(
    at: &RecipeAttach<'_>,
    file: &str,
    vars: BTreeMap<String, String>,
    solver: Option<&dyn recipe::engine::Solver>,
) -> Result<recipe::engine::Outcome, BoxError> {
    let mut store = session::load_session()?;
    let opts = browser::BrowserOptions {
        name: at.browser.to_string(),
        headless: true,
        ignore_https_errors: false,
        stealth: at.stealth,
        // Always attach to the operator's always-on Chrome; never launch.
        connect: Some(at.connect.to_string()),
        copy_cookies: false,
    };
    let conn = browser::resolve_browser(&opts).await?;
    let browser_client = CdpClient::connect(&conn.ws_endpoint).await?;
    let http_endpoint = conn
        .http_endpoint
        .clone()
        .ok_or("no HTTP endpoint on the browser connection")?;

    let target_id = {
        let bs = session::ensure_browser(&mut store, at.browser, &conn.ws_endpoint, conn.pid, true);
        run_helpers::resolve_page_target(&browser_client, bs, at.page).await?
    };
    let _ = session::save_session(&mut store);

    let client = run_helpers::connect_page(&http_endpoint, &target_id, at.stealth).await?;
    if at.activate {
        let _ = client.send("Page.bringToFront", serde_json::json!({})).await;
    }

    let src = std::fs::read_to_string(file).map_err(|e| format!("reading {file}: {e}"))?;
    let rec = recipe::model::Recipe::parse(&src)?;
    let rb = recipe::browser::CdpBrowser::new(
        client,
        target_id,
        at.timeout_secs,
        Some(&browser_client),
        Some(http_endpoint),
        at.stealth,
    );
    let run_opts = recipe::engine::RunOptions {
        log: false,
        step_timeout_ms: at.timeout_secs.saturating_mul(1000).max(1000),
        repair: false,
    };
    recipe::engine::run(&rec, &vars, &rb, &run_opts, solver)
        .await
        .map_err(|e| Box::new(e) as BoxError)
}

/// Attach and navigate the named page to `url`, raising the window — the in-process equivalent of
/// `chrome-agent --browser <b> --page <p> --activate goto <url>`, used to open a login window in the
/// operator's always-on (visible) Chrome. Returns once navigation is issued; the human signs in.
pub async fn open_page(at: &RecipeAttach<'_>, url: &str) -> Result<(), BoxError> {
    let mut store = session::load_session()?;
    let opts = browser::BrowserOptions {
        name: at.browser.to_string(),
        headless: false,
        ignore_https_errors: false,
        stealth: at.stealth,
        connect: Some(at.connect.to_string()),
        copy_cookies: false,
    };
    let conn = browser::resolve_browser(&opts).await?;
    let browser_client = CdpClient::connect(&conn.ws_endpoint).await?;
    let http_endpoint = conn
        .http_endpoint
        .clone()
        .ok_or("no HTTP endpoint on the browser connection")?;
    let target_id = {
        let bs = session::ensure_browser(&mut store, at.browser, &conn.ws_endpoint, conn.pid, false);
        run_helpers::resolve_page_target(&browser_client, bs, at.page).await?
    };
    let _ = session::save_session(&mut store);
    let client = run_helpers::connect_page(&http_endpoint, &target_id, at.stealth).await?;
    client
        .send("Page.navigate", serde_json::json!({ "url": url }))
        .await
        .map_err(|e| format!("navigate {url}: {e}"))?;
    let _ = client.send("Page.bringToFront", serde_json::json!({})).await;
    Ok(())
}

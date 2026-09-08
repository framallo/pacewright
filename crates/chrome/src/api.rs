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
///
/// Thin wrapper over [`run_recipe_attached_src`]: reads the file, then delegates. Callers that
/// already hold the recipe text (a database row, a spool entry) should call that one directly
/// instead of writing a temp file just to have a path.
pub async fn run_recipe_attached(
    at: &RecipeAttach<'_>,
    file: &str,
    vars: BTreeMap<String, String>,
    solver: Option<&dyn recipe::engine::Solver>,
) -> Result<recipe::engine::Outcome, BoxError> {
    let src = std::fs::read_to_string(file).map_err(|e| format!("reading {file}: {e}"))?;
    run_recipe_attached_src(at, &src, vars, solver).await
}

/// Same as [`run_recipe_attached`], but takes the recipe **source** instead of a path.
///
/// Exists because a recipe does not have to live in a file: cazafacturas keeps them as rows in
/// its own database so an authored or self-healed recipe can be stored, versioned and rolled back
/// like any other record. Materialising such a row to a temp file just to hand back a path was the
/// alternative, and it buys nothing — the engine only ever needed the text.
pub async fn run_recipe_attached_src(
    at: &RecipeAttach<'_>,
    src: &str,
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

    // `we_created_tab` decides cleanup. pacewright shares the operator's always-on Chrome with
    // humans and other agents, and for `page = "default"` resolve_page_target *adopts* the first
    // unclaimed tab it finds, which may be someone else's. Closing an adopted tab would destroy
    // their work, so only a tab we created ourselves is ever closed.
    let (target_id, we_created_tab) = {
        let bs = session::ensure_browser(&mut store, at.browser, &conn.ws_endpoint, conn.pid, true);
        run_helpers::resolve_page_target_ext(&browser_client, bs, at.page).await?
    };
    let _ = session::save_session(&mut store);

    let client = run_helpers::connect_page(&http_endpoint, &target_id, at.stealth).await?;
    // Auto-answer native JS dialogs for the whole run. Without this, the Chrome
    // "Leave site? Changes you made may not be saved." beforeunload dialog that
    // X/Twitter raises when navigating away from a compose box (and any
    // alert/confirm/prompt) blocks the page with no DOM signal, so the recipe's
    // next CDP command hangs until the run times out. `connect_page` already
    // enabled the Page domain, so `Page.javascriptDialogOpening` fires. `Accept`
    // matches the CLI default intent: beforeunload → proceed (click Leave),
    // alert/confirm/prompt → accept. The handler lives as long as this client.
    client.spawn_dialog_handler(crate::setup::DialogPolicy::Accept, None);
    // Belt-and-suspenders for the "Leave site?" beforeunload prompt: on top of the
    // dialog handler above, neutralize beforeunload on every document so navigating
    // away from a page that set onbeforeunload (e.g. X's compose box) never raises
    // the prompt at all. Registered for future documents (survives navigations) and
    // run once on the current one. try/catch so a hardened page can never break the run.
    const BEFOREUNLOAD_JS: &str = "try{window.addEventListener('beforeunload',function(e){e.stopImmediatePropagation();delete e['returnValue'];},true);Object.defineProperty(window,'onbeforeunload',{configurable:true,get:function(){return null;},set:function(){}});}catch(e){}";
    let _ = client
        .send(
            "Page.addScriptToEvaluateOnNewDocument",
            serde_json::json!({ "source": BEFOREUNLOAD_JS }),
        )
        .await;
    let _ = crate::commands::eval::run_raw(&client, BEFOREUNLOAD_JS).await;
    if at.activate {
        let _ = client
            .send("Page.bringToFront", serde_json::json!({}))
            .await;
    }

    let rec = recipe::model::Recipe::parse(src)?;
    let rb = recipe::browser::CdpBrowser::new(
        client,
        target_id.clone(),
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
    let outcome = recipe::engine::run(&rec, &vars, &rb, &run_opts, solver).await;

    // Tab hygiene: close the tab this run used so tabs do not pile up across scheduled runs,
    // then unregister it so the next run creates a fresh one. Gated on `we_created_tab`, NOT on
    // the page name: this Chrome is shared with the operator and other agents, and a "default"
    // run may have adopted a tab that a human opened. Closing only what we created is the only
    // safe rule. Runs on both success and failure. Dropping `rb` first releases its borrow of
    // `browser_client`.
    drop(rb);
    if we_created_tab {
        let _: serde_json::Value = browser_client
            .call(
                "Target.closeTarget",
                serde_json::json!({ "targetId": target_id }),
            )
            .await
            .unwrap_or_default();
        if let Ok(mut s) = session::load_session() {
            if let Some(bs) = s.browsers.get_mut(at.browser) {
                bs.pages.remove(at.page);
            }
            let _ = session::save_session(&mut s);
        }
    }

    outcome.map_err(|e| Box::new(e) as BoxError)
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
        let bs =
            session::ensure_browser(&mut store, at.browser, &conn.ws_endpoint, conn.pid, false);
        run_helpers::resolve_page_target(&browser_client, bs, at.page).await?
    };
    let _ = session::save_session(&mut store);
    let client = run_helpers::connect_page(&http_endpoint, &target_id, at.stealth).await?;
    client
        .send("Page.navigate", serde_json::json!({ "url": url }))
        .await
        .map_err(|e| format!("navigate {url}: {e}"))?;
    let _ = client
        .send("Page.bringToFront", serde_json::json!({}))
        .await;
    Ok(())
}

//! Declarative KDL **recipe** engine — run a data file (not code) that navigates a page and
//! extracts structured data with Playwright-style semantic locators.
//!
//! A recipe is inert data: it names locators and steps, never executable JS (the locator
//! runtime ships with the engine). This is the substrate for shareable, testable, and
//! LLM-repairable browser automation. See `docs/` in the pacewright repo for the full spec.
//!
//! Module layout:
//! - `model`   — typed `Recipe`/`Step`/`Locator`/`Output` parsed from `kdl::KdlDocument`.
//! - `browser` — the `RecipeBrowser` trait + `CdpBrowser` (real) / `FakeBrowser` (test).
//! - `engine`  — runs a `Recipe` over a `RecipeBrowser`; a `Locator` is passed to the runtime as
//!   its serde JSON (no separate serializer needed).
//! - `render`  — the minimal output templater + file writing for `output` blocks.
//! - `runtime/locators.js` — the injected `__pw` locator runtime.

pub mod browser;
pub mod engine;
pub mod model;
pub mod render;

use std::collections::BTreeMap;

use serde_json::Value;

use crate::BoxError;
use crate::cdp::client::CdpClient;

/// Parse `--var name=value` pairs and an optional `--vars-json` object into a var map. `--var`
/// wins over `--vars-json` on conflict. JSON scalars are stringified (the recipe's vars are text).
pub fn parse_vars(
    pairs: &[String],
    vars_json: Option<&str>,
) -> Result<BTreeMap<String, String>, BoxError> {
    let mut map = BTreeMap::new();
    if let Some(js) = vars_json {
        let obj: serde_json::Map<String, Value> = serde_json::from_str(js)
            .map_err(|e| format!("--vars-json is not a JSON object: {e}"))?;
        for (k, v) in obj {
            let s = match v {
                Value::String(s) => s,
                other => other.to_string(),
            };
            map.insert(k, s);
        }
    }
    for p in pairs {
        let (k, v) = p
            .split_once('=')
            .ok_or_else(|| format!("--var must be name=value, got {p:?}"))?;
        map.insert(k.to_string(), v.to_string());
    }
    Ok(map)
}

/// Repair flags for `recipe run`.
pub struct RepairOpts {
    pub enabled: bool,
    pub out: Option<String>,
}

/// `chrome-agent recipe run <FILE>` — load, run over the page, print the result as JSON, and
/// write the recipe's output files. Under `--repair`, an unexpected/failed run emits a Claude
/// repair context (recipe + failure + page snapshot + guidance) and exits non-zero.
#[allow(clippy::too_many_arguments)]
pub async fn run_recipe(
    client: CdpClient,
    target_id: String,
    browser_client: &CdpClient,
    http_endpoint: String,
    stealth: bool,
    file: &str,
    vars: BTreeMap<String, String>,
    log: bool,
    repair: RepairOpts,
    timeout_secs: u64,
    json: bool,
) -> Result<(), BoxError> {
    let src = std::fs::read_to_string(file).map_err(|e| format!("reading {file}: {e}"))?;
    let recipe = model::Recipe::parse(&src)?;
    let browser = browser::CdpBrowser::new(
        client,
        target_id,
        timeout_secs,
        Some(browser_client),
        Some(http_endpoint),
        stealth,
    );
    let opts = engine::RunOptions {
        log,
        step_timeout_ms: timeout_secs.saturating_mul(1000).max(1000),
        repair: repair.enabled,
        dry_run: false,
    };

    match engine::run(&recipe, &vars, &browser, &opts, None).await {
        Ok(outcome) => {
            if json {
                let obj = serde_json::json!({
                    "ok": true,
                    "result": outcome.result,
                    "unexpected": outcome.unexpected,
                });
                println!("{}", serde_json::to_string(&obj)?);
            } else {
                println!("{}", serde_json::to_string_pretty(&outcome.result)?);
                if !outcome.unexpected.is_empty() {
                    eprintln!("unexpected: {}", outcome.unexpected.join("; "));
                }
            }
            if repair.enabled && !outcome.unexpected.is_empty() {
                let ctx = repair_context(
                    &browser.active_client(),
                    &src,
                    &recipe,
                    &format!("unexpected result: {}", outcome.unexpected.join("; ")),
                )
                .await;
                emit_repair(&ctx, repair.out.as_deref())?;
                return Err("unexpected result — repair context emitted".into());
            }
            Ok(())
        }
        Err(e) => {
            if repair.enabled {
                let ctx = repair_context(
                    &browser.active_client(),
                    &src,
                    &recipe,
                    &format!("run failed: {e}"),
                )
                .await;
                emit_repair(&ctx, repair.out.as_deref())?;
            }
            Err(Box::new(e))
        }
    }
}

/// Run an `api`-only recipe (no page steps) with **no browser** — no Chrome launch, no session.
/// Mirrors [`run_recipe`] but over a [`browser::NativeBrowser`]; the CLI selects this path when
/// `Recipe::needs_browser()` is false. Repair contexts here omit the page snapshot (there is none).
pub async fn run_recipe_native(
    recipe: model::Recipe,
    src: String,
    vars: BTreeMap<String, String>,
    log: bool,
    repair: RepairOpts,
    json: bool,
) -> Result<(), BoxError> {
    let browser = browser::NativeBrowser::new();
    let opts = engine::RunOptions {
        log,
        step_timeout_ms: 30_000,
        repair: repair.enabled,
        dry_run: false,
    };
    match engine::run(&recipe, &vars, &browser, &opts, None).await {
        Ok(outcome) => {
            if json {
                let obj = serde_json::json!({
                    "ok": true,
                    "result": outcome.result,
                    "unexpected": outcome.unexpected,
                });
                println!("{}", serde_json::to_string(&obj)?);
            } else {
                println!("{}", serde_json::to_string_pretty(&outcome.result)?);
                if !outcome.unexpected.is_empty() {
                    eprintln!("unexpected: {}", outcome.unexpected.join("; "));
                }
            }
            if repair.enabled && !outcome.unexpected.is_empty() {
                let ctx = native_repair_context(
                    &src,
                    &recipe,
                    &format!("unexpected result: {}", outcome.unexpected.join("; ")),
                );
                emit_repair(&ctx, repair.out.as_deref())?;
                return Err("unexpected result — repair context emitted".into());
            }
            Ok(())
        }
        Err(e) => {
            if repair.enabled {
                let ctx = native_repair_context(&src, &recipe, &format!("run failed: {e}"));
                emit_repair(&ctx, repair.out.as_deref())?;
            }
            Err(Box::new(e))
        }
    }
}

/// Repair context for a browser-less run (no accessibility snapshot to include).
fn native_repair_context(src: &str, recipe: &model::Recipe, failure: &str) -> String {
    let guidance = recipe.repair_prompt.as_deref().unwrap_or("(none)");
    format!(
        "# Recipe repair context (browser-less run)\n\n## What went wrong\n{failure}\n\n\
## Recipe `{}`\n```kdl\n{src}\n```\n\n## Author repair guidance\n{guidance}\n",
        recipe.name
    )
}

/// Truncate to at most `max_bytes`, backing off to a char boundary (`String::truncate` panics
/// mid-character, and page text is anything but ASCII).
pub fn truncate_at(s: &mut String, max_bytes: usize) {
    if s.len() <= max_bytes {
        return;
    }
    let mut end = max_bytes;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s.truncate(end);
}

/// Each capture in [`failure_context`] gets this long; the page may be gone (a closed tab is the
/// single most likely failure), and the failure path must never hang the run.
const CAPTURE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
/// `page_text` / `ax_tree` caps (bytes) and the screenshot cap (decoded PNG bytes, estimated
/// from the base64 length) — above it the screenshot is omitted rather than bloating a task row.
const PAGE_TEXT_MAX: usize = 8_000;
const AX_TREE_MAX: usize = 12_000;
const SCREENSHOT_MAX_BYTES: usize = 1_500_000;

/// The live accessibility tree as text, capped at `max_bytes`; a one-line note when unavailable.
async fn ax_snapshot(client: &CdpClient, max_bytes: usize) -> String {
    match tokio::time::timeout(
        CAPTURE_TIMEOUT,
        crate::commands::inspect::run(client, false, Some(12), None, None),
    )
    .await
    {
        Ok(Ok(s)) => {
            let mut t = s.text;
            truncate_at(&mut t, max_bytes);
            t
        }
        Ok(Err(e)) => format!("(accessibility snapshot unavailable: {e})"),
        Err(_) => "(accessibility snapshot timed out)".to_string(),
    }
}

async fn eval_string(client: &CdpClient, js: &str) -> Option<String> {
    match tokio::time::timeout(CAPTURE_TIMEOUT, crate::commands::eval::run_raw(client, js)).await {
        Ok(Ok(Value::String(s))) => Some(s),
        Ok(Ok(Value::Null)) | Ok(Err(_)) | Err(_) => None,
        Ok(Ok(other)) => Some(other.to_string()),
    }
}

/// What the page looked like when a run died, as one JSON object a self-heal loop can read back
/// from the task row:
///
/// ```json
/// {"error": "…", "step_index": 3, "step": "click { locator text=\"Consultar\" }",
///  "url": "…", "title": "…", "page_text": "…", "ax_tree": "…", "screenshot_b64": "…",
///  "unexpected": ["…"]}
/// ```
///
/// Every capture is best-effort and individually bounded by [`CAPTURE_TIMEOUT`]: a field is
/// `null` when the page could not answer. `screenshot_b64` is omitted above
/// [`SCREENSHOT_MAX_BYTES`]. `client` should be the *active* page (a followed tab, if any).
pub async fn failure_context(client: &CdpClient, e: &engine::RecipeError) -> Value {
    let mut obj = page_snapshot(client).await;
    obj["error"] = Value::from(e.message.clone());
    obj["step_index"] = serde_json::json!(e.step_index);
    obj["step"] = serde_json::json!(e.step);
    obj["unexpected"] = serde_json::json!(e.unexpected);
    obj
}

/// The page as it stands — `url`, `title`, `page_text` (bounded), `ax_tree` (bounded) and a
/// `screenshot_b64` when small enough. Each capture is best-effort: `null` when the page could not
/// answer. Shared by [`failure_context`] and the stop point of a dry run.
pub async fn page_snapshot(client: &CdpClient) -> Value {
    let url = eval_string(client, "location.href").await;
    let title = eval_string(client, "document.title").await;
    let page_text = eval_string(client, "(document.body ? document.body.innerText : '')")
        .await
        .map(|mut t| {
            truncate_at(&mut t, PAGE_TEXT_MAX);
            t
        });
    let ax_tree = ax_snapshot(client, AX_TREE_MAX).await;
    let screenshot_b64 = match tokio::time::timeout(
        CAPTURE_TIMEOUT,
        client.call::<_, Value>(
            "Page.captureScreenshot",
            serde_json::json!({ "format": "png" }),
        ),
    )
    .await
    {
        Ok(Ok(v)) => v
            .get("data")
            .and_then(Value::as_str)
            .filter(|b64| b64.len() / 4 * 3 <= SCREENSHOT_MAX_BYTES)
            .map(str::to_string),
        _ => None,
    };
    let mut obj = serde_json::json!({
        "url": url,
        "title": title,
        "page_text": page_text,
        "ax_tree": ax_tree,
    });
    if let Some(shot) = screenshot_b64 {
        obj["screenshot_b64"] = Value::String(shot);
    }
    obj
}

/// Assemble the Claude repair context: a fixed system instruction, the failure, the recipe
/// source (for a surgical edit), the author's repair prompt, and a live accessibility snapshot to
/// re-anchor locators against.
async fn repair_context(
    client: &CdpClient,
    src: &str,
    recipe: &model::Recipe,
    failure: &str,
) -> String {
    const SYSTEM: &str = "You repair declarative KDL browser recipes. Output ONLY a corrected \
recipe. Change locator nodes only; preserve every step key, the step order, and the non-Turing \
schema; add no executable JS; prefer the highest robustness tier that resolves (semantic \
role/text > relative after/near > positional nth/within > css).";

    let snapshot = ax_snapshot(client, 6000).await;
    let guidance = recipe.repair_prompt.as_deref().unwrap_or("(none)");

    format!(
        "# Recipe repair context\n\n## System instruction\n{SYSTEM}\n\n## What went wrong\n{failure}\n\n\
## Recipe `{}`\n```kdl\n{src}\n```\n\n## Author repair guidance\n{guidance}\n\n\
## Current page (accessibility tree)\n```\n{snapshot}\n```\n",
        recipe.name
    )
}

fn emit_repair(ctx: &str, out: Option<&str>) -> Result<(), BoxError> {
    if let Some(path) = out {
        std::fs::write(path, ctx).map_err(|e| format!("writing repair context to {path}: {e}"))?;
        eprintln!("repair context written to {path}");
    } else {
        eprintln!("{ctx}");
    }
    Ok(())
}

/// `chrome-agent recipe check <FILE>` — parse + validate a recipe without a browser. Exit 0 on a
/// well-formed recipe, non-zero (with the parse/validation error) otherwise.
pub fn check(path: &str, json: bool) -> Result<(), BoxError> {
    let src = std::fs::read_to_string(path).map_err(|e| format!("reading {path}: {e}"))?;
    let recipe = model::Recipe::parse(&src)?;
    if json {
        let obj = serde_json::json!({ "ok": true, "recipe": recipe });
        println!("{}", serde_json::to_string(&obj)?);
    } else {
        println!(
            "ok: {} ({} step(s), {} var(s), {} output(s))",
            recipe.name,
            recipe.steps.len(),
            recipe.vars.len(),
            recipe.outputs.len(),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::truncate_at;

    #[test]
    fn truncate_at_backs_off_to_a_char_boundary() {
        let mut s = "ñandú".to_string(); // ñ = 2 bytes, ú = 2 bytes
        truncate_at(&mut s, 2); // inside ñ? no: ñ ends at 2 → "ñ"
        assert_eq!(s, "ñ");
        let mut s = "ñandú".to_string();
        truncate_at(&mut s, 1); // mid-ñ → back off to 0
        assert_eq!(s, "");
        let mut s = "abc".to_string();
        truncate_at(&mut s, 10);
        assert_eq!(s, "abc");
    }
}

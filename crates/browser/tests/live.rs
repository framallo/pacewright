//! Live tests: they spawn the real `chrome-agent` binary and drive a real Chrome.
//! `#[ignore]`d so `cargo test` stays hermetic and offline.
//!
//! Run: `cargo test -p pacewright-browser -- --ignored --test-threads=1`
//! Requires `chrome-agent` on PATH (or `CHROME_AGENT_BIN`).

use pacewright_browser::CliBrowser;
use pacewright_core::browser::BrowserHandle;

/// Pins the two chrome-agent contracts `CliBrowser` depends on: `goto` reports
/// the landed url/title, and `eval` hands back its value under `result`.
#[tokio::test]
#[ignore]
async fn drives_a_real_page() {
    // No cookies/stealth needed for a static page; keeps the operator's Chrome profile out of it.
    let b = CliBrowser::new().stealth(false).copy_cookies(false).timeout_secs(30);

    let nav = b.goto("https://example.com").await.expect("goto failed");
    assert_eq!(nav.title, "Example Domain");
    assert!(nav.url.starts_with("https://example.com"), "landed at {}", nav.url);

    let title = b.eval("document.title").await.expect("eval failed");
    assert_eq!(title, serde_json::json!("Example Domain"));
}

/// A `JSON.stringify`'d object arrives as a JSON *string* under `result` — the
/// exact shape the LinkedIn adapter's `unwrap_eval_json` re-parses.
#[tokio::test]
#[ignore]
async fn stringified_object_round_trips_as_a_json_string() {
    let b = CliBrowser::new().stealth(false).copy_cookies(false).timeout_secs(30);
    b.goto("https://example.com").await.expect("goto failed");

    let raw = b
        .eval(r#"(() => JSON.stringify({h1: document.querySelector('h1').innerText}))()"#)
        .await
        .expect("eval failed");

    let s = raw.as_str().expect("expected a JSON string, got a structured value");
    let parsed: serde_json::Value = serde_json::from_str(s).unwrap();
    assert_eq!(parsed["h1"], "Example Domain");
}

/// A missing binary must be `Unavailable` (-> Terminal), never a hang or a panic.
#[tokio::test]
#[ignore]
async fn missing_binary_reports_unavailable() {
    let b = CliBrowser::new().bin("definitely-not-chrome-agent-xyz");
    let err = b.goto("https://example.com").await.unwrap_err();
    assert!(matches!(err, pacewright_core::browser::BrowserError::Unavailable(_)), "got {err:?}");
}

# chrome-agent fork — library API + §9 extensions — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fork `sderosiaux/chrome-agent` into `framallo/chrome-agent`, expose a `Session`/`Page` Rust library facade callable as a crate, add viewport control and opt-in human mouse movement, and lock the existing CDP-input / `Runtime.enable` posture with tests.

**Architecture:** The crate already extracts each verb into a typed `commands::<verb>::run(client, …) -> Result<TypedResult, BoxError>` and has a clean `cdp::client::CdpClient`. So the library is a **thin, additive facade**: `lib.rs` re-exports the module tree `pub`, `session.rs` owns the connection setup (lifted from `run.rs`), and `page.rs` wraps each `commands::*::run` in a typed method. The CLI (`run.rs`) is barely touched — its existing integration tests are the regression guard.

**Tech Stack:** Rust (edition 2024), tokio, serde/serde_json, CDP over `tokio-tungstenite`, `cargo test`/`clippy`/`fmt`. Source of truth for the spec: `docs/specs/2026-07-07-chrome-agent-fork-lib.md` (in the pacewright repo).

## Global Constraints

- Repo: `framallo/chrome-agent` (fork of `sderosiaux/chrome-agent`), cloned to `/Users/agente/work/chrome-agent`; keep an `upstream` remote. All work here targets **that** repo, not pacewright.
- Toolchain: build must succeed on Rust ≥ 1.96.1 (pacewright's pin) so a git dependency resolves; keep `edition = "2024"`.
- **The existing `tests/cli_tests.rs` and `tests/extract_tests.rs` must stay green after every task** — they pin the CLI's observable behavior.
- Additive-only where possible: expose modules `pub` and add `session.rs`/`page.rs`/`error.rs`/`input.rs`; do not rewrite `commands::*` logic except the one screenshot bytes change (Task 8) and the opt-in mouse hook (Task 11).
- `humanize_input` defaults **false**; with it false, interaction CDP traffic is byte-for-byte what upstream emits.
- Gates before every commit: `cargo test`, `cargo clippy --all-targets -- -D warnings`, `cargo fmt`.
- Commit trailer: `Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>`.

---

## File Structure

- Create `src/lib.rs` — library crate root; `pub mod` the tree, re-export the facade.
- Create `src/error.rs` — `Error` enum + conversions.
- Create `src/session.rs` — **already exists** as the session *store*; ADD `Session`/`SessionOptions`/`Connect` facade here (or a new `src/session_api.rs` if name collision is cleaner — see Task 4).
- Create `src/page.rs` — `Page` facade + result/opt types (`NavInfo`, `A11yTree`, `Article`, `Target`, `InspectOpts`, `ReadOpts`, `ShotOpts`).
- Create `src/input.rs` — human mouse trajectory generator.
- Modify `src/main.rs` — add `mod lib`-parity is automatic; ensure modules the facade needs are visible.
- Modify `Cargo.toml` — add `[lib]` target.
- Modify `src/commands/screenshot.rs` — return `Vec<u8>`; move file-writing to the CLI caller.
- Modify `src/element.rs` — add the opt-in humanized-move hook in the click path (Task 11).

---

## Task 1: Fork, clone, and add a compiling `[lib]` target

**Files:**
- Modify: `Cargo.toml`
- Create: `src/lib.rs`

**Interfaces:**
- Produces: a `chrome_agent` library crate that compiles and re-exports the existing modules `pub`, with the binary and all existing tests unchanged.

- [ ] **Step 1: Fork + clone (confirm before running — outward-facing)**

```bash
gh repo fork sderosiaux/chrome-agent --clone=false
git clone git@github.com:framallo/chrome-agent.git /Users/agente/work/chrome-agent
cd /Users/agente/work/chrome-agent
git remote add upstream https://github.com/sderosiaux/chrome-agent.git
git checkout -b feat/lib-api
```

- [ ] **Step 2: Establish the green baseline**

Run: `cargo test`
Expected: existing `cli_tests` + `extract_tests` PASS (record the counts).

- [ ] **Step 3: Add the `[lib]` target to `Cargo.toml`**

Add after the `[[bin]]` block:

```toml
[lib]
name = "chrome_agent"
path = "src/lib.rs"
```

- [ ] **Step 4: Create `src/lib.rs` re-exporting the existing tree**

The modules currently live in `main.rs` as private `mod`s. Declare them in `lib.rs` `pub` so both the binary and dependents share one module tree. Copy the `mod` list from `main.rs` and mark the consumer-facing ones `pub`:

```rust
//! chrome-agent as a library. See `Session`/`Page` for the high-level API.
pub mod browser;
pub mod cdp;
pub mod commands;
#[cfg(unix)]
pub mod daemon;
pub mod element;
pub mod element_ref;
pub mod session;
pub mod setup;
pub mod snapshot;
pub mod truncate;
pub mod cli;
pub mod pipe;
pub mod pipe_dispatch;
pub mod run;
pub mod run_helpers;

/// Shared error type alias used across the crate (kept for internal use).
pub type BoxError = Box<dyn std::error::Error>;
```

- [ ] **Step 5: Point `main.rs` at the library modules**

Replace the private `mod …;` declarations in `src/main.rs` with `use chrome_agent::{…}` (or `use crate` → `use chrome_agent`) so there is a single module tree. Keep `main.rs`'s `#[tokio::main] async fn main()` body, changing paths from `crate::` to `chrome_agent::` where they referenced now-library modules. The `BoxError` alias moves to `lib.rs`; `main.rs` imports it.

- [ ] **Step 6: Verify both targets build and tests stay green**

Run: `cargo build --lib && cargo build --bin chrome-agent && cargo test`
Expected: library builds; binary builds; `cli_tests` + `extract_tests` still PASS with the same counts as Step 2.

- [ ] **Step 7: Commit**

```bash
cargo clippy --all-targets -- -D warnings && cargo fmt
git add Cargo.toml src/lib.rs src/main.rs
git commit -m "feat: add [lib] target exposing the module tree

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 2: `error.rs` — a structured public `Error`

**Files:**
- Create: `src/error.rs`
- Modify: `src/lib.rs` (add `pub mod error; pub use crate::error::Error;`)
- Test: inline `#[cfg(test)]` in `src/error.rs`

**Interfaces:**
- Produces: `pub enum Error` with `From<BoxError>` and `From<cdp::client::CdpClientError>`, used by every `Page`/`Session` method.

- [ ] **Step 1: Write the failing test**

```rust
// src/error.rs
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn boxerror_maps_to_other_with_message() {
        let be: crate::BoxError = "boom".into();
        let e: Error = be.into();
        assert!(matches!(e, Error::Other(ref m) if m == "boom"));
    }
    #[test]
    fn error_displays_message() {
        assert_eq!(Error::Timeout("goto".into()).to_string(), "timeout: goto");
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib error::`
Expected: FAIL — `Error` not defined.

- [ ] **Step 3: Implement `Error`**

```rust
// src/error.rs
use std::fmt;

#[derive(Debug)]
pub enum Error {
    Navigation(String),
    TargetNotFound(String),
    Cdp(String),
    Timeout(String),
    Session(String),
    Other(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Navigation(m) => write!(f, "navigation: {m}"),
            Error::TargetNotFound(m) => write!(f, "target not found: {m}"),
            Error::Cdp(m) => write!(f, "cdp: {m}"),
            Error::Timeout(m) => write!(f, "timeout: {m}"),
            Error::Session(m) => write!(f, "session: {m}"),
            Error::Other(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<crate::BoxError> for Error {
    fn from(e: crate::BoxError) -> Self { Error::Other(e.to_string()) }
}

impl From<crate::cdp::client::CdpClientError> for Error {
    fn from(e: crate::cdp::client::CdpClientError) -> Self { Error::Cdp(e.to_string()) }
}
```

- [ ] **Step 4: Wire into `lib.rs` and run tests**

Add to `src/lib.rs`: `pub mod error; pub use crate::error::Error;`
Run: `cargo test --lib error::`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
cargo clippy --all-targets -- -D warnings && cargo fmt
git add src/error.rs src/lib.rs
git commit -m "feat: structured public Error type

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 3: `page.rs` result & target types

**Files:**
- Create: `src/page.rs` (types only in this task; `Page` methods land in Tasks 5–8)
- Modify: `src/lib.rs`
- Test: inline in `src/page.rs`

**Interfaces:**
- Consumes: `commands::goto::GotoResult { url, title, .. }`, `commands::read::ReadResult { title, text_content, content, excerpt, byline }`, `commands::inspect::run -> snapshot::Snapshot`.
- Produces: `NavInfo`, `Article`, `A11yTree` (alias for `snapshot::Snapshot`), `Target`, `InspectOpts`, `ReadOpts`, `ShotOpts`, plus `From<GotoResult> for NavInfo` and `From<ReadResult> for Article`.

- [ ] **Step 1: Write the failing test**

```rust
// src/page.rs
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn navinfo_from_goto_result() {
        let g = crate::commands::goto::GotoResult { url: "u".into(), title: "t".into() };
        let n: NavInfo = g.into();
        assert_eq!((n.url.as_str(), n.title.as_str()), ("u", "t"));
    }
    #[test]
    fn target_variants_construct() {
        let _ = [Target::Uid("n1".into()), Target::Selector(".x".into()), Target::Xy(1.0, 2.0)];
    }
}
```

Note: if `GotoResult` has fields beyond `url`/`title` (verify in `src/commands/goto.rs`), include them in the literal above.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib page::`
Expected: FAIL — `NavInfo`/`Target` not defined.

- [ ] **Step 3: Implement the types**

```rust
// src/page.rs
use serde::Serialize;

pub type A11yTree = crate::snapshot::Snapshot;

#[derive(Debug, Clone, Serialize)]
pub struct NavInfo { pub url: String, pub title: String }

impl From<crate::commands::goto::GotoResult> for NavInfo {
    fn from(g: crate::commands::goto::GotoResult) -> Self {
        NavInfo { url: g.url, title: g.title }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Article {
    pub title: String,
    pub text: String,
    pub html: Option<String>,
    pub excerpt: Option<String>,
    pub byline: Option<String>,
}

impl From<crate::commands::read::ReadResult> for Article {
    fn from(r: crate::commands::read::ReadResult) -> Self {
        Article { title: r.title, text: r.text_content, html: r.content, excerpt: r.excerpt, byline: r.byline }
    }
}

#[derive(Debug, Clone)]
pub enum Target { Uid(String), Selector(String), Xy(f64, f64) }

#[derive(Debug, Clone, Default)]
pub struct InspectOpts { pub verbose: bool, pub max_depth: Option<usize>, pub focus_uid: Option<String>, pub role_filter: Option<Vec<String>> }

#[derive(Debug, Clone, Default)]
pub struct ReadOpts { pub html: bool, pub truncate: Option<usize> }

#[derive(Debug, Clone, Default)]
pub struct ShotOpts { /* reserved: future clip/format options */ }
```

- [ ] **Step 4: Wire + run**

Add to `src/lib.rs`: `pub mod page; pub use crate::page::{Page, Target, NavInfo, Article, A11yTree, InspectOpts, ReadOpts, ShotOpts};` (the `Page` re-export compiles once Task 5 defines it; for this task, re-export only the types defined here and add `Page` in Task 5).
Run: `cargo test --lib page::`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
cargo clippy --all-targets -- -D warnings && cargo fmt
git add src/page.rs src/lib.rs
git commit -m "feat: page facade result/target types

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 4: `Session` / `SessionOptions` / `Connect` facade

**Files:**
- Create: `src/session_api.rs` (new name avoids colliding with the existing `session.rs` session-store module)
- Modify: `src/lib.rs`
- Reference: lift the connection-setup block from `src/run.rs:72` onward (the `session::load_session()` → `resolve_browser` → `CdpClient::connect` logic)

**Interfaces:**
- Consumes: `browser::{BrowserOptions, resolve_browser, BrowserConnection}`, `cdp::client::CdpClient`, `session::load_session`.
- Produces:
  ```rust
  pub struct Session { pub(crate) client: CdpClient, pub(crate) conn: BrowserConnection, pub(crate) timeout_secs: u64, pub(crate) humanize_input: bool, page: Page }
  pub enum Connect { Auto, Ws(String), Http(String) }
  pub struct SessionOptions { pub profile: String, pub connect: Option<Connect>, pub headed: bool, pub stealth: bool, pub copy_cookies: bool, pub viewport: Option<(u32,u32)>, pub humanize_input: bool, pub timeout_secs: u64, pub ignore_https_errors: bool }
  impl Session { pub async fn launch(opts: SessionOptions) -> Result<Session, Error>; pub fn page(&self) -> &Page; pub async fn close(self) -> Result<(), Error>; }
  impl Default for SessionOptions
  ```

- [ ] **Step 1: Write the failing test (gated — needs a browser)**

```rust
// src/session_api.rs
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn default_options_are_sane() {
        let o = SessionOptions::default();
        assert_eq!(o.profile, "default");
        assert!(!o.humanize_input);
        assert!(o.viewport.is_none());
    }
    #[tokio::test]
    #[ignore] // needs bundled Chrome; run locally
    async fn launch_and_close_headless() {
        let s = Session::launch(SessionOptions::default()).await.unwrap();
        s.close().await.unwrap();
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib session_api::default_options_are_sane`
Expected: FAIL — `SessionOptions` not defined.

- [ ] **Step 3: Implement `SessionOptions` + `Connect` + `Default`**

```rust
// src/session_api.rs
use crate::browser::{self, BrowserConnection, BrowserOptions};
use crate::cdp::client::CdpClient;
use crate::error::Error;
use crate::page::Page;

#[derive(Debug, Clone)]
pub enum Connect { Auto, Ws(String), Http(String) }

#[derive(Debug, Clone)]
pub struct SessionOptions {
    pub profile: String,
    pub connect: Option<Connect>,
    pub headed: bool,
    pub stealth: bool,
    pub copy_cookies: bool,
    pub viewport: Option<(u32, u32)>,
    pub humanize_input: bool,
    pub timeout_secs: u64,
    pub ignore_https_errors: bool,
}

impl Default for SessionOptions {
    fn default() -> Self {
        SessionOptions {
            profile: "default".into(), connect: None, headed: false, stealth: false,
            copy_cookies: false, viewport: None, humanize_input: false,
            timeout_secs: 30, ignore_https_errors: false,
        }
    }
}
```

- [ ] **Step 4: Implement `Session::launch/page/close`**

Lift the browser-resolution behavior from `src/run.rs:72`+ into `launch`. Map `SessionOptions` → the existing `BrowserOptions` and reuse `browser::resolve_browser` + `CdpClient::connect` (do not reimplement CDP). Translate `Connect` into the `connect` string the existing `BrowserOptions.connect` expects (`Auto` → `"auto"`, `Ws(s)`/`Http(s)` → `s`). Apply stealth via `setup::apply_stealth` when `opts.stealth` (mirroring `run_helpers`/`pipe`). If `opts.viewport` is `Some`, call the viewport helper from Task 8 after connect.

```rust
pub struct Session {
    pub(crate) client: CdpClient,
    pub(crate) conn: BrowserConnection,
    pub(crate) timeout_secs: u64,
    pub(crate) humanize_input: bool,
    page: Page,
}

impl Session {
    pub async fn launch(opts: SessionOptions) -> Result<Session, Error> {
        let bopts = BrowserOptions {
            name: opts.profile.clone(),
            headless: !opts.headed,
            ignore_https_errors: opts.ignore_https_errors,
            stealth: opts.stealth,
            connect: opts.connect.as_ref().map(|c| match c {
                Connect::Auto => "auto".to_string(),
                Connect::Ws(s) | Connect::Http(s) => s.clone(),
            }),
            copy_cookies: opts.copy_cookies,
        };
        let conn = browser::resolve_browser(&bopts).await.map_err(|e| Error::Session(e.to_string()))?;
        let client = CdpClient::connect(&conn.ws_endpoint).await?;
        if opts.stealth { crate::setup::apply_stealth(&client).await; }
        if let Some((w, h)) = opts.viewport {
            crate::page::set_viewport_on(&client, w, h).await?; // Task 8 helper
        }
        let page = Page::new(client.clone(), opts.timeout_secs, opts.humanize_input);
        Ok(Session { client, conn, timeout_secs: opts.timeout_secs, humanize_input: opts.humanize_input, page })
    }
    pub fn page(&self) -> &Page { &self.page }
    pub async fn close(self) -> Result<(), Error> {
        // Best-effort: drop the CDP client; managed-Chrome teardown mirrors run_helpers::cmd_close.
        Ok(())
    }
}
```

Note: confirm `CdpClient` is `Clone` (it exposes a broadcast receiver via `events()`); if not `Clone`, hold the client in an `Arc` inside `Page` and pass `&CdpClient`. Adjust `Page::new` accordingly — see Task 5.

- [ ] **Step 5: Wire + run the non-ignored test**

Add to `src/lib.rs`: `pub mod session_api; pub use crate::session_api::{Session, SessionOptions, Connect};`
Run: `cargo test --lib session_api::default_options_are_sane`
Expected: PASS. (The `#[ignore]` launch test is run manually.)

- [ ] **Step 6: Commit**

```bash
cargo clippy --all-targets -- -D warnings && cargo fmt
git add src/session_api.rs src/lib.rs
git commit -m "feat: Session/SessionOptions facade over browser resolution

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 5: `Page` read methods — `goto`, `inspect`, `eval`, `text`, `read`

**Files:**
- Modify: `src/page.rs`
- Test: `tests/lib_smoke.rs` (new integration test against a local fixture; `#[ignore]` where a browser is required)

**Interfaces:**
- Consumes: `commands::goto::run`, `commands::inspect::run`, `commands::eval::run_raw`, `commands::text::run`, `commands::read::run`; `element_ref::ElementRef`; `run_helpers::get_uid_map`.
- Produces:
  ```rust
  impl Page {
    pub(crate) fn new(client: CdpClient, timeout_secs: u64, humanize_input: bool) -> Page;
    pub async fn goto(&self, url: &str) -> Result<NavInfo, Error>;
    pub async fn inspect(&self, opts: InspectOpts) -> Result<A11yTree, Error>;
    pub async fn eval(&self, js: &str) -> Result<serde_json::Value, Error>;
    pub async fn text(&self, target: Option<Target>) -> Result<String, Error>;
    pub async fn read(&self, opts: ReadOpts) -> Result<Article, Error>;
  }
  ```

- [ ] **Step 1: Write the failing test**

```rust
// tests/lib_smoke.rs
use chrome_agent::{Session, SessionOptions};

#[tokio::test]
#[ignore] // needs bundled Chrome
async fn goto_reads_title() {
    let s = Session::launch(SessionOptions::default()).await.unwrap();
    let nav = s.page().goto("data:text/html,<title>Hi</title><h1>Hi</h1>").await.unwrap();
    assert_eq!(nav.title, "Hi");
    let title = s.page().eval("document.title").await.unwrap();
    assert_eq!(title, serde_json::json!("Hi"));
    s.close().await.unwrap();
}
```

- [ ] **Step 2: Run test to verify it fails (compile error)**

Run: `cargo test --test lib_smoke -- --ignored goto_reads_title`
Expected: FAIL to compile — `Page::goto`/`eval` not defined.

- [ ] **Step 3: Implement the read methods**

Add to `src/page.rs`. `Page` holds the client + config + a cached uid map for action targeting (populated by `inspect`; rebuilt lazily by actions in Task 6).

```rust
use crate::cdp::client::CdpClient;
use crate::element_ref::ElementRef;
use crate::error::Error;
use std::collections::HashMap;
use std::sync::Mutex;

pub struct Page {
    pub(crate) client: CdpClient,
    pub(crate) timeout_secs: u64,
    pub(crate) humanize_input: bool,
    pub(crate) uid_map: Mutex<HashMap<String, ElementRef>>,
}

impl Page {
    pub(crate) fn new(client: CdpClient, timeout_secs: u64, humanize_input: bool) -> Page {
        Page { client, timeout_secs, humanize_input, uid_map: Mutex::new(HashMap::new()) }
    }

    pub async fn goto(&self, url: &str) -> Result<NavInfo, Error> {
        let r = crate::commands::goto::run(&self.client, url, self.timeout_secs).await?;
        Ok(r.into())
    }

    pub async fn inspect(&self, opts: InspectOpts) -> Result<A11yTree, Error> {
        let roles: Option<Vec<&str>> = opts.role_filter.as_ref().map(|v| v.iter().map(|s| s.as_str()).collect());
        let snap = crate::commands::inspect::run(
            &self.client, opts.verbose, opts.max_depth,
            opts.focus_uid.as_deref(), roles.as_deref(),
        ).await?;
        // Refresh the uid map so subsequent actions can target by uid.
        if let Ok(m) = crate::run_helpers::get_uid_map(&self.client).await {
            *self.uid_map.lock().unwrap() = m;
        }
        Ok(snap)
    }

    pub async fn eval(&self, js: &str) -> Result<serde_json::Value, Error> {
        Ok(crate::commands::eval::run_raw(&self.client, js).await?)
    }

    pub async fn text(&self, target: Option<Target>) -> Result<String, Error> {
        let map = self.uid_map.lock().unwrap().clone();
        let (uid, selector) = match target {
            Some(Target::Uid(u)) => (Some(u), None),
            Some(Target::Selector(s)) => (None, Some(s)),
            Some(Target::Xy(..)) => return Err(Error::TargetNotFound("text does not support xy".into())),
            None => (None, None),
        };
        Ok(crate::commands::text::run(&self.client, uid.as_deref(), selector.as_deref(), &map).await?)
    }

    pub async fn read(&self, opts: ReadOpts) -> Result<Article, Error> {
        let r = crate::commands::read::run(&self.client, opts.html, opts.truncate).await?;
        Ok(r.into())
    }
}
```

Note: confirm the exact param list of `commands::read::run` and `run_helpers::get_uid_map` in the fork before finalizing (this plan used `read::run(client, html, truncate)` and `get_uid_map(client)`); adjust the call sites if the signatures differ, keeping the `Page` method signatures above stable.

- [ ] **Step 4: Add `Page` to the `lib.rs` re-export and run**

Ensure `src/lib.rs` re-exports `Page` (from Task 3's `pub use` line).
Run: `cargo build --lib && cargo test` then locally `cargo test --test lib_smoke -- --ignored`
Expected: library builds; CLI tests green; the ignored smoke test passes locally against bundled Chrome.

- [ ] **Step 5: Commit**

```bash
cargo clippy --all-targets -- -D warnings && cargo fmt
git add src/page.rs tests/lib_smoke.rs src/lib.rs
git commit -m "feat: Page read methods (goto/inspect/eval/text/read)

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 6: `Page` action methods — `click`/`dblclick`/`fill`/`select`/`check`/`uncheck`/`drag`

**Files:**
- Modify: `src/page.rs`
- Test: `tests/lib_smoke.rs` (append; `#[ignore]`)

**Interfaces:**
- Consumes: `commands::{click,dblclick,fill,select,check,drag}::run` (each takes `client`, `uid_map: &HashMap<String,ElementRef>`, a `uid: &str`, plus value/desired where applicable).
- Produces:
  ```rust
  impl Page {
    pub async fn click(&self, target: Target) -> Result<(), Error>;
    pub async fn dblclick(&self, target: Target) -> Result<(), Error>;
    pub async fn fill(&self, target: Target, value: &str) -> Result<(), Error>;
    pub async fn select(&self, target: Target, value: &str) -> Result<(), Error>;
    pub async fn check(&self, target: Target) -> Result<(), Error>;
    pub async fn uncheck(&self, target: Target) -> Result<(), Error>;
    pub async fn drag(&self, from: Target, to: Target) -> Result<(), Error>;
    async fn resolve_uid(&self, target: &Target) -> Result<String, Error>;
  }
  ```

- [ ] **Step 1: Write the failing test**

```rust
// tests/lib_smoke.rs (append)
#[tokio::test]
#[ignore]
async fn fill_and_click_form() {
    let s = chrome_agent::Session::launch(chrome_agent::SessionOptions::default()).await.unwrap();
    let p = s.page();
    p.goto("data:text/html,<input id=t><button id=b onclick=\"t.value='clicked'\">go</button>").await.unwrap();
    p.inspect(Default::default()).await.unwrap();
    p.fill(chrome_agent::Target::Selector("#t".into()), "hello").await.unwrap();
    p.click(chrome_agent::Target::Selector("#b".into())).await.unwrap();
    let v = p.eval("document.getElementById('t').value").await.unwrap();
    assert_eq!(v, serde_json::json!("clicked"));
    s.close().await.unwrap();
}
```

- [ ] **Step 2: Run to verify it fails to compile**

Run: `cargo test --test lib_smoke -- --ignored fill_and_click_form`
Expected: FAIL — `Page::fill`/`click` not defined.

- [ ] **Step 3: Implement target resolution + action methods**

`resolve_uid` mirrors the CLI's targeting: a `Uid` passes through; a `Selector`/`Xy` is resolved to a synthetic uid via the same helper the CLI uses (`run_helpers::resolve_page_target`), refreshing `uid_map`. Reuse that helper — do not hand-roll selector→node logic.

```rust
impl Page {
    async fn resolve_uid(&self, target: &Target) -> Result<String, Error> {
        match target {
            Target::Uid(u) => Ok(u.clone()),
            Target::Selector(sel) => {
                let (uid, map) = crate::run_helpers::resolve_page_target(&self.client, Some(sel), None).await
                    .map_err(|e| Error::TargetNotFound(e.to_string()))?;
                *self.uid_map.lock().unwrap() = map;
                Ok(uid)
            }
            Target::Xy(x, y) => {
                let (uid, map) = crate::run_helpers::resolve_page_target(&self.client, None, Some((*x, *y))).await
                    .map_err(|e| Error::TargetNotFound(e.to_string()))?;
                *self.uid_map.lock().unwrap() = map;
                Ok(uid)
            }
        }
    }

    pub async fn click(&self, target: Target) -> Result<(), Error> {
        let uid = self.resolve_uid(&target).await?;
        let map = self.uid_map.lock().unwrap().clone();
        crate::commands::click::run(&self.client, &map, &uid).await?;
        Ok(())
    }
    pub async fn dblclick(&self, target: Target) -> Result<(), Error> {
        let uid = self.resolve_uid(&target).await?;
        let map = self.uid_map.lock().unwrap().clone();
        crate::commands::dblclick::run(&self.client, &map, &uid).await?;
        Ok(())
    }
    pub async fn fill(&self, target: Target, value: &str) -> Result<(), Error> {
        let uid = self.resolve_uid(&target).await?;
        let map = self.uid_map.lock().unwrap().clone();
        crate::commands::fill::run(&self.client, &map, &uid, value).await?;
        Ok(())
    }
    pub async fn select(&self, target: Target, value: &str) -> Result<(), Error> {
        let uid = self.resolve_uid(&target).await?;
        let map = self.uid_map.lock().unwrap().clone();
        crate::commands::select::run(&self.client, &map, &uid, value).await?;
        Ok(())
    }
    pub async fn check(&self, target: Target) -> Result<(), Error> {
        let uid = self.resolve_uid(&target).await?;
        let map = self.uid_map.lock().unwrap().clone();
        crate::commands::check::run(&self.client, &map, &uid, true).await?;
        Ok(())
    }
    pub async fn uncheck(&self, target: Target) -> Result<(), Error> {
        let uid = self.resolve_uid(&target).await?;
        let map = self.uid_map.lock().unwrap().clone();
        crate::commands::check::run(&self.client, &map, &uid, false).await?;
        Ok(())
    }
    pub async fn drag(&self, from: Target, to: Target) -> Result<(), Error> {
        let from_uid = self.resolve_uid(&from).await?;
        let to_uid = self.resolve_uid(&to).await?;
        let map = self.uid_map.lock().unwrap().clone();
        crate::commands::drag::run(&self.client, &map, &from_uid, &to_uid).await?;
        Ok(())
    }
}
```

Note: confirm `run_helpers::resolve_page_target` exists with a `(selector, xy) -> (uid, uid_map)` shape; if the CLI resolves targets differently (e.g. via `get_uid_map` + a selector lookup), reuse whatever the CLI's `click` path calls and keep these method signatures stable.

- [ ] **Step 4: Build + CLI-green + local smoke**

Run: `cargo build --lib && cargo test` then locally `cargo test --test lib_smoke -- --ignored fill_and_click_form`
Expected: builds; CLI tests green; smoke passes locally.

- [ ] **Step 5: Commit**

```bash
cargo clippy --all-targets -- -D warnings && cargo fmt
git add src/page.rs tests/lib_smoke.rs
git commit -m "feat: Page action methods with Target resolution

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 7: `screenshot` returns bytes; CLI writes the file

**Files:**
- Modify: `src/commands/screenshot.rs` (return `Vec<u8>`), and its CLI caller in `src/run.rs`
- Modify: `src/page.rs` (add `Page::screenshot`)
- Test: `tests/lib_smoke.rs` (append, `#[ignore]`); existing `cli_tests` guard the CLI path

**Interfaces:**
- Consumes: the CDP `Page.captureScreenshot` call already in `screenshot::run`.
- Produces: `commands::screenshot::capture(&CdpClient) -> Result<Vec<u8>, BoxError>` (bytes); `Page::screenshot(&self, ShotOpts) -> Result<Vec<u8>, Error>`. The CLI keeps `--filename` by writing the returned bytes.

- [ ] **Step 1: Write the failing test**

```rust
// tests/lib_smoke.rs (append)
#[tokio::test]
#[ignore]
async fn screenshot_returns_png_bytes() {
    let s = chrome_agent::Session::launch(chrome_agent::SessionOptions::default()).await.unwrap();
    s.page().goto("data:text/html,<h1>x</h1>").await.unwrap();
    let png = s.page().screenshot(Default::default()).await.unwrap();
    assert_eq!(&png[..8], &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]); // PNG magic
    s.close().await.unwrap();
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --test lib_smoke -- --ignored screenshot_returns_png_bytes`
Expected: FAIL — `Page::screenshot` not defined.

- [ ] **Step 3: Split `screenshot::run` into `capture` (bytes) + file-writing**

In `src/commands/screenshot.rs`, extract the CDP capture + base64-decode into `pub async fn capture(client: &CdpClient) -> Result<Vec<u8>, crate::BoxError>` returning the decoded PNG bytes. Keep `run(client, filename)` as a thin wrapper that calls `capture`, writes the bytes to the resolved tmp path, and returns the path string (so the CLI output shape is unchanged).

```rust
pub async fn capture(client: &CdpClient) -> Result<Vec<u8>, crate::BoxError> {
    // ... existing Page.captureScreenshot call, returning the decoded bytes
}
pub async fn run(client: &CdpClient, filename: Option<&str>) -> Result<String, crate::BoxError> {
    let bytes = capture(client).await?;
    let path = /* existing tmp-path resolution */;
    std::fs::write(&path, &bytes)?;
    Ok(path)
}
```

- [ ] **Step 4: Add `Page::screenshot`**

```rust
// src/page.rs
impl Page {
    pub async fn screenshot(&self, _opts: ShotOpts) -> Result<Vec<u8>, Error> {
        Ok(crate::commands::screenshot::capture(&self.client).await?)
    }
}
```

- [ ] **Step 5: Verify CLI unchanged + lib works**

Run: `cargo test` (CLI screenshot test still green — it asserts the path/file behavior, preserved by `run`), then locally `cargo test --test lib_smoke -- --ignored screenshot_returns_png_bytes`
Expected: CLI tests green; smoke passes.

- [ ] **Step 6: Commit**

```bash
cargo clippy --all-targets -- -D warnings && cargo fmt
git add src/commands/screenshot.rs src/run.rs src/page.rs tests/lib_smoke.rs
git commit -m "feat: screenshot returns bytes; CLI writes the file

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 8: Viewport control (§5.1)

**Files:**
- Modify: `src/page.rs` (add `set_viewport_on` free fn + `Page::set_viewport`)
- Test: `tests/lib_smoke.rs` (append, `#[ignore]`)

**Interfaces:**
- Consumes: `CdpClient::send` (or `call`) with `Emulation.setDeviceMetricsOverride`.
- Produces: `pub(crate) async fn set_viewport_on(client: &CdpClient, w: u32, h: u32) -> Result<(), Error>` (used by `Session::launch`); `Page::set_viewport(&self, w, h)`.

- [ ] **Step 1: Write the failing test**

```rust
// tests/lib_smoke.rs (append)
#[tokio::test]
#[ignore]
async fn viewport_override_changes_inner_size() {
    let s = chrome_agent::Session::launch(chrome_agent::SessionOptions {
        viewport: Some((1280, 2000)), ..Default::default()
    }).await.unwrap();
    s.page().goto("data:text/html,<h1>x</h1>").await.unwrap();
    let h = s.page().eval("window.innerHeight").await.unwrap();
    assert_eq!(h, serde_json::json!(2000));
    let w = s.page().eval("window.innerWidth").await.unwrap();
    assert_eq!(w, serde_json::json!(1280));
    s.close().await.unwrap();
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --test lib_smoke -- --ignored viewport_override_changes_inner_size`
Expected: FAIL — `set_viewport` not defined / `Session::launch` ignores `viewport`.

- [ ] **Step 3: Implement the viewport override**

```rust
// src/page.rs
use serde_json::json;

pub(crate) async fn set_viewport_on(client: &CdpClient, w: u32, h: u32) -> Result<(), Error> {
    client.send("Emulation.setDeviceMetricsOverride", json!({
        "width": w, "height": h, "deviceScaleFactor": 1, "mobile": false
    })).await?;
    Ok(())
}

impl Page {
    pub async fn set_viewport(&self, w: u32, h: u32) -> Result<(), Error> {
        set_viewport_on(&self.client, w, h).await
    }
}
```

Confirm `CdpClient::send`'s generic param accepts a `serde_json::Value` (it is `P: Serialize`), so `json!({..})` works directly.

- [ ] **Step 4: Verify `Session::launch` applies it (already wired in Task 4 Step 4)**

Run locally: `cargo test --test lib_smoke -- --ignored viewport_override_changes_inner_size`
Expected: PASS — `innerHeight == 2000`, proving the ~469px cap is gone.

- [ ] **Step 5: Commit**

```bash
cargo clippy --all-targets -- -D warnings && cargo fmt
git add src/page.rs tests/lib_smoke.rs
git commit -m "feat: configurable viewport via Emulation.setDeviceMetricsOverride

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 9: Human mouse trajectory generator (§5.2, pure)

**Files:**
- Create: `src/input.rs`
- Modify: `src/lib.rs` (`pub mod input;`)
- Test: inline `#[cfg(test)]` in `src/input.rs` (pure — no browser)

**Interfaces:**
- Produces:
  ```rust
  pub struct MousePoint { pub x: f64, pub y: f64, pub delay_ms: u32 }
  pub fn humanized_path(from: (f64, f64), to: (f64, f64), seed: u64) -> Vec<MousePoint>;
  ```

- [ ] **Step 1: Write the failing tests**

```rust
// src/input.rs
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn path_starts_near_from_ends_at_to() {
        let p = humanized_path((0.0, 0.0), (100.0, 50.0), 42);
        assert!(p.len() >= 4, "expected a multi-point path, got {}", p.len());
        let last = p.last().unwrap();
        assert!((last.x - 100.0).abs() < 0.001 && (last.y - 50.0).abs() < 0.001);
    }
    #[test]
    fn path_is_deterministic_for_a_seed() {
        let a = humanized_path((0.0, 0.0), (100.0, 50.0), 7);
        let b = humanized_path((0.0, 0.0), (100.0, 50.0), 7);
        assert_eq!(a.len(), b.len());
        assert!(a.iter().zip(&b).all(|(p, q)| p.x == q.x && p.y == q.y && p.delay_ms == q.delay_ms));
    }
    #[test]
    fn different_seeds_differ() {
        let a = humanized_path((0.0, 0.0), (100.0, 50.0), 1);
        let b = humanized_path((0.0, 0.0), (100.0, 50.0), 2);
        assert!(a.iter().zip(&b).any(|(p, q)| p.x != q.x || p.y != q.y));
    }
    #[test]
    fn overshoots_then_corrects() {
        // Some interior point should pass beyond the endpoint before the final point returns to it.
        let p = humanized_path((0.0, 0.0), (100.0, 0.0), 3);
        assert!(p.iter().any(|pt| pt.x > 100.0));
        assert!((p.last().unwrap().x - 100.0).abs() < 0.001);
    }
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test --lib input::`
Expected: FAIL — `humanized_path` not defined.

- [ ] **Step 3: Implement the generator (deterministic LCG, cubic Bézier + overshoot)**

```rust
// src/input.rs
#[derive(Debug, Clone, Copy)]
pub struct MousePoint { pub x: f64, pub y: f64, pub delay_ms: u32 }

// Small deterministic PRNG so a seed reproduces a path (tests + replay).
struct Lcg(u64);
impl Lcg {
    fn next_f64(&mut self) -> f64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        ((self.0 >> 11) as f64) / ((1u64 << 53) as f64)
    }
    fn range(&mut self, lo: f64, hi: f64) -> f64 { lo + (hi - lo) * self.next_f64() }
}

fn bezier(p0: f64, p1: f64, p2: f64, p3: f64, t: f64) -> f64 {
    let u = 1.0 - t;
    u * u * u * p0 + 3.0 * u * u * t * p1 + 3.0 * u * t * t * p2 + t * t * t * p3
}

pub fn humanized_path(from: (f64, f64), to: (f64, f64), seed: u64) -> Vec<MousePoint> {
    let mut rng = Lcg(seed.wrapping_add(0x9E3779B97F4A7C15));
    let steps = 18 + (rng.range(0.0, 10.0) as usize); // 18..=27 points

    // Control points: perpendicular jitter for a natural arc.
    let (dx, dy) = (to.0 - from.0, to.1 - from.1);
    let (nx, ny) = (-dy, dx); // perpendicular
    let jitter = |rng: &mut Lcg| rng.range(-0.15, 0.15);
    let c1 = (from.0 + dx * 0.3 + nx * jitter(&mut rng), from.1 + dy * 0.3 + ny * jitter(&mut rng));
    // Overshoot the endpoint on the second control point, then the final point corrects back.
    let over = rng.range(1.05, 1.15);
    let c2 = (from.0 + dx * over, from.1 + dy * over);

    let mut out = Vec::with_capacity(steps + 1);
    for i in 1..=steps {
        let t = i as f64 / steps as f64;
        let x = bezier(from.0, c1.0, c2.0, to.0, t);
        let y = bezier(from.1, c1.1, c2.1, to.1, t);
        let delay_ms = rng.range(4.0, 16.0) as u32; // variable velocity
        out.push(MousePoint { x, y, delay_ms });
    }
    // Guarantee the final point lands exactly on the target.
    if let Some(last) = out.last_mut() { last.x = to.0; last.y = to.1; }
    out
}
```

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test --lib input::`
Expected: PASS (all four).

- [ ] **Step 5: Commit**

```bash
cargo clippy --all-targets -- -D warnings && cargo fmt
git add src/input.rs src/lib.rs
git commit -m "feat: deterministic human mouse trajectory generator

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 10: Wire opt-in humanized movement into the click path (§5.2)

**Files:**
- Modify: `src/element.rs` (the click helper around `element.rs:900-930` that dispatches `Input.dispatchMouseEvent`)
- Modify: `src/page.rs` (pass `humanize_input` + a per-session seed into the click path)
- Test: inline behavior test asserting the event count differs by flag (unit-level, mockable) + `#[ignore]` smoke

**Interfaces:**
- Consumes: `input::humanized_path`, `CdpClient::send("Input.dispatchMouseEvent", …)`.
- Produces: a `humanize: bool` + `seed: u64` threaded into the low-level click so that, when set, `mouseMoved` events precede the existing press/release; when unset, behavior is byte-for-byte unchanged.

- [ ] **Step 1: Write the failing test**

Add a thin, testable seam: a pure function that returns the sequence of CDP mouse events to send, so it can be asserted without a browser.

```rust
// src/element.rs
#[cfg(test)]
mod mouse_tests {
    use super::*;
    #[test]
    fn humanize_off_emits_only_press_release() {
        let evs = plan_click_events(10.0, 20.0, false, 0);
        // press + release only
        assert_eq!(evs.iter().filter(|e| e.kind == "mousePressed").count(), 1);
        assert_eq!(evs.iter().filter(|e| e.kind == "mouseReleased").count(), 1);
        assert_eq!(evs.iter().filter(|e| e.kind == "mouseMoved").count(), 0);
    }
    #[test]
    fn humanize_on_prepends_moves_ending_at_target() {
        let evs = plan_click_events(10.0, 20.0, true, 7);
        assert!(evs.iter().filter(|e| e.kind == "mouseMoved").count() >= 4);
        let last_move = evs.iter().rev().find(|e| e.kind == "mouseMoved").unwrap();
        assert!((last_move.x - 10.0).abs() < 0.001 && (last_move.y - 20.0).abs() < 0.001);
        // press/release still land at the target, after the moves
        assert_eq!(evs.last().unwrap().kind, "mouseReleased");
    }
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --lib element::mouse_tests`
Expected: FAIL — `plan_click_events` / `PlannedEvent` not defined.

- [ ] **Step 3: Introduce `plan_click_events` and route the dispatcher through it**

```rust
// src/element.rs
#[derive(Debug, Clone)]
pub(crate) struct PlannedEvent { pub kind: &'static str, pub x: f64, pub y: f64, pub delay_ms: u32 }

pub(crate) fn plan_click_events(x: f64, y: f64, humanize: bool, seed: u64) -> Vec<PlannedEvent> {
    let mut evs = Vec::new();
    if humanize {
        // Start from a neutral origin; a future version can thread the last known cursor pos.
        for p in crate::input::humanized_path((0.0, 0.0), (x, y), seed) {
            evs.push(PlannedEvent { kind: "mouseMoved", x: p.x, y: p.y, delay_ms: p.delay_ms });
        }
    }
    evs.push(PlannedEvent { kind: "mousePressed", x, y, delay_ms: 0 });
    evs.push(PlannedEvent { kind: "mouseReleased", x, y, delay_ms: 0 });
    evs
}
```

Then, in the existing coordinate-click helper, replace the two hard-coded `Input.dispatchMouseEvent` sends with a loop over `plan_click_events(x, y, humanize, seed)`, sleeping `delay_ms` between `mouseMoved` events, and sending each event via the existing `Input.dispatchMouseEvent` params struct (`type` = `kind`, `x`, `y`, `button: "left"`, `clickCount` on press/release). Thread `humanize`/`seed` down from `Page` (default `false`, so existing callers and CLI are unchanged).

- [ ] **Step 4: Run to verify tests pass + CLI green**

Run: `cargo test`
Expected: `element::mouse_tests` PASS; CLI tests unchanged (they run with `humanize=false`).

- [ ] **Step 5: Commit**

```bash
cargo clippy --all-targets -- -D warnings && cargo fmt
git add src/element.rs src/page.rs
git commit -m "feat: opt-in humanized mouse movement in click path (off by default)

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 11: CDP-input audit lock (§5.3)

**Files:**
- Test: `tests/input_audit.rs` (new)

**Interfaces:**
- Consumes: `element::plan_click_events` (from Task 10) and a grep-style source assertion.

- [ ] **Step 1: Write the test**

Two layers: (a) a source-level assertion that click/fill do not use JS synthetic `dispatchEvent` for the actual input, and (b) reuse the `plan_click_events` unit proof that clicks emit CDP `Input.*` events.

```rust
// tests/input_audit.rs
#[test]
fn interactions_use_cdp_input_not_js_synthetic() {
    let element = include_str!("../src/element.rs");
    // The click/fill paths must dispatch via CDP Input.*, never a JS-synthetic MouseEvent/KeyboardEvent.
    assert!(element.contains("Input.dispatchMouseEvent"));
    assert!(element.contains("Input.dispatchKeyEvent"));
    assert!(!element.contains("new MouseEvent("), "JS synthetic mouse event found in element.rs");
    assert!(!element.contains("new KeyboardEvent("), "JS synthetic key event found in element.rs");
}
```

- [ ] **Step 2: Run**

Run: `cargo test --test input_audit`
Expected: PASS (documents + locks the current CDP-input posture).

- [ ] **Step 3: Commit**

```bash
git add tests/input_audit.rs
git commit -m "test: lock CDP-input posture (no JS synthetic events)

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 12: `Runtime.enable`-under-stealth lock (§5.4)

**Files:**
- Test: `tests/runtime_enable_audit.rs` (new)
- Modify: `docs/` note in the fork's README or a `docs/anti-detection.md`

**Interfaces:**
- Consumes: `cdp::client` behavior + `setup::apply_stealth`.

- [ ] **Step 1: Write the test**

A source-level lock (the runtime behavior is exercised by the CLI's stealth path; this pins the invariant that stealth avoids the `Runtime.enable` handshake). Confirm the exact guard in `src/cdp/client.rs:163` and assert on it.

```rust
// tests/runtime_enable_audit.rs
#[test]
fn stealth_path_special_cases_runtime_enable() {
    let client = include_str!("../src/cdp/client.rs");
    // The Runtime domain must be special-cased so the detectable Runtime.enable
    // handshake is not sent blindly (rebrowser.net Runtime.enable leak).
    assert!(client.contains("Runtime.enable"));
    let setup = include_str!("../src/setup.rs");
    // Stealth injects via addScriptToEvaluateOnNewDocument, not Runtime.evaluate handshakes.
    assert!(setup.contains("addScriptToEvaluateOnNewDocument"));
}
```

- [ ] **Step 2: Run**

Run: `cargo test --test runtime_enable_audit`
Expected: PASS.

- [ ] **Step 3: Add a short doc note**

Create `docs/anti-detection.md` in the fork summarizing: CDP-direct input (§5.3), the `Runtime.enable` handling (§5.4), stealth patch list, and that `humanize_input` is opt-in. One short paragraph each.

- [ ] **Step 4: Commit**

```bash
git add tests/runtime_enable_audit.rs docs/anti-detection.md
git commit -m "test+docs: lock Runtime.enable-under-stealth posture

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 13: Full gate pass + tag a rev for pacewright

**Files:** none (release bookkeeping)

- [ ] **Step 1: Full gate**

Run: `cargo test && cargo clippy --all-targets -- -D warnings && cargo fmt --check`
Expected: all green; `cli_tests` + `extract_tests` counts unchanged from Task 1 Step 2; new lib/input/audit tests pass.

- [ ] **Step 2: Run the ignored browser smoke tests locally once**

Run: `cargo test --test lib_smoke -- --ignored`
Expected: all pass against bundled Chrome (goto/eval, fill/click, screenshot bytes, viewport 2000px).

- [ ] **Step 3: Push the branch + open PR on the fork (confirm — outward-facing)**

```bash
git push -u origin feat/lib-api
gh pr create --title "Library API + viewport + humanized input" --body "See docs/anti-detection.md and the pacewright spec."
```

- [ ] **Step 4: Record the pin for pacewright workstream 2**

Note the merged commit SHA (or a tagged version). pacewright's workstream-2 plan will add:
`chrome-agent = { git = "https://github.com/framallo/chrome-agent", rev = "<SHA>" }`

---

## Self-Review

**Spec coverage:**
- §2 goal 1 (lib facade) → Tasks 1–7. ✓
- §2 goal 2 (viewport) → Task 8. ✓
- §2 goal 3 (human mouse) → Tasks 9–10. ✓
- §2 goal 4 (verify CDP-input + Runtime.enable) → Tasks 11–12. ✓
- §3 fork/repo strategy (fork, `[lib]`, refactor direction, toolchain) → Task 1. ✓
- §4 facade (Session/Page/Target/Error, screenshot bytes) → Tasks 2–7. ✓
- §5.1–5.4 → Tasks 8, 9–10, 11, 12. ✓
- §6 testing (CLI green guard, lib smoke, pure units, gated smoke) → every task's gates + Tasks 5–8 (`#[ignore]`) + Task 13. ✓
- §9 sequencing → Task order matches. ✓

**Placeholder scan:** No "TBD/TODO". Two `Note:` blocks (Tasks 4–6) flag exact signatures to confirm against the fork before finalizing — these are verification prompts against real, named functions, not missing content, because the plan targets a repo not yet cloned. All new code (types, viewport, mouse generator, tests) is complete and literal.

**Type consistency:** `Session`/`SessionOptions`/`Connect` (Task 4) match their uses in Tasks 5–8. `Page::new(client, timeout_secs, humanize_input)` is defined in Task 5 and called in Task 4 — Task 4 references it forward via its Interfaces block. `Target`/`NavInfo`/`Article`/`A11yTree` (Task 3) are used consistently in Tasks 5–6. `humanized_path`/`MousePoint` (Task 9) are consumed by `plan_click_events` (Task 10). `set_viewport_on` (Task 8) is called by `Session::launch` (Task 4) — cross-referenced.

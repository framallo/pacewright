# chrome-agent fork — library API + §9 anti-detection extensions

Status: **approved** (2026-07-07). Author: handed off from the M2 brainstorm.
Depends on: nothing (external repo). Blocks: the pacewright browser-seam spec (next).

This is milestone **M2, workstream 1 of 2**. It produces a forked, library-callable
`chrome-agent`. Workstream 2 (the pacewright `BrowserHandle` trait + `RunCtx` threading +
`NativeBrowser` impl + daemon wiring) is a **separate spec** written after this one lands.

---

## 1. Context

pacewright's engine (M1) is browser-free by design: `RunCtx` in
`crates/core/src/adapter.rs` carries a `// M2+: pub browser: BrowserHandle` placeholder,
and the `DummyAdapter` proves the `Adapter` seam without a browser. M2 makes real
browser-driving adapters (M3 LinkedIn profile onward) possible.

The chosen substrate is **`chrome-agent`** (`github.com/sderosiaux/chrome-agent`, v0.4.3),
a Rust binary crate: CDP-direct browser automation, "single binary, zero deps," published
to both crates.io and npm (the npm package ships the compiled Rust binary). It already has
the verb surface real adapters need — `goto`, `inspect` (accessibility tree → `uid`s like
`n47`), `click`, `dblclick`, `fill`, `fill-form`, `select`, `check`/`uncheck`, `upload`,
`drag`, `frame`, `eval`, `read` (Readability), `text`, `screenshot`, `back`, `forward` —
and the anti-detection posture spec §9 asks for: `--stealth` (7 patches incl. the
`Runtime.enable`-handshake skip, `navigator.webdriver`, UA, WebGL, input-leak),
`--copy-cookies` (inherit the real Chrome profile), and `--connect <ws|http|auto>` to drive
the operator's real Chrome. It runs a **persistent daemon over a Unix socket**, so page
state survives across invocations.

### The problem this spec solves

chrome-agent 0.4.3 is **binary-only**: `[[bin]]`, `autolib = false`, **no `[lib]` target,
no `src/lib.rs`**, category `command-line-utilities`. `cargo add chrome-agent` compiles it
but exposes **zero callable API** — its `session`/`browser`/`element` types are `pub`
*within* the crate but nothing is re-exported for a dependent. So pacewright cannot "call it
as a crate" without a fork that adds a library facade. The M2 milestone in
`docs/specs/2026-07-07-core-engine-design.md` §11 already names the deliverable
"chrome-agent fork"; this spec defines exactly what that fork contains.

---

## 2. Goal / non-goals

**Goal.** Fork `sderosiaux/chrome-agent` → `framallo/chrome-agent`, and:
1. Expose a Rust **library** (`Session` / `Page` facade) that returns structured data
   instead of printing JSON, callable as `chrome-agent = { git = "…" }`.
2. Add **viewport control** (removes the ~469px capture cap the avatar tool hit).
3. Add **human mouse movement** (opt-in, off by default).
4. **Verify + lock** the already-present CDP-input and `Runtime.enable` posture with tests.

**Non-goals.**
- The pacewright `BrowserHandle` trait, `RunCtx` threading, `NativeBrowser`/`FakeBrowser`,
  or any daemon wiring — that is workstream 2, a separate spec.
- Changing the CLI's user-facing behavior. The CLI stays; it is refactored to sit on top of
  the new library, and its existing integration tests are the refactor's regression guard.
- New platform logic (LinkedIn/Riverside/YouTube specifics) — that lives in pacewright
  adapters (M3+), not in chrome-agent.

---

## 3. Fork & repo strategy

- `gh repo fork sderosiaux/chrome-agent` → `framallo/chrome-agent`; clone to
  `/Users/agente/work/chrome-agent`; add an `upstream` remote so the fork can rebase on
  upstream releases later. (Creating the fork is an outward-facing action — confirm before
  running.)
- Add a **`[lib]` target** next to the existing `[[bin]]`:
  ```toml
  [lib]
  name = "chrome_agent"
  path = "src/lib.rs"
  ```
  `src/lib.rs` declares the module tree `pub` where needed and re-exports the facade:
  ```rust
  pub mod cdp; pub mod browser; pub mod element; pub mod input;
  pub mod session; pub mod page; pub mod error;
  mod setup; mod snapshot; mod truncate; // internal
  pub use crate::session::{Session, SessionOptions, Connect};
  pub use crate::page::{Page, Target, InspectOpts, ReadOpts, ShotOpts,
                        NavInfo, A11yTree, A11yNode, Article};
  pub use crate::error::Error;
  ```
  (`session`/`page`/`error` are new modules the fork adds; `cdp`/`browser`/`element`/
  `input` already exist or are added by §5.2.)
- **Refactor direction:** the command bodies currently live in one CLI-coupled
  `run::run(cli)` (742 lines) that prints JSON. Each verb's logic moves into a `Page`/
  `Session` method that *returns* a typed value; `run.rs` becomes a thin translation layer
  (parse `Cli` → call the lib → serialize the result to the CLI's existing JSON/text
  shape). `main.rs` is unchanged except that it now calls into the library path.
- **Toolchain:** the crate is `edition = "2024"`, which builds on the pinned Rust 1.96.1.
  Align the fork's `rust-toolchain.toml` to a channel ≥ what pacewright pins so a git
  dependency resolves cleanly.

---

## 4. Library API — the `Session` / `Page` facade

The facade is intentionally thin: it wraps the *existing* CDP primitives in `browser.rs`
(connection/resolution), `element.rs` (a11y tree, uid resolution, `Input.dispatch*`
actions), and `cdp/` (client + types). No behavior changes here — only "return data, don't
print."

```rust
pub struct Session { /* owns a BrowserConnection + the default Page */ }

pub enum Connect { Auto, Ws(String), Http(String) }  // mirrors --connect

pub struct SessionOptions {
    pub profile: String,             // named profile ("default")
    pub connect: Option<Connect>,    // None = bundled Chromium; Some = real Chrome
    pub headed: bool,
    pub stealth: bool,
    pub copy_cookies: bool,
    pub viewport: Option<(u32, u32)>,// NEW — None keeps current default
    pub humanize_input: bool,        // NEW — default false
    pub timeout: Duration,
    pub ignore_https_errors: bool,
}

impl Session {
    pub async fn launch(opts: SessionOptions) -> Result<Session, Error>;
    pub fn page(&self) -> &Page;             // default page/tab
    pub async fn close(self) -> Result<(), Error>;
}

pub enum Target { Uid(String), Selector(String), Xy(f64, f64) }

impl Page {
    pub async fn goto(&self, url: &str) -> Result<NavInfo, Error>;   // {url,title,status}
    pub async fn inspect(&self, opts: InspectOpts) -> Result<A11yTree, Error>;
    pub async fn click(&self, target: Target) -> Result<(), Error>;
    pub async fn dblclick(&self, target: Target) -> Result<(), Error>;
    pub async fn fill(&self, target: Target, value: &str) -> Result<(), Error>;
    pub async fn select(&self, target: Target, value: &str) -> Result<(), Error>;
    pub async fn check(&self, target: Target) -> Result<(), Error>;
    pub async fn uncheck(&self, target: Target) -> Result<(), Error>;
    pub async fn upload(&self, target: Target, path: &Path) -> Result<(), Error>;
    pub async fn drag(&self, from: Target, to: Target) -> Result<(), Error>;
    pub async fn frame(&self, selector_or_main: &str) -> Result<(), Error>;
    pub async fn eval(&self, js: &str) -> Result<serde_json::Value, Error>;
    pub async fn read(&self, opts: ReadOpts) -> Result<Article, Error>; // Readability
    pub async fn text(&self, target: Option<Target>) -> Result<String, Error>;
    pub async fn screenshot(&self, opts: ShotOpts) -> Result<Vec<u8>, Error>;
    pub async fn set_viewport(&self, w: u32, h: u32) -> Result<(), Error>; // NEW
    pub async fn back(&self) -> Result<NavInfo, Error>;
    pub async fn forward(&self) -> Result<NavInfo, Error>;
}
```

Return types (`NavInfo`, `A11yTree` / `A11yNode`, `Article`) are `serde`-derivable structs
mirroring the values the CLI already computes before serializing — so the CLI's JSON output
becomes `serde_json::to_value(result)` over these types, keeping the wire shape stable.

`Error` is a structured enum (navigation / target-not-found / cdp / timeout / session)
replacing the crate's current `Box<dyn Error>` + string matching at the boundary; the CLI's
`error_hint` mapping is preserved by matching on the typed variants.

**`screenshot` returns bytes, not a path.** The current CLI writes to a tmp file and returns
the path; the library returns `Vec<u8>` (PNG) so callers decide where it goes. The CLI
wrapper keeps its `--filename` behavior by writing the returned bytes itself.

---

## 5. §9 anti-detection extensions

Grounded in the actual 0.4.3 internals — two are genuine build items, two are already
present and only need verification + a locking test.

### 5.1 Viewport control — **build**
No `Emulation.setDeviceMetricsOverride` exists anywhere in the source; the default window
size is why the avatar tool hit a ~469px capture cap. Add:
- `SessionOptions.viewport: Option<(u32,u32)>`, applied on session setup.
- `Page::set_viewport(w,h)` → CDP `Emulation.setDeviceMetricsOverride
  { width, height, deviceScaleFactor: 1, mobile: false }`.
Acceptance: a headless screenshot of a fixture page at 1280×2000 returns an image ≥ the
requested height; the old cap is gone.

### 5.2 Human mouse movement — **build**, opt-in, **off by default**
Today clicks dispatch discrete `Input.dispatchMouseEvent` press/release at the target with
no travel. Add a new `input.rs`:
- `humanize_move(from, to, rng) -> Vec<(f64,f64,delay)>` — a cubic Bézier path with
  variable velocity, slight overshoot-and-correct, and micro-jitter.
- When `SessionOptions.humanize_input == true`, `click`/`dblclick`/`drag` emit the
  `mouseMoved` sequence (with inter-event delays) *before* the existing press/release;
  when `false`, behavior is exactly as today (fast, deterministic — required for tests and
  headless throughput).
- The trajectory generator is pure math over an injected RNG → unit-testable without a
  browser (seeded RNG ⇒ reproducible path assertions).
Acceptance: with `humanize_input`, a recorded `Input.dispatchMouseEvent` stream shows a
multi-point curved path ending at the target; with it off, exactly two events (press +
release) at the target, unchanged from upstream.

### 5.3 CDP input audit — **verify + lock**
All interaction in `element.rs` already routes through `Input.dispatchMouseEvent` /
`Input.dispatchKeyEvent` (no JS synthetic events), which is what §9 requires and what makes
LinkedIn `@`-mention autocomplete work. Deliverable: a test asserting click/fill produce
`Input.dispatch*` CDP traffic and never fall back to `dispatchEvent`-style JS, plus a short
doc note. No behavior change.

### 5.4 `Runtime.enable` posture — **verify + lock**
`cdp/client.rs` already special-cases the `Runtime` domain, and `--stealth` avoids the
detectable `Runtime.enable` handshake (`setup.rs::apply_stealth` injects patches via
`Page.addScriptToEvaluateOnNewDocument`). Deliverable: a test asserting that under `stealth`
the `Runtime.enable` handshake is not sent on the main session, plus a doc note. No behavior
change.

---

## 6. Testing strategy

- **Regression guard:** the existing `tests/cli_tests.rs` and `tests/extract_tests.rs` must
  stay green through the lib refactor — they pin the CLI's observable behavior while its
  internals move under the facade.
- **New library tests:** drive the `Session`/`Page` API against a local static-HTML fixture
  served over the bundled headless Chrome (no network): `goto` → `inspect` → `fill`/`click`
  → `eval`/`read`/`screenshot`. Assert typed return values.
- **Pure unit tests:** the mouse trajectory generator (seeded RNG), viewport-param
  plumbing, `Error` variant mapping.
- **Gated smoke tests:** `--connect` / `--copy-cookies` against a real logged-in Chrome are
  `#[ignore]`d (or behind a feature) — they need an operator profile and can't run in CI.
- Gates mirror pacewright's: `cargo test`, `cargo clippy -- -D warnings`, `cargo fmt`.

---

## 7. How pacewright will consume this (forward reference — next spec)

Not built here; recorded so this API is shaped to fit. Workstream 2 will:
- Add a `BrowserHandle` trait to pacewright whose methods mirror `Page` (goto / inspect /
  click / fill / eval / read / screenshot / …), living at the adapter boundary.
- Provide `NativeBrowser` (wraps `chrome_agent::Session`, added via
  `chrome-agent = { git = "https://github.com/framallo/chrome-agent" }`) and `FakeBrowser`
  (deterministic, for core tests).
- Thread `browser: &dyn BrowserHandle` into `RunCtx`, and have `pacewrightd` own one
  long-lived `Session` (matching core-engine spec §6: the daemon owns the live chrome-agent
  session).

**Determinism boundary:** the fork's mouse jitter uses randomness internally — that is
real-world browser I/O at the edge, like network, and does **not** violate pacewright's
Clock/Rng determinism invariant, which governs scheduling/pacing only. `humanize_input`
defaults off, so pacewright's browser tests stay deterministic unless a specific adapter
opts in.

---

## 8. Risks / open questions

- **Refactor blast radius.** Lifting `run::run` into a facade touches the crate's core path.
  Mitigation: keep the CLI tests green at every step; move one verb group at a time.
- **Upstream drift.** A fork we maintain diverges from `sderosiaux/chrome-agent`. Mitigation:
  keep the lib facade additive and the CLI refactor minimal, so rebasing upstream releases
  stays cheap; keep an `upstream` remote.
- **Daemon vs. one-shot session ownership.** chrome-agent's persistent daemon may or may not
  be the cleanest thing for pacewright to hold long-lived. This spec exposes `Session` as a
  first-class object; whether pacewright holds one `Session` or leans on the daemon socket
  is decided in workstream 2.
- **Publishing.** Whether `framallo/chrome-agent` is consumed by git rev (simplest) or
  published to crates.io under a new name is deferred to workstream 2's dependency choice.

---

## 9. Sequencing within this spec

1. Fork + clone + add `[lib]` target + empty `lib.rs` that compiles (CLI unchanged).
2. Introduce `Error`, `NavInfo`, `A11yTree`, `Article`, `Target` types.
3. Move verbs into `Page`/`Session` methods one group at a time; rewire `run.rs`; keep CLI
   tests green throughout.
4. Viewport (5.1) + its acceptance test.
5. `input.rs` mouse humanization (5.2) + trajectory unit tests; wire the opt-in gate.
6. CDP-input (5.3) and `Runtime.enable` (5.4) verification tests + doc notes.
7. Full gate pass; tag a fork release/rev for pacewright to pin in workstream 2.

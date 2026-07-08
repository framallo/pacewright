# pacewright — handoff

Last updated: 2026-07-08 (M2 browser seam + first real LinkedIn task). Written for the next agent.

## What this is

`pacewright` is a Rust workspace: a browser task queue/scheduler/runner that replaces
a pile of ad-hoc daemon scripts (and eventually Postiz). One background daemon
(`pacewrightd`) owns a durable SQLite-backed queue, a scheduler, a pacing/limits
engine, and a runner that drives per-platform **adapters**. You talk to it from a CLI
(`pacewright` / `pcw`), a live TUI, and later an MCP server + Tauri GUI.

Full context: `README.md` (architecture, CLI, config, roadmap). Design spec:
`docs/specs/2026-07-07-core-engine-design.md`. M1 build plan:
`docs/plans/2026-07-07-m1-core-engine.md`.

## Repo layout

7 crates under `crates/`:
- `core` — engine, scheduler, runner, store (SQLite), limits, config, clock, rng, adapter trait,
  `BrowserHandle` trait + test doubles. The heart. Stays browser-free (trait only, no impl).
- `proto` — JSON-RPC wire types shared by daemon + clients.
- `daemon` — `pacewrightd`: Unix-socket JSON-RPC server + 1s tick loop.
- `cli` — `pacewright`/`pcw`: client subcommands + ratatui TUI.
- `adapter-dummy` — reference `Adapter` (`echo`/`slow`/`flaky`/`always_fail`/`rate_heavy`/`panic`) for testing without a browser.
- `browser` — `CliBrowser`: the real `BrowserHandle`, drives the `chrome-agent` CLI.
- `adapter-linkedin` — `LinkedInAdapter`: `scrape_profile` (M3 first slice).

## Non-negotiable design invariants (do not break these)

- **Determinism.** All time flows through an injected `Clock` (`crates/core/src/clock.rs`);
  all randomness through an injected `Rng` (`crates/core/src/rng.rs`). Core code must
  NEVER call wall-clock (`SystemTime::now`, `chrono::Utc::now`) or the `rand` crate
  directly — tests rely on `TestClock`/`TestRng` for reproducibility. The daemon wires
  in `SystemClock` + a seeded `SeededRng` at the edge (`crates/daemon/src/main.rs`).
- **Error classes** (`AdapterError` in `crates/core/src/model.rs`): `Retryable`
  (backoff + requeue, capped by `max_attempts`), `Terminal` (fail immediately),
  `RateLimited { retry_after }` (defer to a time). Adapters return these; the runner
  (`crates/core/src/runner.rs`) enforces the semantics. A panic inside an adapter's
  `execute()` is now caught at the runner boundary and folded into `Terminal` — see below.
- **Adapters declare limit keys; the engine enforces limits.** An adapter's `ActionSpec`
  lists `limit_keys`; the pacing engine (`crates/core/src/limits.rs`) applies daily caps,
  min-gap, active-hours windows, and jitter per key from `config.toml`.
- **`Response::Ok(Value)` is internally tagged** — every Ok handler must return a JSON
  object (`json!({...})`), never a bare string/array/number/bool, or serialization breaks.

## Toolchain / how to build (IMPORTANT: Rust is not on PATH)

Start every shell with: `source ~/.cargo/env` (Rust 1.96, pinned via `rust-toolchain.toml`).
`rustfmt`/`clippy` are installed for the 1.96.1 toolchain.

Gates that must stay green before any commit:
- `cargo test --workspace` — currently **72 tests, all passing** (+3 `#[ignore]`d live browser
  tests: `cargo test -p pacewright-browser -- --ignored`, needs `chrome-agent` + Chrome).
- `cargo clippy --workspace --all-targets -- -D warnings` — clean, zero warnings.
- `cargo fmt` before committing your own changes.

Note: the repo currently has pre-existing whole-repo `cargo fmt --check` drift (~116
locations, present since M1 — the code was hand-formatted, not rustfmt'd). It is NOT a
CI gate today. Do not do a blanket `cargo fmt` on the whole tree in a feature commit —
that would bury your real change under a huge unrelated reformat. If you want to adopt
rustfmt, do it as its own dedicated commit and flag it. Keep your OWN new code
rustfmt-clean and matching the surrounding hand-formatted style (dense one-line structs,
etc.).

## Workflow (solo developer — no branches)

Federico works solo and merges locally. Commit directly to `main` (or a short-lived
branch merged `--no-ff` immediately — either is fine). **Do NOT push to origin/remote and
do NOT force-push** — Federico pushes manually. Local commits/branches/merges are all yours.

Commit-message trailer convention used in this repo:
`Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>` (add it if you commit).

## State of the tree (git log, newest first)

```
e97abe4 Merge m1-hardening: single-source terminal set, dedup index, pause/resume RPC test
4b962f7 harden(core): single-source terminal set, dedup index, pause/resume RPC test
81e278a docs: add HANDOFF.md for the next agent
953a39e Merge m1-adapter-panic-isolation: catch adapter panics at the runner boundary
66ad34a feat(core): isolate adapter panics at the runner boundary
d650bfa Merge m1-runtime-control: real pause/resume + per-adapter scopes
1717a84 feat(core): real pause/resume for daemon + per-adapter scopes
```

`main` is the integration branch. All old `m1-*` feature branches are fully merged into
`main` and deleted — history is preserved by the `--no-ff` merge commits above.

## What just landed (this session)

1. **Real pause/resume + per-adapter scopes** (was an M1 acknowledged no-op).
   `Engine` holds a `paused: HashSet<String>`. `pause("all"|"daemon")` stops the whole
   tick loop (`tick()` returns early before any store I/O); `pause("<adapter>")` skips
   only that adapter's tasks (they stay `Pending`). RPC `Pause`/`Resume`/`Status` wired
   through `crates/daemon/src/server.rs` (handler now locks the engine `mut`).
   In-memory only — pause does NOT survive a daemon restart (matches M1 architecture;
   see "open ideas" if you want durability).

2. **Adapter-panic isolation at the runner boundary.** `run_task`
   (`crates/core/src/runner.rs`) wraps `adapter.execute()` in
   `AssertUnwindSafe(...).catch_unwind()` (via `futures_util::FutureExt`) and converts a
   caught panic into `AdapterError::Terminal("adapter panicked: ...")`, reusing the normal
   Terminal path (task → `Failed`, `last_error` set, `task_events` audit entry). The tick
   loop and daemon survive; subsequent tasks in the same tick still run. Added a `panic`
   action to `DummyAdapter`.
   - Footgun documented in-code: `panic_message()` takes the payload `Box<dyn Any + Send>`
     BY VALUE and downcasts via method-call autoderef. Passing it as `&(dyn Any + Send)`
     silently unsizes the Box itself into the trait object and every `downcast_ref` misses.
     Don't "simplify" that back to a reference param.

Progress ledger with full detail: `.superpowers/sdd/progress.md` (gitignored, local only).

## Running a real LinkedIn task (works today)

```bash
cargo build --release
./target/release/pacewrightd &                       # or restart the existing one
./target/release/pacewright adapters                 # linkedin/scrape_profile should be listed
./target/release/pacewright add linkedin scrape_profile \
    --params '{"url":"https://www.linkedin.com/in/me/"}'
./target/release/pacewright get <id>                 # result = scraped profile JSON
./target/release/pacewright limits                   # linkedin.profile_scrape counter incremented
```

`/in/me/` resolves to your own profile — it proves the authenticated path **without**
sending a profile-view notification to a third party. Viewing someone *else's* profile
while logged in does notify them; keep that in mind before pointing this at leads.

### chrome-agent gotchas (both cost real debugging time — do not relearn them)

1. **`--copy-cookies` only fires when chrome-agent launches a *fresh* browser.**
   `copy_chrome_cookies` is called inside `launch_browser`; if a session already exists,
   chrome-agent reuses it and silently skips the copy, so you stay logged out and hit the
   auth wall. Fix: `chrome-agent --browser pacewright close --purge`, then retry.
2. **chrome-agent's browsers and pages are *named and global to the machine*.** At the
   defaults every consumer shares one browser and one page called `default`. The
   Riverside/podcast tooling on this box drives that page. Because `goto` and `eval` are
   separate subprocesses, a concurrent consumer can navigate the page between them — we
   observed an eval intended for a LinkedIn profile return `riverside.com`. `CliBrowser`
   therefore pins `--browser pacewright --page pacewright` on **every** verb. Never let
   pacewright touch the `default` page.

Related: `goto` echoes the **requested** URL, not the post-redirect one. Always read the
settled `location.href`/`document.title` back via `eval` before deciding anything (this is
how `LinkedInAdapter` detects auth walls). And LinkedIn ships build-hashed class names
(`e6590096 _3293afb7 …`) — class-based selectors rot instantly; anchor on the `<main>`
heading and stable text patterns instead.

## Roadmap — what to build next

M1 (core engine) is DONE. **M2's in-repo browser seam is DONE**, and **M3's first slice
(`linkedin/scrape_profile`) runs against real LinkedIn.**

| M | Scope | State |
|---|---|---|
| M2a | **Browser handle in `RunCtx`** (`BrowserHandle` trait + `CliBrowser` + `FakeBrowser`) | ✅ done |
| M2b | Fork chrome-agent → library API + viewport + human mouse movement | 📄 spec + plan written, **not built** |
| M3 | LinkedIn **profile** adapter | ⏳ `scrape_profile` done; avatar capture not started |
| M4 | LinkedIn **post / edit-mentions / reply-comments** + **pages** adapter | not started |
| M5 | Riverside adapter (extract raw, export magic clips → Spotify → YouTube unlisted) | not started |
| M6 | YouTube adapter + daily limits | not started |
| M7 | MCP server + Claude skill | not started |
| M8 | Tauri desktop GUI (Postiz replacement) + migrate off Postiz | not started |

### The deliberate detour on M2

Federico's approved design (`docs/specs/2026-07-07-chrome-agent-fork-lib.md` + the plan in
`docs/plans/`) is to **fork `sderosiaux/chrome-agent` into a Rust library** — it is
published to crates.io but is **binary-only** (`[[bin]]`, `autolib = false`, no `lib.rs`),
so `cargo add chrome-agent` gives you nothing callable. That fork is still the intended
substrate.

To get a *working* LinkedIn task without blocking on the fork, `BrowserHandle`'s first impl
(`CliBrowser`) shells out to the chrome-agent **CLI**. The trait's methods deliberately
mirror the fork's planned `Page` API (`goto`/`eval`/`screenshot`), so a native
`chrome_agent::Session`-backed impl drops in behind the same trait with **zero adapter
changes**. Nothing about the fork plan is invalidated; it just isn't on the critical path.

**Recommended next step:** either (a) build the fork per the existing plan and swap in a
`NativeBrowser`, or (b) extend the LinkedIn adapter (avatar capture needs `screenshot` +
the viewport fix, which is exactly what the fork's §5.1 delivers — so (a) unblocks it).

## Remaining M1 "known gaps" (deferred, not bugs — see README)

- `set_limit` RPC — limits set via `config.toml` + restart, no runtime RPC.
- `run_now --force` — re-queues now but does not override an active limit defer.
- `subscribe` push — TUI polls `list` + `status` each second instead.
- Single-instance guard — best-effort `AddrInUse` refusal (no lock file); launchd
  enforces one instance in practice.

## Open ideas / smaller follow-ups (author's notes, none blocking)

- Pause/resume state is in-memory; a `pause_scopes` table (or a row in an existing meta
  table) would make it survive daemon restarts if operators expect that.
- `paused_scopes()` returns an unordered `Vec` from a `HashSet` — fine for JSON/TUI, but
  sort it if you ever add a snapshot-style test. (The new `test_pause_resume_via_dispatch`
  in `crates/daemon/src/server.rs` avoids order-dependence with `contains`.)
- `flaky_state` map in `DummyAdapter` is unbounded (test-only adapter, so harmless).
- **The tick loop holds the engine lock for the whole task.** `serve()` does
  `let e = engine.lock().await; e.tick().await`, and `handle_request` also locks the engine.
  M1's tasks were microseconds; a browser task is seconds-to-90s, so a long
  `linkedin/scrape_profile` will block every RPC (`status`, `list`, the TUI's 1s poll) until
  it finishes. Worth fixing before browser tasks get common: run tasks outside the lock, or
  hold the lock only for store/registry reads.
- **`CliBrowser` has one page.** Concurrent browser tasks in the same tick would interleave
  `goto`/`eval` on the same chrome-agent page and corrupt each other, the same way the
  Riverside tooling corrupted us. Today the engine runs tasks sequentially, so this is
  latent — but any move to parallel task execution must give each task its own `--page`.
- `LinkedInAdapter`'s `headline`/`location` are positional guesses over `top_card`. The raw
  `top_card` array is returned precisely so this can be remapped without a redeploy. Avatar
  capture (M3's other half) needs `screenshot` + a real viewport (chrome-agent's default
  caps around ~469px) — i.e. it needs the fork.

**Resolved in the M1-hardening pass (2026-07-07), no longer open:**
- ✅ Daemon-level RPC round-trip test for `Pause`/`Resume`/`Status` — added
  `test_pause_resume_via_dispatch` (`crates/daemon/src/server.rs`).
- ✅ Terminal-status drift — `store.rs` no longer hardcodes the terminal set in SQL;
  `find_active_by_dedup` derives it from `TaskStatus::terminal_strs()` (single source of
  truth over `is_terminal`), guarded by an exhaustive-match test in `model.rs`. Added a
  partial index `idx_tasks_dedup` on `tasks(dedup_key)`.
- ✅ DST `.single().unwrap()` — was *already* resolved in production code before this pass:
  `limits.rs` uses non-panicking `resolve_local`/`local_from_millis`. The only remaining
  `.single().unwrap()` is a test helper (`noon_ms()`) on a fixed non-DST date. The old
  note here was stale.

## First moves for the next agent

1. `source ~/.cargo/env && cd /Users/agente/work/pacewright`
2. `cargo test --workspace` and `cargo clippy --workspace --all-targets -- -D warnings` —
   confirm the 72-test / clippy-clean baseline before changing anything.
3. Read `docs/specs/2026-07-07-core-engine-design.md`, then `crates/core/src/browser.rs`
   (the `BrowserHandle` seam) and `crates/adapter-linkedin/src/lib.rs`.
4. Prove the stack still works end-to-end: run a real LinkedIn task (see the section above).
   If it hits an auth wall, re-read the two chrome-agent gotchas — it is almost always the
   fresh-launch cookie copy.
5. Then pick up the chrome-agent fork (`docs/plans/2026-07-07-chrome-agent-fork-lib.md`)
   — or ask Federico which milestone he wants next.

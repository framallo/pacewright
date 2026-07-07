# pacewright — Core Engine Design (Milestone 1)

**Date:** 2026-07-07
**Status:** Design approved, pending spec review
**Scope:** Milestone 1 only — the platform-agnostic task engine, proven against a DummyAdapter. Real platform adapters (LinkedIn/Riverside/YouTube), the MCP server, the Claude skill, and the Tauri GUI are later milestones, specified separately.

---

## 1. Purpose

`pacewright` is a Rust tool that **queues, schedules, and runs browser-automation tasks** across platforms (LinkedIn first; Twitter/YouTube/Riverside later), replacing today's scattered `pd_*` launchd daemon scripts and eventually **replacing Postiz** as the publishing scheduler.

The name is a play on **Playwright**: a *wright* (craftsman) of **pace**. It encodes the technical thesis — a **real-CDP, human-paced** browser worker, deliberately *not* Playwright (see §9), because for authenticated LinkedIn automation the safest substrate is the real logged-in Chrome profile driven over CDP, with human pacing and human-like input.

Milestone 1 delivers the **engine**: a long-running daemon that owns a task queue, a scheduler, a human-pacing/daily-limit enforcer, a runner, and durable task tracking — with a **DummyAdapter** that exercises all of it with zero browser involvement.

### Goals (M1)
- One persistent background daemon replaces N little daemon scripts.
- Durable queue + scheduler with recurrence, dependencies, retries.
- Per-(platform, action) **daily caps + human pacing** (min-gap, jitter, active hours).
- Every task tracked end to end and **retained after finishing**, with a full event history.
- Thin clients (CLI, TUI) talk to the daemon over a local socket; MCP and Tauri are future clients on the same API.
- Fully testable without a browser via the DummyAdapter and injected clock/RNG.

### Non-goals (M1)
- No real browser automation, no chrome-agent integration, no platform adapters.
- No MCP server, no Claude skill, no Tauri GUI.
- No Postiz migration yet.

---

## 2. Architecture

```
                    launchd (keeps it alive)
                           │
                           ▼
             ┌─────────────────────────────┐
             │   paperclip pacewrightd      │
             │        (daemon)              │
             │  ┌────────┐   ┌───────────┐  │
   clients   │  │Scheduler│──▶│  Runner   │──┼──▶ Adapters
 ───socket──▶│  │+ Limits │   │(executes) │  │    └ DummyAdapter (M1)
  (JSON-RPC) │  └────┬────┘   └─────┬─────┘  │      (linkedin/riverside/
             │       │              │        │       youtube — later)
             │       ▼              ▼        │
             │   ┌─────────────────────┐    │
             │   │   Store (SQLite/WAL) │    │
             │   └─────────────────────┘    │
             └─────────────────────────────┘
                           ▲
      ┌──────────┬─────────┼──────────┬──────────┐
   pcw CLI   pcw tui   Tauri app   MCP server  (clients; Tauri+MCP later)
```

**Process model:** one long-running daemon (`pacewrightd`) owns the queue, scheduler, runner, SQLite store, and (later) the live chrome-agent session. It listens on a Unix-domain socket (`~/.pacewright/pw.sock`) speaking JSON-RPC. Clients (CLI, TUI, and later MCP and Tauri) are thin and stateless — they connect to the socket, issue requests, and render responses. This is the "replace all the daemons with one service" shape.

**Invocation:** CLI binary is `pacewright` (with a short alias `pcw`). Daemon is `pacewrightd`, managed by launchd (label `com.paperclip.pacewrightd`).

### Crate layout (one Cargo workspace)

| Crate | Responsibility | Depends on |
|---|---|---|
| `pacewright-core` | Task model, Store (SQLite), Scheduler, Limits/pacing, Runner, `Adapter` trait, `Clock`/`Rng` abstractions | — (pure logic; no platform I/O) |
| `pacewright-proto` | JSON-RPC request/response types shared by daemon + all clients | serde |
| `pacewright-daemon` | Long-running process; socket server; adapter registry; wires core together | core, proto |
| `pacewright-cli` | `pacewright`/`pcw` binary: `add`, `list`, `get`, `cancel`, `run-now`, `pause`, `resume`, `limits`, `status`, `tui` | proto |
| `pacewright-adapter-dummy` | Reference `Adapter` impl with fake data + simulated latency/failure | core |
| *(later)* `pacewright-adapter-linkedin`, `-riverside`, `-youtube`, `pacewright-mcp`, Tauri app | | |

**Key boundary:** adapters know nothing about scheduling/limits/persistence — they implement `execute(action, params) -> Result`. The engine knows nothing about any platform — it schedules, enforces limits, persists, tracks, and calls the trait. This boundary is exactly what lets the DummyAdapter fully prove the engine.

---

## 3. Data model

### Task row (SQLite table `tasks`)

| Field | Type | Purpose |
|---|---|---|
| `id` | TEXT (UUID) | primary key |
| `adapter` | TEXT | e.g. `dummy`, later `linkedin-profile` |
| `action` | TEXT | e.g. `echo`, later `post` |
| `params` | TEXT (JSON) | action payload |
| `status` | TEXT | see §4 lifecycle |
| `scheduled_for` | INTEGER (epoch ms) | when the task becomes *eligible* (now = ASAP) |
| `next_eligible_at` | INTEGER NULL | set when deferred by limits/pacing |
| `priority` | INTEGER | tie-breaker among due tasks (higher first) |
| `recurrence` | TEXT NULL | cron/RRULE; on success, re-enqueues next occurrence |
| `depends_on` | TEXT NULL | task id; stays `blocked` until that task `succeeded` |
| `dedup_key` | TEXT NULL | optional idempotency guard (unique among non-terminal tasks) |
| `attempts` | INTEGER | attempts so far |
| `max_attempts` | INTEGER | retry budget (default configurable) |
| `last_error` | TEXT NULL | last failure message + class |
| `result` | TEXT NULL (JSON) | value returned by the adapter |
| `created_at` / `updated_at` / `finished_at` | INTEGER | timestamps |

### Audit table `task_events`

| Field | Purpose |
|---|---|
| `id` | autoincrement |
| `task_id` | FK → tasks.id |
| `at` | epoch ms |
| `from_status` / `to_status` | transition |
| `detail` | JSON: reason (e.g. `over_cap`, `too_soon`, error class + message, limit key spent) |

Every state transition writes a `task_events` row. This is the source of truth for "what did the daemon do today" and per-task history in the TUI/MCP.

### Retention

Terminal tasks (`succeeded`, `failed`, `canceled`) are **never auto-deleted** in M1 — they are retained with full `result`, `last_error`, and event history. (A retention/prune policy is a later concern; if added, it is explicit and logged.)

### Limits state (table `limit_counters`)

| Field | Purpose |
|---|---|
| `limit_key` | e.g. `dummy.capped`, later `linkedin.post` |
| `window_date` | local date (YYYY-MM-DD) the counter applies to |
| `count` | actions spent in the window |
| `last_spent_at` | epoch ms of last spend (for min-gap) |

Counters reset when `window_date` rolls over (local midnight). Caps and pacing params are config (§5), not stored per-row.

---

## 4. Task lifecycle

```
                 ┌───────────────── recurrence? enqueue next occurrence
                 ▼
pending ──due & eligible──▶ running ──ok──▶ succeeded
   ▲                          │
   │                          ├─ Retryable ──▶ pending   (backoff: exp + jitter, attempts++)
   │                          ├─ RateLimited ─▶ deferred (next_eligible_at = retry_after; no attempt burned)
   │                          └─ Terminal ────▶ failed
 deferred ◀── over cap / too soon (records reason + next_eligible_at; re-enters pending when eligible)
 blocked  ◀── depends_on not yet succeeded (re-checked when dependency finishes)
 canceled ◀── user cancels a pending/deferred/blocked task
```

**States**
- `pending` — eligible to run at/after `scheduled_for`, subject to limit/pacing check.
- `blocked` — waiting on `depends_on`.
- `deferred` — was due but pacing/cap bumped it; `next_eligible_at` set; re-enters selection when reached.
- `running` — currently executing in the runner.
- `succeeded` / `failed` / `canceled` — terminal, retained.

**Selection rule (scheduler tick):** among tasks with `status ∈ {pending, deferred}` and `scheduled_for ≤ now` and `next_eligible_at ≤ now` (or null) and no unmet `depends_on`, order by `priority` desc, then `scheduled_for` asc. For each candidate, ask the Limits engine (§5) if it may run now. If yes → run. If no → set `deferred` + `next_eligible_at` + event reason.

**Error classes** (returned by adapters) drive the transitions:
- `Retryable(msg)` — transient (network, element-not-ready). Backoff (exponential + jitter), `attempts++`, back to `pending`. When `attempts ≥ max_attempts` → `failed`.
- `Terminal(msg)` — unrecoverable (auth failure, invalid params). Straight to `failed`.
- `RateLimited { retry_after }` — platform pushed back. `deferred`, `next_eligible_at = retry_after`, **no attempt burned**.

---

## 5. Scheduler, limits & human pacing

The Limits engine is the heart of the anti-ban design. Every action an adapter declares carries a set of **limit keys** it spends (§6). Before the runner executes a task, the engine checks each key against:

1. **Daily cap** — max spends per local-day per key. Over cap → defer to next local day.
2. **Min gap** — minimum time since `last_spent_at` for that key. Too soon → defer to `last_spent_at + gap`.
3. **Active hours** — a per-key or per-platform local-time window (e.g. 09:00–18:00). Outside window → defer to next window open.
4. **Jitter** — the effective gap and any scheduled offsets are randomized by ±J% using a **seeded RNG** so pacing is non-deterministic in production but reproducible in tests.

Example config (TOML, `~/.pacewright/config.toml`):

```toml
[limits."dummy.capped"]
daily_cap = 3
min_gap   = "8m"
jitter    = 0.5              # ±50%
active    = "09:00-18:00"    # local

# Later, real keys:
# [limits."linkedin.post"]        daily_cap = 3   min_gap = "45m" ...
# [limits."linkedin.comment"]     daily_cap = 15  min_gap = "8m"  ...
# [limits."linkedin.profile_scrape"] daily_cap = 40 min_gap = "20s" ...
# [limits."youtube.upload"]       daily_cap = 5   min_gap = "30m" ...
```

**Defer, don't drop:** a task that fails any check is never lost — it becomes `deferred` with a computed `next_eligible_at` and a recorded reason. The scheduler re-considers it when eligible.

**Determinism:** all time flows through an injected `Clock` trait; all randomness through a seeded `Rng`. Core logic never reads the wall clock or `rand` directly. Tests fast-forward the clock and fix the seed to assert exact pacing behavior.

**Pacing vs behavioral realism:** this engine handles *when/how often* actions fire (the behavioral half of anti-detection). The *how* (real-profile CDP, human mouse movement, real input events) is the adapter/browser-substrate concern documented in §9 and built in M2+.

---

## 6. Adapter trait & DummyAdapter

```rust
#[async_trait]
trait Adapter {
    fn name(&self) -> &str;
    fn actions(&self) -> &[ActionSpec];        // each action declares the limit keys it consumes
    async fn execute(&self, ctx: &RunCtx, action: &str, params: Value)
        -> Result<Value, AdapterError>;
}

struct ActionSpec {
    name: String,
    limit_keys: Vec<String>,   // engine reads these to know which daily counters to spend
    params_schema: Value,      // JSON Schema; used for validation + MCP tool schema later
    description: String,
}

enum AdapterError {
    Retryable(String),
    Terminal(String),
    RateLimited { retry_after: /* epoch ms */ i64 },
}
```

- **Limits live in the engine, declared by the adapter.** The adapter never counts; it only names the keys an action spends via `ActionSpec.limit_keys`. The engine enforces caps/gaps/hours and increments counters on successful spend.
- `RunCtx` gives the adapter logging, a cancellation token, and (M2+) a handle to the live browser session. The DummyAdapter ignores the browser handle.

### DummyAdapter actions (exercise the whole engine, zero browser)

| Action | Behavior | Proves |
|---|---|---|
| `echo` | returns `params` immediately | happy path, result persistence |
| `slow` | sleeps `params.ms` then returns | concurrency, min-gap pacing |
| `flaky` | returns `Retryable` `params.fail_times` times, then succeeds | retry + exponential backoff |
| `always_fail` | returns `Terminal` | fail path, retention |
| `rate_heavy` | declares `limit_keys = ["dummy.capped"]`, returns `echo` | cap → defer → next-day rollover |

---

## 7. Control socket API (JSON-RPC over `~/.pacewright/pw.sock`)

Types defined once in `pacewright-proto`; reused by CLI, TUI, and later MCP + Tauri.

| Method | Params | Returns |
|---|---|---|
| `add` | `{adapter, action, params, scheduled_for?, recurrence?, depends_on?, priority?, dedup_key?, max_attempts?}` | `{task_id}` |
| `get` | `{id}` | task + event history |
| `list` | `{status?, adapter?, since?, limit?}` | `[task]` |
| `cancel` | `{id}` | `{ok}` |
| `run_now` | `{id, force?}` | bumps `scheduled_for` to now; `force` also bypasses pacing (still records the spend) |
| `pause` / `resume` | `{scope}` = daemon \| platform \| adapter | `{ok}` |
| `limits` | `{}` | current counters, caps, next reset |
| `set_limit` | `{limit_key, daily_cap?, min_gap?, active?, jitter?}` | persisted; `{ok}` |
| `adapters` | `{}` | registered adapters + their `ActionSpec`s (drives discovery + future MCP schema) |
| `status` | `{}` | daemon uptime, queue depth, running tasks, browser-session state |
| `subscribe` | `{}` | server-push stream of task events (feeds live TUI/Tauri) |

---

## 8. Error handling & resilience

- **Crash recovery:** on startup the daemon reconciles the store — any task left `running` (daemon died mid-execution) is re-queued as `pending` (with an event note) unless it exceeds `max_attempts`. SQLite in WAL mode; all state transitions are transactional.
- **Adapter panics** are caught at the runner boundary and mapped to `Terminal` so one bad task can't take down the daemon.
- **Socket errors** are per-connection; a client disconnect never affects the engine.
- **Config reload:** `set_limit` persists to the store and updates the in-memory limits immediately; the TOML file is the boot default, the store is the runtime source of truth.
- **Single-instance guard:** the daemon takes an exclusive lock on the socket path; a second `pacewrightd` refuses to start.

---

## 9. Browser substrate & anti-detection (context for M2+; not built in M1)

Recorded here so M1's engine boundaries anticipate it. Grounded in 2026 research on automation detection.

**Thesis:** for an *authenticated LinkedIn UI session*, the safest substrate is **real-profile CDP via chrome-agent**, not Playwright. Playwright is itself a CDP client but launches a fresh Chromium with `navigator.webdriver=true` and the detectable `Runtime.enable` handshake, and presents an unproven fingerprint. chrome-agent's connect-to-real-Chrome (`--copy-cookies`) inherits a coherent, already-trusted fingerprint + session — which is precisely what LinkedIn keys on. Empirically, direct-CDP tools (nodriver-style) outperform patched Playwright against protocol-level gates; but for LinkedIn the decisive factors are fingerprint *consistency*, behavioral pacing/volume, session/IP/timezone stability, and content dedup — not the protocol per se.

**Requirements for the "extend chrome-agent" workstream (M2):**
1. Audit chrome-agent for the `Runtime.enable` leak, `--enable-automation`, and `navigator.webdriver`; confirm `--stealth` neutralizes them, else patch (isolated-world contexts, nodriver-style).
2. Use **real CDP input events** (`Input.dispatch*`) for clicks/typing — not JS synthetic events (more detectable; break LinkedIn `@`-mention autocomplete).
3. **Human mouse movement:** Bézier/curved trajectories, variable velocity, slight overshoot-and-correct, idle micro-moves — never teleport-to-coordinate.
4. Keep the session pinned to the real profile + stable (residential) IP + stable timezone; never a throwaway Chromium for LinkedIn.
5. Extend chrome-agent viewport (removes the ~469px avatar-capture cap).
6. Behavioral volume/pacing is already handled by the M1 limits engine (§5).

**Sources:** rebrowser.net (Runtime.enable fix), Castle.io (LinkedIn fingerprint teardown; CDP-signal decay), ianlpaterson.com 2026 anti-detect benchmark, scrapfly.io (Playwright stealth), linkednav.com (LinkedIn detection mechanics).

---

## 10. Testing strategy

The DummyAdapter + injected `Clock`/`Rng` make M1 fully testable with no browser.

- **Unit tests** (`pacewright-core`): eligibility selection; cap/min-gap/active-hours/jitter defer computation; retry backoff schedule; state-transition matrix; local-day counter rollover.
- **Integration tests** (`pacewright-daemon` + DummyAdapter on temp socket + temp SQLite, driven via `pacewright-proto` client):
  - `flaky{fail_times:2}` → `succeeded` after 3 attempts with growing backoff.
  - `rate_heavy` ×4 with `daily_cap=3` → 4th is `deferred` to next local day; cap never exceeded.
  - `slow` batch → `min_gap` always honored between spends of the same key.
  - `always_fail` → `failed`, retained with error + full event history.
  - crash recovery → a `running` task at boot is re-queued to `pending`.
  - `depends_on` chain → dependent stays `blocked` until parent `succeeded`.
- **Determinism:** fixed clock + seeded RNG ⇒ reproducible pacing assertions.
- **CI:** `cargo test` across the workspace.

---

## 11. Milestones

| # | Milestone | Crates / deliverable |
|---|---|---|
| **M1** | **Core engine + DummyAdapter + CLI + minimal TUI + launchd** *(this spec)* | `pacewright-core`, `-proto`, `-daemon`, `-cli`, `-adapter-dummy` |
| M2 | Extend chrome-agent (viewport, real input, mouse movement, `Runtime.enable` audit) + browser handle in `RunCtx` | chrome-agent fork |
| M3 | LinkedIn **profile** adapter (scrape, avatar) — port linkedin_scraper to Rust | `pacewright-adapter-linkedin` |
| M4 | LinkedIn **post / edit-mentions / reply-comments** + **pages** adapter | `-adapter-linkedin(-pages)` |
| M5 | Riverside adapter (extract raw, export magic clips, → Spotify, → YouTube unlisted) | `-adapter-riverside` |
| M6 | YouTube adapter + daily limits | `-adapter-youtube` |
| M7 | MCP server + Claude skill | `pacewright-mcp` + skill |
| M8 | Tauri desktop GUI (Postiz replacement) + migrate off Postiz | Tauri app |

Each later milestone gets its own spec → plan → implementation cycle.

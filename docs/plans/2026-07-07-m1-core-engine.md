# pacewright M1 — Core Engine Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build the platform-agnostic pacewright task engine — a long-running daemon that queues, schedules (with daily caps + human pacing), runs, retries, and durably tracks browser-automation tasks — proven end-to-end against a DummyAdapter with zero browser involvement.

**Architecture:** A Cargo workspace. `pacewright-core` holds all pure logic (task model, SQLite store, limits/pacing, scheduler, runner, engine) behind injected `Clock`/`Rng` so it is fully deterministic in tests. `pacewright-daemon` wraps the engine in a Unix-socket JSON-RPC server; `pacewright-cli` is a thin client + minimal ratatui TUI; `pacewright-adapter-dummy` is the reference `Adapter`. `pacewright-proto` defines the wire types shared by daemon and clients.

**Tech Stack:** Rust 2021, tokio (async runtime), rusqlite (bundled SQLite, WAL), serde/serde_json, async-trait, clap (CLI), ratatui + crossterm (TUI), toml (config), uuid, chrono (local time/dates), rand (`StdRng` seeded), croner (recurrence).

## Global Constraints

- **Rust edition 2021**, toolchain ≥ 1.96 (source `~/.cargo/env` before any cargo command; it is not on the default PATH).
- **Binary names:** CLI `pacewright` (alias `pcw`), daemon `pacewrightd`. launchd label `com.paperclip.pacewrightd`.
- **Socket path:** `~/.pacewright/pw.sock`. **Config:** `~/.pacewright/config.toml`. **DB:** `~/.pacewright/pacewright.db`.
- **No wall-clock or `rand` in `pacewright-core`** — all time via the `Clock` trait, all randomness via the `Rng` trait. This is what makes the engine testable.
- **Limits live in the engine, declared by the adapter** via `ActionSpec.limit_keys`. Adapters never count.
- **Terminal tasks are retained** (never auto-deleted). Every state transition writes a `task_events` row.
- **Error classes:** `Retryable` → backoff + re-queue (burns an attempt); `Terminal` → fail; `RateLimited{retry_after}` → defer (no attempt burned).
- **Time unit on the wire and in the DB:** epoch **milliseconds** (`i64`).
- Run all cargo commands from the workspace root `/Users/agente/work/pacewright`.

---

## File Structure

```
pacewright/
  Cargo.toml                         # workspace manifest
  rust-toolchain.toml
  .gitignore
  crates/
    core/
      Cargo.toml
      src/lib.rs                     # re-exports
      src/clock.rs                   # Clock trait, SystemClock, TestClock
      src/rng.rs                     # Rng trait, SeededRng, TestRng
      src/model.rs                   # Task, TaskStatus, AdapterError, ActionSpec, TaskEvent
      src/store.rs                   # Store: SQLite schema + CRUD + counters
      src/limits.rs                  # LimitConfig, LimitsEngine: cap/gap/hours/jitter/defer
      src/adapter.rs                 # Adapter trait, RunCtx, AdapterRegistry
      src/scheduler.rs               # select_runnable()
      src/runner.rs                  # run_task(): execute + map errors + events + recurrence
      src/engine.rs                  # Engine: boot recovery + tick()
      src/config.rs                  # Config (TOML) -> LimitConfig map
    proto/
      Cargo.toml
      src/lib.rs                     # Request, Response, wire enums
    daemon/
      Cargo.toml
      src/main.rs                    # bin: pacewrightd — socket server + tick loop
      src/server.rs                  # accept loop, dispatch, single-instance lock, subscribe
    cli/
      Cargo.toml
      src/main.rs                    # bin: pacewright — clap subcommands + client
      src/client.rs                  # connect to socket, send Request, read Response
      src/tui.rs                     # minimal ratatui dashboard
    adapter-dummy/
      Cargo.toml
      src/lib.rs                     # DummyAdapter: echo/slow/flaky/always_fail/rate_heavy
  packaging/
    com.paperclip.pacewrightd.plist  # launchd template
  docs/specs/2026-07-07-core-engine-design.md
  docs/plans/2026-07-07-m1-core-engine.md
```

---

### Task 1: Workspace scaffold

**Files:**
- Create: `Cargo.toml`, `rust-toolchain.toml`, `.gitignore`
- Create: `crates/core/Cargo.toml`, `crates/core/src/lib.rs`
- Create: `crates/proto/Cargo.toml`, `crates/proto/src/lib.rs`
- Create: `crates/daemon/Cargo.toml`, `crates/daemon/src/main.rs`
- Create: `crates/cli/Cargo.toml`, `crates/cli/src/main.rs`
- Create: `crates/adapter-dummy/Cargo.toml`, `crates/adapter-dummy/src/lib.rs`

**Interfaces:**
- Consumes: nothing.
- Produces: a buildable 5-crate workspace. Later tasks add modules to these crates.

- [ ] **Step 1: Create the workspace manifest**

`Cargo.toml`:
```toml
[workspace]
resolver = "2"
members = ["crates/core", "crates/proto", "crates/daemon", "crates/cli", "crates/adapter-dummy"]

[workspace.package]
edition = "2021"
version = "0.1.0"
license = "MIT"

[workspace.dependencies]
serde = { version = "1", features = ["derive"] }
serde_json = "1"
tokio = { version = "1", features = ["rt-multi-thread", "macros", "net", "io-util", "time", "sync", "signal"] }
async-trait = "0.1"
rusqlite = { version = "0.31", features = ["bundled"] }
uuid = { version = "1", features = ["v4"] }
chrono = { version = "0.4", features = ["clock"] }
rand = "0.8"
thiserror = "1"
anyhow = "1"
clap = { version = "4", features = ["derive"] }
ratatui = "0.28"
crossterm = "0.28"
toml = "0.8"
croner = "2"
tracing = "0.1"
tracing-subscriber = "0.3"
```

`rust-toolchain.toml`:
```toml
[toolchain]
channel = "1.96.1"
```

`.gitignore`:
```
/target
*.db
*.db-wal
*.db-shm
```

- [ ] **Step 2: Create each crate manifest and a stub entrypoint**

`crates/core/Cargo.toml`:
```toml
[package]
name = "pacewright-core"
edition.workspace = true
version.workspace = true
license.workspace = true

[dependencies]
serde.workspace = true
serde_json.workspace = true
rusqlite.workspace = true
uuid.workspace = true
chrono.workspace = true
rand.workspace = true
async-trait.workspace = true
thiserror.workspace = true
croner.workspace = true
tracing.workspace = true
```
`crates/core/src/lib.rs`:
```rust
//! pacewright-core: platform-agnostic task engine.
```

`crates/proto/Cargo.toml`:
```toml
[package]
name = "pacewright-proto"
edition.workspace = true
version.workspace = true
license.workspace = true

[dependencies]
serde.workspace = true
serde_json.workspace = true
```
`crates/proto/src/lib.rs`:
```rust
//! pacewright-proto: JSON-RPC wire types.
```

`crates/adapter-dummy/Cargo.toml`:
```toml
[package]
name = "pacewright-adapter-dummy"
edition.workspace = true
version.workspace = true
license.workspace = true

[dependencies]
pacewright-core = { path = "../core" }
serde_json.workspace = true
async-trait.workspace = true
tokio = { workspace = true }
```
`crates/adapter-dummy/src/lib.rs`:
```rust
//! pacewright-adapter-dummy: reference Adapter for testing the engine.
```

`crates/daemon/Cargo.toml`:
```toml
[package]
name = "pacewright-daemon"
edition.workspace = true
version.workspace = true
license.workspace = true

[[bin]]
name = "pacewrightd"
path = "src/main.rs"

[dependencies]
pacewright-core = { path = "../core" }
pacewright-proto = { path = "../proto" }
pacewright-adapter-dummy = { path = "../adapter-dummy" }
tokio.workspace = true
serde_json.workspace = true
anyhow.workspace = true
tracing.workspace = true
tracing-subscriber.workspace = true
chrono.workspace = true
```
`crates/daemon/src/main.rs`:
```rust
fn main() {
    println!("pacewrightd stub");
}
```

`crates/cli/Cargo.toml`:
```toml
[package]
name = "pacewright-cli"
edition.workspace = true
version.workspace = true
license.workspace = true

[[bin]]
name = "pacewright"
path = "src/main.rs"

[dependencies]
pacewright-proto = { path = "../proto" }
tokio.workspace = true
serde_json.workspace = true
anyhow.workspace = true
clap.workspace = true
ratatui.workspace = true
crossterm.workspace = true
```
`crates/cli/src/main.rs`:
```rust
fn main() {
    println!("pacewright stub");
}
```

- [ ] **Step 3: Build the workspace**

Run: `source ~/.cargo/env && cd /Users/agente/work/pacewright && cargo build`
Expected: compiles all 5 crates with no errors (warnings about unused stubs are fine).

- [ ] **Step 4: Commit**

```bash
git add -A && git commit -m "chore: scaffold pacewright cargo workspace"
```

---

### Task 2: Clock and Rng abstractions

**Files:**
- Create: `crates/core/src/clock.rs`, `crates/core/src/rng.rs`
- Modify: `crates/core/src/lib.rs`

**Interfaces:**
- Produces:
  - `trait Clock { fn now_ms(&self) -> i64; }` with `SystemClock` and `TestClock`.
  - `TestClock::new(start_ms)`, `TestClock::advance(delta_ms)`, `TestClock::set(ms)`.
  - `trait Rng { fn jitter(&self, base_ms: i64, factor: f64) -> i64; }` with `SeededRng::new(seed: u64)` and `TestRng::fixed(value_ms)`.

- [ ] **Step 1: Write the failing tests**

`crates/core/src/clock.rs`:
```rust
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

pub trait Clock: Send + Sync {
    fn now_ms(&self) -> i64;
}

pub struct SystemClock;
impl Clock for SystemClock {
    fn now_ms(&self) -> i64 {
        chrono::Utc::now().timestamp_millis()
    }
}

#[derive(Clone)]
pub struct TestClock(Arc<AtomicI64>);
impl TestClock {
    pub fn new(start_ms: i64) -> Self { Self(Arc::new(AtomicI64::new(start_ms))) }
    pub fn advance(&self, delta_ms: i64) { self.0.fetch_add(delta_ms, Ordering::SeqCst); }
    pub fn set(&self, ms: i64) { self.0.store(ms, Ordering::SeqCst); }
}
impl Clock for TestClock {
    fn now_ms(&self) -> i64 { self.0.load(Ordering::SeqCst) }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_clock_advances() {
        let c = TestClock::new(1_000);
        assert_eq!(c.now_ms(), 1_000);
        c.advance(500);
        assert_eq!(c.now_ms(), 1_500);
        c.set(42);
        assert_eq!(c.now_ms(), 42);
    }
}
```

`crates/core/src/rng.rs`:
```rust
use rand::rngs::StdRng;
use rand::{Rng as _, SeedableRng};
use std::sync::Mutex;

pub trait Rng: Send + Sync {
    /// Return base_ms scaled by a random factor in [1-factor, 1+factor].
    fn jitter(&self, base_ms: i64, factor: f64) -> i64;
}

pub struct SeededRng(Mutex<StdRng>);
impl SeededRng {
    pub fn new(seed: u64) -> Self { Self(Mutex::new(StdRng::seed_from_u64(seed))) }
}
impl Rng for SeededRng {
    fn jitter(&self, base_ms: i64, factor: f64) -> i64 {
        let f = self.0.lock().unwrap().gen_range(-factor..=factor);
        (base_ms as f64 * (1.0 + f)).round() as i64
    }
}

/// Deterministic: always returns exactly `value_ms` regardless of input.
pub struct TestRng(pub i64);
impl TestRng {
    pub fn fixed(value_ms: i64) -> Self { Self(value_ms) }
}
impl Rng for TestRng {
    fn jitter(&self, _base_ms: i64, _factor: f64) -> i64 { self.0 }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_seeded_is_reproducible() {
        let a = SeededRng::new(7).jitter(1000, 0.5);
        let b = SeededRng::new(7).jitter(1000, 0.5);
        assert_eq!(a, b);
        assert!(a >= 500 && a <= 1500);
    }
    #[test]
    fn test_fixed_rng() {
        assert_eq!(TestRng::fixed(1234).jitter(999, 0.9), 1234);
    }
}
```

- [ ] **Step 2: Wire modules and run tests to verify they fail then pass**

Modify `crates/core/src/lib.rs`:
```rust
//! pacewright-core: platform-agnostic task engine.
pub mod clock;
pub mod rng;
```
Run: `source ~/.cargo/env && cargo test -p pacewright-core clock rng`
Expected: tests compile and PASS (`test_clock_advances`, `test_seeded_is_reproducible`, `test_fixed_rng`).

- [ ] **Step 3: Commit**

```bash
git add -A && git commit -m "feat(core): injectable Clock and Rng abstractions"
```

---

### Task 3: Task model, statuses, and error types

**Files:**
- Create: `crates/core/src/model.rs`
- Modify: `crates/core/src/lib.rs`

**Interfaces:**
- Produces:
  - `enum TaskStatus { Pending, Blocked, Deferred, Running, Succeeded, Failed, Canceled }` (serde `snake_case`).
  - `struct Task { id: String, adapter, action: String, params: serde_json::Value, status: TaskStatus, scheduled_for: i64, next_eligible_at: Option<i64>, priority: i64, recurrence: Option<String>, depends_on: Option<String>, dedup_key: Option<String>, attempts: i64, max_attempts: i64, last_error: Option<String>, result: Option<serde_json::Value>, created_at: i64, updated_at: i64, finished_at: Option<i64> }`.
  - `Task::new_now(adapter, action, params, now_ms)` constructor with defaults.
  - `enum AdapterError { Retryable(String), Terminal(String), RateLimited { retry_after: i64 } }` (implements `std::error::Error`).
  - `struct ActionSpec { name: String, limit_keys: Vec<String>, params_schema: serde_json::Value, description: String }`.
  - `struct TaskEvent { task_id: String, at: i64, from_status: Option<TaskStatus>, to_status: TaskStatus, detail: serde_json::Value }`.

- [ ] **Step 1: Write the failing test**

`crates/core/src/model.rs`:
```rust
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Pending, Blocked, Deferred, Running, Succeeded, Failed, Canceled,
}

impl TaskStatus {
    pub fn is_terminal(&self) -> bool {
        matches!(self, TaskStatus::Succeeded | TaskStatus::Failed | TaskStatus::Canceled)
    }
    pub fn as_str(&self) -> &'static str {
        match self {
            TaskStatus::Pending => "pending",
            TaskStatus::Blocked => "blocked",
            TaskStatus::Deferred => "deferred",
            TaskStatus::Running => "running",
            TaskStatus::Succeeded => "succeeded",
            TaskStatus::Failed => "failed",
            TaskStatus::Canceled => "canceled",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub adapter: String,
    pub action: String,
    pub params: Value,
    pub status: TaskStatus,
    pub scheduled_for: i64,
    pub next_eligible_at: Option<i64>,
    pub priority: i64,
    pub recurrence: Option<String>,
    pub depends_on: Option<String>,
    pub dedup_key: Option<String>,
    pub attempts: i64,
    pub max_attempts: i64,
    pub last_error: Option<String>,
    pub result: Option<Value>,
    pub created_at: i64,
    pub updated_at: i64,
    pub finished_at: Option<i64>,
}

impl Task {
    pub fn new_now(adapter: impl Into<String>, action: impl Into<String>, params: Value, now_ms: i64) -> Self {
        Task {
            id: uuid::Uuid::new_v4().to_string(),
            adapter: adapter.into(),
            action: action.into(),
            params,
            status: TaskStatus::Pending,
            scheduled_for: now_ms,
            next_eligible_at: None,
            priority: 0,
            recurrence: None,
            depends_on: None,
            dedup_key: None,
            attempts: 0,
            max_attempts: 3,
            last_error: None,
            result: None,
            created_at: now_ms,
            updated_at: now_ms,
            finished_at: None,
        }
    }
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum AdapterError {
    #[error("retryable: {0}")]
    Retryable(String),
    #[error("terminal: {0}")]
    Terminal(String),
    #[error("rate limited until {retry_after}")]
    RateLimited { retry_after: i64 },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionSpec {
    pub name: String,
    pub limit_keys: Vec<String>,
    pub params_schema: Value,
    pub description: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskEvent {
    pub task_id: String,
    pub at: i64,
    pub from_status: Option<TaskStatus>,
    pub to_status: TaskStatus,
    pub detail: Value,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_new_now_defaults() {
        let t = Task::new_now("dummy", "echo", serde_json::json!({"x":1}), 5_000);
        assert_eq!(t.adapter, "dummy");
        assert_eq!(t.status, TaskStatus::Pending);
        assert_eq!(t.scheduled_for, 5_000);
        assert_eq!(t.max_attempts, 3);
        assert!(!t.id.is_empty());
    }
    #[test]
    fn test_status_serde_snake_case() {
        let j = serde_json::to_string(&TaskStatus::Deferred).unwrap();
        assert_eq!(j, "\"deferred\"");
    }
    #[test]
    fn test_terminal_flag() {
        assert!(TaskStatus::Succeeded.is_terminal());
        assert!(!TaskStatus::Pending.is_terminal());
    }
}
```

- [ ] **Step 2: Wire module and run tests**

Add to `crates/core/src/lib.rs`:
```rust
pub mod model;
```
Run: `source ~/.cargo/env && cargo test -p pacewright-core model`
Expected: PASS (`test_new_now_defaults`, `test_status_serde_snake_case`, `test_terminal_flag`).

- [ ] **Step 3: Commit**

```bash
git add -A && git commit -m "feat(core): task model, statuses, error and event types"
```

---

### Task 4: SQLite store

**Files:**
- Create: `crates/core/src/store.rs`
- Modify: `crates/core/src/lib.rs`

**Interfaces:**
- Consumes: `model::{Task, TaskStatus, TaskEvent}`.
- Produces `struct Store` with:
  - `Store::open(path: &str) -> rusqlite::Result<Store>` (runs schema, sets WAL).
  - `Store::open_in_memory() -> rusqlite::Result<Store>` (for tests).
  - `insert_task(&self, t: &Task) -> rusqlite::Result<()>`
  - `get_task(&self, id: &str) -> rusqlite::Result<Option<Task>>`
  - `update_task(&self, t: &Task) -> rusqlite::Result<()>` (bumps nothing itself — caller sets fields)
  - `list_tasks(&self, status: Option<TaskStatus>, limit: i64) -> rusqlite::Result<Vec<Task>>`
  - `append_event(&self, e: &TaskEvent) -> rusqlite::Result<()>`
  - `events_for(&self, task_id: &str) -> rusqlite::Result<Vec<TaskEvent>>`
  - `find_active_by_dedup(&self, key: &str) -> rusqlite::Result<Option<Task>>` (non-terminal only)
  - `counter_get(&self, key: &str, date: &str) -> rusqlite::Result<(i64, Option<i64>)>` → `(count, last_spent_at)`
  - `counter_spend(&self, key: &str, date: &str, at_ms: i64) -> rusqlite::Result<()>` (upsert +1, set last_spent_at)
  - `tasks_in_status(&self, status: TaskStatus) -> rusqlite::Result<Vec<Task>>`

- [ ] **Step 1: Write the failing test**

`crates/core/src/store.rs`:
```rust
use crate::model::{Task, TaskEvent, TaskStatus};
use rusqlite::{params, Connection, OptionalExtension};
use std::sync::Mutex;

pub struct Store {
    conn: Mutex<Connection>,
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS tasks (
    id TEXT PRIMARY KEY,
    adapter TEXT NOT NULL,
    action TEXT NOT NULL,
    params TEXT NOT NULL,
    status TEXT NOT NULL,
    scheduled_for INTEGER NOT NULL,
    next_eligible_at INTEGER,
    priority INTEGER NOT NULL DEFAULT 0,
    recurrence TEXT,
    depends_on TEXT,
    dedup_key TEXT,
    attempts INTEGER NOT NULL DEFAULT 0,
    max_attempts INTEGER NOT NULL DEFAULT 3,
    last_error TEXT,
    result TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    finished_at INTEGER
);
CREATE INDEX IF NOT EXISTS idx_tasks_status ON tasks(status);
CREATE TABLE IF NOT EXISTS task_events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    task_id TEXT NOT NULL,
    at INTEGER NOT NULL,
    from_status TEXT,
    to_status TEXT NOT NULL,
    detail TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_events_task ON task_events(task_id);
CREATE TABLE IF NOT EXISTS limit_counters (
    limit_key TEXT NOT NULL,
    window_date TEXT NOT NULL,
    count INTEGER NOT NULL DEFAULT 0,
    last_spent_at INTEGER,
    PRIMARY KEY (limit_key, window_date)
);
"#;

impl Store {
    pub fn open(path: &str) -> rusqlite::Result<Store> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.execute_batch(SCHEMA)?;
        Ok(Store { conn: Mutex::new(conn) })
    }
    pub fn open_in_memory() -> rusqlite::Result<Store> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        Ok(Store { conn: Mutex::new(conn) })
    }

    fn row_to_task(row: &rusqlite::Row) -> rusqlite::Result<Task> {
        let status: String = row.get("status")?;
        let params: String = row.get("params")?;
        let result: Option<String> = row.get("result")?;
        Ok(Task {
            id: row.get("id")?,
            adapter: row.get("adapter")?,
            action: row.get("action")?,
            params: serde_json::from_str(&params).unwrap_or(serde_json::Value::Null),
            status: status_from_str(&status),
            scheduled_for: row.get("scheduled_for")?,
            next_eligible_at: row.get("next_eligible_at")?,
            priority: row.get("priority")?,
            recurrence: row.get("recurrence")?,
            depends_on: row.get("depends_on")?,
            dedup_key: row.get("dedup_key")?,
            attempts: row.get("attempts")?,
            max_attempts: row.get("max_attempts")?,
            last_error: row.get("last_error")?,
            result: result.and_then(|s| serde_json::from_str(&s).ok()),
            created_at: row.get("created_at")?,
            updated_at: row.get("updated_at")?,
            finished_at: row.get("finished_at")?,
        })
    }

    pub fn insert_task(&self, t: &Task) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO tasks (id,adapter,action,params,status,scheduled_for,next_eligible_at,priority,recurrence,depends_on,dedup_key,attempts,max_attempts,last_error,result,created_at,updated_at,finished_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18)",
            params![
                t.id, t.adapter, t.action, serde_json::to_string(&t.params).unwrap(),
                t.status.as_str(), t.scheduled_for, t.next_eligible_at, t.priority,
                t.recurrence, t.depends_on, t.dedup_key, t.attempts, t.max_attempts,
                t.last_error, t.result.as_ref().map(|v| serde_json::to_string(v).unwrap()),
                t.created_at, t.updated_at, t.finished_at
            ],
        )?;
        Ok(())
    }

    pub fn update_task(&self, t: &Task) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE tasks SET adapter=?2,action=?3,params=?4,status=?5,scheduled_for=?6,next_eligible_at=?7,priority=?8,recurrence=?9,depends_on=?10,dedup_key=?11,attempts=?12,max_attempts=?13,last_error=?14,result=?15,created_at=?16,updated_at=?17,finished_at=?18 WHERE id=?1",
            params![
                t.id, t.adapter, t.action, serde_json::to_string(&t.params).unwrap(),
                t.status.as_str(), t.scheduled_for, t.next_eligible_at, t.priority,
                t.recurrence, t.depends_on, t.dedup_key, t.attempts, t.max_attempts,
                t.last_error, t.result.as_ref().map(|v| serde_json::to_string(v).unwrap()),
                t.created_at, t.updated_at, t.finished_at
            ],
        )?;
        Ok(())
    }

    pub fn get_task(&self, id: &str) -> rusqlite::Result<Option<Task>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row("SELECT * FROM tasks WHERE id=?1", params![id], Self::row_to_task).optional()
    }

    pub fn list_tasks(&self, status: Option<TaskStatus>, limit: i64) -> rusqlite::Result<Vec<Task>> {
        let conn = self.conn.lock().unwrap();
        let mut out = Vec::new();
        match status {
            Some(s) => {
                let mut stmt = conn.prepare("SELECT * FROM tasks WHERE status=?1 ORDER BY created_at DESC LIMIT ?2")?;
                let rows = stmt.query_map(params![s.as_str(), limit], Self::row_to_task)?;
                for r in rows { out.push(r?); }
            }
            None => {
                let mut stmt = conn.prepare("SELECT * FROM tasks ORDER BY created_at DESC LIMIT ?1")?;
                let rows = stmt.query_map(params![limit], Self::row_to_task)?;
                for r in rows { out.push(r?); }
            }
        }
        Ok(out)
    }

    pub fn tasks_in_status(&self, status: TaskStatus) -> rusqlite::Result<Vec<Task>> {
        self.list_tasks(Some(status), i64::MAX)
    }

    pub fn find_active_by_dedup(&self, key: &str) -> rusqlite::Result<Option<Task>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT * FROM tasks WHERE dedup_key=?1 AND status NOT IN ('succeeded','failed','canceled') LIMIT 1",
            params![key], Self::row_to_task,
        ).optional()
    }

    pub fn append_event(&self, e: &TaskEvent) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO task_events (task_id,at,from_status,to_status,detail) VALUES (?1,?2,?3,?4,?5)",
            params![e.task_id, e.at, e.from_status.map(|s| s.as_str()), e.to_status.as_str(), serde_json::to_string(&e.detail).unwrap()],
        )?;
        Ok(())
    }

    pub fn events_for(&self, task_id: &str) -> rusqlite::Result<Vec<TaskEvent>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT task_id,at,from_status,to_status,detail FROM task_events WHERE task_id=?1 ORDER BY id ASC")?;
        let rows = stmt.query_map(params![task_id], |row| {
            let from: Option<String> = row.get("from_status")?;
            let to: String = row.get("to_status")?;
            let detail: String = row.get("detail")?;
            Ok(TaskEvent {
                task_id: row.get("task_id")?,
                at: row.get("at")?,
                from_status: from.map(|s| status_from_str(&s)),
                to_status: status_from_str(&to),
                detail: serde_json::from_str(&detail).unwrap_or(serde_json::Value::Null),
            })
        })?;
        let mut out = Vec::new();
        for r in rows { out.push(r?); }
        Ok(out)
    }

    pub fn counter_get(&self, key: &str, date: &str) -> rusqlite::Result<(i64, Option<i64>)> {
        let conn = self.conn.lock().unwrap();
        let res = conn.query_row(
            "SELECT count,last_spent_at FROM limit_counters WHERE limit_key=?1 AND window_date=?2",
            params![key, date], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Option<i64>>(1)?)),
        ).optional()?;
        Ok(res.unwrap_or((0, None)))
    }

    pub fn counter_spend(&self, key: &str, date: &str, at_ms: i64) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO limit_counters (limit_key,window_date,count,last_spent_at) VALUES (?1,?2,1,?3)
             ON CONFLICT(limit_key,window_date) DO UPDATE SET count=count+1, last_spent_at=?3",
            params![key, date, at_ms],
        )?;
        Ok(())
    }
}

fn status_from_str(s: &str) -> TaskStatus {
    match s {
        "pending" => TaskStatus::Pending,
        "blocked" => TaskStatus::Blocked,
        "deferred" => TaskStatus::Deferred,
        "running" => TaskStatus::Running,
        "succeeded" => TaskStatus::Succeeded,
        "failed" => TaskStatus::Failed,
        "canceled" => TaskStatus::Canceled,
        _ => TaskStatus::Pending,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Task;

    #[test]
    fn test_insert_get_update_roundtrip() {
        let s = Store::open_in_memory().unwrap();
        let mut t = Task::new_now("dummy", "echo", serde_json::json!({"x":1}), 1000);
        s.insert_task(&t).unwrap();
        let got = s.get_task(&t.id).unwrap().unwrap();
        assert_eq!(got.action, "echo");
        t.status = TaskStatus::Succeeded;
        t.result = Some(serde_json::json!({"ok":true}));
        s.update_task(&t).unwrap();
        let got2 = s.get_task(&t.id).unwrap().unwrap();
        assert_eq!(got2.status, TaskStatus::Succeeded);
        assert_eq!(got2.result, Some(serde_json::json!({"ok":true})));
    }

    #[test]
    fn test_dedup_only_matches_active() {
        let s = Store::open_in_memory().unwrap();
        let mut t = Task::new_now("dummy", "echo", serde_json::json!({}), 1000);
        t.dedup_key = Some("k1".into());
        s.insert_task(&t).unwrap();
        assert!(s.find_active_by_dedup("k1").unwrap().is_some());
        t.status = TaskStatus::Succeeded;
        s.update_task(&t).unwrap();
        assert!(s.find_active_by_dedup("k1").unwrap().is_none());
    }

    #[test]
    fn test_counter_spend_and_rollover() {
        let s = Store::open_in_memory().unwrap();
        s.counter_spend("acme.post", "2026-07-07", 100).unwrap();
        s.counter_spend("acme.post", "2026-07-07", 200).unwrap();
        let (c, last) = s.counter_get("acme.post", "2026-07-07").unwrap();
        assert_eq!(c, 2);
        assert_eq!(last, Some(200));
        // different day is a fresh counter
        assert_eq!(s.counter_get("acme.post", "2026-07-08").unwrap(), (0, None));
    }

    #[test]
    fn test_events_ordered() {
        let s = Store::open_in_memory().unwrap();
        let t = Task::new_now("dummy", "echo", serde_json::json!({}), 1000);
        s.insert_task(&t).unwrap();
        s.append_event(&TaskEvent { task_id: t.id.clone(), at: 1, from_status: None, to_status: TaskStatus::Pending, detail: serde_json::json!({}) }).unwrap();
        s.append_event(&TaskEvent { task_id: t.id.clone(), at: 2, from_status: Some(TaskStatus::Pending), to_status: TaskStatus::Running, detail: serde_json::json!({}) }).unwrap();
        let evs = s.events_for(&t.id).unwrap();
        assert_eq!(evs.len(), 2);
        assert_eq!(evs[1].to_status, TaskStatus::Running);
    }
}
```

- [ ] **Step 2: Wire module, run tests**

Add to `crates/core/src/lib.rs`:
```rust
pub mod store;
```
Run: `source ~/.cargo/env && cargo test -p pacewright-core store`
Expected: PASS (4 tests: roundtrip, dedup, counter rollover, events ordered).

- [ ] **Step 3: Commit**

```bash
git add -A && git commit -m "feat(core): SQLite store for tasks, events, and limit counters"
```

---

### Task 5: Adapter trait, RunCtx, and registry

**Files:**
- Create: `crates/core/src/adapter.rs`
- Modify: `crates/core/src/lib.rs`

**Interfaces:**
- Consumes: `model::{ActionSpec, AdapterError}`.
- Produces:
  - `struct RunCtx { pub task_id: String, pub cancel: tokio_util-free flag }` — for M1, `RunCtx { pub task_id: String }` (browser handle added in M2).
  - `#[async_trait] trait Adapter: Send + Sync { fn name(&self) -> &str; fn actions(&self) -> Vec<ActionSpec>; async fn execute(&self, ctx: &RunCtx, action: &str, params: serde_json::Value) -> Result<serde_json::Value, AdapterError>; }`
  - `Adapter::limit_keys_for(&self, action: &str) -> Vec<String>` (default impl: look up in `actions()`).
  - `struct AdapterRegistry` with `register(Arc<dyn Adapter>)`, `get(name) -> Option<Arc<dyn Adapter>>`, `all() -> Vec<Arc<dyn Adapter>>`.

- [ ] **Step 1: Add async-trait/tokio to core and write the failing test**

First add to `crates/core/Cargo.toml` `[dependencies]`:
```toml
tokio = { workspace = true }
```

`crates/core/src/adapter.rs`:
```rust
use crate::model::{ActionSpec, AdapterError};
use async_trait::async_trait;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;

pub struct RunCtx {
    pub task_id: String,
    // M2+: pub browser: BrowserHandle
}

#[async_trait]
pub trait Adapter: Send + Sync {
    fn name(&self) -> &str;
    fn actions(&self) -> Vec<ActionSpec>;
    async fn execute(&self, ctx: &RunCtx, action: &str, params: Value) -> Result<Value, AdapterError>;

    /// Which daily-limit keys the given action spends. Default: read from `actions()`.
    fn limit_keys_for(&self, action: &str) -> Vec<String> {
        self.actions()
            .into_iter()
            .find(|a| a.name == action)
            .map(|a| a.limit_keys)
            .unwrap_or_default()
    }
}

#[derive(Default, Clone)]
pub struct AdapterRegistry {
    map: HashMap<String, Arc<dyn Adapter>>,
}

impl AdapterRegistry {
    pub fn new() -> Self { Self::default() }
    pub fn register(&mut self, a: Arc<dyn Adapter>) { self.map.insert(a.name().to_string(), a); }
    pub fn get(&self, name: &str) -> Option<Arc<dyn Adapter>> { self.map.get(name).cloned() }
    pub fn all(&self) -> Vec<Arc<dyn Adapter>> { self.map.values().cloned().collect() }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeAdapter;
    #[async_trait]
    impl Adapter for FakeAdapter {
        fn name(&self) -> &str { "fake" }
        fn actions(&self) -> Vec<ActionSpec> {
            vec![ActionSpec { name: "go".into(), limit_keys: vec!["fake.go".into()], params_schema: Value::Null, description: "".into() }]
        }
        async fn execute(&self, _ctx: &RunCtx, _action: &str, _params: Value) -> Result<Value, AdapterError> {
            Ok(Value::Null)
        }
    }

    #[tokio::test]
    async fn test_registry_and_limit_keys() {
        let mut reg = AdapterRegistry::new();
        reg.register(Arc::new(FakeAdapter));
        let a = reg.get("fake").unwrap();
        assert_eq!(a.name(), "fake");
        assert_eq!(a.limit_keys_for("go"), vec!["fake.go".to_string()]);
        assert!(a.limit_keys_for("missing").is_empty());
        let ctx = RunCtx { task_id: "t1".into() };
        assert_eq!(a.execute(&ctx, "go", Value::Null).await.unwrap(), Value::Null);
    }
}
```

- [ ] **Step 2: Wire module, run test**

Add to `crates/core/src/lib.rs`:
```rust
pub mod adapter;
```
Run: `source ~/.cargo/env && cargo test -p pacewright-core adapter`
Expected: PASS (`test_registry_and_limit_keys`).

- [ ] **Step 3: Commit**

```bash
git add -A && git commit -m "feat(core): Adapter trait, RunCtx, and adapter registry"
```

---

### Task 6: DummyAdapter

**Files:**
- Modify: `crates/adapter-dummy/src/lib.rs`

**Interfaces:**
- Consumes: `pacewright_core::adapter::{Adapter, RunCtx}`, `pacewright_core::model::{ActionSpec, AdapterError}`.
- Produces: `struct DummyAdapter` implementing `Adapter` with actions `echo`, `slow`, `flaky`, `always_fail`, `rate_heavy`. `flaky` uses an internal attempt counter keyed by `ctx.task_id` so it fails `fail_times` then succeeds.

- [ ] **Step 1: Write the failing test**

`crates/adapter-dummy/src/lib.rs`:
```rust
use async_trait::async_trait;
use pacewright_core::adapter::{Adapter, RunCtx};
use pacewright_core::model::{ActionSpec, AdapterError};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Mutex;

#[derive(Default)]
pub struct DummyAdapter {
    // task_id -> attempts already made (for `flaky`)
    flaky_state: Mutex<HashMap<String, u64>>,
}

impl DummyAdapter {
    pub fn new() -> Self { Self::default() }
}

#[async_trait]
impl Adapter for DummyAdapter {
    fn name(&self) -> &str { "dummy" }

    fn actions(&self) -> Vec<ActionSpec> {
        vec![
            ActionSpec { name: "echo".into(), limit_keys: vec![], params_schema: Value::Null, description: "returns params".into() },
            ActionSpec { name: "slow".into(), limit_keys: vec![], params_schema: json!({"ms":"number"}), description: "sleeps ms".into() },
            ActionSpec { name: "flaky".into(), limit_keys: vec![], params_schema: json!({"fail_times":"number"}), description: "fails then succeeds".into() },
            ActionSpec { name: "always_fail".into(), limit_keys: vec![], params_schema: Value::Null, description: "terminal error".into() },
            ActionSpec { name: "rate_heavy".into(), limit_keys: vec!["dummy.capped".into()], params_schema: Value::Null, description: "spends dummy.capped".into() },
        ]
    }

    async fn execute(&self, ctx: &RunCtx, action: &str, params: Value) -> Result<Value, AdapterError> {
        match action {
            "echo" | "rate_heavy" => Ok(params),
            "slow" => {
                let ms = params.get("ms").and_then(|v| v.as_u64()).unwrap_or(0);
                tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
                Ok(json!({"slept_ms": ms}))
            }
            "always_fail" => Err(AdapterError::Terminal("always fails".into())),
            "flaky" => {
                let fail_times = params.get("fail_times").and_then(|v| v.as_u64()).unwrap_or(1);
                let mut st = self.flaky_state.lock().unwrap();
                let n = st.entry(ctx.task_id.clone()).or_insert(0);
                if *n < fail_times {
                    *n += 1;
                    Err(AdapterError::Retryable(format!("flaky attempt {}", *n)))
                } else {
                    Ok(json!({"succeeded_after": *n}))
                }
            }
            other => Err(AdapterError::Terminal(format!("unknown action {other}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(id: &str) -> RunCtx { RunCtx { task_id: id.into() } }

    #[tokio::test]
    async fn test_echo_and_always_fail() {
        let a = DummyAdapter::new();
        assert_eq!(a.execute(&ctx("t"), "echo", json!({"a":1})).await.unwrap(), json!({"a":1}));
        assert!(matches!(a.execute(&ctx("t"), "always_fail", Value::Null).await, Err(AdapterError::Terminal(_))));
    }

    #[tokio::test]
    async fn test_flaky_fails_then_succeeds() {
        let a = DummyAdapter::new();
        let p = json!({"fail_times": 2});
        assert!(a.execute(&ctx("x"), "flaky", p.clone()).await.is_err());
        assert!(a.execute(&ctx("x"), "flaky", p.clone()).await.is_err());
        assert!(a.execute(&ctx("x"), "flaky", p.clone()).await.is_ok());
    }

    #[tokio::test]
    async fn test_rate_heavy_declares_limit_key() {
        let a = DummyAdapter::new();
        assert_eq!(a.limit_keys_for("rate_heavy"), vec!["dummy.capped".to_string()]);
    }
}
```

- [ ] **Step 2: Run tests**

Run: `source ~/.cargo/env && cargo test -p pacewright-adapter-dummy`
Expected: PASS (3 tests).

- [ ] **Step 3: Commit**

```bash
git add -A && git commit -m "feat(adapter-dummy): reference adapter with echo/slow/flaky/always_fail/rate_heavy"
```

---

### Task 7: Config + LimitConfig parsing

**Files:**
- Create: `crates/core/src/config.rs`
- Modify: `crates/core/src/lib.rs`, `crates/core/Cargo.toml`

**Interfaces:**
- Produces:
  - `struct LimitConfig { pub daily_cap: i64, pub min_gap_ms: i64, pub jitter: f64, pub active_start_min: i32, pub active_end_min: i32 }` (active_*_min = minutes since local midnight; default 0..1440 = always).
  - `struct Config { pub limits: HashMap<String, LimitConfig> }`.
  - `Config::from_toml(s: &str) -> anyhow::Result<Config>` — parses gaps like `"8m"`, `"20s"`, `"45m"`, active like `"09:00-18:00"`.
  - `Config::default()` — empty limits map (everything unrestricted).
  - `Config::limit_for(&self, key: &str) -> LimitConfig` — returns configured or a permissive default (`daily_cap=i64::MAX, min_gap_ms=0, jitter=0.0, active 0..1440`).

- [ ] **Step 1: Add deps and write the failing test**

Add to `crates/core/Cargo.toml` `[dependencies]`:
```toml
toml.workspace = true
anyhow.workspace = true
serde.workspace = true
```

`crates/core/src/config.rs`:
```rust
use anyhow::{anyhow, Result};
use serde::Deserialize;
use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq)]
pub struct LimitConfig {
    pub daily_cap: i64,
    pub min_gap_ms: i64,
    pub jitter: f64,
    pub active_start_min: i32,
    pub active_end_min: i32,
}

impl LimitConfig {
    pub fn permissive() -> Self {
        LimitConfig { daily_cap: i64::MAX, min_gap_ms: 0, jitter: 0.0, active_start_min: 0, active_end_min: 1440 }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Config {
    pub limits: HashMap<String, LimitConfig>,
}

#[derive(Deserialize)]
struct RawConfig {
    #[serde(default)]
    limits: HashMap<String, RawLimit>,
}
#[derive(Deserialize)]
struct RawLimit {
    #[serde(default)]
    daily_cap: Option<i64>,
    #[serde(default)]
    min_gap: Option<String>,
    #[serde(default)]
    jitter: Option<f64>,
    #[serde(default)]
    active: Option<String>,
}

fn parse_duration_ms(s: &str) -> Result<i64> {
    let s = s.trim();
    let (num, mult) = if let Some(v) = s.strip_suffix("ms") { (v, 1) }
        else if let Some(v) = s.strip_suffix('s') { (v, 1000) }
        else if let Some(v) = s.strip_suffix('m') { (v, 60_000) }
        else if let Some(v) = s.strip_suffix('h') { (v, 3_600_000) }
        else { (s, 1) };
    Ok(num.trim().parse::<i64>().map_err(|_| anyhow!("bad duration {s}"))? * mult)
}

fn parse_active(s: &str) -> Result<(i32, i32)> {
    let (a, b) = s.split_once('-').ok_or_else(|| anyhow!("bad active window {s}"))?;
    let to_min = |hm: &str| -> Result<i32> {
        let (h, m) = hm.trim().split_once(':').ok_or_else(|| anyhow!("bad time {hm}"))?;
        Ok(h.trim().parse::<i32>()? * 60 + m.trim().parse::<i32>()?)
    };
    Ok((to_min(a)?, to_min(b)?))
}

impl Config {
    pub fn from_toml(s: &str) -> Result<Config> {
        let raw: RawConfig = toml::from_str(s)?;
        let mut limits = HashMap::new();
        for (k, v) in raw.limits {
            let (astart, aend) = match v.active {
                Some(a) => parse_active(&a)?,
                None => (0, 1440),
            };
            limits.insert(k, LimitConfig {
                daily_cap: v.daily_cap.unwrap_or(i64::MAX),
                min_gap_ms: match v.min_gap { Some(g) => parse_duration_ms(&g)?, None => 0 },
                jitter: v.jitter.unwrap_or(0.0),
                active_start_min: astart,
                active_end_min: aend,
            });
        }
        Ok(Config { limits })
    }

    pub fn limit_for(&self, key: &str) -> LimitConfig {
        self.limits.get(key).cloned().unwrap_or_else(LimitConfig::permissive)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_parse_full_config() {
        let toml = r#"
[limits."dummy.capped"]
daily_cap = 3
min_gap = "8m"
jitter = 0.5
active = "09:00-18:00"
"#;
        let c = Config::from_toml(toml).unwrap();
        let l = c.limit_for("dummy.capped");
        assert_eq!(l.daily_cap, 3);
        assert_eq!(l.min_gap_ms, 8 * 60_000);
        assert_eq!(l.jitter, 0.5);
        assert_eq!(l.active_start_min, 540);
        assert_eq!(l.active_end_min, 1080);
    }
    #[test]
    fn test_unknown_key_is_permissive() {
        let c = Config::default();
        let l = c.limit_for("anything");
        assert_eq!(l, LimitConfig::permissive());
    }
    #[test]
    fn test_duration_units() {
        assert_eq!(parse_duration_ms("20s").unwrap(), 20_000);
        assert_eq!(parse_duration_ms("2h").unwrap(), 7_200_000);
        assert_eq!(parse_duration_ms("500ms").unwrap(), 500);
    }
}
```

- [ ] **Step 2: Wire module, run test**

Add to `crates/core/src/lib.rs`:
```rust
pub mod config;
```
Run: `source ~/.cargo/env && cargo test -p pacewright-core config`
Expected: PASS (3 tests).

- [ ] **Step 3: Commit**

```bash
git add -A && git commit -m "feat(core): TOML config parsing for daily caps and pacing"
```

---

### Task 8: Limits engine (cap / gap / active-hours / jitter → decision)

**Files:**
- Create: `crates/core/src/limits.rs`
- Modify: `crates/core/src/lib.rs`

**Interfaces:**
- Consumes: `config::{Config, LimitConfig}`, `clock::Clock`, `rng::Rng`, `store::Store`.
- Produces:
  - `enum LimitDecision { Allow, Defer { until_ms: i64, reason: String } }`
  - `fn local_date_str(now_ms: i64) -> String` — `YYYY-MM-DD` in local tz.
  - `fn minutes_since_local_midnight(now_ms: i64) -> i32`
  - `fn check_limits(store: &Store, cfg: &Config, clock: &dyn Clock, rng: &dyn Rng, keys: &[String]) -> rusqlite::Result<LimitDecision>` — evaluates all keys; the strictest defer wins; `Allow` only if every key passes.
  - `fn spend_limits(store: &Store, clock: &dyn Clock, keys: &[String]) -> rusqlite::Result<()>` — increments each key's counter for today.

- [ ] **Step 1: Write the failing test**

`crates/core/src/limits.rs`:
```rust
use crate::clock::Clock;
use crate::config::Config;
use crate::rng::Rng;
use crate::store::Store;
use chrono::{Local, TimeZone, Timelike};

#[derive(Debug, Clone, PartialEq)]
pub enum LimitDecision {
    Allow,
    Defer { until_ms: i64, reason: String },
}

pub fn local_date_str(now_ms: i64) -> String {
    let dt = Local.timestamp_millis_opt(now_ms).single().unwrap();
    dt.format("%Y-%m-%d").to_string()
}

pub fn minutes_since_local_midnight(now_ms: i64) -> i32 {
    let dt = Local.timestamp_millis_opt(now_ms).single().unwrap();
    (dt.hour() * 60 + dt.minute()) as i32
}

fn next_local_midnight_ms(now_ms: i64) -> i64 {
    let dt = Local.timestamp_millis_opt(now_ms).single().unwrap();
    let next = (dt + chrono::Duration::days(1)).date_naive().and_hms_opt(0, 0, 0).unwrap();
    Local.from_local_datetime(&next).single().unwrap().timestamp_millis()
}

fn local_time_at_minute_ms(now_ms: i64, minute_of_day: i32) -> i64 {
    let dt = Local.timestamp_millis_opt(now_ms).single().unwrap();
    let base = dt.date_naive().and_hms_opt((minute_of_day / 60) as u32, (minute_of_day % 60) as u32, 0).unwrap();
    Local.from_local_datetime(&base).single().unwrap().timestamp_millis()
}

pub fn check_limits(
    store: &Store, cfg: &Config, clock: &dyn Clock, rng: &dyn Rng, keys: &[String],
) -> rusqlite::Result<LimitDecision> {
    let now = clock.now_ms();
    let date = local_date_str(now);
    let now_min = minutes_since_local_midnight(now);
    let mut worst: Option<(i64, String)> = None;
    let mut consider = |until: i64, reason: String, worst: &mut Option<(i64, String)>| {
        match worst {
            Some((u, _)) if *u >= until => {}
            _ => *worst = Some((until, reason)),
        }
    };

    for key in keys {
        let lc = cfg.limit_for(key);
        let (count, last_spent) = store.counter_get(key, &date)?;

        // 1. daily cap -> defer to next local midnight
        if count >= lc.daily_cap {
            consider(next_local_midnight_ms(now), format!("over_cap:{key}"), &mut worst);
            continue;
        }
        // 2. active hours -> defer to window open (today or next day)
        if now_min < lc.active_start_min {
            consider(local_time_at_minute_ms(now, lc.active_start_min), format!("before_active:{key}"), &mut worst);
            continue;
        }
        if now_min >= lc.active_end_min {
            let open_next = local_time_at_minute_ms(next_local_midnight_ms(now), lc.active_start_min);
            consider(open_next, format!("after_active:{key}"), &mut worst);
            continue;
        }
        // 3. min gap (jittered) since last spend
        if let Some(last) = last_spent {
            let gap = rng.jitter(lc.min_gap_ms, lc.jitter);
            let earliest = last + gap;
            if now < earliest {
                consider(earliest, format!("too_soon:{key}"), &mut worst);
            }
        }
    }

    Ok(match worst {
        Some((until, reason)) => LimitDecision::Defer { until_ms: until, reason },
        None => LimitDecision::Allow,
    })
}

pub fn spend_limits(store: &Store, clock: &dyn Clock, keys: &[String]) -> rusqlite::Result<()> {
    let now = clock.now_ms();
    let date = local_date_str(now);
    for key in keys {
        store.counter_spend(key, &date, now)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::TestClock;
    use crate::config::Config;
    use crate::rng::TestRng;

    // A fixed instant: 2026-07-07 12:00 local. We compute via chrono to stay tz-independent.
    fn noon_ms() -> i64 {
        let naive = chrono::NaiveDate::from_ymd_opt(2026, 7, 7).unwrap().and_hms_opt(12, 0, 0).unwrap();
        Local.from_local_datetime(&naive).single().unwrap().timestamp_millis()
    }

    fn cfg() -> Config {
        Config::from_toml(r#"
[limits."dummy.capped"]
daily_cap = 3
min_gap = "8m"
jitter = 0.0
active = "09:00-18:00"
"#).unwrap()
    }

    #[test]
    fn test_allow_when_under_everything() {
        let store = Store::open_in_memory().unwrap();
        let clock = TestClock::new(noon_ms());
        let rng = TestRng::fixed(8 * 60_000);
        let d = check_limits(&store, &cfg(), &clock, &rng, &["dummy.capped".into()]).unwrap();
        assert_eq!(d, LimitDecision::Allow);
    }

    #[test]
    fn test_defer_over_cap_to_next_midnight() {
        let store = Store::open_in_memory().unwrap();
        let clock = TestClock::new(noon_ms());
        let date = local_date_str(noon_ms());
        for _ in 0..3 { store.counter_spend("dummy.capped", &date, noon_ms()).unwrap(); }
        let rng = TestRng::fixed(0);
        let d = check_limits(&store, &cfg(), &clock, &rng, &["dummy.capped".into()]).unwrap();
        match d {
            LimitDecision::Defer { until_ms, reason } => {
                assert!(until_ms > noon_ms());
                assert!(reason.starts_with("over_cap"));
            }
            _ => panic!("expected defer"),
        }
    }

    #[test]
    fn test_defer_too_soon_after_recent_spend() {
        let store = Store::open_in_memory().unwrap();
        let clock = TestClock::new(noon_ms());
        // last spend 2 minutes ago; gap is 8m
        store.counter_spend("dummy.capped", &local_date_str(noon_ms()), noon_ms() - 2 * 60_000).unwrap();
        let rng = TestRng::fixed(8 * 60_000);
        let d = check_limits(&store, &cfg(), &clock, &rng, &["dummy.capped".into()]).unwrap();
        match d {
            LimitDecision::Defer { until_ms, reason } => {
                assert_eq!(until_ms, noon_ms() - 2 * 60_000 + 8 * 60_000);
                assert!(reason.starts_with("too_soon"));
            }
            _ => panic!("expected defer"),
        }
    }

    #[test]
    fn test_permissive_key_always_allows() {
        let store = Store::open_in_memory().unwrap();
        let clock = TestClock::new(noon_ms());
        let rng = TestRng::fixed(0);
        // key not in config -> permissive
        let d = check_limits(&store, &Config::default(), &clock, &rng, &["unknown.key".into()]).unwrap();
        assert_eq!(d, LimitDecision::Allow);
    }
}
```

- [ ] **Step 2: Wire module, run tests**

Add to `crates/core/src/lib.rs`:
```rust
pub mod limits;
```
Run: `source ~/.cargo/env && cargo test -p pacewright-core limits`
Expected: PASS (4 tests: allow, over-cap defer, too-soon defer, permissive).

- [ ] **Step 3: Commit**

```bash
git add -A && git commit -m "feat(core): limits engine (cap/gap/active-hours/jitter -> allow|defer)"
```

---

### Task 9: Scheduler selection

**Files:**
- Create: `crates/core/src/scheduler.rs`
- Modify: `crates/core/src/lib.rs`

**Interfaces:**
- Consumes: `store::Store`, `model::{Task, TaskStatus}`, `clock::Clock`.
- Produces:
  - `fn select_runnable(store: &Store, clock: &dyn Clock) -> rusqlite::Result<Vec<Task>>` — returns tasks with `status ∈ {Pending, Deferred}`, `scheduled_for ≤ now`, `next_eligible_at` null or `≤ now`, and (if `depends_on` set) that dependency `Succeeded`; ordered by `priority` DESC then `scheduled_for` ASC.
  - `fn resolve_blocked(store: &Store, clock: &dyn Clock) -> rusqlite::Result<()>` — flips `Blocked` tasks to `Pending` when their dependency has `Succeeded` (or to `Failed` if the dependency `Failed`/`Canceled`).

- [ ] **Step 1: Write the failing test**

`crates/core/src/scheduler.rs`:
```rust
use crate::clock::Clock;
use crate::model::{Task, TaskStatus};
use crate::store::Store;

pub fn select_runnable(store: &Store, clock: &dyn Clock) -> rusqlite::Result<Vec<Task>> {
    let now = clock.now_ms();
    let mut candidates: Vec<Task> = Vec::new();
    for status in [TaskStatus::Pending, TaskStatus::Deferred] {
        for t in store.tasks_in_status(status)? {
            if t.scheduled_for > now { continue; }
            if let Some(nea) = t.next_eligible_at { if nea > now { continue; } }
            if let Some(dep) = &t.depends_on {
                match store.get_task(dep)? {
                    Some(d) if d.status == TaskStatus::Succeeded => {}
                    _ => continue,
                }
            }
            candidates.push(t);
        }
    }
    candidates.sort_by(|a, b| b.priority.cmp(&a.priority).then(a.scheduled_for.cmp(&b.scheduled_for)));
    Ok(candidates)
}

pub fn resolve_blocked(store: &Store, clock: &dyn Clock) -> rusqlite::Result<()> {
    let now = clock.now_ms();
    for mut t in store.tasks_in_status(TaskStatus::Blocked)? {
        let Some(dep) = t.depends_on.clone() else { continue };
        match store.get_task(&dep)? {
            Some(d) if d.status == TaskStatus::Succeeded => {
                t.status = TaskStatus::Pending;
                t.updated_at = now;
                store.update_task(&t)?;
            }
            Some(d) if matches!(d.status, TaskStatus::Failed | TaskStatus::Canceled) => {
                t.status = TaskStatus::Failed;
                t.last_error = Some(format!("dependency {} ended {}", dep, d.status.as_str()));
                t.finished_at = Some(now);
                t.updated_at = now;
                store.update_task(&t)?;
            }
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::TestClock;

    #[test]
    fn test_selects_due_and_orders_by_priority() {
        let store = Store::open_in_memory().unwrap();
        let clock = TestClock::new(1000);
        let mut a = Task::new_now("dummy", "echo", serde_json::json!({}), 500); a.priority = 1;
        let mut b = Task::new_now("dummy", "echo", serde_json::json!({}), 400); b.priority = 5;
        let c = Task::new_now("dummy", "echo", serde_json::json!({}), 2000); // future, excluded
        store.insert_task(&a).unwrap();
        store.insert_task(&b).unwrap();
        store.insert_task(&c).unwrap();
        let got = select_runnable(&store, &clock).unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].id, b.id); // higher priority first
    }

    #[test]
    fn test_excludes_not_yet_eligible_deferred() {
        let store = Store::open_in_memory().unwrap();
        let clock = TestClock::new(1000);
        let mut d = Task::new_now("dummy", "echo", serde_json::json!({}), 500);
        d.status = TaskStatus::Deferred;
        d.next_eligible_at = Some(5000);
        store.insert_task(&d).unwrap();
        assert!(select_runnable(&store, &clock).unwrap().is_empty());
        clock.set(5000);
        assert_eq!(select_runnable(&store, &clock).unwrap().len(), 1);
    }

    #[test]
    fn test_depends_on_gating_and_resolve() {
        let store = Store::open_in_memory().unwrap();
        let clock = TestClock::new(1000);
        let mut parent = Task::new_now("dummy", "echo", serde_json::json!({}), 100);
        let mut child = Task::new_now("dummy", "echo", serde_json::json!({}), 100);
        child.status = TaskStatus::Blocked;
        child.depends_on = Some(parent.id.clone());
        store.insert_task(&parent).unwrap();
        store.insert_task(&child).unwrap();
        // blocked child is not selected, and stays blocked while parent pending
        resolve_blocked(&store, &clock).unwrap();
        assert_eq!(store.get_task(&child.id).unwrap().unwrap().status, TaskStatus::Blocked);
        // parent succeeds -> child becomes pending -> selectable
        parent.status = TaskStatus::Succeeded;
        store.update_task(&parent).unwrap();
        resolve_blocked(&store, &clock).unwrap();
        assert_eq!(store.get_task(&child.id).unwrap().unwrap().status, TaskStatus::Pending);
        assert_eq!(select_runnable(&store, &clock).unwrap().len(), 1);
    }
}
```

- [ ] **Step 2: Wire module, run tests**

Add to `crates/core/src/lib.rs`:
```rust
pub mod scheduler;
```
Run: `source ~/.cargo/env && cargo test -p pacewright-core scheduler`
Expected: PASS (3 tests).

- [ ] **Step 3: Commit**

```bash
git add -A && git commit -m "feat(core): scheduler selection with priority, deferral, and dependency gating"
```

---

### Task 10: Runner (execute + transitions + backoff + recurrence + events)

**Files:**
- Create: `crates/core/src/runner.rs`
- Modify: `crates/core/src/lib.rs`

**Interfaces:**
- Consumes: `store::Store`, `adapter::{Adapter, RunCtx}`, `model::{Task, TaskStatus, TaskEvent, AdapterError}`, `clock::Clock`, `limits::spend_limits`.
- Produces:
  - `fn backoff_ms(attempts: i64) -> i64` — exponential: `min(1000 * 2^attempts, 3_600_000)`.
  - `async fn run_task(store: &Store, adapter: &dyn Adapter, clock: &dyn Clock, mut task: Task) -> rusqlite::Result<()>` — sets `Running` (+event), calls `adapter.execute`, then on:
    - `Ok(v)` → spend the action's limit keys, set `Succeeded` + result + finished_at (+event); if `recurrence` set, enqueue the next occurrence as a new `Pending` task.
    - `Err(Retryable)` → attempts++, if `< max_attempts` set `Pending` with `scheduled_for = now + backoff_ms` (+event `retry`), else `Failed` (+event).
    - `Err(Terminal)` → `Failed` + last_error + finished_at (+event).
    - `Err(RateLimited{retry_after})` → `Deferred` + `next_eligible_at = retry_after` (+event; attempts unchanged).
  - `fn next_occurrence_ms(cron: &str, after_ms: i64) -> Option<i64>` — parse via `croner`, next fire after `after_ms` (UTC).

- [ ] **Step 1: Write the failing test**

`crates/core/src/runner.rs`:
```rust
use crate::adapter::{Adapter, RunCtx};
use crate::clock::Clock;
use crate::limits::spend_limits;
use crate::model::{AdapterError, Task, TaskEvent, TaskStatus};
use crate::store::Store;
use croner::Cron;
use std::str::FromStr;

pub fn backoff_ms(attempts: i64) -> i64 {
    let base = 1000i64.saturating_mul(1i64 << attempts.min(20));
    base.min(3_600_000)
}

pub fn next_occurrence_ms(cron: &str, after_ms: i64) -> Option<i64> {
    let c = Cron::from_str(cron).ok()?;
    let after = chrono::Utc.timestamp_millis_opt(after_ms).single()?;
    c.find_next_occurrence(&after, false).ok().map(|dt| dt.timestamp_millis())
}

fn event(task: &Task, from: TaskStatus, to: TaskStatus, at: i64, detail: serde_json::Value) -> TaskEvent {
    TaskEvent { task_id: task.id.clone(), at, from_status: Some(from), to_status: to, detail }
}

pub async fn run_task(
    store: &Store, adapter: &dyn Adapter, clock: &dyn Clock, mut task: Task,
) -> rusqlite::Result<()> {
    let now = clock.now_ms();
    let prev = task.status;
    task.status = TaskStatus::Running;
    task.updated_at = now;
    store.update_task(&task)?;
    store.append_event(&event(&task, prev, TaskStatus::Running, now, serde_json::json!({})))?;

    let ctx = RunCtx { task_id: task.id.clone() };
    let result = adapter.execute(&ctx, &task.action, task.params.clone()).await;
    let now = clock.now_ms();

    match result {
        Ok(v) => {
            let keys = adapter.limit_keys_for(&task.action);
            spend_limits(store, clock, &keys)?;
            task.status = TaskStatus::Succeeded;
            task.result = Some(v);
            task.finished_at = Some(now);
            task.updated_at = now;
            store.update_task(&task)?;
            store.append_event(&event(&task, TaskStatus::Running, TaskStatus::Succeeded, now, serde_json::json!({"spent": keys})))?;

            if let Some(cron) = task.recurrence.clone() {
                if let Some(next_ms) = next_occurrence_ms(&cron, now) {
                    let mut nxt = Task::new_now(task.adapter.clone(), task.action.clone(), task.params.clone(), next_ms);
                    nxt.recurrence = Some(cron);
                    nxt.priority = task.priority;
                    nxt.max_attempts = task.max_attempts;
                    store.insert_task(&nxt)?;
                    store.append_event(&TaskEvent { task_id: nxt.id.clone(), at: now, from_status: None, to_status: TaskStatus::Pending, detail: serde_json::json!({"recurred_from": task.id}) })?;
                }
            }
        }
        Err(AdapterError::Retryable(msg)) => {
            task.attempts += 1;
            if task.attempts < task.max_attempts {
                let delay = backoff_ms(task.attempts);
                task.status = TaskStatus::Pending;
                task.scheduled_for = now + delay;
                task.last_error = Some(msg.clone());
                task.updated_at = now;
                store.update_task(&task)?;
                store.append_event(&event(&task, TaskStatus::Running, TaskStatus::Pending, now, serde_json::json!({"retry": true, "in_ms": delay, "error": msg})))?;
            } else {
                task.status = TaskStatus::Failed;
                task.last_error = Some(msg.clone());
                task.finished_at = Some(now);
                task.updated_at = now;
                store.update_task(&task)?;
                store.append_event(&event(&task, TaskStatus::Running, TaskStatus::Failed, now, serde_json::json!({"error": msg, "exhausted": true})))?;
            }
        }
        Err(AdapterError::Terminal(msg)) => {
            task.status = TaskStatus::Failed;
            task.last_error = Some(msg.clone());
            task.finished_at = Some(now);
            task.updated_at = now;
            store.update_task(&task)?;
            store.append_event(&event(&task, TaskStatus::Running, TaskStatus::Failed, now, serde_json::json!({"error": msg})))?;
        }
        Err(AdapterError::RateLimited { retry_after }) => {
            task.status = TaskStatus::Deferred;
            task.next_eligible_at = Some(retry_after);
            task.updated_at = now;
            store.update_task(&task)?;
            store.append_event(&event(&task, TaskStatus::Running, TaskStatus::Deferred, now, serde_json::json!({"rate_limited_until": retry_after})))?;
        }
    }
    Ok(())
}

use chrono::TimeZone;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::TestClock;
    use pacewright_adapter_dummy_stub::*; // replaced below

    // NOTE: tests use an inline stub adapter to avoid a dev-dep cycle.
    use crate::adapter::Adapter;
    use crate::model::ActionSpec;
    use async_trait::async_trait;
    use serde_json::{json, Value};
    use std::collections::HashMap;
    use std::sync::Mutex;

    #[derive(Default)]
    struct StubAdapter { flaky: Mutex<HashMap<String, u64>> }
    #[async_trait]
    impl Adapter for StubAdapter {
        fn name(&self) -> &str { "dummy" }
        fn actions(&self) -> Vec<ActionSpec> {
            vec![ActionSpec { name: "rate_heavy".into(), limit_keys: vec!["dummy.capped".into()], params_schema: Value::Null, description: "".into() }]
        }
        async fn execute(&self, ctx: &RunCtx, action: &str, params: Value) -> Result<Value, AdapterError> {
            match action {
                "echo" | "rate_heavy" => Ok(params),
                "always_fail" => Err(AdapterError::Terminal("boom".into())),
                "flaky" => {
                    let n = { let mut g = self.flaky.lock().unwrap(); let e = g.entry(ctx.task_id.clone()).or_insert(0); *e += 1; *e };
                    if n <= 2 { Err(AdapterError::Retryable(format!("try {n}"))) } else { Ok(json!({"ok": n})) }
                }
                _ => Err(AdapterError::Terminal("unknown".into())),
            }
        }
    }

    #[tokio::test]
    async fn test_success_sets_result_and_spends_limit() {
        let store = Store::open_in_memory().unwrap();
        let clock = TestClock::new(1_000);
        let a = StubAdapter::default();
        let t = Task::new_now("dummy", "rate_heavy", json!({"n":1}), 500);
        store.insert_task(&t).unwrap();
        run_task(&store, &a, &clock, t.clone()).await.unwrap();
        let got = store.get_task(&t.id).unwrap().unwrap();
        assert_eq!(got.status, TaskStatus::Succeeded);
        assert_eq!(got.result, Some(json!({"n":1})));
        let (c, _) = store.counter_get("dummy.capped", &crate::limits::local_date_str(1_000)).unwrap();
        assert_eq!(c, 1);
    }

    #[tokio::test]
    async fn test_retryable_reschedules_with_backoff_then_fails_after_max() {
        let store = Store::open_in_memory().unwrap();
        let clock = TestClock::new(1_000);
        let a = StubAdapter::default();
        let mut t = Task::new_now("dummy", "flaky", json!({}), 500);
        t.max_attempts = 2;
        store.insert_task(&t).unwrap();
        run_task(&store, &a, &clock, t.clone()).await.unwrap();
        let g1 = store.get_task(&t.id).unwrap().unwrap();
        assert_eq!(g1.status, TaskStatus::Pending);
        assert_eq!(g1.attempts, 1);
        assert_eq!(g1.scheduled_for, 1_000 + backoff_ms(1));
        // second attempt hits max_attempts -> failed
        run_task(&store, &a, &clock, g1.clone()).await.unwrap();
        let g2 = store.get_task(&t.id).unwrap().unwrap();
        assert_eq!(g2.status, TaskStatus::Failed);
    }

    #[tokio::test]
    async fn test_terminal_fails_immediately() {
        let store = Store::open_in_memory().unwrap();
        let clock = TestClock::new(1_000);
        let a = StubAdapter::default();
        let t = Task::new_now("dummy", "always_fail", json!({}), 500);
        store.insert_task(&t).unwrap();
        run_task(&store, &a, &clock, t.clone()).await.unwrap();
        assert_eq!(store.get_task(&t.id).unwrap().unwrap().status, TaskStatus::Failed);
    }

    #[tokio::test]
    async fn test_recurrence_enqueues_next() {
        let store = Store::open_in_memory().unwrap();
        let clock = TestClock::new(1_000);
        let a = StubAdapter::default();
        let mut t = Task::new_now("dummy", "echo", json!({}), 500);
        t.recurrence = Some("0 0 * * * *".into()); // top of every hour (croner 6-field)
        store.insert_task(&t).unwrap();
        run_task(&store, &a, &clock, t.clone()).await.unwrap();
        // original succeeded + one new pending recurrence
        let pend = store.tasks_in_status(TaskStatus::Pending).unwrap();
        assert_eq!(pend.len(), 1);
        assert!(pend[0].recurrence.is_some());
    }

    #[test]
    fn test_backoff_growth_and_cap() {
        assert_eq!(backoff_ms(0), 1000);
        assert_eq!(backoff_ms(1), 2000);
        assert_eq!(backoff_ms(3), 8000);
        assert_eq!(backoff_ms(40), 3_600_000);
    }
}
```

Note for the implementer: delete the bogus `use pacewright_adapter_dummy_stub::*;` line — it is a placeholder marker; the real stub is defined inline in the test module directly below it. Add `croner` and `chrono` usage is already covered by core deps.

- [ ] **Step 2: Wire module, run tests**

Add to `crates/core/src/lib.rs`:
```rust
pub mod runner;
```
Run: `source ~/.cargo/env && cargo test -p pacewright-core runner`
Expected: PASS (5 tests). Fix the placeholder `use` line if the compiler flags it.

- [ ] **Step 3: Commit**

```bash
git add -A && git commit -m "feat(core): runner with transitions, backoff, recurrence, and event logging"
```

---

### Task 11: Engine (boot recovery + tick)

**Files:**
- Create: `crates/core/src/engine.rs`
- Modify: `crates/core/src/lib.rs`

**Interfaces:**
- Consumes: everything above.
- Produces:
  - `struct Engine { store: Arc<Store>, registry: AdapterRegistry, cfg: Config, clock: Arc<dyn Clock>, rng: Arc<dyn Rng> }`
  - `Engine::new(store, registry, cfg, clock, rng)`
  - `Engine::recover_on_boot(&self)` — any `Running` task → `Pending` (+event `recovered`).
  - `async fn tick(&self)` — `resolve_blocked`; then for each `select_runnable`, run `check_limits`; if `Allow` → `run_task`; if `Defer` → set task `Deferred` + `next_eligible_at` (+event). One eligible task consumed per limit-key per tick is not required in M1 (limits are re-checked each tick); process candidates in order, re-checking limits each time.
  - `Engine::add_task(&self, task) -> rusqlite::Result<String>` — dedup guard (if `dedup_key` matches an active task, return that id), else insert (+event `created`), set `Blocked` if `depends_on` unmet.

- [ ] **Step 1: Write the failing test**

`crates/core/src/engine.rs`:
```rust
use crate::adapter::AdapterRegistry;
use crate::clock::Clock;
use crate::config::Config;
use crate::limits::{check_limits, LimitDecision};
use crate::model::{Task, TaskEvent, TaskStatus};
use crate::rng::Rng;
use crate::runner::run_task;
use crate::scheduler::{resolve_blocked, select_runnable};
use crate::store::Store;
use std::sync::Arc;

pub struct Engine {
    pub store: Arc<Store>,
    pub registry: AdapterRegistry,
    pub cfg: Config,
    pub clock: Arc<dyn Clock>,
    pub rng: Arc<dyn Rng>,
}

impl Engine {
    pub fn new(store: Arc<Store>, registry: AdapterRegistry, cfg: Config, clock: Arc<dyn Clock>, rng: Arc<dyn Rng>) -> Self {
        Engine { store, registry, cfg, clock, rng }
    }

    pub fn recover_on_boot(&self) -> rusqlite::Result<()> {
        let now = self.clock.now_ms();
        for mut t in self.store.tasks_in_status(TaskStatus::Running)? {
            let prev = t.status;
            t.status = TaskStatus::Pending;
            t.updated_at = now;
            self.store.update_task(&t)?;
            self.store.append_event(&TaskEvent { task_id: t.id.clone(), at: now, from_status: Some(prev), to_status: TaskStatus::Pending, detail: serde_json::json!({"recovered": true}) })?;
        }
        Ok(())
    }

    pub fn add_task(&self, mut task: Task) -> rusqlite::Result<String> {
        if let Some(key) = &task.dedup_key {
            if let Some(existing) = self.store.find_active_by_dedup(key)? {
                return Ok(existing.id);
            }
        }
        // gate on unmet dependency
        if let Some(dep) = &task.depends_on {
            let dep_done = matches!(self.store.get_task(dep)?, Some(d) if d.status == TaskStatus::Succeeded);
            if !dep_done { task.status = TaskStatus::Blocked; }
        }
        let created = task.status;
        self.store.insert_task(&task)?;
        self.store.append_event(&TaskEvent { task_id: task.id.clone(), at: task.created_at, from_status: None, to_status: created, detail: serde_json::json!({"created": true}) })?;
        Ok(task.id)
    }

    pub async fn tick(&self) -> rusqlite::Result<()> {
        resolve_blocked(&*self.store, &*self.clock)?;
        let runnable = select_runnable(&*self.store, &*self.clock)?;
        for task in runnable {
            let Some(adapter) = self.registry.get(&task.adapter) else {
                // unknown adapter -> fail fast
                let now = self.clock.now_ms();
                let mut t = task;
                let prev = t.status;
                t.status = TaskStatus::Failed;
                t.last_error = Some(format!("no adapter '{}'", t.adapter));
                t.finished_at = Some(now);
                t.updated_at = now;
                self.store.update_task(&t)?;
                self.store.append_event(&TaskEvent { task_id: t.id.clone(), at: now, from_status: Some(prev), to_status: TaskStatus::Failed, detail: serde_json::json!({"error":"no_adapter"}) })?;
                continue;
            };
            let keys = adapter.limit_keys_for(&task.action);
            let decision = check_limits(&*self.store, &self.cfg, &*self.clock, &*self.rng, &keys)?;
            match decision {
                LimitDecision::Allow => {
                    run_task(&*self.store, &*adapter, &*self.clock, task).await?;
                }
                LimitDecision::Defer { until_ms, reason } => {
                    let now = self.clock.now_ms();
                    let mut t = task;
                    let prev = t.status;
                    t.status = TaskStatus::Deferred;
                    t.next_eligible_at = Some(until_ms);
                    t.updated_at = now;
                    self.store.update_task(&t)?;
                    self.store.append_event(&TaskEvent { task_id: t.id.clone(), at: now, from_status: Some(prev), to_status: TaskStatus::Deferred, detail: serde_json::json!({"reason": reason, "until": until_ms}) })?;
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::TestClock;
    use crate::rng::TestRng;
    use pacewright_adapter_dummy::DummyAdapter;

    fn engine(clock: TestClock, cfg: Config) -> Engine {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let mut reg = AdapterRegistry::new();
        reg.register(Arc::new(DummyAdapter::new()));
        Engine::new(store, reg, cfg, Arc::new(clock), Arc::new(TestRng::fixed(0)))
    }

    #[tokio::test]
    async fn test_tick_runs_echo_to_success() {
        let clock = TestClock::new(1_000);
        let e = engine(clock.clone(), Config::default());
        let id = e.add_task(Task::new_now("dummy", "echo", serde_json::json!({"a":1}), 500)).unwrap();
        e.tick().await.unwrap();
        assert_eq!(e.store.get_task(&id).unwrap().unwrap().status, TaskStatus::Succeeded);
    }

    #[tokio::test]
    async fn test_recover_running_on_boot() {
        let clock = TestClock::new(1_000);
        let e = engine(clock.clone(), Config::default());
        let mut t = Task::new_now("dummy", "echo", serde_json::json!({}), 100);
        t.status = TaskStatus::Running;
        e.store.insert_task(&t).unwrap();
        e.recover_on_boot().unwrap();
        assert_eq!(e.store.get_task(&t.id).unwrap().unwrap().status, TaskStatus::Pending);
    }

    #[tokio::test]
    async fn test_dedup_returns_existing() {
        let clock = TestClock::new(1_000);
        let e = engine(clock.clone(), Config::default());
        let mut a = Task::new_now("dummy", "echo", serde_json::json!({}), 500);
        a.dedup_key = Some("k".into());
        let id1 = e.add_task(a).unwrap();
        let mut b = Task::new_now("dummy", "echo", serde_json::json!({}), 500);
        b.dedup_key = Some("k".into());
        let id2 = e.add_task(b).unwrap();
        assert_eq!(id1, id2);
    }

    #[tokio::test]
    async fn test_cap_defers_fourth_rate_heavy() {
        let clock = TestClock::new({
            // noon local on 2026-07-07 so active window 00:00-24:00 default is fine
            use chrono::TimeZone;
            let naive = chrono::NaiveDate::from_ymd_opt(2026,7,7).unwrap().and_hms_opt(12,0,0).unwrap();
            chrono::Local.from_local_datetime(&naive).single().unwrap().timestamp_millis()
        });
        let cfg = Config::from_toml("[limits.\"dummy.capped\"]\ndaily_cap = 3\nmin_gap = \"0s\"\njitter = 0.0\n").unwrap();
        let e = engine(clock.clone(), cfg);
        let mut ids = vec![];
        for _ in 0..4 {
            ids.push(e.add_task(Task::new_now("dummy", "rate_heavy", serde_json::json!({}), clock.now_ms() - 1)).unwrap());
        }
        e.tick().await.unwrap();
        let statuses: Vec<_> = ids.iter().map(|i| e.store.get_task(i).unwrap().unwrap().status).collect();
        let succeeded = statuses.iter().filter(|s| **s == TaskStatus::Succeeded).count();
        let deferred = statuses.iter().filter(|s| **s == TaskStatus::Deferred).count();
        assert_eq!(succeeded, 3);
        assert_eq!(deferred, 1);
    }
}
```

Add dev-dependency so the engine test can use the real DummyAdapter. In `crates/core/Cargo.toml` add:
```toml
[dev-dependencies]
pacewright-adapter-dummy = { path = "../adapter-dummy" }
tokio = { workspace = true }
```

- [ ] **Step 2: Wire module, run tests**

Add to `crates/core/src/lib.rs`:
```rust
pub mod engine;
```
Run: `source ~/.cargo/env && cargo test -p pacewright-core engine`
Expected: PASS (4 tests: echo→success, boot recovery, dedup, cap defers 4th).

- [ ] **Step 3: Run the whole core test suite**

Run: `source ~/.cargo/env && cargo test -p pacewright-core`
Expected: all core tests green.

- [ ] **Step 4: Commit**

```bash
git add -A && git commit -m "feat(core): engine with boot recovery, dedup, and limit-gated tick"
```

---

### Task 12: Proto wire types

**Files:**
- Modify: `crates/proto/src/lib.rs`

**Interfaces:**
- Produces (serde, `serde_json`-friendly):
  - `struct AddTaskReq { adapter, action: String, params: Value, scheduled_for: Option<i64>, recurrence: Option<String>, depends_on: Option<String>, priority: Option<i64>, dedup_key: Option<String>, max_attempts: Option<i64> }`
  - `enum Request { Add(AddTaskReq), Get{id}, List{status: Option<String>, adapter: Option<String>, limit: Option<i64>}, Cancel{id}, RunNow{id, force: bool}, Pause{scope: String}, Resume{scope: String}, Limits, Adapters, Status, Subscribe }` — serde `#[serde(tag="method", content="params", rename_all="snake_case")]`.
  - `enum Response { Ok(Value), Error{message: String} }` — serde `#[serde(tag="type", rename_all="snake_case")]`.

- [ ] **Step 1: Write the failing test**

`crates/proto/src/lib.rs`:
```rust
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AddTaskReq {
    pub adapter: String,
    pub action: String,
    pub params: Value,
    #[serde(default)] pub scheduled_for: Option<i64>,
    #[serde(default)] pub recurrence: Option<String>,
    #[serde(default)] pub depends_on: Option<String>,
    #[serde(default)] pub priority: Option<i64>,
    #[serde(default)] pub dedup_key: Option<String>,
    #[serde(default)] pub max_attempts: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "method", content = "params", rename_all = "snake_case")]
pub enum Request {
    Add(AddTaskReq),
    Get { id: String },
    List { #[serde(default)] status: Option<String>, #[serde(default)] adapter: Option<String>, #[serde(default)] limit: Option<i64> },
    Cancel { id: String },
    RunNow { id: String, #[serde(default)] force: bool },
    Pause { scope: String },
    Resume { scope: String },
    Limits,
    Adapters,
    Status,
    Subscribe,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    Ok(Value),
    Error { message: String },
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_add_request_roundtrips() {
        let req = Request::Add(AddTaskReq {
            adapter: "dummy".into(), action: "echo".into(), params: serde_json::json!({"a":1}),
            scheduled_for: None, recurrence: None, depends_on: None, priority: Some(2), dedup_key: None, max_attempts: None,
        });
        let s = serde_json::to_string(&req).unwrap();
        let back: Request = serde_json::from_str(&s).unwrap();
        assert_eq!(req, back);
    }
    #[test]
    fn test_list_tagged_shape() {
        let s = serde_json::to_string(&Request::List { status: Some("pending".into()), adapter: None, limit: Some(10) }).unwrap();
        assert!(s.contains("\"method\":\"list\""));
    }
    #[test]
    fn test_response_ok() {
        let s = serde_json::to_string(&Response::Ok(serde_json::json!({"x":1}))).unwrap();
        assert!(s.contains("\"type\":\"ok\""));
    }
}
```

- [ ] **Step 2: Run tests**

Run: `source ~/.cargo/env && cargo test -p pacewright-proto`
Expected: PASS (3 tests).

- [ ] **Step 3: Commit**

```bash
git add -A && git commit -m "feat(proto): JSON-RPC request/response wire types"
```

---

### Task 13: Daemon socket server + tick loop

**Files:**
- Create: `crates/daemon/src/server.rs`
- Modify: `crates/daemon/src/main.rs`

**Interfaces:**
- Consumes: `pacewright_core::engine::Engine`, `pacewright_proto::{Request, Response, AddTaskReq}`, `pacewright_core::model::Task`.
- Produces:
  - `async fn serve(engine: Arc<Mutex<Engine>>, socket_path: &Path) -> anyhow::Result<()>` — binds a `UnixListener` (removing a stale socket first), spawns the tick loop (every 1s: `engine.lock().tick()`), and per connection reads newline-delimited JSON `Request`s, dispatches against the engine, writes newline-delimited `Response`s.
  - `async fn handle_request(engine: &Arc<Mutex<Engine>>, req: Request) -> Response` — maps each variant to a store/engine call.
  - Single-instance guard: if binding fails with `AddrInUse`, exit with a clear error.

- [ ] **Step 1: Write the request-dispatch logic with an integration test**

`crates/daemon/src/server.rs`:
```rust
use anyhow::Result;
use pacewright_core::engine::Engine;
use pacewright_core::model::{Task, TaskStatus};
use pacewright_proto::{AddTaskReq, Request, Response};
use std::path::Path;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Mutex;

fn status_from_opt(s: &Option<String>) -> Option<TaskStatus> {
    match s.as_deref() {
        Some("pending") => Some(TaskStatus::Pending),
        Some("blocked") => Some(TaskStatus::Blocked),
        Some("deferred") => Some(TaskStatus::Deferred),
        Some("running") => Some(TaskStatus::Running),
        Some("succeeded") => Some(TaskStatus::Succeeded),
        Some("failed") => Some(TaskStatus::Failed),
        Some("canceled") => Some(TaskStatus::Canceled),
        _ => None,
    }
}

pub async fn handle_request(engine: &Arc<Mutex<Engine>>, req: Request) -> Response {
    let e = engine.lock().await;
    let now = e.clock.now_ms();
    let res: Result<serde_json::Value, String> = (|| {
        match req {
            Request::Add(a) => {
                let mut t = Task::new_now(a.adapter, a.action, a.params, a.scheduled_for.unwrap_or(now));
                t.recurrence = a.recurrence;
                t.depends_on = a.depends_on;
                t.dedup_key = a.dedup_key;
                if let Some(p) = a.priority { t.priority = p; }
                if let Some(m) = a.max_attempts { t.max_attempts = m; }
                let id = e.add_task(t).map_err(|e| e.to_string())?;
                Ok(serde_json::json!({ "id": id }))
            }
            Request::Get { id } => {
                let t = e.store.get_task(&id).map_err(|e| e.to_string())?;
                let events = match &t { Some(_) => e.store.events_for(&id).map_err(|e| e.to_string())?, None => vec![] };
                Ok(serde_json::json!({ "task": t, "events": events }))
            }
            Request::List { status, adapter, limit } => {
                let mut tasks = e.store.list_tasks(status_from_opt(&status), limit.unwrap_or(100)).map_err(|e| e.to_string())?;
                if let Some(ad) = adapter { tasks.retain(|t| t.adapter == ad); }
                Ok(serde_json::json!({ "tasks": tasks }))
            }
            Request::Cancel { id } => {
                if let Some(mut t) = e.store.get_task(&id).map_err(|e| e.to_string())? {
                    if !t.status.is_terminal() {
                        t.status = TaskStatus::Canceled;
                        t.finished_at = Some(now);
                        t.updated_at = now;
                        e.store.update_task(&t).map_err(|e| e.to_string())?;
                    }
                    Ok(serde_json::json!({ "canceled": true }))
                } else { Err(format!("no task {id}")) }
            }
            Request::RunNow { id, force: _ } => {
                if let Some(mut t) = e.store.get_task(&id).map_err(|e| e.to_string())? {
                    t.scheduled_for = now;
                    t.next_eligible_at = None;
                    if t.status == TaskStatus::Deferred { t.status = TaskStatus::Pending; }
                    t.updated_at = now;
                    e.store.update_task(&t).map_err(|e| e.to_string())?;
                    Ok(serde_json::json!({ "ok": true }))
                } else { Err(format!("no task {id}")) }
            }
            Request::Limits => Ok(serde_json::json!({ "note": "counters are per-key per-day in the store" })),
            Request::Adapters => {
                let list: Vec<_> = e.registry.all().iter().map(|a| serde_json::json!({ "name": a.name(), "actions": a.actions() })).collect();
                Ok(serde_json::json!({ "adapters": list }))
            }
            Request::Status => {
                let pending = e.store.tasks_in_status(TaskStatus::Pending).map_err(|e| e.to_string())?.len();
                let running = e.store.tasks_in_status(TaskStatus::Running).map_err(|e| e.to_string())?.len();
                Ok(serde_json::json!({ "pending": pending, "running": running }))
            }
            Request::Pause { scope } | Request::Resume { scope: _scope @ scope } => {
                // M1: pause/resume are accepted but no-op beyond acknowledging; full impl in a later task.
                Ok(serde_json::json!({ "scope": scope, "note": "acknowledged" }))
            }
            Request::Subscribe => Ok(serde_json::json!({ "note": "subscribe stream not enabled on this request path" })),
        }
    })();
    match res {
        Ok(v) => Response::Ok(v),
        Err(message) => Response::Error { message },
    }
}

async fn handle_conn(engine: Arc<Mutex<Engine>>, stream: UnixStream) {
    let (r, mut w) = stream.into_split();
    let mut lines = BufReader::new(r).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if line.trim().is_empty() { continue; }
        let resp = match serde_json::from_str::<Request>(&line) {
            Ok(req) => handle_request(&engine, req).await,
            Err(e) => Response::Error { message: format!("bad request: {e}") },
        };
        let mut out = serde_json::to_string(&resp).unwrap();
        out.push('\n');
        if w.write_all(out.as_bytes()).await.is_err() { break; }
    }
}

pub async fn serve(engine: Arc<Mutex<Engine>>, socket_path: &Path) -> Result<()> {
    let _ = std::fs::remove_file(socket_path);
    let listener = UnixListener::bind(socket_path)?;

    // tick loop
    {
        let engine = engine.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
            loop {
                interval.tick().await;
                let e = engine.lock().await;
                if let Err(err) = e.tick().await { tracing::error!("tick error: {err}"); }
            }
        });
    }

    loop {
        let (stream, _) = listener.accept().await?;
        let engine = engine.clone();
        tokio::spawn(handle_conn(engine, stream));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pacewright_core::adapter::AdapterRegistry;
    use pacewright_core::clock::SystemClock;
    use pacewright_core::config::Config;
    use pacewright_core::rng::SeededRng;
    use pacewright_core::store::Store;
    use pacewright_adapter_dummy::DummyAdapter;

    async fn test_engine() -> Arc<Mutex<Engine>> {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let mut reg = AdapterRegistry::new();
        reg.register(Arc::new(DummyAdapter::new()));
        let e = Engine::new(store, reg, Config::default(), Arc::new(SystemClock), Arc::new(SeededRng::new(1)));
        Arc::new(Mutex::new(e))
    }

    #[tokio::test]
    async fn test_add_then_get_via_dispatch() {
        let engine = test_engine().await;
        let add = Request::Add(AddTaskReq {
            adapter: "dummy".into(), action: "echo".into(), params: serde_json::json!({"a":1}),
            scheduled_for: None, recurrence: None, depends_on: None, priority: None, dedup_key: None, max_attempts: None,
        });
        let resp = handle_request(&engine, add).await;
        let id = match resp { Response::Ok(v) => v["id"].as_str().unwrap().to_string(), _ => panic!("add failed") };
        let get = handle_request(&engine, Request::Get { id: id.clone() }).await;
        match get { Response::Ok(v) => assert_eq!(v["task"]["action"], "echo"), _ => panic!("get failed") };
    }

    #[tokio::test]
    async fn test_bad_adapter_lists_adapters() {
        let engine = test_engine().await;
        let resp = handle_request(&engine, Request::Adapters).await;
        match resp { Response::Ok(v) => assert_eq!(v["adapters"][0]["name"], "dummy"), _ => panic!() };
    }
}
```

Note for the implementer: the `Request::Pause { scope } | Request::Resume { scope: _scope @ scope }` or-pattern binding is illustrative — if it does not compile cleanly, split into two separate match arms that each return the same acknowledgment JSON. Keep behavior identical.

- [ ] **Step 2: Wire `main.rs` to build the engine and serve**

`crates/daemon/src/main.rs`:
```rust
mod server;

use anyhow::Result;
use pacewright_core::adapter::AdapterRegistry;
use pacewright_core::clock::SystemClock;
use pacewright_core::config::Config;
use pacewright_core::engine::Engine;
use pacewright_core::rng::SeededRng;
use pacewright_core::store::Store;
use pacewright_adapter_dummy::DummyAdapter;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

fn home_dir() -> PathBuf { PathBuf::from(std::env::var("HOME").unwrap()) }

fn pw_dir() -> PathBuf {
    let d = home_dir().join(".pacewright");
    std::fs::create_dir_all(&d).ok();
    d
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let dir = pw_dir();
    let db_path = dir.join("pacewright.db");
    let sock_path = dir.join("pw.sock");
    let cfg_path = dir.join("config.toml");

    let cfg = match std::fs::read_to_string(&cfg_path) {
        Ok(s) => Config::from_toml(&s).unwrap_or_default(),
        Err(_) => Config::default(),
    };

    let store = Arc::new(Store::open(db_path.to_str().unwrap())?);
    let mut reg = AdapterRegistry::new();
    reg.register(Arc::new(DummyAdapter::new()));
    // M2+: register acme/globex adapters here.

    let engine = Engine::new(store, reg, cfg, Arc::new(SystemClock), Arc::new(SeededRng::new(rand_seed())));
    engine.recover_on_boot()?;
    let engine = Arc::new(Mutex::new(engine));

    tracing::info!("pacewrightd listening on {}", sock_path.display());
    server::serve(engine, &sock_path).await
}

// A boot-time seed derived from the pid + start; randomness only affects pacing jitter.
fn rand_seed() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0) ^ (std::process::id() as u64)
}
```

- [ ] **Step 3: Run tests + build the daemon binary**

Run: `source ~/.cargo/env && cargo test -p pacewright-daemon`
Expected: PASS (2 dispatch tests).
Run: `source ~/.cargo/env && cargo build -p pacewright-daemon`
Expected: `pacewrightd` builds.

- [ ] **Step 4: Manual smoke test**

Run the daemon in the background and hit the socket with `nc`:
```bash
source ~/.cargo/env
./target/debug/pacewrightd &   # writes ~/.pacewright/pw.sock
sleep 1
printf '{"method":"add","params":{"adapter":"dummy","action":"echo","params":{"a":1}}}\n' | nc -U ~/.pacewright/pw.sock
sleep 2
printf '{"method":"list","params":{"status":"succeeded"}}\n' | nc -U ~/.pacewright/pw.sock
kill %1
```
Expected: first call returns `{"type":"ok","...":{"id":"..."}}`; after ~1–2s the task shows as `succeeded` in the list.

- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "feat(daemon): unix-socket JSON-RPC server + 1s tick loop"
```

---

### Task 14: CLI client + subcommands

**Files:**
- Create: `crates/cli/src/client.rs`
- Modify: `crates/cli/src/main.rs`

**Interfaces:**
- Consumes: `pacewright_proto::{Request, Response, AddTaskReq}`.
- Produces:
  - `async fn call(sock: &Path, req: Request) -> anyhow::Result<Response>` — connect, write one JSON line, read one JSON line.
  - clap subcommands: `add <adapter> <action> [--params JSON] [--at MS] [--every CRON] [--depends-on ID] [--priority N] [--dedup KEY]`, `list [--status S]`, `get <id>`, `cancel <id>`, `run-now <id> [--force]`, `pause <scope>`, `resume <scope>`, `limits`, `adapters`, `status`, `tui`.

- [ ] **Step 1: Write the client with a unit test for request construction**

`crates/cli/src/client.rs`:
```rust
use anyhow::Result;
use pacewright_proto::{Request, Response};
use std::path::Path;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

pub async fn call(sock: &Path, req: Request) -> Result<Response> {
    let stream = UnixStream::connect(sock).await?;
    let (r, mut w) = stream.into_split();
    let mut line = serde_json::to_string(&req)?;
    line.push('\n');
    w.write_all(line.as_bytes()).await?;
    let mut reader = BufReader::new(r).lines();
    let resp_line = reader.next_line().await?.ok_or_else(|| anyhow::anyhow!("no response"))?;
    Ok(serde_json::from_str(&resp_line)?)
}

#[cfg(test)]
mod tests {
    use pacewright_proto::{AddTaskReq, Request};
    #[test]
    fn test_build_add_request() {
        let req = Request::Add(AddTaskReq {
            adapter: "dummy".into(), action: "echo".into(), params: serde_json::json!({}),
            scheduled_for: None, recurrence: None, depends_on: None, priority: None, dedup_key: None, max_attempts: None,
        });
        let s = serde_json::to_string(&req).unwrap();
        assert!(s.contains("\"method\":\"add\""));
    }
}
```

- [ ] **Step 2: Write the clap CLI**

`crates/cli/src/main.rs`:
```rust
mod client;
mod tui;

use anyhow::Result;
use clap::{Parser, Subcommand};
use pacewright_proto::{AddTaskReq, Request};
use std::path::PathBuf;

fn sock_path() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap()).join(".pacewright").join("pw.sock")
}

#[derive(Parser)]
#[command(name = "pacewright", bin_name = "pacewright")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    Add {
        adapter: String,
        action: String,
        #[arg(long, default_value = "{}")] params: String,
        #[arg(long)] at: Option<i64>,
        #[arg(long)] every: Option<String>,
        #[arg(long)] depends_on: Option<String>,
        #[arg(long)] priority: Option<i64>,
        #[arg(long)] dedup: Option<String>,
    },
    List { #[arg(long)] status: Option<String> },
    Get { id: String },
    Cancel { id: String },
    RunNow { id: String, #[arg(long)] force: bool },
    Pause { scope: String },
    Resume { scope: String },
    Limits,
    Adapters,
    Status,
    Tui,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let sock = sock_path();
    let req = match cli.cmd {
        Cmd::Add { adapter, action, params, at, every, depends_on, priority, dedup } => {
            Request::Add(AddTaskReq {
                adapter, action, params: serde_json::from_str(&params)?,
                scheduled_for: at, recurrence: every, depends_on, priority, dedup_key: dedup, max_attempts: None,
            })
        }
        Cmd::List { status } => Request::List { status, adapter: None, limit: Some(100) },
        Cmd::Get { id } => Request::Get { id },
        Cmd::Cancel { id } => Request::Cancel { id },
        Cmd::RunNow { id, force } => Request::RunNow { id, force },
        Cmd::Pause { scope } => Request::Pause { scope },
        Cmd::Resume { scope } => Request::Resume { scope },
        Cmd::Limits => Request::Limits,
        Cmd::Adapters => Request::Adapters,
        Cmd::Status => Request::Status,
        Cmd::Tui => { return tui::run(&sock).await; }
    };
    let resp = client::call(&sock, req).await?;
    println!("{}", serde_json::to_string_pretty(&resp)?);
    Ok(())
}
```

- [ ] **Step 3: Provide a minimal TUI stub so the module compiles (fleshed out in Task 15)**

`crates/cli/src/tui.rs`:
```rust
use anyhow::Result;
use std::path::Path;

pub async fn run(_sock: &Path) -> Result<()> {
    println!("tui: implemented in Task 15");
    Ok(())
}
```

- [ ] **Step 4: Run tests + build**

Run: `source ~/.cargo/env && cargo test -p pacewright-cli && cargo build -p pacewright-cli`
Expected: PASS + `pacewright` binary builds.

- [ ] **Step 5: End-to-end smoke test against the running daemon**

```bash
source ~/.cargo/env
./target/debug/pacewrightd &
sleep 1
./target/debug/pacewright add dummy echo --params '{"hello":"world"}'
sleep 2
./target/debug/pacewright list --status succeeded
./target/debug/pacewright adapters
kill %1
```
Expected: `add` prints an id; `list` shows the task `succeeded`; `adapters` lists `dummy` with its 5 actions.

- [ ] **Step 6: Commit**

```bash
git add -A && git commit -m "feat(cli): pacewright client + subcommands over the socket"
```

---

### Task 15: Minimal ratatui TUI dashboard

**Files:**
- Modify: `crates/cli/src/tui.rs`

**Interfaces:**
- Consumes: `client::call`, `pacewright_proto::{Request, Response}`.
- Produces: `async fn run(sock: &Path) -> Result<()>` — every 1s calls `Request::List{status:None}` and `Request::Status`, renders a table (id short, adapter, action, status, attempts) + a header line (pending/running counts). `q` quits.

- [ ] **Step 1: Implement the TUI (smoke-tested manually — ratatui rendering is not unit-tested)**

`crates/cli/src/tui.rs`:
```rust
use anyhow::Result;
use crossterm::event::{self, Event, KeyCode};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::execute;
use pacewright_proto::{Request, Response};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Row, Table, Cell};
use std::io::stdout;
use std::path::Path;
use std::time::{Duration, Instant};

use crate::client::call;

pub async fn run(sock: &Path) -> Result<()> {
    enable_raw_mode()?;
    execute!(stdout(), EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout()))?;
    let mut last = Instant::now() - Duration::from_secs(2);
    let mut tasks_json = serde_json::json!({"tasks": []});
    let mut status_json = serde_json::json!({"pending":0,"running":0});

    loop {
        if last.elapsed() >= Duration::from_secs(1) {
            if let Ok(Response::Ok(v)) = call(sock, Request::List { status: None, adapter: None, limit: Some(50) }).await { tasks_json = v; }
            if let Ok(Response::Ok(v)) = call(sock, Request::Status).await { status_json = v; }
            last = Instant::now();
        }

        terminal.draw(|f| {
            let area = f.area();
            let header = format!(" pacewright — pending {} · running {}   (q to quit) ",
                status_json["pending"], status_json["running"]);
            let rows: Vec<Row> = tasks_json["tasks"].as_array().cloned().unwrap_or_default().iter().map(|t| {
                let id = t["id"].as_str().unwrap_or("");
                Row::new(vec![
                    Cell::from(id.chars().take(8).collect::<String>()),
                    Cell::from(t["adapter"].as_str().unwrap_or("").to_string()),
                    Cell::from(t["action"].as_str().unwrap_or("").to_string()),
                    Cell::from(t["status"].as_str().unwrap_or("").to_string()),
                    Cell::from(t["attempts"].to_string()),
                ])
            }).collect();
            let table = Table::new(rows, [Constraint::Length(10), Constraint::Length(14), Constraint::Length(16), Constraint::Length(12), Constraint::Length(6)])
                .header(Row::new(vec!["id","adapter","action","status","try"]).style(Style::new().bold()))
                .block(Block::default().borders(Borders::ALL).title(header));
            f.render_widget(table, area);
        })?;

        if event::poll(Duration::from_millis(200))? {
            if let Event::Key(k) = event::read()? {
                if k.code == KeyCode::Char('q') { break; }
            }
        }
    }

    disable_raw_mode()?;
    execute!(stdout(), LeaveAlternateScreen)?;
    Ok(())
}
```

- [ ] **Step 2: Build + manual smoke test**

Run: `source ~/.cargo/env && cargo build -p pacewright-cli`
Then with the daemon running and a couple of tasks added, run `./target/debug/pacewright tui`, confirm the table renders and updates, press `q` to exit.
Expected: dashboard shows tasks and pending/running counts; `q` quits cleanly.

- [ ] **Step 3: Commit**

```bash
git add -A && git commit -m "feat(cli): minimal ratatui dashboard (pcw tui)"
```

---

### Task 16: launchd packaging + config example

**Files:**
- Create: `packaging/com.paperclip.pacewrightd.plist`
- Create: `packaging/config.example.toml`
- Create: `README.md`

**Interfaces:**
- Produces: a launchd plist that runs `pacewrightd` at login and keeps it alive; an example config; install instructions.

- [ ] **Step 1: Write the launchd plist**

`packaging/com.paperclip.pacewrightd.plist` (implementer replaces `USER` with the real home path at install, or uses the sed line below):
```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>com.paperclip.pacewrightd</string>
    <key>ProgramArguments</key>
    <array>
        <string>__HOME__/work/pacewright/target/release/pacewrightd</string>
    </array>
    <key>RunAtLoad</key><true/>
    <key>KeepAlive</key><true/>
    <key>StandardOutPath</key><string>__HOME__/.pacewright/pacewrightd.out.log</string>
    <key>StandardErrorPath</key><string>__HOME__/.pacewright/pacewrightd.err.log</string>
    <key>ProcessType</key><string>Background</string>
</dict>
</plist>
```

`packaging/config.example.toml`:
```toml
# ~/.pacewright/config.toml — copy and edit.
# Every (platform, action) that needs throttling gets a limit key.
# Keys not listed here are unrestricted.

[limits."dummy.capped"]
daily_cap = 3
min_gap   = "8m"
jitter    = 0.5
active    = "09:00-18:00"

# Real keys arrive with their adapters (M3+):
# [limits."acme.post"]           daily_cap = 3   min_gap = "45m"  jitter = 0.5  active = "09:00-18:00"
# [limits."acme.comment"]        daily_cap = 15  min_gap = "8m"   jitter = 0.5  active = "09:00-18:00"
# [limits."acme.profile_scrape"] daily_cap = 40  min_gap = "20s"  jitter = 0.3
# [limits."globex.upload"]       daily_cap = 5   min_gap = "30m"
```

- [ ] **Step 2: Write install instructions in the README**

`README.md`:
```markdown
# pacewright

Queues, schedules (daily caps + human pacing), runs, and tracks browser-automation
tasks. M1 is the engine + a DummyAdapter (no browser). See `docs/specs/` and `docs/plans/`.

## Build
```
source ~/.cargo/env
cargo build --release
```

## Run the daemon
```
./target/release/pacewrightd          # creates ~/.pacewright/{pw.sock,pacewright.db}
```

## Use the CLI
```
./target/release/pacewright add dummy echo --params '{"hi":1}'
./target/release/pacewright list --status succeeded
./target/release/pacewright tui
```

## Install as a launchd service
```
cp packaging/config.example.toml ~/.pacewright/config.toml   # edit limits
sed "s#__HOME__#$HOME#g" packaging/com.paperclip.pacewrightd.plist > ~/Library/LaunchAgents/com.paperclip.pacewrightd.plist
launchctl load ~/Library/LaunchAgents/com.paperclip.pacewrightd.plist
```
```

- [ ] **Step 3: Verify the release build**

Run: `source ~/.cargo/env && cargo build --release`
Expected: `target/release/pacewrightd` and `target/release/pacewright` exist.

- [ ] **Step 4: Commit**

```bash
git add -A && git commit -m "chore: launchd plist, example config, and README"
```

---

### Task 17: End-to-end integration test (daemon + DummyAdapter over a real socket)

**Files:**
- Create: `crates/daemon/tests/e2e.rs`

**Interfaces:**
- Consumes: the `pacewrightd` binary behavior via a spawned in-process server on a temp socket, driven by a `pacewright_proto` client.

This task proves the spec's acceptance scenarios against a real socket, not just in-process dispatch.

- [ ] **Step 1: Write the integration test**

`crates/daemon/tests/e2e.rs`:
```rust
use pacewright_core::adapter::AdapterRegistry;
use pacewright_core::clock::SystemClock;
use pacewright_core::config::Config;
use pacewright_core::engine::Engine;
use pacewright_core::rng::SeededRng;
use pacewright_core::store::Store;
use pacewright_adapter_dummy::DummyAdapter;
use pacewright_proto::{AddTaskReq, Request, Response};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::Mutex;

// Re-declare the server path by depending on the daemon lib. To allow this,
// the daemon exposes `server` as a lib module (see step 2).
use pacewright_daemon::server::serve;

async fn client_call(sock: &std::path::Path, req: Request) -> Response {
    let stream = UnixStream::connect(sock).await.unwrap();
    let (r, mut w) = stream.into_split();
    let mut line = serde_json::to_string(&req).unwrap();
    line.push('\n');
    w.write_all(line.as_bytes()).await.unwrap();
    let mut lines = BufReader::new(r).lines();
    let l = lines.next_line().await.unwrap().unwrap();
    serde_json::from_str(&l).unwrap()
}

#[tokio::test]
async fn test_e2e_echo_runs_to_success() {
    let dir = std::env::temp_dir().join(format!("pw-e2e-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let sock = dir.join("pw.sock");

    let store = Arc::new(Store::open_in_memory().unwrap());
    let mut reg = AdapterRegistry::new();
    reg.register(Arc::new(DummyAdapter::new()));
    let engine = Arc::new(Mutex::new(Engine::new(store, reg, Config::default(), Arc::new(SystemClock), Arc::new(SeededRng::new(1)))));

    let sock2 = sock.clone();
    tokio::spawn(async move { serve(engine, &sock2).await.unwrap(); });
    // wait for socket
    for _ in 0..50 { if sock.exists() { break; } tokio::time::sleep(Duration::from_millis(20)).await; }

    let resp = client_call(&sock, Request::Add(AddTaskReq {
        adapter: "dummy".into(), action: "echo".into(), params: serde_json::json!({"a":1}),
        scheduled_for: None, recurrence: None, depends_on: None, priority: None, dedup_key: None, max_attempts: None,
    })).await;
    let id = match resp { Response::Ok(v) => v["id"].as_str().unwrap().to_string(), _ => panic!("add failed") };

    // tick runs every 1s; wait up to 3s
    let mut ok = false;
    for _ in 0..30 {
        tokio::time::sleep(Duration::from_millis(200)).await;
        if let Response::Ok(v) = client_call(&sock, Request::Get { id: id.clone() }).await {
            if v["task"]["status"] == "succeeded" { ok = true; break; }
        }
    }
    assert!(ok, "task did not reach succeeded");
    std::fs::remove_dir_all(&dir).ok();
}
```

- [ ] **Step 2: Expose the daemon `server` module as a lib target**

Add a lib target to the daemon so the integration test can import `serve`. Modify `crates/daemon/Cargo.toml`:
```toml
[lib]
name = "pacewright_daemon"
path = "src/lib.rs"
```
Create `crates/daemon/src/lib.rs`:
```rust
pub mod server;
```
Modify `crates/daemon/src/main.rs` to use the lib module instead of a local `mod server;`:
```rust
use pacewright_daemon::server;
```
(remove the top `mod server;` line in `main.rs`).

- [ ] **Step 3: Run the integration test**

Run: `source ~/.cargo/env && cargo test -p pacewright-daemon --test e2e`
Expected: PASS (`test_e2e_echo_runs_to_success`) within ~3s.

- [ ] **Step 4: Full workspace test run**

Run: `source ~/.cargo/env && cargo test`
Expected: every crate's tests pass.

- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "test(daemon): end-to-end echo-to-success over a real unix socket"
```

---

## Self-Review

**Spec coverage check (spec §→task):**
- §2 architecture / crate layout → Task 1.
- §3 data model (tasks/task_events/limit_counters) → Tasks 3, 4.
- §4 lifecycle + error classes → Tasks 3 (types), 10 (transitions), 11 (deferral).
- §5 scheduler + cap/gap/active-hours/jitter + determinism (Clock/Rng) → Tasks 2, 7, 8, 9, 11.
- §6 Adapter trait + DummyAdapter → Tasks 5, 6.
- §7 socket API (all 12 methods) → Tasks 12 (types), 13 (dispatch). *Note: `subscribe` is stubbed in M1 (the TUI polls via `list`+`status` in Task 15 rather than a push stream); this is called out in Task 13 and matches the "minimal TUI" scope — full push `subscribe` deferred within M1 as non-essential.*
- §8 resilience: crash recovery → Task 11; single-instance/stale-socket → Task 13; adapter-panic mapping → see note below.
- §9 browser/anti-detection → out of M1 scope by design (documented, not built).
- §10 testing strategy → Tasks 2–17 (unit) + Task 17 (e2e).
- §11 milestones → this plan is M1.

**Gaps found and resolved inline:**
- *Adapter panic isolation (§8):* not yet caught in Task 10's `run_task`. Add to Task 10 a follow-up when real adapters land (M3); for M1 the DummyAdapter never panics, so it is not blocking. Recorded here rather than adding a placeholder task.
- *`pause`/`resume` (§7):* accepted as acknowledged no-ops in M1 (Task 13) — full scoped pause is a small follow-up once real adapters exist and there is something to pause. Flagged in Task 13.

**Placeholder scan:** no "TBD/TODO/implement later" in task steps. The one intentional placeholder marker (`use pacewright_adapter_dummy_stub::*;` in Task 10) is explicitly called out with removal instructions.

**Type consistency:** `TaskStatus`, `Task`, `AdapterError`, `ActionSpec`, `Adapter::execute`, `RunCtx`, `Store::*`, `check_limits`/`spend_limits`, `select_runnable`/`resolve_blocked`, `run_task`, `Engine::{new,recover_on_boot,add_task,tick}`, and `Request`/`Response` names are used identically across tasks. Wire time unit is epoch-ms `i64` everywhere.

**Scope:** M1 only — engine + DummyAdapter + CLI + TUI + launchd, no browser. Sized for one implementation pass.

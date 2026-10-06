use crate::adapter::Adapter;
use crate::adapter::AdapterRegistry;
use crate::browser::{BrowserHandle, NullBrowser};
use crate::clock::Clock;
use crate::config::Config;
use crate::limits::{check_limits, LimitDecision};
use crate::model::{Task, TaskEvent, TaskStatus};

/// A task asks for the **background lane** with `"background": true` in its params: it runs beside
/// the serial queue instead of in it, one background task at a time. Meant for long model rounds
/// (a recipe-authoring `claude_cli/run` can take 40 minutes) that must not hold up short, user-
/// facing tasks behind them. Everything else keeps the original one-at-a-time order.
pub fn wants_background_lane(task: &Task) -> bool {
    task.params
        .get("background")
        .and_then(serde_json::Value::as_bool)
        == Some(true)
}
use crate::rng::Rng;
use crate::runner::{execute_and_record, mark_running};
use crate::scheduler::{resolve_blocked, select_runnable};
use crate::store::Store;
use std::collections::HashSet;
use std::sync::Arc;

pub struct Engine {
    pub store: Arc<Store>,
    pub registry: AdapterRegistry,
    pub cfg: Config,
    pub clock: Arc<dyn Clock>,
    pub rng: Arc<dyn Rng>,
    /// Shared with every task the runner executes. Defaults to `NullBrowser`;
    /// the daemon swaps in a real handle via `with_browser`.
    pub browser: Arc<dyn BrowserHandle>,
    /// Called by the runner when a task fails terminally / auto-pauses its scope. Defaults to the
    /// no-op [`NullNotifier`]; the daemon swaps in one that writes an escalation outbox + optionally
    /// shells `claude -p`.
    pub notifier: Arc<dyn crate::notify::Notifier>,
}

impl Engine {
    pub fn new(
        store: Arc<Store>,
        registry: AdapterRegistry,
        cfg: Config,
        clock: Arc<dyn Clock>,
        rng: Arc<dyn Rng>,
    ) -> Self {
        Engine {
            store,
            registry,
            cfg,
            clock,
            rng,
            browser: Arc::new(NullBrowser),
            notifier: crate::notify::null_notifier(),
        }
    }

    /// Attach the escalation notifier the runner calls on terminal failures / scope-pauses. Kept
    /// out of `new` (like `with_browser`) so browser-free callers and the test suite default to the
    /// no-op [`NullNotifier`].
    pub fn with_notifier(mut self, notifier: Arc<dyn crate::notify::Notifier>) -> Self {
        self.notifier = notifier;
        self
    }

    /// Attach the browser every browser-driving adapter will receive in its `RunCtx`.
    /// Kept out of `new` so browser-free callers (and the whole M1 test suite) stay
    /// untouched, and so a daemon with no browser configured still boots.
    pub fn with_browser(mut self, browser: Arc<dyn BrowserHandle>) -> Self {
        self.browser = browser;
        self
    }

    /// Pause the given scope: `"all"` (or the alias `"daemon"`) pauses the whole engine; any other
    /// string is an adapter name. Persisted to the store so a daemon restart RE-ASSERTS it rather
    /// than silently resuming.
    pub fn pause(&self, scope: String) -> rusqlite::Result<()> {
        let scope = if scope == "daemon" {
            "all".to_string()
        } else {
            scope
        };
        self.store.pause_scope(&scope)
    }

    /// Resume the given scope. Same scope semantics as `pause`; removes the persisted row.
    pub fn resume(&self, scope: &str) -> rusqlite::Result<()> {
        let scope = if scope == "daemon" { "all" } else { scope };
        self.store.resume_scope(scope)
    }

    pub fn is_paused_all(&self) -> bool {
        self.paused_scopes().iter().any(|s| s == "all")
    }

    pub fn is_adapter_paused(&self, adapter: &str) -> bool {
        self.paused_scopes().iter().any(|s| s == adapter)
    }

    /// Every currently-paused scope (store-backed). Empty on a read error.
    pub fn paused_scopes(&self) -> Vec<String> {
        self.store.paused_scopes().unwrap_or_default()
    }

    pub fn recover_on_boot(&self) -> rusqlite::Result<()> {
        let now = self.clock.now_ms();
        for mut t in self.store.tasks_in_status(TaskStatus::Running)? {
            let prev = t.status;
            t.status = TaskStatus::Pending;
            t.updated_at = now;
            self.store.update_task(&t)?;
            self.store.append_event(&TaskEvent {
                task_id: t.id.clone(),
                at: now,
                from_status: Some(prev),
                to_status: TaskStatus::Pending,
                detail: serde_json::json!({"recovered": true}),
            })?;
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
            let dep_done =
                matches!(self.store.get_task(dep)?, Some(d) if d.status == TaskStatus::Succeeded);
            if !dep_done {
                task.status = TaskStatus::Blocked;
            }
        }
        let created = task.status;
        self.store.insert_task(&task)?;
        self.store.append_event(&TaskEvent {
            task_id: task.id.clone(),
            at: task.created_at,
            from_status: None,
            to_status: created,
            detail: serde_json::json!({"created": true}),
        })?;
        Ok(task.id)
    }

    /// Claim the next runnable task: resolve blocked deps, pick a pending task that passes
    /// adapter-pause + limit checks, mark it `Running`, and return it with its adapter. Fast — a few
    /// store reads/writes, no adapter execution. Fail-fast (unknown adapter) and limit-`Defer` are
    /// handled here as side effects; those tasks are skipped and the search continues.
    ///
    /// The daemon calls this under the engine lock, then releases the lock and runs
    /// `execute_and_record` on the claim — so the slow browser subprocess never holds the lock.
    /// Returns `None` when nothing is runnable this pass.
    pub fn claim_one(&self) -> rusqlite::Result<Option<Claimed>> {
        self.claim_one_where(&|_| false)
    }

    /// [`Engine::claim_one`], passing over every runnable task for which `skip` is true (it stays
    /// pending, untouched). The daemon uses it to keep a second background-lane task from being
    /// claimed while one is already running.
    pub fn claim_one_where(
        &self,
        skip: &dyn Fn(&Task) -> bool,
    ) -> rusqlite::Result<Option<Claimed>> {
        // Snapshot the persisted pauses once (source of truth: survives restart, and the off-lock
        // runner can auto-pause a scope on failure). `all` halts the whole engine.
        let paused: HashSet<String> = self.store.paused_scopes()?.into_iter().collect();
        if paused.contains("all") {
            return Ok(None);
        }
        resolve_blocked(&self.store, &*self.clock, &*self.rng)?;
        let runnable = select_runnable(&self.store, &*self.clock)?;
        for task in runnable {
            if paused.contains(&task.adapter) || skip(&task) {
                continue;
            }
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
                self.store.append_event(&TaskEvent {
                    task_id: t.id.clone(),
                    at: now,
                    from_status: Some(prev),
                    to_status: TaskStatus::Failed,
                    detail: serde_json::json!({"error":"no_adapter"}),
                })?;
                continue;
            };
            let keys = adapter.limit_keys_for_task(&task.action, &task.params);
            let decision = check_limits(&self.store, &self.cfg, &*self.clock, &*self.rng, &keys)?;
            match decision {
                LimitDecision::Allow => {
                    let task = mark_running(&self.store, &*self.clock, task)?;
                    return Ok(Some(Claimed { task, adapter }));
                }
                LimitDecision::Defer { until_ms, reason } => {
                    let now = self.clock.now_ms();
                    let mut t = task;
                    let prev = t.status;
                    t.status = TaskStatus::Deferred;
                    t.next_eligible_at = Some(until_ms);
                    t.updated_at = now;
                    self.store.update_task(&t)?;
                    self.store.append_event(&TaskEvent {
                        task_id: t.id.clone(),
                        at: now,
                        from_status: Some(prev),
                        to_status: TaskStatus::Deferred,
                        detail: serde_json::json!({"reason": reason, "until": until_ms}),
                    })?;
                }
            }
        }
        Ok(None)
    }

    /// Drain every runnable task, executing each to completion. Used by in-process callers and tests;
    /// the daemon instead loops `claim_one` + `execute_and_record` so the execute runs off the lock.
    pub async fn tick(&self) -> rusqlite::Result<()> {
        while let Some(Claimed { task, adapter }) = self.claim_one()? {
            execute_and_record(
                &self.store,
                &*adapter,
                &*self.clock,
                self.browser.clone(),
                self.notifier.clone(),
                task,
            )
            .await?;
        }
        Ok(())
    }
}

/// A task claimed by [`Engine::claim_one`]: already marked `Running`, paired with its adapter, ready
/// to hand to `execute_and_record` off the engine lock.
pub struct Claimed {
    pub task: Task,
    pub adapter: Arc<dyn Adapter>,
}

// NOTE: the engine tests below use the real `pacewright-adapter-dummy` crate,
// which itself depends on `pacewright-core` (see crates/adapter-dummy/Cargo.toml).
// That makes it a dev-dependency cycle: if these tests lived in a `#[cfg(test)]`
// module here, Cargo would compile pacewright-core twice for `cargo test` (once
// as the `--test` unit under test, once as the plain library that adapter-dummy
// depends on), producing two incompatible instances of the `Adapter` trait and a
// "multiple different versions of crate `pacewright_core`" compile error (the
// same cycle `runner.rs` sidesteps with an inline stub adapter). Integration
// tests under `tests/` link against the single normal library build instead, so
// the cycle resolves cleanly — see `crates/core/tests/engine.rs`.

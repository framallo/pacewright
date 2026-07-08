use crate::adapter::AdapterRegistry;
use crate::clock::Clock;
use crate::config::Config;
use crate::limits::{check_limits, LimitDecision};
use crate::model::{Task, TaskEvent, TaskStatus};
use crate::rng::Rng;
use crate::runner::run_task;
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
    paused: HashSet<String>,
}

impl Engine {
    pub fn new(store: Arc<Store>, registry: AdapterRegistry, cfg: Config, clock: Arc<dyn Clock>, rng: Arc<dyn Rng>) -> Self {
        Engine { store, registry, cfg, clock, rng, paused: HashSet::new() }
    }

    /// Pause the given scope: `"all"` (or the alias `"daemon"`) pauses the
    /// whole engine; any other string is treated as an adapter name and
    /// pauses only tasks routed to that adapter.
    pub fn pause(&mut self, scope: String) {
        let scope = if scope == "daemon" { "all".to_string() } else { scope };
        self.paused.insert(scope);
    }

    /// Resume the given scope. Same scope semantics as `pause`.
    pub fn resume(&mut self, scope: &str) {
        let scope = if scope == "daemon" { "all" } else { scope };
        self.paused.remove(scope);
    }

    pub fn is_paused_all(&self) -> bool {
        self.paused.contains("all")
    }

    pub fn is_adapter_paused(&self, adapter: &str) -> bool {
        self.paused.contains(adapter)
    }

    pub fn paused_scopes(&self) -> Vec<String> {
        self.paused.iter().cloned().collect()
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
        if self.is_paused_all() {
            return Ok(());
        }
        resolve_blocked(&self.store, &*self.clock)?;
        let runnable = select_runnable(&self.store, &*self.clock)?;
        for task in runnable {
            if self.is_adapter_paused(&task.adapter) {
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
                self.store.append_event(&TaskEvent { task_id: t.id.clone(), at: now, from_status: Some(prev), to_status: TaskStatus::Failed, detail: serde_json::json!({"error":"no_adapter"}) })?;
                continue;
            };
            let keys = adapter.limit_keys_for(&task.action);
            let decision = check_limits(&self.store, &self.cfg, &*self.clock, &*self.rng, &keys)?;
            match decision {
                LimitDecision::Allow => {
                    run_task(&self.store, &*adapter, &*self.clock, task).await?;
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

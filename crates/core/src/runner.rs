use crate::adapter::{Adapter, RunCtx};
use crate::browser::BrowserHandle;
use crate::clock::Clock;
use crate::limits::spend_limits;
use crate::model::{AdapterError, Task, TaskEvent, TaskStatus};
use crate::notify::{EscalationEvent, EscalationKind, Notifier};
use crate::store::Store;
use croner::Cron;
use futures_util::FutureExt;
use rand::Rng;
use std::any::Any;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;

pub fn backoff_ms(attempts: i64) -> i64 {
    let base = 1000i64.saturating_mul(1i64 << attempts.min(20));
    base.min(3_600_000)
}

pub fn next_occurrence_ms(cron: &str, after_ms: i64) -> Option<i64> {
    // A recurrence string may carry an optional `|jitter=<ms>` suffix (encoded by the
    // scheduler so nothing in the schema/Task struct changes). Split it off: the left part
    // is the real croner pattern; the right part, if it parses, is a +/- spread in ms.
    let (clean, jitter_ms) = match cron.split_once("|jitter=") {
        Some((c, j)) => (c, j.trim().parse::<i64>().ok()),
        None => (cron, None),
    };
    // `Cron::from_str` (via FromStr) constructs but does not parse the pattern, so
    // fields stay unset and never match. Parse explicitly, allowing an optional
    // leading seconds field (5- or 6-part patterns), per croner 2.x semantics.
    let c = Cron::new(clean).with_seconds_optional().parse().ok()?;
    let after = chrono::Utc.timestamp_millis_opt(after_ms).single()?;
    let base = c
        .find_next_occurrence(&after, false)
        .ok()
        .map(|dt| dt.timestamp_millis())?;
    match jitter_ms {
        // Apply a random offset in the CLOSED range [-jitter, +jitter], then clamp so the
        // result never lands in the past / immediately (never earlier than after_ms + 1s).
        Some(jitter) if jitter > 0 => {
            let offset = rand::thread_rng().gen_range(-jitter..=jitter);
            Some((base + offset).max(after_ms + 1000))
        }
        _ => Some(base),
    }
}

fn event(
    task: &Task,
    from: TaskStatus,
    to: TaskStatus,
    at: i64,
    detail: serde_json::Value,
) -> TaskEvent {
    TaskEvent {
        task_id: task.id.clone(),
        at,
        from_status: Some(from),
        to_status: to,
        detail,
    }
}

/// Fail-safe for sensitive outward jobs. When a task with `pause_scope_on_failure` fails terminally,
/// pause its adapter scope (persisted) so queued siblings don't repeat the same failure unattended,
/// and log it loudly. A human resumes with `pcw resume <adapter>` after fixing the cause.
fn maybe_pause_scope(store: &Store, task: &Task) -> rusqlite::Result<()> {
    if !task.pause_scope_on_failure {
        return Ok(());
    }
    store.pause_scope(&task.adapter)?;
    store.append_event(&event(
        task,
        TaskStatus::Failed,
        TaskStatus::Failed,
        task.updated_at,
        serde_json::json!({ "auto_paused_scope": task.adapter }),
    ))?;
    tracing::warn!(
        adapter = %task.adapter,
        task = %task.id,
        "terminal failure with pause_scope_on_failure set; auto-paused the adapter scope so queued siblings do not repeat it"
    );
    Ok(())
}

/// Hand a terminal failure to the notifier — the active "call Claude when there is an issue" side.
/// `ScopePaused` (the loudest: the batch is now halted until a human resumes) when the task also
/// auto-paused its scope, else `TaskFailed`. Best-effort: the notifier swallows its own errors.
fn escalate(notifier: &Arc<dyn Notifier>, task: &Task) {
    let kind = if task.pause_scope_on_failure {
        EscalationKind::ScopePaused
    } else {
        EscalationKind::TaskFailed
    };
    notifier.notify(&EscalationEvent {
        kind,
        task_id: task.id.clone(),
        adapter: task.adapter.clone(),
        action: task.action.clone(),
        run_id: task.run_id.clone(),
        step_name: task.step_name.clone(),
        paused_scope: task.pause_scope_on_failure.then(|| task.adapter.clone()),
        error: task.last_error.clone(),
        at_ms: task.updated_at,
    });
}

/// Best-effort extraction of a human-readable message from a caught panic payload
/// (`std::panic::catch_unwind`'s `Err` value). Panics raised via `panic!("...")`,
/// `.unwrap()`, or `.expect("...")` carry a `&str` or `String` payload; anything else
/// falls back to a generic message rather than failing to report at all.
///
/// Deliberately takes the boxed payload by value instead of a `&dyn Any` reference.
/// The boxed trait object type also satisfies `Any`'s own blanket impl, so coercing a
/// reference to the box at a function-argument boundary can unsize the box itself into
/// the trait object instead of dereferencing to the inner payload, and every
/// `downcast_ref` on it then misses. Calling `.downcast_ref()` directly on the owned box
/// uses ordinary method-call autoderef and reaches the real inner value.
fn panic_message(payload: Box<dyn Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "adapter panicked with a non-string payload".to_string()
    }
}

/// Mark a task `Running` (a fast store write) and return the updated task. Split out of `run_task`
/// so the daemon can *claim* a task under the engine lock, then release the lock and run the slow
/// `execute_and_record` (the browser subprocess) unlocked — keeping RPCs responsive while a task runs.
pub fn mark_running(store: &Store, clock: &dyn Clock, mut task: Task) -> rusqlite::Result<Task> {
    let now = clock.now_ms();
    let prev = task.status;
    task.status = TaskStatus::Running;
    task.updated_at = now;
    store.update_task(&task)?;
    store.append_event(&event(
        &task,
        prev,
        TaskStatus::Running,
        now,
        serde_json::json!({}),
    ))?;
    Ok(task)
}

/// Mark `Running` then execute + record — the whole run, lock-held end to end. Kept for in-process
/// callers (`Engine::tick`, tests); the daemon uses `mark_running` + `execute_and_record` to run the
/// slow middle off the engine lock.
pub async fn run_task(
    store: &Store,
    adapter: &dyn Adapter,
    clock: &dyn Clock,
    browser: Arc<dyn BrowserHandle>,
    notifier: Arc<dyn Notifier>,
    task: Task,
) -> rusqlite::Result<()> {
    let task = mark_running(store, clock, task)?;
    execute_and_record(store, adapter, clock, browser, notifier, task).await
}

/// Run an already-`Running` task's adapter and record the outcome. This holds NO engine lock (only
/// the store's own internal mutex, briefly, for the result writes), so the browser subprocess — the
/// seconds-to-90s part — no longer blocks `add`/`status`/`list`/the TUI/web.
pub async fn execute_and_record(
    store: &Store,
    adapter: &dyn Adapter,
    clock: &dyn Clock,
    browser: Arc<dyn BrowserHandle>,
    notifier: Arc<dyn Notifier>,
    mut task: Task,
) -> rusqlite::Result<()> {
    let ctx = RunCtx {
        task_id: task.id.clone(),
        browser,
    };
    // Adapters are third-party-ish, unreviewed code from the engine's point of view
    // (M2+ will add browser-driving adapters that shell out / scrape). A panic inside
    // `execute()` must not take down the tick loop or the daemon: catch it here and
    // fold it into the same Terminal path as any other unrecoverable adapter error, so
    // it fails just that task with a normal audit trail instead of crashing the process.
    // Resolve {{ steps.*.result.* }} / {{ vars.* }} against this run's sibling results at
    // DISPATCH time, not enqueue time: the referenced step has only just produced its
    // result. Non-run tasks are untouched, so nothing about existing behaviour changes.
    let params = match task.run_id.clone() {
        None => task.params.clone(),
        Some(rid) => {
            let mut results = std::collections::HashMap::new();
            let mut vars = serde_json::Value::Object(Default::default());
            for sib in store.tasks_in_run(&rid)? {
                if sib.step_name.as_deref() == Some(crate::run::VARS_STEP) {
                    vars = sib.params.clone();
                } else if let (Some(name), Some(res)) = (sib.step_name.clone(), sib.result.clone())
                {
                    results.insert(name, res);
                }
            }
            // Project the run's state to its shared dataset before dispatch. Recipes run
            // in a separate process and cannot read the store, so this file is how a recipe
            // sees the run. Best-effort: a write failure must not fail the task, since the
            // file is a projection and the store remains the source of truth.
            let ds = crate::run::write_dataset(&crate::run::home_dir(), &rid, &vars, &results).ok();
            if let (Some(p), Some(o)) = (ds, vars.as_object_mut()) {
                o.insert(
                    "dataset".into(),
                    serde_json::Value::String(p.display().to_string()),
                );
            }

            match crate::refs::resolve(&task.params, &vars, &results) {
                Ok(v) => v,
                Err(e) => {
                    // Refuse to hand a literal "{{ … }}" to an adapter.
                    let now = clock.now_ms();
                    task.status = TaskStatus::Failed;
                    task.last_error = Some(e.to_string());
                    task.finished_at = Some(now);
                    task.updated_at = now;
                    store.update_task(&task)?;
                    store.append_event(&event(
                        &task,
                        TaskStatus::Running,
                        TaskStatus::Failed,
                        now,
                        serde_json::json!({"unresolved": true}),
                    ))?;
                    return Ok(());
                }
            }
        }
    };

    let result = match AssertUnwindSafe(adapter.execute(&ctx, &task.action, params))
        .catch_unwind()
        .await
    {
        Ok(r) => r,
        Err(panic_payload) => Err(AdapterError::Terminal(format!(
            "adapter panicked: {}",
            panic_message(panic_payload)
        ))),
    };
    let now = clock.now_ms();

    match result {
        Ok(v) => {
            let keys = adapter.limit_keys_for_task(&task.action, &task.params);
            spend_limits(store, clock, &keys)?;
            task.status = TaskStatus::Succeeded;
            task.result = Some(v);
            task.finished_at = Some(now);
            task.updated_at = now;
            store.update_task(&task)?;
            store.append_event(&event(
                &task,
                TaskStatus::Running,
                TaskStatus::Succeeded,
                now,
                serde_json::json!({"spent": keys}),
            ))?;

            // R7 ledger: if this is a fanned-out act task, record its target as touched so it is
            // never acted on again — the built-in form of the per-script "SENT <id>" ledgers.
            if let (Some(scope), Some(id)) = (task.touch_scope.clone(), task.touch_id.clone()) {
                if store.mark_touched(&scope, &id, now)? {
                    tracing::info!(scope = %scope, target = %id, "marked target touched (R7 ledger)");
                }
            }

            // R5 fan-out: turn this producer's array result into paced, deduped act tasks.
            if task.fanout.is_some() {
                let acts = crate::run::materialize_fanout(store, &task, now)?;
                let n = acts.len();
                for a in acts {
                    store.insert_task(&a)?;
                    store.append_event(&TaskEvent {
                        task_id: a.id.clone(),
                        at: now,
                        from_status: None,
                        to_status: a.status,
                        detail: serde_json::json!({ "fanout_from": task.id, "target": a.touch_id }),
                    })?;
                }
                if n > 0 {
                    tracing::info!(producer = %task.id, count = n, "fanned out act tasks");
                }
            }

            // ADJUDICATION. If this task is the escalation backstopping another, a
            // well-formed verdict can flip that task to Succeeded. Deliberately narrow,
            // because an adjudicator asked "did this work?" drifts toward yes, and that
            // is the exact failure mode verification exists to prevent:
            //   - it may overturn a VERIFY only, never a real action's failure
            //   - it must return ok:true AND non-empty evidence; absence of evidence is
            //     failure, not success
            //   - the outcome is recorded `adjudicated`, never `verified`
            if let Some(rid) = task.run_id.clone() {
                let verdict = task.result.clone().unwrap_or(serde_json::Value::Null);
                let ok = verdict
                    .get("ok")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false);
                let has_evidence = match verdict.get("evidence") {
                    Some(serde_json::Value::String(s)) => !s.trim().is_empty(),
                    Some(serde_json::Value::Array(a)) => !a.is_empty(),
                    Some(serde_json::Value::Object(o)) => !o.is_empty(),
                    _ => false,
                };
                for mut backstopped in store.tasks_in_run(&rid)? {
                    if backstopped.escalation.as_deref() != Some(task.id.as_str()) {
                        continue;
                    }
                    if ok && has_evidence {
                        backstopped.status = TaskStatus::Succeeded;
                        backstopped.last_error = None;
                        let mut res = backstopped.result.clone().unwrap_or(serde_json::json!({}));
                        if let Some(o) = res.as_object_mut() {
                            o.insert("adjudicated".into(), serde_json::Value::Bool(true));
                            o.insert("adjudication".into(), verdict.clone());
                        }
                        backstopped.result = Some(res);
                    } else {
                        backstopped.last_error = Some(format!(
                            "escalation declined to confirm (ok={ok}, evidence={has_evidence})"
                        ));
                    }
                    backstopped.finished_at = Some(now);
                    backstopped.updated_at = now;
                    store.update_task(&backstopped)?;
                }
            }

            if let Some(cron) = task.recurrence.clone() {
                if let Some(next_ms) = next_occurrence_ms(&cron, now) {
                    let mut nxt = Task::new_now(
                        task.adapter.clone(),
                        task.action.clone(),
                        task.params.clone(),
                        next_ms,
                    );
                    nxt.recurrence = Some(cron);
                    nxt.priority = task.priority;
                    nxt.max_attempts = task.max_attempts;
                    // Carry the dedup key onto the next occurrence so the schedule reconciler
                    // keeps recognizing a recurring schedule task across its firings (otherwise
                    // each occurrence would look new and reconcile would duplicate it).
                    nxt.dedup_key = task.dedup_key.clone();
                    store.insert_task(&nxt)?;
                    store.append_event(&TaskEvent {
                        task_id: nxt.id.clone(),
                        at: now,
                        from_status: None,
                        to_status: TaskStatus::Pending,
                        detail: serde_json::json!({"recurred_from": task.id}),
                    })?;
                }
            }
        }
        Err(e @ (AdapterError::Retryable(_) | AdapterError::RetryableWith { .. })) => {
            let msg = e.message().to_string();
            // A structured failure context (page state, failing step) is persisted in `result`
            // even though the task did not succeed — `{"failure": …}` — so a self-heal loop can
            // read it back. On a later successful attempt the real result replaces it.
            if let Some(detail) = e.detail() {
                task.result = Some(serde_json::json!({ "failure": detail }));
            }
            task.attempts += 1;
            if task.attempts < task.max_attempts {
                let delay = backoff_ms(task.attempts);
                task.status = TaskStatus::Pending;
                task.scheduled_for = now + delay;
                task.last_error = Some(msg.clone());
                task.updated_at = now;
                store.update_task(&task)?;
                store.append_event(&event(
                    &task,
                    TaskStatus::Running,
                    TaskStatus::Pending,
                    now,
                    serde_json::json!({"retry": true, "in_ms": delay, "error": msg}),
                ))?;
            } else {
                task.status = TaskStatus::Failed;
                task.last_error = Some(msg.clone());
                task.finished_at = Some(now);
                task.updated_at = now;
                store.update_task(&task)?;
                store.append_event(&event(
                    &task,
                    TaskStatus::Running,
                    TaskStatus::Failed,
                    now,
                    serde_json::json!({"error": msg, "exhausted": true}),
                ))?;
                maybe_pause_scope(store, &task)?;
                escalate(&notifier, &task);
            }
        }
        Err(e @ (AdapterError::Terminal(_) | AdapterError::TerminalWith { .. })) => {
            let msg = e.message().to_string();
            if let Some(detail) = e.detail() {
                task.result = Some(serde_json::json!({ "failure": detail }));
            }
            task.status = TaskStatus::Failed;
            task.last_error = Some(msg.clone());
            task.finished_at = Some(now);
            task.updated_at = now;
            store.update_task(&task)?;
            store.append_event(&event(
                &task,
                TaskStatus::Running,
                TaskStatus::Failed,
                now,
                serde_json::json!({"error": msg}),
            ))?;
            maybe_pause_scope(store, &task)?;
            escalate(&notifier, &task);
        }
        Err(AdapterError::RateLimited { retry_after }) => {
            task.status = TaskStatus::Deferred;
            task.next_eligible_at = Some(retry_after);
            task.updated_at = now;
            store.update_task(&task)?;
            store.append_event(&event(
                &task,
                TaskStatus::Running,
                TaskStatus::Deferred,
                now,
                serde_json::json!({"rate_limited_until": retry_after}),
            ))?;
        }
    }
    Ok(())
}

use chrono::TimeZone;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::TestClock;

    // NOTE: tests use an inline stub adapter to avoid a dev-dep cycle.
    use crate::adapter::Adapter;
    use crate::model::ActionSpec;
    use async_trait::async_trait;
    use serde_json::{json, Value};
    use std::collections::HashMap;
    use std::sync::Mutex;

    #[derive(Default)]
    struct StubAdapter {
        flaky: Mutex<HashMap<String, u64>>,
    }
    #[async_trait]
    impl Adapter for StubAdapter {
        fn name(&self) -> &str {
            "dummy"
        }
        fn actions(&self) -> Vec<ActionSpec> {
            vec![ActionSpec {
                name: "rate_heavy".into(),
                limit_keys: vec!["dummy.capped".into()],
                params_schema: Value::Null,
                description: "".into(),
            }]
        }
        async fn execute(
            &self,
            ctx: &RunCtx,
            action: &str,
            params: Value,
        ) -> Result<Value, AdapterError> {
            match action {
                "echo" | "rate_heavy" => Ok(params),
                "always_fail" => Err(AdapterError::Terminal("boom".into())),
                "fail_with_detail" => Err(AdapterError::TerminalWith {
                    message: "step 3 died".into(),
                    detail: json!({"step_index": 2, "url": "https://x.test/p"}),
                }),
                "retry_with_detail" => Err(AdapterError::RetryableWith {
                    message: "not ready".into(),
                    detail: json!({"step_index": 0}),
                }),
                "flaky" => {
                    let n = {
                        let mut g = self.flaky.lock().unwrap();
                        let e = g.entry(ctx.task_id.clone()).or_insert(0);
                        *e += 1;
                        *e
                    };
                    if n <= 2 {
                        Err(AdapterError::Retryable(format!("try {n}")))
                    } else {
                        Ok(json!({"ok": n}))
                    }
                }
                "panic" => panic!("stub adapter deliberately panicked"),
                _ => Err(AdapterError::Terminal("unknown".into())),
            }
        }
    }

    fn fake() -> std::sync::Arc<dyn crate::browser::BrowserHandle> {
        std::sync::Arc::new(crate::browser::NullBrowser)
    }

    fn notifier() -> std::sync::Arc<dyn crate::notify::Notifier> {
        crate::notify::null_notifier()
    }

    #[tokio::test]
    async fn test_success_sets_result_and_spends_limit() {
        let store = Store::open_in_memory().unwrap();
        let clock = TestClock::new(1_000);
        let a = StubAdapter::default();
        let t = Task::new_now("dummy", "rate_heavy", json!({"n":1}), 500);
        store.insert_task(&t).unwrap();
        run_task(&store, &a, &clock, fake(), notifier(), t.clone())
            .await
            .unwrap();
        let got = store.get_task(&t.id).unwrap().unwrap();
        assert_eq!(got.status, TaskStatus::Succeeded);
        assert_eq!(got.result, Some(json!({"n":1})));
        let (c, _) = store
            .counter_get("dummy.capped", &crate::limits::local_date_str(1_000))
            .unwrap();
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
        run_task(&store, &a, &clock, fake(), notifier(), t.clone())
            .await
            .unwrap();
        let g1 = store.get_task(&t.id).unwrap().unwrap();
        assert_eq!(g1.status, TaskStatus::Pending);
        assert_eq!(g1.attempts, 1);
        assert_eq!(g1.scheduled_for, 1_000 + backoff_ms(1));
        // second attempt hits max_attempts -> failed
        run_task(&store, &a, &clock, fake(), notifier(), g1.clone())
            .await
            .unwrap();
        let g2 = store.get_task(&t.id).unwrap().unwrap();
        assert_eq!(g2.status, TaskStatus::Failed);
    }

    #[tokio::test]
    async fn test_panicking_adapter_becomes_terminal_failure_with_event() {
        let store = Store::open_in_memory().unwrap();
        let clock = TestClock::new(1_000);
        let a = StubAdapter::default();
        let t = Task::new_now("dummy", "panic", json!({}), 500);
        store.insert_task(&t).unwrap();
        // Must not propagate the panic out of run_task / poison anything.
        run_task(&store, &a, &clock, fake(), notifier(), t.clone())
            .await
            .unwrap();
        let got = store.get_task(&t.id).unwrap().unwrap();
        assert_eq!(got.status, TaskStatus::Failed);
        assert!(got.last_error.as_deref().unwrap_or("").contains("panicked"));
        assert!(got
            .last_error
            .as_deref()
            .unwrap_or("")
            .contains("stub adapter deliberately panicked"));

        let events = store.events_for(&t.id).unwrap();
        let last = events.last().unwrap();
        assert_eq!(last.to_status, TaskStatus::Failed);
        assert_eq!(last.from_status, Some(TaskStatus::Running));
    }

    #[tokio::test]
    async fn test_terminal_fails_immediately() {
        let store = Store::open_in_memory().unwrap();
        let clock = TestClock::new(1_000);
        let a = StubAdapter::default();
        let t = Task::new_now("dummy", "always_fail", json!({}), 500);
        store.insert_task(&t).unwrap();
        run_task(&store, &a, &clock, fake(), notifier(), t.clone())
            .await
            .unwrap();
        assert_eq!(
            store.get_task(&t.id).unwrap().unwrap().status,
            TaskStatus::Failed
        );
    }

    #[tokio::test]
    async fn test_terminal_with_detail_persists_failure_in_result() {
        let store = Store::open_in_memory().unwrap();
        let clock = TestClock::new(1_000);
        let a = StubAdapter::default();
        let t = Task::new_now("dummy", "fail_with_detail", json!({}), 500);
        store.insert_task(&t).unwrap();
        run_task(&store, &a, &clock, fake(), notifier(), t.clone())
            .await
            .unwrap();
        let got = store.get_task(&t.id).unwrap().unwrap();
        assert_eq!(got.status, TaskStatus::Failed);
        assert_eq!(got.last_error.as_deref(), Some("step 3 died"));
        assert_eq!(
            got.result,
            Some(json!({"failure": {"step_index": 2, "url": "https://x.test/p"}}))
        );
        let last = store.events_for(&t.id).unwrap().pop().unwrap();
        assert_eq!(last.detail["error"], "step 3 died");
    }

    #[tokio::test]
    async fn test_retryable_with_detail_honors_max_attempts_and_keeps_context() {
        // max_attempts = 1: the first retryable failure is already the last one. The backend relies
        // on this for recipes that must never be re-run by the engine (a real invoice was issued).
        let store = Store::open_in_memory().unwrap();
        let clock = TestClock::new(1_000);
        let a = StubAdapter::default();
        let mut t = Task::new_now("dummy", "retry_with_detail", json!({}), 500);
        t.max_attempts = 1;
        store.insert_task(&t).unwrap();
        run_task(&store, &a, &clock, fake(), notifier(), t.clone())
            .await
            .unwrap();
        let got = store.get_task(&t.id).unwrap().unwrap();
        assert_eq!(got.status, TaskStatus::Failed);
        assert_eq!(got.attempts, 1);
        assert_eq!(got.last_error.as_deref(), Some("not ready"));
        assert_eq!(got.result, Some(json!({"failure": {"step_index": 0}})));

        // With room to retry, the task goes back to Pending but KEEPS the last attempt's context.
        let mut t2 = Task::new_now("dummy", "retry_with_detail", json!({}), 500);
        t2.max_attempts = 3;
        store.insert_task(&t2).unwrap();
        run_task(&store, &a, &clock, fake(), notifier(), t2.clone())
            .await
            .unwrap();
        let g2 = store.get_task(&t2.id).unwrap().unwrap();
        assert_eq!(g2.status, TaskStatus::Pending);
        assert_eq!(g2.result, Some(json!({"failure": {"step_index": 0}})));
    }

    #[tokio::test]
    async fn test_recurrence_enqueues_next() {
        let store = Store::open_in_memory().unwrap();
        let clock = TestClock::new(1_000);
        let a = StubAdapter::default();
        let mut t = Task::new_now("dummy", "echo", json!({}), 500);
        t.recurrence = Some("0 0 * * * *".into()); // top of every hour (croner 6-field)
        t.dedup_key = Some("schedule:hourly".into());
        store.insert_task(&t).unwrap();
        run_task(&store, &a, &clock, fake(), notifier(), t.clone())
            .await
            .unwrap();
        // original succeeded + one new pending recurrence
        let pend = store.tasks_in_status(TaskStatus::Pending).unwrap();
        assert_eq!(pend.len(), 1);
        assert!(pend[0].recurrence.is_some());
        // the next occurrence carries the dedup key so the schedule reconciler still owns it
        assert_eq!(pend[0].dedup_key.as_deref(), Some("schedule:hourly"));
    }

    #[test]
    fn test_backoff_growth_and_cap() {
        assert_eq!(backoff_ms(0), 1000);
        assert_eq!(backoff_ms(1), 2000);
        assert_eq!(backoff_ms(3), 8000);
        assert_eq!(backoff_ms(40), 3_600_000);
    }
}

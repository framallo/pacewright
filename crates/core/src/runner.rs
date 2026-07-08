use crate::adapter::{Adapter, RunCtx};
use crate::clock::Clock;
use crate::limits::spend_limits;
use crate::model::{AdapterError, Task, TaskEvent, TaskStatus};
use crate::store::Store;
use croner::Cron;
use futures_util::FutureExt;
use std::any::Any;
use std::panic::AssertUnwindSafe;

pub fn backoff_ms(attempts: i64) -> i64 {
    let base = 1000i64.saturating_mul(1i64 << attempts.min(20));
    base.min(3_600_000)
}

pub fn next_occurrence_ms(cron: &str, after_ms: i64) -> Option<i64> {
    // `Cron::from_str` (via FromStr) constructs but does not parse the pattern, so
    // fields stay unset and never match. Parse explicitly, allowing an optional
    // leading seconds field (5- or 6-part patterns), per croner 2.x semantics.
    let c = Cron::new(cron).with_seconds_optional().parse().ok()?;
    let after = chrono::Utc.timestamp_millis_opt(after_ms).single()?;
    c.find_next_occurrence(&after, false).ok().map(|dt| dt.timestamp_millis())
}

fn event(task: &Task, from: TaskStatus, to: TaskStatus, at: i64, detail: serde_json::Value) -> TaskEvent {
    TaskEvent { task_id: task.id.clone(), at, from_status: Some(from), to_status: to, detail }
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
    // Adapters are third-party-ish, unreviewed code from the engine's point of view
    // (M2+ will add browser-driving adapters that shell out / scrape). A panic inside
    // `execute()` must not take down the tick loop or the daemon: catch it here and
    // fold it into the same Terminal path as any other unrecoverable adapter error, so
    // it fails just that task with a normal audit trail instead of crashing the process.
    let result = match AssertUnwindSafe(adapter.execute(&ctx, &task.action, task.params.clone())).catch_unwind().await {
        Ok(r) => r,
        Err(panic_payload) => Err(AdapterError::Terminal(format!("adapter panicked: {}", panic_message(panic_payload)))),
    };
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
                "panic" => panic!("stub adapter deliberately panicked"),
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
    async fn test_panicking_adapter_becomes_terminal_failure_with_event() {
        let store = Store::open_in_memory().unwrap();
        let clock = TestClock::new(1_000);
        let a = StubAdapter::default();
        let t = Task::new_now("dummy", "panic", json!({}), 500);
        store.insert_task(&t).unwrap();
        // Must not propagate the panic out of run_task / poison anything.
        run_task(&store, &a, &clock, t.clone()).await.unwrap();
        let got = store.get_task(&t.id).unwrap().unwrap();
        assert_eq!(got.status, TaskStatus::Failed);
        assert!(got.last_error.as_deref().unwrap_or("").contains("panicked"));
        assert!(got.last_error.as_deref().unwrap_or("").contains("stub adapter deliberately panicked"));

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

use pacewright_adapter_dummy::DummyAdapter;
use pacewright_core::adapter::AdapterRegistry;
use pacewright_core::clock::{Clock, TestClock};
use pacewright_core::config::Config;
use pacewright_core::engine::Engine;
use pacewright_core::model::{Task, TaskStatus};
use pacewright_core::rng::TestRng;
use pacewright_core::store::Store;
use std::sync::Arc;

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
async fn test_pause_all_blocks_tick() {
    let clock = TestClock::new(1_000);
    let mut e = engine(clock.clone(), Config::default());
    let id = e.add_task(Task::new_now("dummy", "echo", serde_json::json!({"a":1}), 500)).unwrap();

    e.pause("all".into());
    e.tick().await.unwrap();
    assert_eq!(e.store.get_task(&id).unwrap().unwrap().status, TaskStatus::Pending);

    e.resume("all");
    e.tick().await.unwrap();
    assert_eq!(e.store.get_task(&id).unwrap().unwrap().status, TaskStatus::Succeeded);
}

#[tokio::test]
async fn test_pause_adapter_skips_only_that_adapter() {
    let clock = TestClock::new(1_000);
    let mut e = engine(clock.clone(), Config::default());

    e.pause("dummy".into());
    let id = e.add_task(Task::new_now("dummy", "echo", serde_json::json!({"a":1}), 500)).unwrap();
    e.tick().await.unwrap();
    assert_eq!(e.store.get_task(&id).unwrap().unwrap().status, TaskStatus::Pending);

    e.resume("dummy");
    e.tick().await.unwrap();
    assert_eq!(e.store.get_task(&id).unwrap().unwrap().status, TaskStatus::Succeeded);
}

#[tokio::test]
async fn test_panic_in_adapter_is_isolated_and_tick_continues() {
    let clock = TestClock::new(1_000);
    let e = engine(clock.clone(), Config::default());
    // Order matters: the panicking task is scheduled first (lower scheduled_for), so if
    // the panic ever escaped run_task and unwound the tick loop, the echo task below
    // would never run.
    let panic_id = e.add_task(Task::new_now("dummy", "panic", serde_json::json!({}), 100)).unwrap();
    let echo_id = e.add_task(Task::new_now("dummy", "echo", serde_json::json!({"a": 1}), 200)).unwrap();

    e.tick().await.unwrap();

    let panicked = e.store.get_task(&panic_id).unwrap().unwrap();
    assert_eq!(panicked.status, TaskStatus::Failed);
    assert!(panicked.last_error.as_deref().unwrap_or("").contains("panicked"));
    let events = e.store.events_for(&panic_id).unwrap();
    let last = events.last().unwrap();
    assert_eq!(last.to_status, TaskStatus::Failed);

    // The tick loop survived the panic and kept processing the rest of the batch.
    let echoed = e.store.get_task(&echo_id).unwrap().unwrap();
    assert_eq!(echoed.status, TaskStatus::Succeeded);

    // The engine itself is still usable for subsequent ticks.
    let after_id = e.add_task(Task::new_now("dummy", "echo", serde_json::json!({"b": 2}), 300)).unwrap();
    e.tick().await.unwrap();
    assert_eq!(e.store.get_task(&after_id).unwrap().unwrap().status, TaskStatus::Succeeded);
}

#[tokio::test]
async fn test_cap_defers_fourth_rate_heavy() {
    let clock = TestClock::new({
        // noon local on 2026-07-07 so active window 00:00-24:00 default is fine
        use chrono::TimeZone;
        let naive = chrono::NaiveDate::from_ymd_opt(2026, 7, 7).unwrap().and_hms_opt(12, 0, 0).unwrap();
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

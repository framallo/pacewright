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
async fn test_claim_one_marks_running_and_does_not_double_claim() {
    // The daemon claims a task under the engine lock, then runs it OUTSIDE the lock. So a claimed
    // task must be marked Running immediately (it's off the pending set) and a second claim — the
    // state the RPC handlers see while the first task's browser subprocess is still running — must
    // NOT hand out the same task again.
    use pacewright_core::runner::execute_and_record;
    let clock = TestClock::new(1_000);
    let e = engine(clock.clone(), Config::default());
    let id = e.add_task(Task::new_now("dummy", "echo", serde_json::json!({"a":1}), 500)).unwrap();

    let claimed = e.claim_one().unwrap().expect("a task should be claimable");
    assert_eq!(claimed.task.id, id);
    // Claimed == Running in the store, before any execution has happened.
    assert_eq!(e.store.get_task(&id).unwrap().unwrap().status, TaskStatus::Running);
    // While it's "running", nothing else is runnable — the same task is never re-claimed.
    assert!(e.claim_one().unwrap().is_none());

    // Recording the outcome (the off-lock half) still drives it to success.
    execute_and_record(&e.store, &*claimed.adapter, &*e.clock, e.browser.clone(), claimed.task).await.unwrap();
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

#[test]
fn test_run_columns_roundtrip_and_lookup() {
    let store = Store::open_in_memory().unwrap();
    let mut t = Task::new_now("dummy", "echo", serde_json::json!({}), 1_000);
    t.run_id = Some("ep172".into());
    t.step_name = Some("publish_long".into());
    t.dedup_key = Some("ep172:publish_long".into());
    store.insert_task(&t).unwrap();

    let got = store.get_task(&t.id).unwrap().unwrap();
    assert_eq!(got.run_id.as_deref(), Some("ep172"));
    assert_eq!(got.step_name.as_deref(), Some("publish_long"));
    assert_eq!(store.tasks_in_run("ep172").unwrap().len(), 1);

    // find_by_dedup_any must see TERMINAL tasks (this is what makes resume work,
    // unlike find_active_by_dedup which deliberately excludes them).
    let mut done = got.clone();
    done.status = TaskStatus::Succeeded;
    store.update_task(&done).unwrap();
    assert!(store.find_active_by_dedup("ep172:publish_long").unwrap().is_none());
    assert!(store.find_by_dedup_any("ep172:publish_long").unwrap().is_some());
}

/// Drive ticks until the run settles or we hit a bounded cap.
async fn drain(e: &Engine, run_id: &str) {
    for _ in 0..40 {
        e.tick().await.unwrap();
        let tasks = e.store.tasks_in_run(run_id).unwrap();
        if tasks.iter().all(|t| {
            matches!(t.status, TaskStatus::Succeeded | TaskStatus::Failed | TaskStatus::Canceled)
        }) {
            return;
        }
    }
}

fn step_of(e: &Engine, run_id: &str, name: &str) -> Task {
    e.store
        .tasks_in_run(run_id)
        .unwrap()
        .into_iter()
        .find(|t| t.step_name.as_deref() == Some(name))
        .unwrap_or_else(|| panic!("no step {name}"))
}

#[tokio::test]
async fn test_failing_verify_blocks_the_dependent_step() {
    // The regression test for the real incidents: riverside/publish_clips reported
    // success while publishing ZERO shorts, and share_spotify reported success while
    // leaving a blank draft. Here the step "succeeds" but its verify fails, and the
    // next step MUST NOT run on the strength of that false success.
    let clock = TestClock::new(1_000);
    let e = engine(clock.clone(), Config::default());
    let src = r#"pipeline "d" {
        step "publish" recipe="dummy/echo" {
            verify recipe="dummy/always_fail" { }
        }
        step "after" recipe="dummy/echo" after="publish" { }
    }"#;
    let def = pacewright_core::pipeline::parse_pipeline(src).unwrap();
    pacewright_core::run::start(&e.store, &def, "r1", &serde_json::json!({}), 1_000).unwrap();

    drain(&e, "r1").await;

    assert_eq!(step_of(&e, "r1", "publish").status, TaskStatus::Succeeded);
    assert_eq!(step_of(&e, "r1", "publish.verify").status, TaskStatus::Failed);
    assert_ne!(
        step_of(&e, "r1", "after").status,
        TaskStatus::Succeeded,
        "a dependent must never run past a failed verification"
    );
}

#[tokio::test]
async fn test_passing_verify_releases_the_dependent_step() {
    let clock = TestClock::new(1_000);
    let e = engine(clock.clone(), Config::default());
    let src = r#"pipeline "d" {
        step "publish" recipe="dummy/echo" {
            verify recipe="dummy/echo" { }
        }
        step "after" recipe="dummy/echo" after="publish" { }
    }"#;
    let def = pacewright_core::pipeline::parse_pipeline(src).unwrap();
    pacewright_core::run::start(&e.store, &def, "r2", &serde_json::json!({}), 1_000).unwrap();

    drain(&e, "r2").await;

    assert_eq!(step_of(&e, "r2", "publish.verify").status, TaskStatus::Succeeded);
    assert_eq!(step_of(&e, "r2", "after").status, TaskStatus::Succeeded);
}

#[tokio::test]
async fn test_result_flows_from_one_step_into_the_next() {
    let clock = TestClock::new(1_000);
    let e = engine(clock.clone(), Config::default());
    // dummy/echo returns its params, so `one` produces {"id":"abc"} and `two` should
    // receive that value resolved out of it.
    let src = r#"pipeline "d" {
        step "one" recipe="dummy/echo" { params { id "abc" } }
        step "two" recipe="dummy/echo" after="one" {
            params {
                got "{{ steps.one.result.id }}"
                label "EP{{ vars.n }}"
            }
        }
    }"#;
    let def = pacewright_core::pipeline::parse_pipeline(src).unwrap();
    pacewright_core::run::start(&e.store, &def, "r3", &serde_json::json!({"n": "172"}), 1_000)
        .unwrap();

    drain(&e, "r3").await;

    let two = step_of(&e, "r3", "two");
    assert_eq!(two.status, TaskStatus::Succeeded);
    let res = two.result.unwrap();
    assert_eq!(res["got"], "abc", "a step result must reach the next step");
    assert_eq!(res["label"], "EP172", "run vars must resolve too");
}

#[tokio::test]
async fn test_unresolvable_reference_fails_the_task_instead_of_leaking_braces() {
    let clock = TestClock::new(1_000);
    let e = engine(clock.clone(), Config::default());
    let src = r#"pipeline "d" {
        step "only" recipe="dummy/echo" { params { x "{{ steps.ghost.result.y }}" } }
    }"#;
    let def = pacewright_core::pipeline::parse_pipeline(src).unwrap();
    pacewright_core::run::start(&e.store, &def, "r4", &serde_json::json!({}), 1_000).unwrap();

    drain(&e, "r4").await;

    let t = step_of(&e, "r4", "only");
    assert_eq!(t.status, TaskStatus::Failed);
    assert!(t.last_error.unwrap().contains("steps.ghost.result.y"));
}

#[tokio::test]
async fn test_fallback_can_adjudicate_a_failed_verify_but_only_with_evidence() {
    // The escape hatch: a verify can itself be wrong. A fallback may overturn it, but
    // only on a well-formed verdict, and the outcome is marked `adjudicated` so a run
    // that leaned on it is visibly different from one that passed clean.
    let clock = TestClock::new(1_000);
    let e = engine(clock.clone(), Config::default());
    let src = r#"pipeline "d" {
        step "publish" recipe="dummy/echo" {
            verify recipe="dummy/always_fail" { }
            fallback recipe="dummy/verdict_ok" { }
        }
        step "after" recipe="dummy/echo" after="publish" { }
    }"#;
    let def = pacewright_core::pipeline::parse_pipeline(src).unwrap();
    pacewright_core::run::start(&e.store, &def, "adj", &serde_json::json!({}), 1_000).unwrap();

    drain(&e, "adj").await;

    let verify = step_of(&e, "adj", "publish.verify");
    assert_eq!(verify.status, TaskStatus::Succeeded, "a sound verdict overturns the verify");
    assert_eq!(
        verify.result.as_ref().unwrap()["adjudicated"],
        serde_json::json!(true),
        "and it is recorded as adjudicated, never as verified"
    );
    assert_eq!(step_of(&e, "adj", "after").status, TaskStatus::Succeeded);
}

#[tokio::test]
async fn test_fallback_claiming_success_without_evidence_is_refused() {
    // An adjudicator asked "did this work?" drifts toward yes. Absence of evidence is
    // failure, not success, or the whole verification layer becomes a rubber stamp.
    let clock = TestClock::new(1_000);
    let e = engine(clock.clone(), Config::default());
    let src = r#"pipeline "d" {
        step "publish" recipe="dummy/echo" {
            verify recipe="dummy/always_fail" { }
            fallback recipe="dummy/verdict_no_evidence" { }
        }
        step "after" recipe="dummy/echo" after="publish" { }
    }"#;
    let def = pacewright_core::pipeline::parse_pipeline(src).unwrap();
    pacewright_core::run::start(&e.store, &def, "adj2", &serde_json::json!({}), 1_000).unwrap();

    drain(&e, "adj2").await;

    let verify = step_of(&e, "adj2", "publish.verify");
    assert_eq!(verify.status, TaskStatus::Failed, "no evidence, no pass");
    assert!(verify.last_error.unwrap().contains("declined to confirm"));
    assert_ne!(step_of(&e, "adj2", "after").status, TaskStatus::Succeeded);
}

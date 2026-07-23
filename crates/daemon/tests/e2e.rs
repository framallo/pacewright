use pacewright_adapter_dummy::DummyAdapter;
use pacewright_adapter_recipe::{AuthManager, CliLoginLauncher, CliRecipeRunner, RecipeRegistry};
use pacewright_core::adapter::AdapterRegistry;
use pacewright_core::clock::SystemClock;
use pacewright_core::config::Config;
use pacewright_core::engine::Engine;
use pacewright_core::rng::SeededRng;
use pacewright_core::store::Store;
use pacewright_proto::{AddTaskReq, Request, Response};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::Mutex;

// Re-declare the server path by depending on the daemon lib. To allow this,
// the daemon exposes `server` as a lib module (see step 2).
use pacewright_daemon::server::{serve, Server};

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
    let engine = Arc::new(Mutex::new(Engine::new(
        store,
        reg,
        Config::default(),
        Arc::new(SystemClock),
        Arc::new(SeededRng::new(1)),
    )));
    let registry = Arc::new(RecipeRegistry::new());
    let recipe_runner: Arc<dyn pacewright_adapter_recipe::RecipeRunner> =
        Arc::new(CliRecipeRunner::new());
    let auth = Arc::new(AuthManager::new(
        registry.clone(),
        recipe_runner.clone(),
        Arc::new(CliLoginLauncher::new()),
    ));
    let srv = Arc::new(Server {
        engine,
        registry: std::sync::RwLock::new(registry),
        recipes_dir: dir.join("recipes"),
        recipe_runner,
        schedules_dir: dir.join("schedules"),
        auth,
    });

    let sock2 = sock.clone();
    tokio::spawn(async move {
        serve(srv, &sock2).await.unwrap();
    });
    // wait for socket
    for _ in 0..50 {
        if sock.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let resp = client_call(
        &sock,
        Request::Add(AddTaskReq {
            adapter: "dummy".into(),
            action: "echo".into(),
            params: serde_json::json!({"a":1}),
            scheduled_for: None,
            recurrence: None,
            depends_on: None,
            priority: None,
            dedup_key: None,
            max_attempts: None,
        }),
    )
    .await;
    let id = match resp {
        Response::Ok(v) => v["id"].as_str().unwrap().to_string(),
        _ => panic!("add failed"),
    };

    // tick runs every 1s; wait up to 3s
    let mut ok = false;
    for _ in 0..30 {
        tokio::time::sleep(Duration::from_millis(200)).await;
        if let Response::Ok(v) = client_call(&sock, Request::Get { id: id.clone() }).await {
            if v["task"]["status"] == "succeeded" {
                ok = true;
                break;
            }
        }
    }
    assert!(ok, "task did not reach succeeded");
    std::fs::remove_dir_all(&dir).ok();
}

/// An adapter that records the maximum number of `execute` calls ever in flight at once.
struct ConcurrencyProbe {
    in_flight: Arc<std::sync::atomic::AtomicUsize>,
    max_seen: Arc<std::sync::atomic::AtomicUsize>,
    executed: Arc<std::sync::atomic::AtomicUsize>,
}

#[async_trait::async_trait]
impl pacewright_core::adapter::Adapter for ConcurrencyProbe {
    fn name(&self) -> &str {
        "probe"
    }
    fn actions(&self) -> Vec<pacewright_core::model::ActionSpec> {
        vec![pacewright_core::model::ActionSpec {
            name: "work".into(),
            description: "sleeps so overlap is observable".into(),
            params_schema: serde_json::json!({}),
            limit_keys: vec![],
        }]
    }
    async fn execute(
        &self,
        _ctx: &pacewright_core::adapter::RunCtx,
        _action: &str,
        _params: serde_json::Value,
    ) -> Result<serde_json::Value, pacewright_core::model::AdapterError> {
        use std::sync::atomic::Ordering;
        let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_seen.fetch_max(now, Ordering::SeqCst);
        // Long enough that a concurrent dispatcher would demonstrably overlap these.
        tokio::time::sleep(Duration::from_millis(300)).await;
        self.in_flight.fetch_sub(1, Ordering::SeqCst);
        self.executed.fetch_add(1, Ordering::SeqCst);
        Ok(serde_json::json!({"ok": true}))
    }
}

/// Pins the property that makes a foreground *mutex* unnecessary: the daemon's tick loop awaits
/// each task before claiming the next, so tasks NEVER overlap.
///
/// This matters because every site now shares ONE attached Chrome, in which exactly one tab can be
/// foreground. `foreground #true` (Riverside renders) is only safe without a lock while this holds.
/// If someone makes the dispatch loop concurrent, this test fails FIRST — read it as: you have just
/// made two recipes able to fight over the foreground tab, so `foreground` now needs real
/// serialization. See docs/plans/2026-07-16-single-chrome-attach.md, Phase 4.
#[tokio::test]
async fn foreground_serialization_is_load_bearing() {
    use std::sync::atomic::Ordering;
    let dir = std::env::temp_dir().join(format!("pw-e2e-serial-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let sock = dir.join("pw.sock");

    let in_flight = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let max_seen = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let executed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let store = Arc::new(Store::open_in_memory().unwrap());
    let mut reg = AdapterRegistry::new();
    reg.register(Arc::new(ConcurrencyProbe {
        in_flight: in_flight.clone(),
        max_seen: max_seen.clone(),
        executed: executed.clone(),
    }));
    let engine = Arc::new(Mutex::new(Engine::new(
        store,
        reg,
        Config::default(),
        Arc::new(SystemClock),
        Arc::new(SeededRng::new(1)),
    )));
    let registry = Arc::new(RecipeRegistry::new());
    let recipe_runner: Arc<dyn pacewright_adapter_recipe::RecipeRunner> =
        Arc::new(CliRecipeRunner::new());
    let auth = Arc::new(AuthManager::new(
        registry.clone(),
        recipe_runner.clone(),
        Arc::new(CliLoginLauncher::new()),
    ));
    let srv = Arc::new(Server {
        engine,
        registry: std::sync::RwLock::new(registry),
        recipes_dir: dir.join("recipes"),
        recipe_runner,
        schedules_dir: dir.join("schedules"),
        auth,
    });
    let sock2 = sock.clone();
    tokio::spawn(async move {
        serve(srv, &sock2).await.unwrap();
    });
    for _ in 0..50 {
        if sock.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // Queue three tasks that are all runnable in the same tick.
    for _ in 0..3 {
        client_call(
            &sock,
            Request::Add(AddTaskReq {
                adapter: "probe".into(),
                action: "work".into(),
                params: serde_json::json!({}),
                scheduled_for: None,
                recurrence: None,
                depends_on: None,
                priority: None,
                dedup_key: None,
                max_attempts: None,
            }),
        )
        .await;
    }

    // Poll for completion rather than sleeping a fixed budget: the tick loop wakes on a 1s
    // interval and then runs 3 × 300ms serially, so a fixed sleep that looks generous in isolation
    // gets flaky under a loaded `cargo test --workspace`. Wait for the work to actually finish.
    let mut done = false;
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if executed.load(Ordering::SeqCst) == 3 && in_flight.load(Ordering::SeqCst) == 0 {
            done = true;
            break;
        }
    }
    assert!(
        done,
        "probe tasks never finished (executed={}, in_flight={}) — the assertion below would be \
         measuring nothing",
        executed.load(Ordering::SeqCst),
        in_flight.load(Ordering::SeqCst)
    );

    assert_eq!(
        max_seen.load(Ordering::SeqCst),
        1,
        "tasks must never overlap — the dispatch loop awaits each before claiming the next. \
         If this now exceeds 1, `foreground #true` needs a real mutex (see Phase 4)."
    );
}

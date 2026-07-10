use pacewright_core::adapter::AdapterRegistry;
use pacewright_core::clock::SystemClock;
use pacewright_core::config::Config;
use pacewright_core::engine::Engine;
use pacewright_core::rng::SeededRng;
use pacewright_core::store::Store;
use pacewright_adapter_dummy::DummyAdapter;
use pacewright_adapter_recipe::{AuthManager, CliLoginLauncher, CliRecipeRunner, RecipeRegistry};
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
    let engine = Arc::new(Mutex::new(Engine::new(store, reg, Config::default(), Arc::new(SystemClock), Arc::new(SeededRng::new(1)))));
    let registry = Arc::new(RecipeRegistry::new());
    let auth = Arc::new(AuthManager::new(
        registry.clone(),
        Arc::new(CliRecipeRunner::new()),
        Arc::new(CliLoginLauncher::new()),
    ));
    let srv = Arc::new(Server {
        engine,
        registry,
        schedules_dir: dir.join("schedules"),
        auth,
    });

    let sock2 = sock.clone();
    tokio::spawn(async move { serve(srv, &sock2).await.unwrap(); });
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

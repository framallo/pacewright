use anyhow::Result;
use pacewright_core::engine::Engine;
use pacewright_core::model::{Task, TaskStatus};
use pacewright_proto::{Request, Response};
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
            // M1: pause/resume are accepted but no-op beyond acknowledging; full impl in a later task.
            // NOTE: the brief's or-pattern `Request::Pause { scope } | Request::Resume { scope: _scope @ scope }`
            // does not compile (binding-mode mismatch across or-pattern arms), so this is split into two
            // arms that return the identical acknowledgment JSON.
            Request::Pause { scope } => {
                Ok(serde_json::json!({ "scope": scope, "note": "acknowledged" }))
            }
            Request::Resume { scope } => {
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
    let listener = match UnixListener::bind(socket_path) {
        Ok(l) => l,
        Err(err) if err.kind() == std::io::ErrorKind::AddrInUse => {
            anyhow::bail!("another pacewrightd instance is already listening on {}", socket_path.display());
        }
        Err(err) => return Err(err.into()),
    };

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
    use pacewright_proto::AddTaskReq;

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

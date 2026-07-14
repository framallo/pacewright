use anyhow::Result;
use pacewright_adapter_recipe::{schedule, AuthManager, RecipeRegistry};
use pacewright_core::config::LimitConfig;
use pacewright_core::engine::Engine;
use pacewright_core::runner::execute_and_record;
use pacewright_core::model::{Task, TaskStatus};
use pacewright_proto::{Request, Response};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Mutex;

/// Everything a request handler needs: the engine, plus the recipe registry and schedules
/// directory the declarative-scheduler RPCs read, and the auth manager the `Auth*` RPCs drive.
pub struct Server {
    pub engine: Arc<Mutex<Engine>>,
    pub registry: Arc<RecipeRegistry>,
    pub schedules_dir: PathBuf,
    pub auth: Arc<AuthManager>,
}

/// A login's bounded post-open poll: recheck every 4 s, up to 5 minutes, then give up and clear
/// `logging_in`. Matches the spec's "poll `login.check` every few seconds (bounded, e.g. 5 min)".
const LOGIN_POLL_EVERY: Duration = Duration::from_secs(4);
const LOGIN_POLL_MAX_RECHECKS: u32 = 75;

/// Serialize one `AccountInfo` to the `AuthList` row shape (`signed_in` is `true|false|null`).
fn account_json(a: &pacewright_adapter_recipe::AccountInfo) -> serde_json::Value {
    serde_json::json!({
        "account": a.account,
        "login_url": a.login_url,
        "recipes": a.recipes,
        "signed_in": a.status.signed_in,
        "last_checked": a.status.last_checked_ms,
        "logging_in": a.status.logging_in,
    })
}

/// Spawn the detached bounded poll that flips an account green once its check passes after a login.
/// Captures the engine clock so `last_checked` stamps stay on the daemon's clock.
fn spawn_login_poll(srv: &Server, account: String, clock: Arc<dyn pacewright_core::clock::Clock>) {
    let auth = srv.auth.clone();
    let now = Arc::new(move || clock.now_ms());
    tokio::spawn(auth.poll_until_signed_in(account, LOGIN_POLL_EVERY, LOGIN_POLL_MAX_RECHECKS, now));
}

/// The `Auth*` RPCs, handled outside the engine-locked synchronous dispatch because they spawn
/// subprocesses and detached polls. Returns `Some(response)` for an auth request, `None` otherwise.
async fn handle_auth(srv: &Server, req: &Request) -> Option<Response> {
    let clock = { srv.engine.lock().await.clock.clone() };
    let now = clock.now_ms();
    let res: Result<serde_json::Value, String> = match req {
        Request::AuthList => {
            let rows: Vec<_> = srv.auth.list().iter().map(account_json).collect();
            Ok(serde_json::json!({ "accounts": rows }))
        }
        Request::AuthRecheck { account } => match account {
            Some(a) => srv.auth.recheck(a, now).await.map(|_| {
                serde_json::json!({ "accounts": srv.auth.list().iter().map(account_json).collect::<Vec<_>>() })
            }),
            None => {
                for row in srv.auth.list() {
                    let _ = srv.auth.recheck(&row.account, now).await;
                }
                Ok(serde_json::json!({ "accounts": srv.auth.list().iter().map(account_json).collect::<Vec<_>>() }))
            }
        },
        Request::AuthLogin { account } => match srv.auth.login(account).await {
            Ok(()) => {
                spawn_login_poll(srv, account.clone(), clock.clone());
                Ok(serde_json::json!({ "account": account, "logging_in": true }))
            }
            Err(e) => Err(e),
        },
        Request::AuthLoginAll => {
            let mut opened = Vec::new();
            // Open one window at a time for every account not already known to be signed in.
            for row in srv.auth.list() {
                if row.status.signed_in != Some(true) && srv.auth.login(&row.account).await.is_ok() {
                    spawn_login_poll(srv, row.account.clone(), clock.clone());
                    opened.push(row.account);
                }
            }
            Ok(serde_json::json!({ "opened": opened }))
        }
        _ => return None,
    };
    Some(match res {
        Ok(v) => Response::Ok(v),
        Err(message) => Response::Error { message },
    })
}

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

/// Set a schedule entry's runtime enabled override, then reconcile so it takes effect
/// immediately (enable → enqueue; disable → cancel its live task).
fn set_enabled(e: &Engine, srv: &Server, id: &str, enabled: bool, now: i64) -> Result<serde_json::Value, String> {
    e.store.schedule_state_set(id, enabled, now).map_err(|err| err.to_string())?;
    let (entries, _) = schedule::load_dir(&srv.schedules_dir);
    let (valid, _) = schedule::partition(&entries, &srv.registry);
    schedule::reconcile(&e.store, &*e.clock, &valid, false).map_err(|err| err.to_string())?;
    Ok(serde_json::json!({ "id": id, "enabled": enabled }))
}

pub async fn handle_request(srv: &Server, req: Request) -> Response {
    if let Some(resp) = handle_auth(srv, &req).await {
        return resp;
    }
    let mut e = srv.engine.lock().await;
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
            Request::Limits => {
                let date = pacewright_core::limits::local_date_str(e.clock.now_ms());
                let counters = e.store.list_counters(&date).map_err(|e| e.to_string())?;
                let counters: Vec<_> = counters
                    .into_iter()
                    .map(|(key, count, last_spent_at)| serde_json::json!({
                        "key": key,
                        "count": count,
                        "last_spent_at": last_spent_at,
                    }))
                    .collect();
                Ok(serde_json::json!({ "date": date, "counters": counters }))
            }
            Request::Adapters => {
                let list: Vec<_> = e.registry.all().iter().map(|a| serde_json::json!({ "name": a.name(), "actions": a.actions() })).collect();
                Ok(serde_json::json!({ "adapters": list }))
            }
            Request::Status => {
                let pending = e.store.tasks_in_status(TaskStatus::Pending).map_err(|e| e.to_string())?.len();
                let running = e.store.tasks_in_status(TaskStatus::Running).map_err(|e| e.to_string())?.len();
                Ok(serde_json::json!({ "pending": pending, "running": running, "paused": e.paused_scopes() }))
            }
            Request::Pause { scope } => {
                e.pause(scope.clone());
                Ok(serde_json::json!({ "scope": scope, "paused": true }))
            }
            Request::Resume { scope } => {
                e.resume(&scope);
                Ok(serde_json::json!({ "scope": scope, "paused": false }))
            }
            Request::Subscribe => Ok(serde_json::json!({ "note": "subscribe stream not enabled on this request path" })),

            Request::ScheduleApply { prune } => {
                let (entries, mut errors) = schedule::load_dir(&srv.schedules_dir);
                let (valid, verrs) = schedule::partition(&entries, &srv.registry);
                errors.extend(verrs);
                let report = schedule::reconcile(&e.store, &*e.clock, &valid, prune).map_err(|e| e.to_string())?;
                Ok(serde_json::json!({
                    "created": report.created,
                    "updated": report.updated,
                    "canceled": report.canceled,
                    "errors": errors,
                }))
            }
            Request::ScheduleList => {
                let (entries, errors) = schedule::load_dir(&srv.schedules_dir);
                let overrides = e.store.schedule_state_all().map_err(|e| e.to_string())?;
                let list: Vec<_> = entries
                    .iter()
                    .map(|en| {
                        let effective = overrides.get(&en.id).copied().unwrap_or(en.enabled);
                        let live = e.store.find_active_by_dedup(&en.dedup_key()).ok().flatten();
                        let (every, at) = match &en.timing {
                            schedule::Timing::Every(c) => (Some(c.clone()), None),
                            schedule::Timing::At(ms) => (None, Some(*ms)),
                            schedule::Timing::OnApply => (None, None),
                        };
                        serde_json::json!({
                            "id": en.id,
                            "recipe": en.recipe,
                            "every": every,
                            "at": at,
                            "enabled_default": en.enabled,
                            "enabled": effective,
                            "next_fire": en.next_fire(now),
                            "live_status": live.as_ref().map(|t| t.status.as_str()),
                            "last_error": live.as_ref().and_then(|t| t.last_error.clone()),
                        })
                    })
                    .collect();
                Ok(serde_json::json!({ "schedules": list, "errors": errors }))
            }
            Request::ScheduleEnable { id } => set_enabled(&e, srv, &id, true, now),
            Request::ScheduleDisable { id } => set_enabled(&e, srv, &id, false, now),
            Request::SetLimit { key, config } => {
                let cfg = LimitConfig::from_parts(
                    config.daily_cap,
                    config.min_gap.as_deref(),
                    config.jitter,
                    config.active.as_deref(),
                )
                .map_err(|e| e.to_string())?;
                e.store.limit_override_set(&key, &cfg, now).map_err(|e| e.to_string())?;
                e.cfg.set_limit(key.clone(), cfg);
                Ok(serde_json::json!({ "key": key, "set": true }))
            }
            // Auth RPCs are intercepted by `handle_auth` before this synchronous dispatch.
            Request::AuthList
            | Request::AuthRecheck { .. }
            | Request::AuthLogin { .. }
            | Request::AuthLoginAll => unreachable!("auth requests are handled by handle_auth"),
        }
    })();
    match res {
        Ok(v) => Response::Ok(v),
        Err(message) => Response::Error { message },
    }
}

async fn handle_conn(srv: Arc<Server>, stream: UnixStream) {
    let (r, mut w) = stream.into_split();
    let mut lines = BufReader::new(r).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if line.trim().is_empty() { continue; }
        let resp = match serde_json::from_str::<Request>(&line) {
            Ok(req) => handle_request(&srv, req).await,
            Err(e) => Response::Error { message: format!("bad request: {e}") },
        };
        let mut out = serde_json::to_string(&resp).unwrap();
        out.push('\n');
        if w.write_all(out.as_bytes()).await.is_err() { break; }
    }
}

pub async fn serve(srv: Arc<Server>, socket_path: &Path) -> Result<()> {
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
        let engine = srv.engine.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
            loop {
                interval.tick().await;
                // Drain every runnable task this tick, but hold the engine lock ONLY to *claim*
                // each task (a few fast store ops) — then release it and run the slow browser
                // subprocess unlocked, so `add`/`status`/`list`/the TUI/web stay responsive while a
                // task runs. Each execute is isolated in its own task so an adapter/store panic
                // can't take down the loop and silently freeze the scheduler.
                loop {
                    let claimed = {
                        let e = engine.lock().await;
                        match e.claim_one() {
                            Ok(Some(c)) => Some((c, e.store.clone(), e.clock.clone(), e.browser.clone())),
                            Ok(None) => None,
                            Err(err) => {
                                tracing::error!("claim error: {err}");
                                None
                            }
                        }
                    };
                    let Some((claimed, store, clock, browser)) = claimed else { break };
                    let handle = tokio::spawn(async move {
                        execute_and_record(&store, &*claimed.adapter, &*clock, browser, claimed.task).await
                    });
                    match handle.await {
                        Ok(Ok(())) => {}
                        Ok(Err(run_err)) => tracing::error!("task run error: {run_err}"),
                        Err(join_err) if join_err.is_panic() => {
                            tracing::error!("task panicked: {join_err}");
                        }
                        Err(join_err) => tracing::error!("task failed: {join_err}"),
                    }
                }
            }
        });
    }

    loop {
        let (stream, _) = listener.accept().await?;
        let srv = srv.clone();
        tokio::spawn(handle_conn(srv, stream));
    }
}

/// Shared fixtures for the dispatch tests here and the web-layer tests in `web.rs` (same crate).
#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use pacewright_core::adapter::AdapterRegistry;
    use pacewright_core::clock::SystemClock;
    use pacewright_core::config::Config;
    use pacewright_core::rng::SeededRng;
    use pacewright_core::store::Store;
    use pacewright_adapter_dummy::DummyAdapter;

    pub fn scratch(prefix: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!(
            "{prefix}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    pub async fn test_server() -> Server {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let mut reg = AdapterRegistry::new();
        reg.register(Arc::new(DummyAdapter::new()));
        let e = Engine::new(store, reg, Config::default(), Arc::new(SystemClock), Arc::new(SeededRng::new(1)));
        let registry = Arc::new(RecipeRegistry::new());
        Server {
            engine: Arc::new(Mutex::new(e)),
            auth: auth_for(registry.clone()),
            registry,
            schedules_dir: std::env::temp_dir().join("pcw-no-such-schedules-dir"),
        }
    }

    /// An `AuthManager` over the given registry for tests. Uses real Cli backends, but `AuthList`
    /// (the only auth RPC exercised in dispatch tests) never spawns them; login/recheck are covered
    /// by the `adapter-recipe` unit tests against a faked runner/launcher.
    pub fn auth_for(registry: Arc<RecipeRegistry>) -> Arc<AuthManager> {
        Arc::new(AuthManager::new(
            registry,
            Arc::new(pacewright_adapter_recipe::CliRecipeRunner::new()),
            Arc::new(pacewright_adapter_recipe::CliLoginLauncher::new()),
        ))
    }

    /// A server whose recipe registry has one `dummy/echo` recipe and whose schedules dir holds
    /// the given TOML — enough to exercise the declarative-scheduler RPCs end to end.
    pub async fn test_server_with_schedule(schedule_toml: &str) -> Server {
        let mut srv = test_server().await;
        let recipes = scratch("pcw-srv-recipes");
        std::fs::write(recipes.join("echo.kdl"), "recipe \"dummy/echo\" {}\n").unwrap();
        srv.registry = Arc::new(RecipeRegistry::load_dir(&recipes));
        srv.auth = auth_for(srv.registry.clone());
        let sched = scratch("pcw-srv-schedules");
        std::fs::write(sched.join("s.toml"), schedule_toml).unwrap();
        srv.schedules_dir = sched;
        srv
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::test_support::{auth_for, scratch, test_server, test_server_with_schedule};
    use pacewright_proto::AddTaskReq;

    #[tokio::test]
    async fn test_auth_list_reports_accounts_with_recipes_and_unknown_status() {
        let mut srv = test_server().await;
        let recipes = scratch("pcw-srv-auth");
        std::fs::create_dir_all(recipes.join("accounts")).unwrap();
        std::fs::write(
            recipes.join("accounts/prevetted-riverside.kdl"),
            "recipe \"accounts/prevetted-riverside\" { login-url \"https://riverside.com/login\"\n step { goto \"https://riverside.com/dashboard\" } }",
        )
        .unwrap();
        std::fs::write(
            recipes.join("rv.kdl"),
            "recipe \"riverside/generate_magic_clips\" { auth account=\"prevetted-riverside\" }",
        )
        .unwrap();
        srv.registry = Arc::new(RecipeRegistry::load_dir(&recipes));
        srv.auth = auth_for(srv.registry.clone());

        let resp = handle_request(&srv, Request::AuthList).await;
        match resp {
            Response::Ok(v) => {
                let accts = v["accounts"].as_array().unwrap();
                assert_eq!(accts.len(), 1);
                assert_eq!(accts[0]["account"], "prevetted-riverside");
                assert_eq!(accts[0]["login_url"], "https://riverside.com/login");
                assert_eq!(accts[0]["recipes"][0], "riverside/generate_magic_clips");
                assert!(accts[0]["signed_in"].is_null(), "unknown before any check");
                assert_eq!(accts[0]["logging_in"], false);
            }
            _ => panic!("auth_list failed"),
        }
    }

    #[tokio::test]
    async fn test_auth_login_unknown_account_errors() {
        let srv = test_server().await;
        let resp = handle_request(&srv, Request::AuthLogin { account: "nope".into() }).await;
        assert!(matches!(resp, Response::Error { .. }), "got {resp:?}");
    }

    #[tokio::test]
    async fn test_add_then_get_via_dispatch() {
        let srv = test_server().await;
        let add = Request::Add(AddTaskReq {
            adapter: "dummy".into(), action: "echo".into(), params: serde_json::json!({"a":1}),
            scheduled_for: None, recurrence: None, depends_on: None, priority: None, dedup_key: None, max_attempts: None,
        });
        let resp = handle_request(&srv, add).await;
        let id = match resp { Response::Ok(v) => v["id"].as_str().unwrap().to_string(), _ => panic!("add failed") };
        let get = handle_request(&srv, Request::Get { id: id.clone() }).await;
        match get { Response::Ok(v) => assert_eq!(v["task"]["action"], "echo"), _ => panic!("get failed") };
    }

    #[tokio::test]
    async fn test_schedule_apply_list_and_toggle() {
        let srv = test_server_with_schedule("[[task]]\nid = \"t1\"\nrecipe = \"dummy/echo\"\n").await;

        // apply → one task created for the entry
        let resp = handle_request(&srv, Request::ScheduleApply { prune: false }).await;
        match resp {
            Response::Ok(v) => {
                assert_eq!(v["created"][0], "t1");
                assert!(v["errors"].as_array().unwrap().is_empty());
            }
            _ => panic!("apply failed"),
        }

        // list → the catalog shows it enabled with a live status
        let resp = handle_request(&srv, Request::ScheduleList).await;
        match resp {
            Response::Ok(v) => {
                assert_eq!(v["schedules"][0]["id"], "t1");
                assert_eq!(v["schedules"][0]["enabled"], true);
                assert_eq!(v["schedules"][0]["live_status"], "pending");
            }
            _ => panic!("list failed"),
        }

        // disable → its live task is canceled and the catalog shows it off
        handle_request(&srv, Request::ScheduleDisable { id: "t1".into() }).await;
        let resp = handle_request(&srv, Request::ScheduleList).await;
        match resp {
            Response::Ok(v) => {
                assert_eq!(v["schedules"][0]["enabled"], false);
                assert!(v["schedules"][0]["live_status"].is_null());
            }
            _ => panic!("list failed"),
        }

        // enable again → re-queued
        handle_request(&srv, Request::ScheduleEnable { id: "t1".into() }).await;
        let resp = handle_request(&srv, Request::ScheduleList).await;
        match resp {
            Response::Ok(v) => assert_eq!(v["schedules"][0]["live_status"], "pending"),
            _ => panic!("list failed"),
        }
    }

    #[tokio::test]
    async fn test_schedule_apply_reports_invalid_entries() {
        // `bad` references a missing recipe; `t1` is fine — apply the good one, report the bad.
        let toml = "[[task]]\nid=\"t1\"\nrecipe=\"dummy/echo\"\n[[task]]\nid=\"bad\"\nrecipe=\"no/such\"\n";
        let srv = test_server_with_schedule(toml).await;
        let resp = handle_request(&srv, Request::ScheduleApply { prune: false }).await;
        match resp {
            Response::Ok(v) => {
                assert_eq!(v["created"], serde_json::json!(["t1"]));
                assert!(v["errors"].as_array().unwrap().iter().any(|e| e.as_str().unwrap().contains("no recipe")));
            }
            _ => panic!("apply failed"),
        }
    }

    #[tokio::test]
    async fn test_set_limit_persists_override() {
        let srv = test_server().await;
        let resp = handle_request(&srv, Request::SetLimit {
            key: "dummy.capped".into(),
            config: pacewright_proto::LimitSpec { daily_cap: Some(3), min_gap: Some("8m".into()), jitter: Some(0.5), active: None },
        }).await;
        assert!(matches!(resp, Response::Ok(_)));
        let e = srv.engine.lock().await;
        // in-memory cfg updated
        assert_eq!(e.cfg.limit_for("dummy.capped").daily_cap, 3);
        assert_eq!(e.cfg.limit_for("dummy.capped").min_gap_ms, 8 * 60_000);
        // and persisted for the next boot
        let stored = e.store.limit_overrides_all().unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].1.daily_cap, 3);
    }

    #[tokio::test]
    async fn test_bad_adapter_lists_adapters() {
        let srv = test_server().await;
        let resp = handle_request(&srv, Request::Adapters).await;
        match resp { Response::Ok(v) => assert_eq!(v["adapters"][0]["name"], "dummy"), _ => panic!() };
    }

    #[tokio::test]
    async fn test_pause_resume_via_dispatch() {
        let srv = test_server().await;

        // pause a specific adapter scope -> echoed back, and reflected in status
        let resp = handle_request(&srv, Request::Pause { scope: "dummy".into() }).await;
        match resp {
            Response::Ok(v) => { assert_eq!(v["scope"], "dummy"); assert_eq!(v["paused"], true); }
            _ => panic!("pause failed"),
        }
        let status = handle_request(&srv, Request::Status).await;
        match status {
            Response::Ok(v) => {
                let scopes: Vec<String> = serde_json::from_value(v["paused"].clone()).unwrap();
                assert!(scopes.contains(&"dummy".to_string()));
                assert!(v["pending"].is_number());
                assert!(v["running"].is_number());
            }
            _ => panic!("status failed"),
        }

        // "daemon" is the alias for the "all" scope; status should surface "all"
        handle_request(&srv, Request::Pause { scope: "daemon".into() }).await;
        let status = handle_request(&srv, Request::Status).await;
        match status {
            Response::Ok(v) => {
                let scopes: Vec<String> = serde_json::from_value(v["paused"].clone()).unwrap();
                assert!(scopes.contains(&"all".to_string()));
            }
            _ => panic!("status failed"),
        }

        // resume both -> paused set drains to empty
        handle_request(&srv, Request::Resume { scope: "dummy".into() }).await;
        let resp = handle_request(&srv, Request::Resume { scope: "daemon".into() }).await;
        match resp {
            Response::Ok(v) => { assert_eq!(v["scope"], "daemon"); assert_eq!(v["paused"], false); }
            _ => panic!("resume failed"),
        }
        let status = handle_request(&srv, Request::Status).await;
        match status {
            Response::Ok(v) => {
                let scopes: Vec<String> = serde_json::from_value(v["paused"].clone()).unwrap();
                assert!(scopes.is_empty(), "expected no paused scopes, got {scopes:?}");
            }
            _ => panic!("status failed"),
        }
    }
}

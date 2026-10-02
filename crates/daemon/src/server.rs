use anyhow::Result;
use pacewright_adapter_agent::{AgentAdapter, AnthropicCompleter, Completer};
use pacewright_adapter_dummy::DummyAdapter;
use pacewright_adapter_recipe::{
    schedule, AuthManager, RecipeAdapter, RecipeRegistry, RecipeRunner, RecipeSrcAdapter,
    RECIPE_SRC_ADAPTER,
};
use pacewright_core::adapter::AdapterRegistry;
use pacewright_core::config::LimitConfig;
use pacewright_core::engine::Engine;
use pacewright_core::model::{Task, TaskStatus};
use pacewright_core::runner::execute_and_record;
use pacewright_proto::{Request, Response};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Mutex;

/// Everything a request handler needs: the engine, plus the recipe registry and schedules
/// directory the declarative-scheduler RPCs read, and the auth manager the `Auth*` RPCs drive.
///
/// `registry` is behind an `RwLock` so `RecipeReload` can swap in a freshly-loaded registry (and
/// rebuild the engine's adapters) without a daemon restart. Reads clone the `Arc` and drop the lock
/// immediately. Note the `AuthManager` keeps its own boot-time registry `Arc`, so *account* recipes
/// (`accounts/*`) still require a restart to change — only task recipes hot-reload.
pub struct Server {
    pub engine: Arc<Mutex<Engine>>,
    pub registry: RwLock<Arc<RecipeRegistry>>,
    pub recipes_dir: PathBuf,
    pub recipe_runner: Arc<dyn RecipeRunner>,
    pub schedules_dir: PathBuf,
    pub auth: Arc<AuthManager>,
}

/// Build the engine's adapter set from the recipe registry: the built-in `DummyAdapter` plus one
/// `RecipeAdapter` per distinct `<adapter>` prefix among the installed recipes. Built-ins win a name
/// collision (a `dummy/*` recipe can't shadow the real `DummyAdapter`). Shared by daemon boot and
/// `RecipeReload` so both produce an identical adapter set.
pub fn build_adapter_registry(
    recipe_registry: &Arc<RecipeRegistry>,
    recipe_runner: &Arc<dyn RecipeRunner>,
    store: &Arc<pacewright_core::store::Store>,
    clock: &Arc<dyn pacewright_core::clock::Clock>,
) -> AdapterRegistry {
    let mut reg = AdapterRegistry::new();
    reg.register(Arc::new(DummyAdapter::new()));
    // The reasoning adapter: `agent/ask` + `agent/adjudicate` (aliased `claude/*`), backed by the
    // Anthropic Messages API. One shared transport; auth comes from the daemon's env at call time.
    // A recipe prefix `agent`/`claude` cannot shadow these (built-ins win the collision below).
    let completer: Arc<dyn Completer> = Arc::new(AnthropicCompleter::new());
    reg.register(Arc::new(AgentAdapter::with_completer(
        "agent",
        completer.clone(),
    )));
    reg.register(Arc::new(AgentAdapter::with_completer("claude", completer)));
    // The full `claude -p` agent step (filesystem + tools), distinct from the single-turn `agent`.
    // Absorbs content-drafting and paced commenting rounds with a daemon-owned cap (R1+R4).
    reg.register(Arc::new(
        pacewright_adapter_agent::claude_cli::ClaudeCliAdapter::new("claude_cli"),
    ));
    // The built-in pipeline launcher: `pipeline/start` kicks a fresh dated run so a scan-then-act
    // pipeline can recur on the declarative schedule.
    reg.register(Arc::new(crate::pipeline_adapter::PipelineAdapter::new(
        store.clone(),
        clock.clone(),
    )));
    // Non-browser built-ins: `data/*` persists task output as JSON datasets; `http/request` sources
    // from a REST API with env-injected secrets. Together they let scan/sourcing/generation jobs run
    // without Chrome (Apollo, personalize-hooks, gen-*).
    reg.register(Arc::new(crate::data_adapter::DataAdapter::new(
        pacewright_core::datastore::Datastore::new(pacewright_core::run::home_dir().join("data")),
    )));
    reg.register(Arc::new(crate::http_adapter::HttpAdapter::new()));
    // `recipe_src/run`: a recipe handed over as source by the `run_src` RPC (a backend's database
    // row), run through the same runner as the installed recipes below.
    reg.register(Arc::new(RecipeSrcAdapter::new(recipe_runner.clone())));
    for adapter_name in recipe_registry.adapters() {
        if reg.get(&adapter_name).is_some() {
            tracing::warn!("recipe prefix `{adapter_name}` collides with a built-in adapter — skipping the recipe-backed one");
            continue;
        }
        tracing::info!("registering recipe-backed adapter `{adapter_name}`");
        reg.register(Arc::new(RecipeAdapter::new(
            adapter_name,
            recipe_registry.clone(),
            recipe_runner.clone(),
        )));
    }
    reg
}

/// The daemon's default browser idle-reap threshold when `config.toml` doesn't set one: 10 minutes.
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
        // Open the headed login window and stop. We deliberately do NOT drive the browser while the
        // human signs in — an automated recheck loop here would navigate the very profile being
        // logged into, spawning tabs and interrupting sign-in (fatal on a bot-sensitive site). The
        // operator runs `pcw auth recheck <account>` when done; that reads the session and flips the
        // account green (and clears `logging_in`).
        Request::AuthLogin { account } => match srv.auth.login(account).await {
            Ok(()) => Ok(serde_json::json!({ "account": account, "logging_in": true })),
            Err(e) => Err(e),
        },
        Request::AuthLoginAll => {
            let mut opened = Vec::new();
            // Open one window at a time for every account not already known to be signed in.
            for row in srv.auth.list() {
                if row.status.signed_in != Some(true) && srv.auth.login(&row.account).await.is_ok() {
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
fn set_enabled(
    e: &Engine,
    srv: &Server,
    id: &str,
    enabled: bool,
    now: i64,
) -> Result<serde_json::Value, String> {
    e.store
        .schedule_state_set(id, enabled, now)
        .map_err(|err| err.to_string())?;
    let registry = srv.registry.read().unwrap().clone();
    let (entries, _) = schedule::load_dir(&srv.schedules_dir);
    let (valid, _) = schedule::partition(&entries, &registry);
    schedule::reconcile(&e.store, &*e.clock, &valid, false).map_err(|err| err.to_string())?;
    Ok(serde_json::json!({ "id": id, "enabled": enabled }))
}

/// Login OAuth de Claude en dos pasos, SIN loopback — para producción, donde el
/// server no tiene navegador ni acceso a localhost. Va antes del cierre
/// síncrono de `handle_request` porque el intercambio del código es asíncrono.
///   - `AnthropicLoginUrl`: arma la URL a abrir + el PKCE (`verifier`+`state`)
///     que el cliente guarda; el daemon no guarda estado entre pasos.
///   - `AnthropicLoginSubmit`: cambia el código pegado por tokens y los guarda
///     en `~/.pacewright/secrets.json` (0600, auto-refresh al usarlos).
async fn handle_anthropic_login(req: &Request) -> Option<Response> {
    use pacewright_adapter_agent::anthropic_oauth as oauth;
    let res: Result<serde_json::Value, String> = match req {
        Request::AnthropicLoginUrl => {
            let pkce = oauth::generate_pkce();
            let state = oauth::generate_pkce().verifier;
            let url = oauth::build_authorize_url(&state, oauth::REDIRECT_URI, &pkce.challenge);
            Ok(serde_json::json!({
                "authorize_url": url,
                "verifier": pkce.verifier,
                "state": state,
            }))
        }
        Request::AnthropicLoginSubmit {
            pasted,
            verifier,
            state,
        } => {
            let (code, ret_state) = oauth::split_code_state(pasted.trim(), state);
            let http = oauth::ReqwestTokenHttp::default();
            let now_ms = chrono::Utc::now().timestamp_millis();
            match oauth::exchange_code(
                &http,
                code,
                ret_state,
                oauth::REDIRECT_URI,
                verifier,
                now_ms,
            )
            .await
            {
                Ok(tokens) => {
                    let path = pacewright_core::run::home_dir().join("secrets.json");
                    oauth::store_login(&path, &tokens)
                        .map(|()| serde_json::json!({ "signed_in": true, "email": tokens.email }))
                }
                Err(e) => Err(e),
            }
        }
        _ => return None,
    };
    Some(match res {
        Ok(v) => Response::Ok(v),
        Err(message) => Response::Error { message },
    })
}

pub async fn handle_request(srv: &Server, req: Request) -> Response {
    if let Some(resp) = handle_auth(srv, &req).await {
        return resp;
    }
    if let Some(resp) = handle_anthropic_login(&req).await {
        return resp;
    }
    let mut e = srv.engine.lock().await;
    let now = e.clock.now_ms();
    let res: Result<serde_json::Value, String> = (|| {
        match req {
            Request::Add(a) => {
                let mut t = Task::new_now(
                    a.adapter,
                    a.action,
                    a.params,
                    a.scheduled_for.unwrap_or(now),
                );
                t.recurrence = a.recurrence;
                t.depends_on = a.depends_on;
                t.dedup_key = a.dedup_key;
                if let Some(p) = a.priority {
                    t.priority = p;
                }
                if let Some(m) = a.max_attempts {
                    t.max_attempts = m;
                }
                let id = e.add_task(t).map_err(|e| e.to_string())?;
                Ok(serde_json::json!({ "id": id }))
            }
            Request::RunSrc(r) => {
                if r.recipe_src.trim().is_empty() {
                    return Err("run_src: `recipe_src` is empty".to_string());
                }
                let vars = match r.params {
                    serde_json::Value::Null => serde_json::json!({}),
                    v @ serde_json::Value::Object(_) => v,
                    other => {
                        return Err(format!(
                            "run_src: `params` must be a JSON object of recipe vars, got {other}"
                        ))
                    }
                };
                // Same task shape as `add`, on the built-in source adapter; the vars nest under
                // `vars` so they can never collide with the two reserved keys.
                let mut t = Task::new_now(
                    RECIPE_SRC_ADAPTER,
                    "run",
                    RecipeSrcAdapter::params(&r.recipe_src, r.name.as_deref(), vars),
                    r.scheduled_for.unwrap_or(now),
                );
                t.dedup_key = r.dedup_key;
                if let Some(p) = r.priority {
                    t.priority = p;
                }
                if let Some(m) = r.max_attempts {
                    t.max_attempts = m;
                }
                let id = e.add_task(t).map_err(|e| e.to_string())?;
                Ok(serde_json::json!({ "id": id }))
            }
            Request::Get { id } => {
                let t = e.store.get_task(&id).map_err(|e| e.to_string())?;
                let events = match &t {
                    Some(_) => e.store.events_for(&id).map_err(|e| e.to_string())?,
                    None => vec![],
                };
                Ok(serde_json::json!({ "task": t, "events": events }))
            }
            Request::List {
                status,
                adapter,
                limit,
            } => {
                let mut tasks = e
                    .store
                    .list_tasks(status_from_opt(&status), limit.unwrap_or(100))
                    .map_err(|e| e.to_string())?;
                if let Some(ad) = adapter {
                    tasks.retain(|t| t.adapter == ad);
                }
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
                } else {
                    Err(format!("no task {id}"))
                }
            }
            Request::RunNow { id, force: _ } => {
                if let Some(mut t) = e.store.get_task(&id).map_err(|e| e.to_string())? {
                    t.scheduled_for = now;
                    t.next_eligible_at = None;
                    if t.status == TaskStatus::Deferred {
                        t.status = TaskStatus::Pending;
                    }
                    t.updated_at = now;
                    e.store.update_task(&t).map_err(|e| e.to_string())?;
                    Ok(serde_json::json!({ "ok": true }))
                } else {
                    Err(format!("no task {id}"))
                }
            }
            Request::Limits => {
                let date = pacewright_core::limits::local_date_str(e.clock.now_ms());
                let counters = e.store.list_counters(&date).map_err(|e| e.to_string())?;
                let counters: Vec<_> = counters
                    .into_iter()
                    .map(|(key, count, last_spent_at)| {
                        serde_json::json!({
                            "key": key,
                            "count": count,
                            "last_spent_at": last_spent_at,
                        })
                    })
                    .collect();
                Ok(serde_json::json!({ "date": date, "counters": counters }))
            }
            Request::RunStart {
                pipeline,
                run_id,
                params,
                retry_failed,
            } => {
                // Pipelines live beside recipes: ~/.pacewright/recipes/pipelines/<name>.kdl
                // with '/' in the pipeline name flattened to '-'.
                let dir = std::env::var("PACEWRIGHT_HOME")
                    .map(std::path::PathBuf::from)
                    .unwrap_or_else(|_| {
                        std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default())
                            .join(".pacewright")
                    })
                    .join("recipes/pipelines");
                let file = dir.join(format!("{}.kdl", pipeline.replace('/', "-")));
                let src = std::fs::read_to_string(&file)
                    .map_err(|err| format!("cannot read {}: {err}", file.display()))?;
                let def =
                    pacewright_core::pipeline::parse_pipeline(&src).map_err(|e| e.to_string())?;
                let now = e.clock.now_ms();
                let requeued = if retry_failed {
                    pacewright_core::run::retry_failed(&e.store, &run_id, now)
                        .map_err(|e| e.to_string())?
                } else {
                    0
                };
                let started = pacewright_core::run::start(&e.store, &def, &run_id, &params, now)
                    .map_err(|e| e.to_string())?;
                Ok(serde_json::json!({
                    "run_id": run_id,
                    "pipeline": def.name,
                    "created": started.iter().filter_map(|t| t.step_name.clone()).collect::<Vec<_>>(),
                    "requeued_failed": requeued,
                }))
            }
            Request::RunList => {
                let mut runs: std::collections::BTreeMap<
                    String,
                    serde_json::Map<String, serde_json::Value>,
                > = Default::default();
                for t in e
                    .store
                    .list_tasks(None, i64::MAX)
                    .map_err(|e| e.to_string())?
                {
                    let Some(rid) = t.run_id.clone() else {
                        continue;
                    };
                    let entry = runs.entry(rid).or_default();
                    let k = t.status.as_str().to_string();
                    let n = entry
                        .get(&k)
                        .and_then(serde_json::Value::as_i64)
                        .unwrap_or(0)
                        + 1;
                    entry.insert(k, serde_json::Value::from(n));
                }
                let list: Vec<_> = runs
                    .into_iter()
                    .map(|(id, counts)| serde_json::json!({ "run_id": id, "steps": counts }))
                    .collect();
                Ok(serde_json::json!({ "runs": list }))
            }
            Request::RunShow { run_id } => {
                let tasks = e.store.tasks_in_run(&run_id).map_err(|e| e.to_string())?;
                // Collapse the pipeline's `output` block into a report of (verified) values.
                let output = pacewright_core::run::resolve_output(&tasks);
                let steps: Vec<_> = tasks
                    .into_iter()
                    .map(|t| {
                        let name = t.step_name.clone().unwrap_or_default();
                        // A `<step>.verify` task is the independent confirmation; naming
                        // the kind here is what makes "verified" visible in the report.
                        let kind = if name == pacewright_core::run::VARS_STEP {
                            "vars"
                        } else if name.ends_with(".verify") {
                            "verify"
                        } else {
                            "step"
                        };
                        serde_json::json!({
                            "step": name,
                            "kind": kind,
                            "status": t.status.as_str(),
                            "attempts": t.attempts,
                            "error": t.last_error,
                            "result": t.result,
                        })
                    })
                    .collect();
                Ok(serde_json::json!({ "run_id": run_id, "steps": steps, "output": output }))
            }
            Request::Adapters => {
                let list: Vec<_> = e
                    .registry
                    .all()
                    .iter()
                    .map(|a| serde_json::json!({ "name": a.name(), "actions": a.actions() }))
                    .collect();
                Ok(serde_json::json!({ "adapters": list }))
            }
            Request::Status => {
                let pending = e
                    .store
                    .tasks_in_status(TaskStatus::Pending)
                    .map_err(|e| e.to_string())?
                    .len();
                let running = e
                    .store
                    .tasks_in_status(TaskStatus::Running)
                    .map_err(|e| e.to_string())?
                    .len();
                Ok(
                    serde_json::json!({ "pending": pending, "running": running, "paused": e.paused_scopes() }),
                )
            }
            Request::Pause { scope } => {
                e.pause(scope.clone()).map_err(|e| e.to_string())?;
                Ok(serde_json::json!({ "scope": scope, "paused": true }))
            }
            Request::Resume { scope } => {
                e.resume(&scope).map_err(|e| e.to_string())?;
                Ok(serde_json::json!({ "scope": scope, "paused": false }))
            }
            Request::Digest => {
                use chrono::{Local, TimeZone};
                let all = e
                    .store
                    .list_tasks(None, i64::MAX)
                    .map_err(|e| e.to_string())?;
                let now = Local::now();
                let midnight_ms = now
                    .date_naive()
                    .and_hms_opt(0, 0, 0)
                    .and_then(|d| Local.from_local_datetime(&d).single())
                    .map(|dt| dt.timestamp_millis())
                    .unwrap_or(0);
                let today = |t: &Task| t.finished_at.is_some_and(|f| f >= midnight_ms);
                let (mut ran, mut running, mut queued) = (0i64, 0i64, 0i64);
                let mut failed = Vec::new();
                for t in &all {
                    match t.status {
                        TaskStatus::Succeeded if today(t) => ran += 1,
                        TaskStatus::Failed if today(t) => failed.push(serde_json::json!({
                            "id": t.id, "adapter": t.adapter, "action": t.action,
                            "step": t.step_name, "run": t.run_id, "error": t.last_error,
                        })),
                        TaskStatus::Running => running += 1,
                        TaskStatus::Pending | TaskStatus::Blocked | TaskStatus::Deferred => {
                            queued += 1
                        }
                        _ => {}
                    }
                }
                let paused = e.paused_scopes();
                let escalations = crate::notify::list_escalations(false);
                let failed_count = failed.len();
                Ok(serde_json::json!({
                    "date": now.format("%Y-%m-%d").to_string(),
                    "ran_today": ran,
                    "running": running,
                    "queued": queued,
                    "failed_today": failed,
                    "paused_scopes": paused.clone(),
                    "escalations": escalations.len(),
                    // What needs a human: scopes to resume, failures to fix, and issues Claude was
                    // asked to triage.
                    "waiting_on_human": {
                        "paused_scopes": paused,
                        "failed_count": failed_count,
                        "escalations": escalations.len(),
                    },
                }))
            }
            Request::Escalations { drain } => {
                let items = crate::notify::list_escalations(drain);
                Ok(serde_json::json!({ "count": items.len(), "escalations": items }))
            }
            Request::DataList => {
                let ds = pacewright_core::datastore::Datastore::new(
                    pacewright_core::run::home_dir().join("data"),
                );
                let sets: Vec<serde_json::Value> = ds
                    .list()
                    .into_iter()
                    .map(|(name, count)| serde_json::json!({ "name": name, "count": count }))
                    .collect();
                Ok(serde_json::json!({ "datasets": sets }))
            }
            Request::DataShow { name, limit } => {
                let ds = pacewright_core::datastore::Datastore::new(
                    pacewright_core::run::home_dir().join("data"),
                );
                let mut rows = ds.read(&name).map_err(|e| e.to_string())?;
                let count = rows.len();
                if let Some(l) = limit {
                    rows.truncate(l.max(0) as usize);
                }
                Ok(serde_json::json!({ "name": name, "count": count, "items": rows }))
            }
            Request::LedgerStats => {
                let scopes: Vec<serde_json::Value> = e
                    .store
                    .touched_scopes()
                    .map_err(|e| e.to_string())?
                    .into_iter()
                    .map(|(scope, count)| serde_json::json!({ "scope": scope, "count": count }))
                    .collect();
                Ok(serde_json::json!({ "scopes": scopes }))
            }
            Request::AnthropicStatus => {
                use pacewright_core::secrets::SecretStore;
                let path = pacewright_core::run::home_dir().join("secrets.json");
                let now = e.clock.now_ms();
                let (signed_in, expires_at, state) = match SecretStore::load(&path)
                    .ok()
                    .and_then(|s| s.get("anthropic").cloned())
                {
                    Some(rec) if rec.access_token.is_some() => {
                        let exp = rec.expires_at_ms.unwrap_or(0);
                        let st = if now < exp - 5 * 60 * 1000 {
                            "valid"
                        } else if rec.refresh_token.is_some() {
                            "refreshable"
                        } else {
                            "expired"
                        };
                        (true, rec.expires_at_ms, st)
                    }
                    _ => (false, None, "signed_out"),
                };
                // La OTRA credencial: el token de `claude setup-token`. Es la
                // única que puede correr una ronda de `claude_cli`, así que el
                // panel que mira esto necesita las dos por separado. Presencia,
                // nunca el valor.
                let setup_token = SecretStore::load(&path).ok().is_some_and(|s| {
                    s.static_token(pacewright_core::secrets::CLAUDE_CODE)
                        .is_some()
                });
                Ok(serde_json::json!({
                    "signed_in": signed_in,
                    "state": state,
                    "expires_at_ms": expires_at,
                    "setup_token": setup_token,
                }))
            }
            Request::Subscribe => Ok(
                serde_json::json!({ "note": "subscribe stream not enabled on this request path" }),
            ),

            Request::ScheduleApply { prune } => {
                let registry = srv.registry.read().unwrap().clone();
                let (entries, mut errors) = schedule::load_dir(&srv.schedules_dir);
                let (valid, verrs) = schedule::partition(&entries, &registry);
                errors.extend(verrs);
                let report = schedule::reconcile(&e.store, &*e.clock, &valid, prune)
                    .map_err(|e| e.to_string())?;
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
                e.store
                    .limit_override_set(&key, &cfg, now)
                    .map_err(|e| e.to_string())?;
                e.cfg.set_limit(key.clone(), cfg);
                Ok(serde_json::json!({ "key": key, "set": true }))
            }
            Request::RecipeReload => {
                // Re-read every `.kdl` from disk, rebuild the engine's adapter set from it, then
                // publish the fresh registry. New/changed task recipes become runnable immediately;
                // account recipes stay on the boot registry the AuthManager holds (needs a restart).
                let fresh = Arc::new(RecipeRegistry::load_dir(&srv.recipes_dir));
                let adapters = fresh.adapters();
                e.registry = build_adapter_registry(&fresh, &srv.recipe_runner, &e.store, &e.clock);
                *srv.registry.write().unwrap() = fresh;
                Ok(serde_json::json!({ "reloaded": true, "adapters": adapters }))
            }
            // Auth RPCs are intercepted by `handle_auth` before this synchronous dispatch.
            Request::AuthList
            | Request::AuthRecheck { .. }
            | Request::AuthLogin { .. }
            | Request::AuthLoginAll => unreachable!("auth requests are handled by handle_auth"),
            Request::ClaudeTokenSet { token } => {
                use pacewright_core::secrets::{SecretStore, CLAUDE_CODE};
                let token = token.trim();
                // Se valida la forma acá también, no sólo del lado del que
                // pega: este socket es una interfaz propia, y una interfaz que
                // confía en que el llamador ya validó es una que se rompe con
                // el segundo llamador. El valor no entra en el error.
                if !token.starts_with("sk-ant-oat01-") || token.len() < 33 {
                    return Err("that does not look like `claude setup-token` output \
                                (expected `sk-ant-oat01-…`)"
                        .to_string());
                }
                let path = pacewright_core::run::home_dir().join("secrets.json");
                let mut store = SecretStore::load(&path).map_err(|e| e.to_string())?;
                store.set_static_token(CLAUDE_CODE, token);
                store.save().map_err(|e| e.to_string())?;
                Ok(serde_json::json!({ "stored": true }))
            }
            Request::ClaudeTokenClear => {
                use pacewright_core::secrets::{SecretStore, CLAUDE_CODE};
                let path = pacewright_core::run::home_dir().join("secrets.json");
                let mut store = SecretStore::load(&path).map_err(|e| e.to_string())?;
                let had = store.remove_provider(CLAUDE_CODE);
                store.save().map_err(|e| e.to_string())?;
                Ok(serde_json::json!({ "cleared": had }))
            }
            Request::AnthropicLoginUrl | Request::AnthropicLoginSubmit { .. } => {
                unreachable!("anthropic login is handled by handle_anthropic_login")
            }
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
        if line.trim().is_empty() {
            continue;
        }
        let resp = match serde_json::from_str::<Request>(&line) {
            Ok(req) => handle_request(&srv, req).await,
            Err(e) => Response::Error {
                message: format!("bad request: {e}"),
            },
        };
        let mut out = serde_json::to_string(&resp).unwrap();
        out.push('\n');
        if w.write_all(out.as_bytes()).await.is_err() {
            break;
        }
    }
}

pub async fn serve(srv: Arc<Server>, socket_path: &Path) -> Result<()> {
    let _ = std::fs::remove_file(socket_path);
    let listener = match UnixListener::bind(socket_path) {
        Ok(l) => l,
        Err(err) if err.kind() == std::io::ErrorKind::AddrInUse => {
            anyhow::bail!(
                "another pacewrightd instance is already listening on {}",
                socket_path.display()
            );
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
                            Ok(Some(c)) => Some((
                                c,
                                e.store.clone(),
                                e.clock.clone(),
                                e.browser.clone(),
                                e.notifier.clone(),
                            )),
                            Ok(None) => None,
                            Err(err) => {
                                tracing::error!("claim error: {err}");
                                None
                            }
                        }
                    };
                    let Some((claimed, store, clock, browser, notifier)) = claimed else {
                        break;
                    };
                    let handle = tokio::spawn(async move {
                        execute_and_record(
                            &store,
                            &*claimed.adapter,
                            &*clock,
                            browser,
                            notifier,
                            claimed.task,
                        )
                        .await
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
    use pacewright_adapter_dummy::DummyAdapter;
    use pacewright_core::adapter::AdapterRegistry;
    use pacewright_core::clock::SystemClock;
    use pacewright_core::config::Config;
    use pacewright_core::rng::SeededRng;
    use pacewright_core::store::Store;

    pub fn scratch(prefix: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!(
            "{prefix}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    pub async fn test_server() -> Server {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let mut reg = AdapterRegistry::new();
        reg.register(Arc::new(DummyAdapter::new()));
        let e = Engine::new(
            store,
            reg,
            Config::default(),
            Arc::new(SystemClock),
            Arc::new(SeededRng::new(1)),
        );
        let registry = Arc::new(RecipeRegistry::new());
        Server {
            engine: Arc::new(Mutex::new(e)),
            auth: auth_for(registry.clone()),
            registry: RwLock::new(registry),
            recipes_dir: std::env::temp_dir().join("pcw-no-such-recipes-dir"),
            recipe_runner: Arc::new(pacewright_adapter_recipe::CliRecipeRunner::new()),
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
        let registry = Arc::new(RecipeRegistry::load_dir(&recipes));
        srv.registry = RwLock::new(registry.clone());
        srv.recipes_dir = recipes;
        srv.auth = auth_for(registry);
        let sched = scratch("pcw-srv-schedules");
        std::fs::write(sched.join("s.toml"), schedule_toml).unwrap();
        srv.schedules_dir = sched;
        srv
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{auth_for, scratch, test_server, test_server_with_schedule};
    use super::*;
    use pacewright_proto::AddTaskReq;

    #[tokio::test]
    async fn test_agent_reasoning_adapter_is_registered() {
        // "Involve Claude" must resolve to a real adapter, not `no_adapter`. Both the generic
        // `agent` prefix and the `claude` alias are present with the ask + adjudicate actions and
        // their own limit keys, so a scheduled task or a pipeline fallback can reach them.
        let empty: Arc<RecipeRegistry> = Arc::new(RecipeRegistry::new());
        let runner: Arc<dyn RecipeRunner> =
            Arc::new(pacewright_adapter_recipe::CliRecipeRunner::new());
        let store = Arc::new(pacewright_core::store::Store::open_in_memory().unwrap());
        let clock: Arc<dyn pacewright_core::clock::Clock> =
            Arc::new(pacewright_core::clock::SystemClock);
        let reg = build_adapter_registry(&empty, &runner, &store, &clock);
        for name in ["agent", "claude"] {
            let a = reg
                .get(name)
                .unwrap_or_else(|| panic!("{name} adapter missing"));
            let actions: Vec<String> = a.actions().into_iter().map(|s| s.name).collect();
            assert!(actions.contains(&"ask".to_string()), "{name} exposes ask");
            assert!(
                actions.contains(&"adjudicate".to_string()),
                "{name} exposes adjudicate"
            );
            assert_eq!(a.limit_keys_for("ask"), vec![format!("{name}.ask")]);
        }
    }
    #[tokio::test]
    async fn test_auth_list_reports_accounts_with_recipes_and_unknown_status() {
        let mut srv = test_server().await;
        let recipes = scratch("pcw-srv-auth");
        std::fs::create_dir_all(recipes.join("accounts")).unwrap();
        std::fs::write(
            recipes.join("accounts/globex-account.kdl"),
            "recipe \"accounts/globex-account\" { login-url \"https://globex.example/login\"\n step { goto \"https://globex.example/dashboard\" } }",
        )
        .unwrap();
        std::fs::write(
            recipes.join("rv.kdl"),
            "recipe \"globex/generate_clips\" { auth account=\"globex-account\" }",
        )
        .unwrap();
        let registry = Arc::new(RecipeRegistry::load_dir(&recipes));
        srv.registry = RwLock::new(registry.clone());
        srv.recipes_dir = recipes;
        srv.auth = auth_for(registry);

        let resp = handle_request(&srv, Request::AuthList).await;
        match resp {
            Response::Ok(v) => {
                let accts = v["accounts"].as_array().unwrap();
                assert_eq!(accts.len(), 1);
                assert_eq!(accts[0]["account"], "globex-account");
                assert_eq!(accts[0]["login_url"], "https://globex.example/login");
                assert_eq!(accts[0]["recipes"][0], "globex/generate_clips");
                assert!(accts[0]["signed_in"].is_null(), "unknown before any check");
                assert_eq!(accts[0]["logging_in"], false);
            }
            _ => panic!("auth_list failed"),
        }
    }

    #[tokio::test]
    async fn test_auth_login_unknown_account_errors() {
        let srv = test_server().await;
        let resp = handle_request(
            &srv,
            Request::AuthLogin {
                account: "nope".into(),
            },
        )
        .await;
        assert!(matches!(resp, Response::Error { .. }), "got {resp:?}");
    }

    #[tokio::test]
    async fn test_recipe_reload_picks_up_a_new_adapter() {
        // A recipe dropped into the recipes dir AFTER boot is invisible until reload: the engine's
        // adapter set is built once. `RecipeReload` re-reads the dir and rebuilds it in place.
        let mut srv = test_server().await;
        let recipes = scratch("pcw-srv-reload");
        srv.recipes_dir = recipes.clone();

        // Nothing installed yet → only the built-in `dummy` adapter.
        let before = handle_request(&srv, Request::Adapters).await;
        let names_before: Vec<String> = match before {
            Response::Ok(v) => v["adapters"]
                .as_array()
                .unwrap()
                .iter()
                .map(|a| a["name"].as_str().unwrap().to_string())
                .collect(),
            _ => panic!("adapters failed"),
        };
        assert!(
            !names_before.iter().any(|n| n == "globex"),
            "globex shouldn't exist pre-reload"
        );

        // Install a recipe, then reload.
        std::fs::write(
            recipes.join("rv.kdl"),
            "recipe \"globex/list_projects\" {}\n",
        )
        .unwrap();
        let resp = handle_request(&srv, Request::RecipeReload).await;
        match resp {
            Response::Ok(v) => {
                assert_eq!(v["reloaded"], true);
                assert!(v["adapters"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|a| a == "globex"));
            }
            _ => panic!("reload failed"),
        }

        // The engine now routes `globex/*` to a recipe adapter, and the published registry sees it.
        let after = handle_request(&srv, Request::Adapters).await;
        match after {
            Response::Ok(v) => assert!(v["adapters"]
                .as_array()
                .unwrap()
                .iter()
                .any(|a| a["name"] == "globex")),
            _ => panic!("adapters failed"),
        }
        assert!(srv
            .registry
            .read()
            .unwrap()
            .adapters()
            .iter()
            .any(|n| n == "globex"));
    }

    #[tokio::test]
    async fn test_add_then_get_via_dispatch() {
        let srv = test_server().await;
        let add = Request::Add(AddTaskReq {
            adapter: "dummy".into(),
            action: "echo".into(),
            params: serde_json::json!({"a":1}),
            scheduled_for: None,
            recurrence: None,
            depends_on: None,
            priority: None,
            dedup_key: None,
            max_attempts: None,
        });
        let resp = handle_request(&srv, add).await;
        let id = match resp {
            Response::Ok(v) => v["id"].as_str().unwrap().to_string(),
            _ => panic!("add failed"),
        };
        let get = handle_request(&srv, Request::Get { id: id.clone() }).await;
        match get {
            Response::Ok(v) => assert_eq!(v["task"]["action"], "echo"),
            _ => panic!("get failed"),
        };
    }

    #[tokio::test]
    async fn test_run_src_enqueues_a_recipe_src_task_with_add_semantics() {
        use pacewright_proto::RunSrcReq;
        let srv = test_server().await;
        let src = "recipe \"facturagas/facturar\" {\n  limit-key \"facturagas.facturar\"\n}";
        let req = |dedup: Option<&str>| {
            Request::RunSrc(RunSrcReq {
                recipe_src: src.into(),
                params: serde_json::json!({"rfc": "XAXX010101000"}),
                dedup_key: dedup.map(str::to_string),
                priority: Some(5),
                max_attempts: Some(1),
                scheduled_for: Some(123_456),
                name: Some("facturagas/facturar".into()),
            })
        };
        let id = match handle_request(&srv, req(Some("inv-1"))).await {
            Response::Ok(v) => v["id"].as_str().unwrap().to_string(),
            other => panic!("run_src failed: {other:?}"),
        };
        let task = match handle_request(&srv, Request::Get { id: id.clone() }).await {
            Response::Ok(v) => v["task"].clone(),
            _ => panic!("get failed"),
        };
        assert_eq!(task["adapter"], "recipe_src");
        assert_eq!(task["action"], "run");
        assert_eq!(task["params"]["__recipe_src"], src);
        assert_eq!(task["params"]["__name"], "facturagas/facturar");
        assert_eq!(
            task["params"]["vars"],
            serde_json::json!({"rfc": "XAXX010101000"})
        );
        assert_eq!(
            task["max_attempts"], 1,
            "a backend's `1` must be honored verbatim"
        );
        assert_eq!(task["priority"], 5);
        assert_eq!(task["scheduled_for"], 123_456);
        assert_eq!(task["dedup_key"], "inv-1");

        // dedup: the same key while the first is still active returns the SAME id
        match handle_request(&srv, req(Some("inv-1"))).await {
            Response::Ok(v) => assert_eq!(v["id"], id),
            other => panic!("{other:?}"),
        }
        // no key → a fresh task
        match handle_request(&srv, req(None)).await {
            Response::Ok(v) => assert_ne!(v["id"], id),
            other => panic!("{other:?}"),
        }

        // `build_adapter_registry` (what boot and `RecipeReload` share) registers the built-in,
        // so the task will be claimed, not failed with no_adapter — and paced by the key
        // declared IN the source
        assert!(matches!(
            handle_request(&srv, Request::RecipeReload).await,
            Response::Ok(_)
        ));
        let adapter = srv
            .engine
            .lock()
            .await
            .registry
            .get("recipe_src")
            .expect("recipe_src registered as a built-in");
        let t: Task = serde_json::from_value(task).unwrap();
        assert_eq!(
            adapter.limit_keys_for_task(&t.action, &t.params),
            vec!["facturagas.facturar".to_string()]
        );

        // bad input is refused up front
        let bad = Request::RunSrc(RunSrcReq {
            recipe_src: "  ".into(),
            params: serde_json::Value::Null,
            dedup_key: None,
            priority: None,
            max_attempts: None,
            scheduled_for: None,
            name: None,
        });
        assert!(matches!(
            handle_request(&srv, bad).await,
            Response::Error { .. }
        ));
        let bad = Request::RunSrc(RunSrcReq {
            recipe_src: src.into(),
            params: serde_json::json!([1, 2]),
            dedup_key: None,
            priority: None,
            max_attempts: None,
            scheduled_for: None,
            name: None,
        });
        assert!(matches!(
            handle_request(&srv, bad).await,
            Response::Error { .. }
        ));
    }

    #[tokio::test]
    async fn test_schedule_apply_list_and_toggle() {
        let srv =
            test_server_with_schedule("[[task]]\nid = \"t1\"\nrecipe = \"dummy/echo\"\n").await;

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
                assert!(v["errors"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|e| e.as_str().unwrap().contains("no recipe")));
            }
            _ => panic!("apply failed"),
        }
    }

    #[tokio::test]
    async fn test_set_limit_persists_override() {
        let srv = test_server().await;
        let resp = handle_request(
            &srv,
            Request::SetLimit {
                key: "dummy.capped".into(),
                config: pacewright_proto::LimitSpec {
                    daily_cap: Some(3),
                    min_gap: Some("8m".into()),
                    jitter: Some(0.5),
                    active: None,
                },
            },
        )
        .await;
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
        match resp {
            Response::Ok(v) => assert_eq!(v["adapters"][0]["name"], "dummy"),
            _ => panic!(),
        };
    }

    #[tokio::test]
    async fn test_pause_resume_via_dispatch() {
        let srv = test_server().await;

        // pause a specific adapter scope -> echoed back, and reflected in status
        let resp = handle_request(
            &srv,
            Request::Pause {
                scope: "dummy".into(),
            },
        )
        .await;
        match resp {
            Response::Ok(v) => {
                assert_eq!(v["scope"], "dummy");
                assert_eq!(v["paused"], true);
            }
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
        handle_request(
            &srv,
            Request::Pause {
                scope: "daemon".into(),
            },
        )
        .await;
        let status = handle_request(&srv, Request::Status).await;
        match status {
            Response::Ok(v) => {
                let scopes: Vec<String> = serde_json::from_value(v["paused"].clone()).unwrap();
                assert!(scopes.contains(&"all".to_string()));
            }
            _ => panic!("status failed"),
        }

        // resume both -> paused set drains to empty
        handle_request(
            &srv,
            Request::Resume {
                scope: "dummy".into(),
            },
        )
        .await;
        let resp = handle_request(
            &srv,
            Request::Resume {
                scope: "daemon".into(),
            },
        )
        .await;
        match resp {
            Response::Ok(v) => {
                assert_eq!(v["scope"], "daemon");
                assert_eq!(v["paused"], false);
            }
            _ => panic!("resume failed"),
        }
        let status = handle_request(&srv, Request::Status).await;
        match status {
            Response::Ok(v) => {
                let scopes: Vec<String> = serde_json::from_value(v["paused"].clone()).unwrap();
                assert!(
                    scopes.is_empty(),
                    "expected no paused scopes, got {scopes:?}"
                );
            }
            _ => panic!("status failed"),
        }
    }

    #[tokio::test]
    async fn test_digest_reports_ran_failed_and_paused() {
        let srv = test_server().await;
        let now = chrono::Local::now().timestamp_millis();
        {
            let e = srv.engine.lock().await;
            let mut ok = Task::new_now("dummy", "echo", serde_json::json!({}), now);
            ok.status = TaskStatus::Succeeded;
            ok.finished_at = Some(now);
            e.store.insert_task(&ok).unwrap();
            let mut bad = Task::new_now("dummy", "echo", serde_json::json!({}), now);
            bad.status = TaskStatus::Failed;
            bad.finished_at = Some(now);
            bad.last_error = Some("logged out".into());
            e.store.insert_task(&bad).unwrap();
            let mut queued = Task::new_now("dummy", "echo", serde_json::json!({}), now);
            queued.status = TaskStatus::Pending;
            e.store.insert_task(&queued).unwrap();
            e.pause("dummy".to_string()).unwrap();
        }
        match handle_request(&srv, Request::Digest).await {
            Response::Ok(v) => {
                assert!(
                    v["ran_today"].as_i64().unwrap() >= 1,
                    "the succeeded task is counted"
                );
                assert_eq!(v["queued"].as_i64().unwrap(), 1);
                let failed = v["failed_today"].as_array().unwrap();
                assert_eq!(failed.len(), 1);
                assert_eq!(failed[0]["error"], "logged out");
                let paused: Vec<String> =
                    serde_json::from_value(v["paused_scopes"].clone()).unwrap();
                assert!(
                    paused.contains(&"dummy".to_string()),
                    "paused scope surfaces for the human"
                );
                assert_eq!(v["waiting_on_human"]["failed_count"], 1);
            }
            other => panic!("digest failed: {other:?}"),
        }
    }
}

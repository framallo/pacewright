use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AddTaskReq {
    pub adapter: String,
    pub action: String,
    pub params: Value,
    #[serde(default)]
    pub scheduled_for: Option<i64>,
    #[serde(default)]
    pub recurrence: Option<String>,
    #[serde(default)]
    pub depends_on: Option<String>,
    #[serde(default)]
    pub priority: Option<i64>,
    #[serde(default)]
    pub dedup_key: Option<String>,
    #[serde(default)]
    pub max_attempts: Option<i64>,
}

/// `run_src`: enqueue a task that runs a recipe handed over as **KDL source** — for a controller
/// that keeps its recipes in its own database rather than under `~/.pacewright/recipes/`. The
/// daemon turns it into a normal task on the built-in `recipe_src` adapter (`action = run`) with
/// params `{"__recipe_src": <src>, "__name": <name>, "vars": <params>}`, so dedup, priority,
/// `max_attempts` and `scheduled_for` behave exactly as in [`Request::Add`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RunSrcReq {
    /// The recipe's KDL text.
    pub recipe_src: String,
    /// The recipe's vars (a JSON object). Nested under `vars` in the task params.
    #[serde(default)]
    pub params: Value,
    #[serde(default)]
    pub dedup_key: Option<String>,
    #[serde(default)]
    pub priority: Option<i64>,
    /// Pass `1` for a run that must never be retried by the engine's own backoff loop (a recipe
    /// that already issued a real invoice).
    #[serde(default)]
    pub max_attempts: Option<i64>,
    #[serde(default)]
    pub scheduled_for: Option<i64>,
    /// Display name for logs/digest, e.g. `"facturagas/facturar"`.
    #[serde(default)]
    pub name: Option<String>,
}

/// A human-friendly pacing spec for `set_limit`, mirroring the `config.toml` shape
/// (`min_gap`/`active` as strings). The daemon parses it into the engine's limit config.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LimitSpec {
    #[serde(default)]
    pub daily_cap: Option<i64>,
    #[serde(default)]
    pub min_gap: Option<String>,
    #[serde(default)]
    pub jitter: Option<f64>,
    #[serde(default)]
    pub active: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "method", content = "params", rename_all = "snake_case")]
pub enum Request {
    Add(AddTaskReq),
    /// Enqueue a recipe run from source — see [`RunSrcReq`]. Answers `{"id": <task id>}`.
    RunSrc(RunSrcReq),
    Get {
        id: String,
    },
    List {
        #[serde(default)]
        status: Option<String>,
        #[serde(default)]
        adapter: Option<String>,
        #[serde(default)]
        limit: Option<i64>,
    },
    Cancel {
        id: String,
    },
    RunNow {
        id: String,
        #[serde(default)]
        force: bool,
    },
    Pause {
        scope: String,
    },
    Resume {
        scope: String,
    },
    Limits,
    Adapters,
    Status,
    Subscribe,
    /// Reconcile the schedule files into the queue.
    ScheduleApply {
        #[serde(default)]
        prune: bool,
    },
    /// The declared schedule catalog + effective enabled state + next fire + live status.
    ScheduleList,
    /// Runtime-enable a schedule entry (override its file default) and reconcile.
    ScheduleEnable {
        id: String,
    },
    /// Runtime-disable a schedule entry and reconcile (cancels its live task).
    ScheduleDisable {
        id: String,
    },
    /// Set a runtime pacing override for a limit key (persists over `config.toml`).
    SetLimit {
        key: String,
        config: LimitSpec,
    },
    /// Start or resume a pipeline run. Idempotent: already-succeeded steps are skipped.
    RunStart {
        pipeline: String,
        run_id: String,
        #[serde(default)]
        params: Value,
        /// Re-queue this run's FAILED steps before starting (succeeded work is kept).
        #[serde(default)]
        retry_failed: bool,
    },
    /// Every run with a rollup of its step statuses.
    RunList,
    /// One run's steps, in order, with status / attempts / error / result.
    RunShow {
        run_id: String,
    },
    /// The account catalog + cached signed-in status (read side; no checks run).
    AuthList,
    /// Re-run an account's signed-in check headless in its profile and update the cache.
    /// `account` omitted → recheck every account.
    AuthRecheck {
        #[serde(default)]
        account: Option<String>,
    },
    /// Open a headed login window for `account` and poll its check until it passes.
    AuthLogin {
        account: String,
    },
    /// Open a login window, one at a time, for each account not known to be signed in.
    AuthLoginAll,
    /// Reload the recipe registry from disk and rebuild the engine's adapter set, so recipes
    /// added/changed since boot become runnable without restarting the daemon.
    RecipeReload,
    /// A structured daily summary: what ran, what's queued, what failed (with errors), and what is
    /// waiting on a human (paused scopes to resume, failures to fix).
    Digest,
    /// The escalation outbox: issues the notifier raised (terminal failures / paused scopes) for
    /// Claude/a human to triage. `drain` deletes each after reading (a one-shot pull).
    Escalations {
        #[serde(default)]
        drain: bool,
    },
    /// Saved JSON datasets (task output): each `(name, row_count)`.
    DataList,
    /// One dataset's rows (capped by `limit`).
    DataShow {
        name: String,
        #[serde(default)]
        limit: Option<i64>,
    },
    /// All-time dedup ledger: per-scope touched counts (R7).
    LedgerStats,
    /// Whether a Claude Max/Pro subscription is signed in, its freshness, and email.
    AnthropicStatus,
    /// Empieza el login OAuth de Claude SIN loopback: devuelve la URL a abrir y
    /// el PKCE (`verifier`+`state`) que el cliente guarda para el submit. El
    /// daemon no guarda estado entre las dos llamadas.
    AnthropicLoginUrl,
    /// Termina el login: intercambia el código pegado (`code` o `code#state`)
    /// por los tokens y los guarda. `verifier`/`state` son los que devolvió
    /// [`Request::AnthropicLoginUrl`].
    AnthropicLoginSubmit {
        pasted: String,
        verifier: String,
        state: String,
    },
    /// Guarda el token de `claude setup-token` (`sk-ant-oat01-…`), que es el
    /// único que puede gastar una suscripción Max: lo acepta el binario
    /// `claude` y la API de mensajes lo rechaza. Es OTRA credencial que la de
    /// [`Request::AnthropicLoginSubmit`], que es OAuth para la API.
    ///
    /// Va por acá y no en los `params` de la tarea a propósito: los params se
    /// guardan en la base, y un secreto ahí queda en claro.
    ClaudeTokenSet {
        token: String,
    },
    /// Olvida ese token. Después de esto las rondas de `claude_cli` corren sin
    /// credencial propia.
    ClaudeTokenClear,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    Ok(Value),
    Error { message: String },
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_add_request_roundtrips() {
        let req = Request::Add(AddTaskReq {
            adapter: "dummy".into(),
            action: "echo".into(),
            params: serde_json::json!({"a":1}),
            scheduled_for: None,
            recurrence: None,
            depends_on: None,
            priority: Some(2),
            dedup_key: None,
            max_attempts: None,
        });
        let s = serde_json::to_string(&req).unwrap();
        let back: Request = serde_json::from_str(&s).unwrap();
        assert_eq!(req, back);
    }
    #[test]
    fn test_run_src_request_roundtrips_with_the_documented_wire_shape() {
        let wire = r#"{"method":"run_src","params":{"recipe_src":"recipe \"a/b\" {}","params":{"rfc":"X"},"dedup_key":"inv-1","priority":0,"max_attempts":1,"scheduled_for":null,"name":"facturagas/facturar"}}"#;
        let req: Request = serde_json::from_str(wire).unwrap();
        let expected = Request::RunSrc(RunSrcReq {
            recipe_src: "recipe \"a/b\" {}".into(),
            params: serde_json::json!({"rfc": "X"}),
            dedup_key: Some("inv-1".into()),
            priority: Some(0),
            max_attempts: Some(1),
            scheduled_for: None,
            name: Some("facturagas/facturar".into()),
        });
        assert_eq!(req, expected);
        let s = serde_json::to_string(&req).unwrap();
        assert!(s.contains("\"method\":\"run_src\""));
        let back: Request = serde_json::from_str(&s).unwrap();
        assert_eq!(back, expected);
        // every field but the source is optional
        let minimal: Request =
            serde_json::from_str(r#"{"method":"run_src","params":{"recipe_src":"x"}}"#).unwrap();
        match minimal {
            Request::RunSrc(r) => {
                assert_eq!(r.recipe_src, "x");
                assert!(r.params.is_null());
                assert_eq!(r.max_attempts, None);
            }
            other => panic!("{other:?}"),
        }
    }
    #[test]
    fn test_list_tagged_shape() {
        let s = serde_json::to_string(&Request::List {
            status: Some("pending".into()),
            adapter: None,
            limit: Some(10),
        })
        .unwrap();
        assert!(s.contains("\"method\":\"list\""));
    }
    #[test]
    fn test_response_ok() {
        let s = serde_json::to_string(&Response::Ok(serde_json::json!({"x":1}))).unwrap();
        assert!(s.contains("\"type\":\"ok\""));
    }
    #[test]
    fn test_auth_requests_wire_shape() {
        assert!(serde_json::to_string(&Request::AuthList)
            .unwrap()
            .contains("\"method\":\"auth_list\""));
        let recheck = serde_json::to_string(&Request::AuthRecheck {
            account: Some("rv".into()),
        })
        .unwrap();
        assert!(
            recheck.contains("\"method\":\"auth_recheck\"")
                && recheck.contains("\"account\":\"rv\"")
        );
        let login = Request::AuthLogin {
            account: "rv".into(),
        };
        assert_eq!(
            serde_json::from_str::<Request>(&serde_json::to_string(&login).unwrap()).unwrap(),
            login
        );
        assert!(serde_json::to_string(&Request::AuthLoginAll)
            .unwrap()
            .contains("\"method\":\"auth_login_all\""));
    }
}

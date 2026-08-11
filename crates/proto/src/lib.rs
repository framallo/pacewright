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

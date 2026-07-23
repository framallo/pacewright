use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Pending, Blocked, Deferred, Running, Succeeded, Failed, Canceled,
}

impl TaskStatus {
    /// Every variant, for exhaustive iteration. Adding a status here (the
    /// compiler will not force it — keep it in sync with the enum) lets
    /// `terminal_strs` derive the DB terminal-set from `is_terminal` alone,
    /// so the SQL in `store.rs` can never drift from the Rust definition.
    pub const ALL: [TaskStatus; 7] = [
        TaskStatus::Pending, TaskStatus::Blocked, TaskStatus::Deferred, TaskStatus::Running,
        TaskStatus::Succeeded, TaskStatus::Failed, TaskStatus::Canceled,
    ];

    pub fn is_terminal(&self) -> bool {
        matches!(self, TaskStatus::Succeeded | TaskStatus::Failed | TaskStatus::Canceled)
    }

    /// The wire strings of the terminal statuses, derived from `is_terminal`.
    /// Single source of truth for any query that must exclude finished tasks.
    pub fn terminal_strs() -> Vec<&'static str> {
        Self::ALL.iter().filter(|s| s.is_terminal()).map(|s| s.as_str()).collect()
    }
    pub fn as_str(&self) -> &'static str {
        match self {
            TaskStatus::Pending => "pending",
            TaskStatus::Blocked => "blocked",
            TaskStatus::Deferred => "deferred",
            TaskStatus::Running => "running",
            TaskStatus::Succeeded => "succeeded",
            TaskStatus::Failed => "failed",
            TaskStatus::Canceled => "canceled",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub adapter: String,
    pub action: String,
    pub params: Value,
    pub status: TaskStatus,
    pub scheduled_for: i64,
    pub next_eligible_at: Option<i64>,
    pub priority: i64,
    pub recurrence: Option<String>,
    pub depends_on: Option<String>,
    pub dedup_key: Option<String>,
    pub run_id: Option<String>,
    pub step_name: Option<String>,
    /// When released by a dependency, wait a jittered pause of about this long
    /// before becoming eligible. Humanizes the gap between pipeline steps.
    pub pace_ms: Option<i64>,
    /// Release this task when its dependency FAILED rather than succeeded. The escape
    /// hatch edge: a fallback runs precisely when the thing it backstops did not work.
    pub dep_on_failure: bool,
    /// Task id of an escalation that backstops this one. If that task succeeds with a
    /// well-formed verdict, this task is marked Succeeded-by-adjudication.
    pub escalation: Option<String>,
    pub attempts: i64,
    pub max_attempts: i64,
    pub last_error: Option<String>,
    pub result: Option<Value>,
    pub created_at: i64,
    pub updated_at: i64,
    pub finished_at: Option<i64>,
}

impl Task {
    pub fn new_now(adapter: impl Into<String>, action: impl Into<String>, params: Value, now_ms: i64) -> Self {
        Task {
            id: uuid::Uuid::new_v4().to_string(),
            adapter: adapter.into(),
            action: action.into(),
            params,
            status: TaskStatus::Pending,
            scheduled_for: now_ms,
            next_eligible_at: None,
            priority: 0,
            recurrence: None,
            depends_on: None,
            dedup_key: None,
            run_id: None,
            step_name: None,
            pace_ms: None,
            dep_on_failure: false,
            escalation: None,
            attempts: 0,
            max_attempts: 3,
            last_error: None,
            result: None,
            created_at: now_ms,
            updated_at: now_ms,
            finished_at: None,
        }
    }
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum AdapterError {
    #[error("retryable: {0}")]
    Retryable(String),
    #[error("terminal: {0}")]
    Terminal(String),
    #[error("rate limited until {retry_after}")]
    RateLimited { retry_after: i64 },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionSpec {
    pub name: String,
    pub limit_keys: Vec<String>,
    pub params_schema: Value,
    pub description: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskEvent {
    pub task_id: String,
    pub at: i64,
    pub from_status: Option<TaskStatus>,
    pub to_status: TaskStatus,
    pub detail: Value,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_new_now_defaults() {
        let t = Task::new_now("dummy", "echo", serde_json::json!({"x":1}), 5_000);
        assert_eq!(t.adapter, "dummy");
        assert_eq!(t.status, TaskStatus::Pending);
        assert_eq!(t.scheduled_for, 5_000);
        assert_eq!(t.max_attempts, 3);
        assert!(!t.id.is_empty());
    }
    #[test]
    fn test_status_serde_snake_case() {
        let j = serde_json::to_string(&TaskStatus::Deferred).unwrap();
        assert_eq!(j, "\"deferred\"");
    }
    #[test]
    fn test_terminal_flag() {
        assert!(TaskStatus::Succeeded.is_terminal());
        assert!(!TaskStatus::Pending.is_terminal());
    }

    #[test]
    fn test_terminal_strs_matches_is_terminal_and_all_is_exhaustive() {
        // `TaskStatus::ALL` has no compiler-enforced exhaustiveness, so this
        // match is the guard: add a variant and this fails to compile until it
        // is both classified here and (by review) appended to `ALL`.
        for s in TaskStatus::ALL {
            let expected_terminal = match s {
                TaskStatus::Pending | TaskStatus::Blocked | TaskStatus::Deferred | TaskStatus::Running => false,
                TaskStatus::Succeeded | TaskStatus::Failed | TaskStatus::Canceled => true,
            };
            assert_eq!(s.is_terminal(), expected_terminal, "{s:?}");
        }
        // Every ALL entry is distinct -> ALL is not missing/duplicating a variant.
        let mut seen: Vec<&str> = TaskStatus::ALL.iter().map(|s| s.as_str()).collect();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), TaskStatus::ALL.len());

        let mut terminal = TaskStatus::terminal_strs();
        terminal.sort_unstable();
        assert_eq!(terminal, vec!["canceled", "failed", "succeeded"]);
    }
}

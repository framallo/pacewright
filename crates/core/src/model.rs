use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Pending,
    Blocked,
    Deferred,
    Running,
    Succeeded,
    Failed,
    Canceled,
}

impl TaskStatus {
    /// Every variant, for exhaustive iteration. Adding a status here (the
    /// compiler will not force it — keep it in sync with the enum) lets
    /// `terminal_strs` derive the DB terminal-set from `is_terminal` alone,
    /// so the SQL in `store.rs` can never drift from the Rust definition.
    pub const ALL: [TaskStatus; 7] = [
        TaskStatus::Pending,
        TaskStatus::Blocked,
        TaskStatus::Deferred,
        TaskStatus::Running,
        TaskStatus::Succeeded,
        TaskStatus::Failed,
        TaskStatus::Canceled,
    ];

    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            TaskStatus::Succeeded | TaskStatus::Failed | TaskStatus::Canceled
        )
    }

    /// The wire strings of the terminal statuses, derived from `is_terminal`.
    /// Single source of truth for any query that must exclude finished tasks.
    pub fn terminal_strs() -> Vec<&'static str> {
        Self::ALL
            .iter()
            .filter(|s| s.is_terminal())
            .map(|s| s.as_str())
            .collect()
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
    /// Fail-safe for sensitive outward jobs: on a terminal failure (e.g. a preflight throw —
    /// logged out, wrong account), pause this task's adapter scope so the next queued items don't
    /// repeat the failure unattended. Opt-in; internal/data tasks leave it false.
    pub pause_scope_on_failure: bool,
    /// Fan-out spec (from a pipeline step's `fanout` block). When this task SUCCEEDS, its
    /// `result` is turned into N paced, deduped act tasks — the declarative form of the
    /// bash "scan into a queue, then paced `while read` loop" (R5). `None` for ordinary tasks.
    pub fanout: Option<Value>,
    /// All-time dedup ledger identity (R7). Set on a fanned-out act task: when it succeeds, the
    /// engine records `(touch_scope, touch_id)` in the `touched` table so the target is never
    /// acted on again — replacing the per-script `mm-pitched-all.log` / `.apollo-revealed.txt`.
    pub touch_scope: Option<String>,
    pub touch_id: Option<String>,
    pub attempts: i64,
    pub max_attempts: i64,
    pub last_error: Option<String>,
    pub result: Option<Value>,
    pub created_at: i64,
    pub updated_at: i64,
    pub finished_at: Option<i64>,
}

impl Task {
    pub fn new_now(
        adapter: impl Into<String>,
        action: impl Into<String>,
        params: Value,
        now_ms: i64,
    ) -> Self {
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
            pause_scope_on_failure: false,
            fanout: None,
            touch_scope: None,
            touch_id: None,
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
    /// `Retryable`, plus a structured failure context the engine persists in `task.result` as
    /// `{"failure": detail}` (even though the task did not succeed), so a self-heal loop can see
    /// *where* and *on what page* the run died instead of only `last_error`.
    #[error("retryable: {message}")]
    RetryableWith { message: String, detail: Value },
    /// `Terminal`, plus the same structured failure context (see [`AdapterError::RetryableWith`]).
    #[error("terminal: {message}")]
    TerminalWith { message: String, detail: Value },
}

impl AdapterError {
    /// Attach a structured failure context: `Terminal` → `TerminalWith`, `Retryable` →
    /// `RetryableWith` (an existing detail is replaced). `RateLimited` carries none and is
    /// returned unchanged.
    pub fn with_detail(self, detail: Value) -> Self {
        match self {
            AdapterError::Retryable(message) | AdapterError::RetryableWith { message, .. } => {
                AdapterError::RetryableWith { message, detail }
            }
            AdapterError::Terminal(message) | AdapterError::TerminalWith { message, .. } => {
                AdapterError::TerminalWith { message, detail }
            }
            other => other,
        }
    }

    /// The human message without the class prefix (`RateLimited` has none → empty).
    pub fn message(&self) -> &str {
        match self {
            AdapterError::Retryable(m) | AdapterError::Terminal(m) => m,
            AdapterError::RetryableWith { message, .. }
            | AdapterError::TerminalWith { message, .. } => message,
            AdapterError::RateLimited { .. } => "",
        }
    }

    /// The structured failure context, if the adapter attached one.
    pub fn detail(&self) -> Option<&Value> {
        match self {
            AdapterError::RetryableWith { detail, .. }
            | AdapterError::TerminalWith { detail, .. } => Some(detail),
            _ => None,
        }
    }
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
    fn test_with_detail_upgrades_class_and_keeps_display() {
        let d = serde_json::json!({"step_index": 2});
        let e = AdapterError::Terminal("boom".into()).with_detail(d.clone());
        assert!(matches!(e, AdapterError::TerminalWith { .. }));
        assert_eq!(e.to_string(), "terminal: boom");
        assert_eq!(e.message(), "boom");
        assert_eq!(e.detail(), Some(&d));
        let r = AdapterError::Retryable("slow".into()).with_detail(d.clone());
        assert!(matches!(r, AdapterError::RetryableWith { .. }));
        assert_eq!(r.to_string(), "retryable: slow");
        // rate-limited has nowhere to put a detail and stays as is
        let rl = AdapterError::RateLimited { retry_after: 5 }.with_detail(d);
        assert!(matches!(rl, AdapterError::RateLimited { retry_after: 5 }));
        assert!(rl.detail().is_none());
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
                TaskStatus::Pending
                | TaskStatus::Blocked
                | TaskStatus::Deferred
                | TaskStatus::Running => false,
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

//! Escalation notifier — "call Claude when there is an issue".
//!
//! The engine already surfaces trouble passively (the daemon `digest` RPC lists failures and
//! auto-paused scopes). This adds the ACTIVE side the fleet asked for: when a task fails terminally
//! or its scope auto-pauses, the runner hands a structured [`EscalationEvent`] to a [`Notifier`].
//!
//! Core owns only the trait + the event shape + a no-op default, so it stays deterministic and
//! I/O-free: tests wire a [`RecordingNotifier`] and assert on what would have been sent. The daemon
//! provides the real impl (write an outbox file the pacewright MCP/skill drains, and optionally
//! shell `claude -p`).
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Why a task reached a human/Claude's attention.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EscalationKind {
    /// A task exhausted its retries and failed.
    TaskFailed,
    /// A task failed terminally AND auto-paused its adapter scope (`pause_scope_on_failure`), so the
    /// queued siblings are now halted until a human resumes the scope — the loudest signal.
    ScopePaused,
}

impl EscalationKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            EscalationKind::TaskFailed => "task_failed",
            EscalationKind::ScopePaused => "scope_paused",
        }
    }
}

/// A single issue worth Claude's attention. Carries just enough for a repair prompt without the
/// notifier having to re-query the store: what failed, where, why, and (for a paused scope) which
/// scope is now halted.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EscalationEvent {
    pub kind: EscalationKind,
    pub task_id: String,
    pub adapter: String,
    pub action: String,
    pub run_id: Option<String>,
    pub step_name: Option<String>,
    /// The adapter scope that got paused (only set for `ScopePaused`).
    pub paused_scope: Option<String>,
    pub error: Option<String>,
    pub at_ms: i64,
}

/// The one thing the outside world provides: deliver an escalation. Kept infallible-by-design — a
/// notifier failure must never fail the task it is reporting on (best-effort), so impls swallow
/// their own errors (log + move on) rather than propagate.
pub trait Notifier: Send + Sync {
    fn notify(&self, ev: &EscalationEvent);
}

/// The default: escalations go nowhere. Used by every browser-free / in-process caller and test that
/// does not care about notification.
#[derive(Debug, Default, Clone, Copy)]
pub struct NullNotifier;

impl Notifier for NullNotifier {
    fn notify(&self, _ev: &EscalationEvent) {}
}

/// A shared no-op notifier, so callers that don't wire one avoid re-allocating.
pub fn null_notifier() -> Arc<dyn Notifier> {
    Arc::new(NullNotifier)
}

/// Test double: records every event it is handed, so a test can assert the runner escalated the
/// right things without any I/O.
#[derive(Debug, Default)]
pub struct RecordingNotifier {
    events: parking_lot::Mutex<Vec<EscalationEvent>>,
}

impl RecordingNotifier {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn events(&self) -> Vec<EscalationEvent> {
        self.events.lock().clone()
    }
    pub fn len(&self) -> usize {
        self.events.lock().len()
    }
    pub fn is_empty(&self) -> bool {
        self.events.lock().is_empty()
    }
}

impl Notifier for RecordingNotifier {
    fn notify(&self, ev: &EscalationEvent) {
        self.events.lock().push(ev.clone());
    }
}

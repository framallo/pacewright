//! The real escalation notifier — "call Claude when there is an issue".
//!
//! Two channels, both best-effort (a notifier failure must never fail the task it reports on):
//!
//! 1. **Outbox** (always): write the escalation as a JSON file under
//!    `~/.pacewright/escalations/`. The pacewright MCP server / Claude skill drains this
//!    (`escalations` tool, `pcw escalations`) so Claude sees exactly what failed and why — the
//!    MCP-driven form of "call Claude".
//! 2. **Active `claude -p`** (opt-in via `PACEWRIGHT_CLAUDE_NOTIFY=1`): spawn a headless Claude with
//!    a repair prompt, fire-and-forget. Off by default so a daemon on a machine without the `claude`
//!    CLI, or an operator who reviews via the digest, isn't surprised by spawned processes.
use pacewright_core::notify::{EscalationEvent, Notifier};
use std::path::PathBuf;

/// `~/.pacewright/escalations` (honoring `PACEWRIGHT_HOME`). Shared with the escalations RPC so the
/// daemon reads back exactly what the notifier wrote.
pub fn escalations_dir() -> PathBuf {
    pacewright_core::run::home_dir().join("escalations")
}

/// Writes each escalation to the outbox and (opt-in) shells `claude -p`.
pub struct OutboxNotifier;

impl OutboxNotifier {
    pub fn new() -> Self {
        OutboxNotifier
    }
}

impl Default for OutboxNotifier {
    fn default() -> Self {
        Self::new()
    }
}

/// A one-line human summary + the concrete commands a human/Claude would run to investigate and
/// clear the issue. Pure so it can be unit-tested and reused by the `claude -p` prompt.
pub fn repair_hint(ev: &EscalationEvent) -> String {
    let mut lines = vec![format!(
        "pacewright escalation ({}): task {} [{}/{}] failed: {}",
        ev.kind.as_str(),
        ev.task_id,
        ev.adapter,
        ev.action,
        ev.error.as_deref().unwrap_or("(no error recorded)"),
    )];
    lines.push(format!("Investigate: pcw get {}", ev.task_id));
    if let Some(scope) = &ev.paused_scope {
        lines.push(format!(
            "Scope `{scope}` is PAUSED so queued siblings don't repeat this. \
             After fixing the cause: pcw resume {scope}"
        ));
    }
    if let Some(run) = &ev.run_id {
        lines.push(format!("Run: pcw show {run}  (resume with: pcw run <pipeline> --run-id {run} --retry-failed)"));
    }
    lines.join("\n")
}

impl Notifier for OutboxNotifier {
    fn notify(&self, ev: &EscalationEvent) {
        let dir = escalations_dir();
        if let Err(e) = std::fs::create_dir_all(&dir) {
            tracing::error!("escalation outbox: cannot create {}: {e}", dir.display());
            return;
        }
        let file = dir.join(format!("{}-{}.json", ev.at_ms, ev.task_id));
        let doc = serde_json::json!({
            "event": ev,
            "hint": repair_hint(ev),
        });
        match serde_json::to_vec_pretty(&doc) {
            Ok(bytes) => {
                if let Err(e) = std::fs::write(&file, bytes) {
                    tracing::error!("escalation outbox: write {} failed: {e}", file.display());
                } else {
                    tracing::warn!(
                        task = %ev.task_id,
                        kind = %ev.kind.as_str(),
                        "escalation written to {}",
                        file.display()
                    );
                }
            }
            Err(e) => tracing::error!("escalation outbox: serialize failed: {e}"),
        }

        // Opt-in active channel: actually shell `claude -p`, fire-and-forget.
        if std::env::var("PACEWRIGHT_CLAUDE_NOTIFY").as_deref() == Ok("1") {
            spawn_claude(ev);
        }
    }
}

/// Fire-and-forget `claude -p <repair prompt>`. Best-effort: a spawn failure (no `claude` on PATH)
/// is logged and ignored — the outbox file already captured the issue.
fn spawn_claude(ev: &EscalationEvent) {
    let prompt = format!(
        "A pacewright automation task just failed and needs triage. {}\n\n\
         Use the pacewright MCP tools (get_task, digest, resume) to investigate, and either fix the \
         root cause or explain what a human must do. Do not blindly resume a paused scope.",
        repair_hint(ev)
    );
    let mut cmd = std::process::Command::new("claude");
    cmd.arg("-p")
        .arg(prompt)
        .arg("--dangerously-skip-permissions");
    if let Ok(model) = std::env::var("PACEWRIGHT_CLAUDE_MODEL") {
        cmd.arg("--model").arg(model);
    }
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    match cmd.spawn() {
        Ok(_child) => tracing::warn!(task = %ev.task_id, "spawned `claude -p` to triage the failure"),
        Err(e) => tracing::error!("could not spawn `claude -p`: {e} (the escalation is still in the outbox)"),
    }
}

/// List the outbox escalations, newest first. Each entry is the JSON document written by
/// [`OutboxNotifier`]. `drain=true` deletes each file after reading it (a one-shot pull).
pub fn list_escalations(drain: bool) -> Vec<serde_json::Value> {
    let dir = escalations_dir();
    let mut files: Vec<PathBuf> = match std::fs::read_dir(&dir) {
        Ok(rd) => rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "json"))
            .collect(),
        Err(_) => return Vec::new(),
    };
    // File names are `<at_ms>-<task_id>.json`, so a reverse lexical sort is newest-first.
    files.sort();
    files.reverse();
    let mut out = Vec::new();
    for f in files {
        if let Ok(text) = std::fs::read_to_string(&f) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                out.push(v);
            }
        }
        if drain {
            std::fs::remove_file(&f).ok();
        }
    }
    out
}

//! Expanding a `PipelineDef` into ordinary tasks, and starting/resuming a run.
//!
//! The engine gains no knowledge of what any step does, only how steps relate. The
//! verification gate is expressed purely in existing primitives: a step declaring
//! `verify` expands into TWO tasks, and dependents wait on the verify rather than on the
//! step. So "a step is not done until it is confirmed" needs no runner changes at all.
use crate::model::{Task, TaskStatus};
use crate::pipeline::PipelineDef;
use crate::store::Store;
use serde_json::{Map, Value};
use std::collections::HashMap;

#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error("missing required var: {0}")]
    MissingVar(String),
    #[error("step {0} lists multiple `after` deps; only one is supported")]
    MultiDep(String),
    #[error("store error: {0}")]
    Store(#[from] rusqlite::Error),
}

/// The synthetic step holding a run's vars, so steps reference them through the same
/// mechanism as step results with no extra table.
pub const VARS_STEP: &str = "__vars";

/// "adapter/action"; a bare name means an action on the recipe adapter.
fn split_recipe(r: &str) -> (String, String) {
    match r.split_once('/') {
        Some((a, b)) => (a.to_string(), b.to_string()),
        None => ("recipe".to_string(), r.to_string()),
    }
}

/// Apply declared defaults, then enforce `required`.
fn effective_vars(def: &PipelineDef, vars: &Value) -> Result<Value, RunError> {
    let mut v: Map<String, Value> = vars.as_object().cloned().unwrap_or_default();
    for vd in &def.vars {
        if !v.contains_key(&vd.name) {
            match &vd.default {
                Some(d) => {
                    v.insert(vd.name.clone(), Value::String(d.clone()));
                }
                None if vd.required => return Err(RunError::MissingVar(vd.name.clone())),
                None => {}
            }
        }
    }
    Ok(Value::Object(v))
}

/// Expand a pipeline into the tasks that implement it. Pure: touches no store.
pub fn expand(
    def: &PipelineDef, run_id: &str, vars: &Value, now_ms: i64,
) -> Result<Vec<Task>, RunError> {
    let vars = effective_vars(def, vars)?;

    let mut out: Vec<Task> = Vec::new();

    // The vars sentinel is already terminal: it carries data, it never executes.
    let mut vt = Task::new_now("noop", "vars", vars, now_ms);
    vt.run_id = Some(run_id.to_string());
    vt.step_name = Some(VARS_STEP.to_string());
    vt.dedup_key = Some(format!("{run_id}:{VARS_STEP}"));
    vt.status = TaskStatus::Succeeded;
    vt.finished_at = Some(now_ms);
    out.push(vt);

    // step name -> the task id a DEPENDENT must wait on (the verify, when present).
    let mut gate: HashMap<String, String> = HashMap::new();

    for s in &def.steps {
        if s.after.len() > 1 {
            return Err(RunError::MultiDep(s.name.clone()));
        }
        let (adapter, action) = split_recipe(&s.recipe);
        let mut t = Task::new_now(adapter, action, s.params.clone(), now_ms);
        t.run_id = Some(run_id.to_string());
        t.step_name = Some(s.name.clone());
        t.dedup_key = Some(format!("{run_id}:{}", s.name));
        t.max_attempts = s.attempts.max(1);
        t.pace_ms = if def.pace_min_ms > 0 { Some(def.pace_min_ms) } else { None };
        if let Some(dep) = s.after.first() {
            t.depends_on = gate.get(dep).cloned();
            t.status = TaskStatus::Blocked;
        }
        let step_id = t.id.clone();
        out.push(t);

        match &s.verify {
            Some(sub) => {
                let (va, vac) = split_recipe(&sub.recipe);
                let mut vt = Task::new_now(va, vac, sub.params.clone(), now_ms);
                vt.run_id = Some(run_id.to_string());
                vt.step_name = Some(format!("{}.verify", s.name));
                vt.dedup_key = Some(format!("{run_id}:{}.verify", s.name));
                vt.depends_on = Some(step_id);
                vt.status = TaskStatus::Blocked;
                // A verification is a check, not an action: retrying it forever just
                // repeats the same query. One attempt unless the pipeline says otherwise.
                vt.max_attempts = 1;
                let vid = vt.id.clone();
                out.push(vt);
                // THE GATE: dependents wait on the verify, never on the raw step.
                gate.insert(s.name.clone(), vid);
            }
            None => {
                gate.insert(s.name.clone(), step_id);
            }
        }
    }
    Ok(out)
}

/// Start or resume a run. Idempotent: a step whose task already exists is never
/// recreated, so re-running after a mid-pipeline failure continues from the failed step
/// and redoes nothing that already worked.
pub fn start(
    store: &Store, def: &PipelineDef, run_id: &str, vars: &Value, now_ms: i64,
) -> Result<Vec<Task>, RunError> {
    let planned = expand(def, run_id, vars, now_ms)?;
    let mut inserted = Vec::new();
    for t in planned {
        let key = t.dedup_key.clone().unwrap_or_default();
        if store.find_by_dedup_any(&key)?.is_some() {
            continue; // already present in any state: succeeded, running, or queued
        }
        store.insert_task(&t)?;
        inserted.push(t);
    }
    Ok(inserted)
}

/// Re-queue the failed steps of a run so a resume can retry them. Succeeded steps are
/// left untouched; that is the point.
pub fn retry_failed(store: &Store, run_id: &str, now_ms: i64) -> Result<usize, RunError> {
    let mut n = 0;
    for mut t in store.tasks_in_run(run_id)? {
        if t.status == TaskStatus::Failed {
            t.status = if t.depends_on.is_some() { TaskStatus::Blocked } else { TaskStatus::Pending };
            t.attempts = 0;
            t.last_error = None;
            t.finished_at = None;
            t.updated_at = now_ms;
            store.update_task(&t)?;
            n += 1;
        }
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::parse_pipeline;
    use serde_json::json;

    const SRC: &str = r#"
pipeline "demo" {
    step "first" recipe="dummy/echo" {
        params { a "{{ vars.a }}" }
        verify recipe="dummy/check" { params { id "{{ steps.first.result.id }}" } }
    }
    step "second" recipe="dummy/echo" after="first" { params { b "2" } }
}
"#;

    fn by<'a>(tasks: &'a [Task], n: &str) -> &'a Task {
        tasks.iter().find(|t| t.step_name.as_deref() == Some(n)).unwrap()
    }

    #[test]
    fn verify_task_is_inserted_and_gates_the_dependent() {
        let def = parse_pipeline(SRC).unwrap();
        let tasks = expand(&def, "run1", &json!({"a": "x"}), 1_000).unwrap();
        assert_eq!(tasks.len(), 4, "__vars, first, first.verify, second");

        let first = by(&tasks, "first");
        let verify = by(&tasks, "first.verify");
        let second = by(&tasks, "second");

        assert_eq!(verify.depends_on.as_deref(), Some(first.id.as_str()));
        // The gate: `second` waits on the VERIFY, never on the raw step.
        assert_eq!(second.depends_on.as_deref(), Some(verify.id.as_str()));

        assert_eq!(first.run_id.as_deref(), Some("run1"));
        assert_eq!(first.dedup_key.as_deref(), Some("run1:first"));
        assert_eq!(verify.dedup_key.as_deref(), Some("run1:first.verify"));
        assert_eq!(verify.max_attempts, 1, "a check should not be retried by default");
    }

    #[test]
    fn a_step_without_verify_is_depended_on_directly() {
        let src = r#"pipeline "d" {
            step "a" recipe="dummy/echo" { }
            step "b" recipe="dummy/echo" after="a" { }
        }"#;
        let def = parse_pipeline(src).unwrap();
        let tasks = expand(&def, "r", &json!({}), 0).unwrap();
        assert_eq!(tasks.len(), 3, "__vars, a, b");
        assert_eq!(by(&tasks, "b").depends_on.as_deref(), Some(by(&tasks, "a").id.as_str()));
    }

    #[test]
    fn vars_sentinel_carries_defaults_and_is_already_terminal() {
        let src = r#"pipeline "d" {
            var "greeting" default="hi"
            step "a" recipe="dummy/echo" { }
        }"#;
        let def = parse_pipeline(src).unwrap();
        let tasks = expand(&def, "r", &json!({}), 0).unwrap();
        let v = by(&tasks, VARS_STEP);
        assert_eq!(v.status, TaskStatus::Succeeded);
        assert_eq!(v.params["greeting"], "hi");
    }

    #[test]
    fn missing_required_var_is_rejected() {
        // Nodes need a newline (or `;`) between them, else `step` parses as another
        // entry of the `var` node rather than a sibling.
        let src = r#"pipeline "d" {
            var "need" required=#true
            step "a" recipe="dummy/echo" { }
        }"#;
        let def = parse_pipeline(src).unwrap();
        assert!(matches!(
            expand(&def, "r", &json!({}), 0),
            Err(RunError::MissingVar(ref s)) if s == "need"
        ));
    }

    #[test]
    fn start_is_idempotent_and_resume_skips_succeeded_steps() {
        let store = Store::open_in_memory().unwrap();
        let src = r#"pipeline "d" {
            step "a" recipe="dummy/echo" { }
            step "b" recipe="dummy/echo" after="a" { }
        }"#;
        let def = parse_pipeline(src).unwrap();

        let first = start(&store, &def, "r1", &json!({}), 0).unwrap();
        assert_eq!(first.len(), 3, "__vars, a, b");

        // Mark `a` succeeded, then re-start the same run.
        let mut a = store
            .tasks_in_run("r1")
            .unwrap()
            .into_iter()
            .find(|t| t.step_name.as_deref() == Some("a"))
            .unwrap();
        a.status = TaskStatus::Succeeded;
        store.update_task(&a).unwrap();

        let again = start(&store, &def, "r1", &json!({}), 0).unwrap();
        assert!(again.is_empty(), "nothing may be recreated");
        assert_eq!(store.tasks_in_run("r1").unwrap().len(), 3, "no duplicate rows");
        assert_eq!(
            store
                .tasks_in_run("r1")
                .unwrap()
                .iter()
                .filter(|t| t.step_name.as_deref() == Some(VARS_STEP))
                .count(),
            1,
            "the vars sentinel must not be re-inserted"
        );
    }

    #[test]
    fn retry_failed_requeues_only_failed_steps() {
        let store = Store::open_in_memory().unwrap();
        let src = r#"pipeline "d" {
            step "a" recipe="dummy/echo" { }
            step "b" recipe="dummy/echo" after="a" { }
        }"#;
        let def = parse_pipeline(src).unwrap();
        start(&store, &def, "r2", &json!({}), 0).unwrap();

        let mut tasks = store.tasks_in_run("r2").unwrap();
        for t in tasks.iter_mut() {
            match t.step_name.as_deref() {
                Some("a") => t.status = TaskStatus::Succeeded,
                Some("b") => {
                    t.status = TaskStatus::Failed;
                    t.attempts = 3;
                }
                _ => {}
            }
            store.update_task(t).unwrap();
        }

        assert_eq!(retry_failed(&store, "r2", 10).unwrap(), 1);
        let after = store.tasks_in_run("r2").unwrap();
        let a = after.iter().find(|t| t.step_name.as_deref() == Some("a")).unwrap();
        let b = after.iter().find(|t| t.step_name.as_deref() == Some("b")).unwrap();
        assert_eq!(a.status, TaskStatus::Succeeded, "succeeded work is never redone");
        assert_eq!(b.status, TaskStatus::Blocked);
        assert_eq!(b.attempts, 0);
    }
}

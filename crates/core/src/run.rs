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
    def: &PipelineDef,
    run_id: &str,
    vars: &Value,
    now_ms: i64,
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
    // Stash the pipeline's output templates on the terminal vars sentinel so `show` can collapse
    // the run into a final report later, without re-reading the pipeline file.
    vt.result = Some(serde_json::json!({ "output": def.output.clone() }));
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
        t.pace_ms = if def.pace_min_ms > 0 {
            Some(def.pace_min_ms)
        } else {
            None
        };
        // If a fanout declares this step as its producer, stash the spec on the task so the runner
        // can materialize the paced/deduped act tasks when this step succeeds — without re-reading
        // the pipeline file. (Multiple fanouts off one producer aren't supported; take the first.)
        if let Some(f) = def.fanouts.iter().find(|f| f.after == s.name) {
            t.fanout = Some(serde_json::to_value(f).expect("FanoutDef serializes"));
        }
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

                // A fallback runs only when the verify FAILED, and can adjudicate it.
                if let Some(fb) = &s.fallback {
                    let (fa, fac) = split_recipe(&fb.recipe);
                    let mut ft = Task::new_now(fa, fac, fb.params.clone(), now_ms);
                    ft.run_id = Some(run_id.to_string());
                    ft.step_name = Some(format!("{}.fallback", s.name));
                    ft.dedup_key = Some(format!("{run_id}:{}.fallback", s.name));
                    ft.depends_on = Some(vid.clone());
                    ft.dep_on_failure = true;
                    ft.status = TaskStatus::Blocked;
                    ft.max_attempts = 1;
                    vt.escalation = Some(ft.id.clone());
                    out.push(vt);
                    out.push(ft);
                } else {
                    out.push(vt);
                }
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
    store: &Store,
    def: &PipelineDef,
    run_id: &str,
    vars: &Value,
    now_ms: i64,
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
            t.status = if t.depends_on.is_some() {
                TaskStatus::Blocked
            } else {
                TaskStatus::Pending
            };
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
        tasks
            .iter()
            .find(|t| t.step_name.as_deref() == Some(n))
            .unwrap()
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
        assert_eq!(
            verify.max_attempts, 1,
            "a check should not be retried by default"
        );
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
        assert_eq!(
            by(&tasks, "b").depends_on.as_deref(),
            Some(by(&tasks, "a").id.as_str())
        );
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
        assert_eq!(
            store.tasks_in_run("r1").unwrap().len(),
            3,
            "no duplicate rows"
        );
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
        let a = after
            .iter()
            .find(|t| t.step_name.as_deref() == Some("a"))
            .unwrap();
        let b = after
            .iter()
            .find(|t| t.step_name.as_deref() == Some("b"))
            .unwrap();
        assert_eq!(
            a.status,
            TaskStatus::Succeeded,
            "succeeded work is never redone"
        );
        assert_eq!(b.status, TaskStatus::Blocked);
        assert_eq!(b.attempts, 0);
    }

    #[test]
    fn resolve_output_fills_in_from_results() {
        let src = r#"pipeline "d" {
            step "a" recipe="dummy/echo" { }
            output {
                got "{{ steps.a.result.id }}"
                note "{{ vars.n }}"
            }
        }"#;
        let def = parse_pipeline(src).unwrap();
        let mut tasks = expand(&def, "r", &json!({"n": "hi"}), 0).unwrap();
        // Before `a` produces a result its key is omitted; the vars-backed key already resolves.
        let early = resolve_output(&tasks).unwrap();
        assert_eq!(early.get("note").unwrap(), "hi");
        assert!(
            early.get("got").is_none(),
            "an unresolved step ref is omitted until produced"
        );
        // Give `a` a result -> the full report resolves.
        for t in tasks.iter_mut() {
            if t.step_name.as_deref() == Some("a") {
                t.result = Some(json!({"id": "X9"}));
            }
        }
        let full = resolve_output(&tasks).unwrap();
        assert_eq!(full.get("got").unwrap(), "X9");
        assert_eq!(full.get("note").unwrap(), "hi");
    }

    #[test]
    fn resolve_output_is_none_without_an_output_block() {
        let src = r#"pipeline "d" { step "a" recipe="dummy/echo" { } }"#;
        let def = parse_pipeline(src).unwrap();
        let tasks = expand(&def, "r", &json!({}), 0).unwrap();
        assert!(resolve_output(&tasks).is_none());
    }
}

/// The pacewright home dir. Read once by callers, never inside helpers, so tests never
/// have to mutate process-global env (which destabilizes parallel tests).
pub fn home_dir() -> std::path::PathBuf {
    std::env::var("PACEWRIGHT_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".pacewright")
        })
}

/// Where a run's shared dataset lives. Recipes execute in a separate process and cannot
/// read the store, so the run's accumulated state is projected to a JSON file they load.
pub fn dataset_path(home: &std::path::Path, run_id: &str) -> std::path::PathBuf {
    home.join("runs").join(format!("{run_id}.json"))
}

/// Write the run dataset. A REGENERABLE PROJECTION of the tasks, never the source of
/// truth: if it is missing or stale it is simply rewritten from the store before the next
/// dispatch. Making the file authoritative would reintroduce the "did this actually
/// happen" ambiguity that verification exists to remove.
pub fn write_dataset(
    home: &std::path::Path,
    run_id: &str,
    vars: &Value,
    results: &HashMap<String, Value>,
) -> std::io::Result<std::path::PathBuf> {
    let path = dataset_path(home, run_id);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let steps: Map<String, Value> = results
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let doc = serde_json::json!({
        "run_id": run_id,
        "vars": vars,
        "steps": Value::Object(steps),
    });
    std::fs::write(&path, serde_json::to_vec_pretty(&doc)?)?;
    Ok(path)
}

/// Collapse a run's declared `output` block into a final report. The output templates were stashed
/// on the vars sentinel at expansion; here they are resolved key-by-key against the run's
/// accumulated (and verified) results. A key whose references cannot yet resolve is omitted, so the
/// report fills in as the run progresses. Returns None when the pipeline declared no output.
/// Generic: it understands only `{{ steps.*.result.* }}` / `{{ steps.*.verify.result.* }}` / `{{ vars.* }}`.
pub fn resolve_output(tasks: &[Task]) -> Option<Value> {
    let mut vars = Value::Object(Map::new());
    let mut results: HashMap<String, Value> = HashMap::new();
    let mut template: Option<Value> = None;
    for t in tasks {
        match t.step_name.as_deref() {
            Some(VARS_STEP) => {
                vars = t.params.clone();
                template = t.result.as_ref().and_then(|r| r.get("output").cloned());
            }
            Some(name) => {
                if let Some(res) = t.result.clone() {
                    results.insert(name.to_string(), res);
                }
            }
            None => {}
        }
    }
    let template = template?;
    let obj = template.as_object()?;
    if obj.is_empty() {
        return None;
    }
    let mut out = Map::new();
    for (k, v) in obj {
        if let Ok(resolved) = crate::refs::resolve(v, &vars, &results) {
            out.insert(k.clone(), resolved);
        }
    }
    Some(Value::Object(out))
}

/// The pipeline file for `name`: `<home>/recipes/pipelines/<name>.kdl`, with `/` in the name
/// flattened to `-` (so `linkedin/comment` -> `linkedin-comment.kdl`). The single source of this
/// path convention, shared by the daemon's RunStart handler and the pipeline launcher.
pub fn pipeline_path(home: &std::path::Path, name: &str) -> std::path::PathBuf {
    home.join("recipes/pipelines")
        .join(format!("{}.kdl", name.replace('/', "-")))
}

/// Dig a dotted path into a JSON value; an empty path returns the value itself.
fn dig_path(root: &Value, path: &str) -> Value {
    if path.trim().is_empty() {
        return root.clone();
    }
    let mut cur = root;
    for p in path.split('.') {
        match cur.get(p) {
            Some(v) => cur = v,
            None => return Value::Null,
        }
    }
    cur.clone()
}

/// Turn a succeeded fanout-producer task into paced, deduped act tasks (R5 + R7) — the declarative
/// replacement for the bash "paced `while read` loop calling the act recipe once per queued item".
/// Clock-free (`now_ms` passed in); reads the store only to skip already-touched targets (the
/// all-time ledger) and already-queued siblings. A missing/invalid spec or a non-array result fans
/// out nothing rather than erroring, so a producer with no usable output is a clean no-op.
///
/// Each act task is INDEPENDENT (no `run_id`): its params are already fully resolved here with the
/// item bound, so it must not be re-resolved at dispatch against the run's (item-less) vars. Pacing
/// and the daily cap come from the act recipe's `limit-key` via the limits engine, exactly as the
/// per-script gap+cap did. A preflight throw pauses the scope (R9) so the batch doesn't repeat it.
pub fn materialize_fanout(
    store: &Store,
    producer: &Task,
    now_ms: i64,
) -> rusqlite::Result<Vec<Task>> {
    let Some(spec_val) = producer.fanout.clone() else {
        return Ok(Vec::new());
    };
    let Ok(spec) = serde_json::from_value::<crate::pipeline::FanoutDef>(spec_val) else {
        return Ok(Vec::new());
    };
    let result = producer.result.clone().unwrap_or(Value::Null);
    let items = dig_path(&result, &spec.items);
    let Some(items) = items.as_array() else {
        return Ok(Vec::new());
    };

    // Bind the run's vars so per-item templates can reference `{{ vars.<pipeline-var> }}` too.
    let mut base_vars = Value::Object(Default::default());
    if let Some(rid) = &producer.run_id {
        for sib in store.tasks_in_run(rid)? {
            if sib.step_name.as_deref() == Some(VARS_STEP) {
                base_vars = sib.params.clone();
            }
        }
    }
    let (adapter, action) = split_recipe(&spec.recipe);
    let no_results: HashMap<String, Value> = HashMap::new();
    let mut out = Vec::new();
    for item in items {
        let mut vars = base_vars.clone();
        if let Some(o) = vars.as_object_mut() {
            o.insert(spec.as_var.clone(), item.clone());
        }
        let id = match crate::refs::resolve(&Value::String(spec.id.clone()), &vars, &no_results) {
            Ok(Value::String(s)) => s,
            Ok(other) => other.to_string(),
            Err(_) => continue, // an item missing the id field is skipped, not fatal
        };
        if id.trim().is_empty() {
            continue;
        }
        // All-time ledger: never act on the same target twice (R7).
        if store.is_touched(&spec.scope, &id)? {
            continue;
        }
        // Active-task dedup on the LEDGER identity, so a still-queued attempt from a prior run is
        // reused, not duplicated; a terminal-failed prior attempt is inactive, so it re-queues.
        let dedup = format!("touch:{}:{}", spec.scope, id);
        if store.find_active_by_dedup(&dedup)?.is_some() {
            continue;
        }
        let params = match crate::refs::resolve(&spec.params, &vars, &no_results) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let mut t = Task::new_now(adapter.clone(), action.clone(), params, now_ms);
        t.dedup_key = Some(dedup);
        t.touch_scope = Some(spec.scope.clone());
        t.touch_id = Some(id);
        t.pause_scope_on_failure = true;
        out.push(t);
    }
    Ok(out)
}

#[cfg(test)]
mod dataset_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn dataset_is_written_with_vars_and_step_results() {
        let tmp = std::env::temp_dir().join(format!("pw-ds-{}", std::process::id()));
        let results = HashMap::from([(
            "publish_long".to_string(),
            json!({"publish": {"video_id": "abc123"}}),
        )]);
        let path =
            write_dataset(&tmp, "ep172", &json!({"episode_number": "172"}), &results).unwrap();

        let doc: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(doc["run_id"], "ep172");
        assert_eq!(doc["vars"]["episode_number"], "172");
        assert_eq!(
            doc["steps"]["publish_long"]["publish"]["video_id"],
            "abc123"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }
}

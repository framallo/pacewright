# Pipeline Runs (Phase 1) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Run a declarative multi-step pipeline as one durable, resumable, paced group of tasks that passes results between steps and gates each step behind an independent verification, with no domain concepts in `pacewright-core`.

**Architecture:** A *run* is a group of ordinary `Task` rows sharing `run_id`, each tagged `step_name`, wired with the existing `depends_on`. A step declaring `verify` expands into **two** tasks (`<step>` and `<step>.verify`); anything depending on that step depends on the **verify** task. The verification gate therefore emerges from the existing dependency scheduler and **the runner is not modified at all**. Params are resolved against sibling results immediately before dispatch.

**Tech Stack:** Rust 2021, `rusqlite` (SQLite WAL), `serde_json`, `kdl`, `tokio`, `async-trait`. Tests use `pacewright-adapter-dummy`, no browser.

## Global Constraints

- `pacewright-core` MUST NOT contain podcast, Riverside, YouTube, or Spotify concepts. Step logic lives in recipes.
- Runs are automatic. No approval gates, no human pause states.
- A step is not complete until its `verify` passes. Dependents release on verified completion.
- Re-running a pipeline MUST NOT re-execute a step that already succeeded.
- `cargo clippy -- -D warnings` must stay clean (repo standard).
- The existing `Task`, `scheduler`, `runner`, and `limits` behaviour must not regress; all current tests keep passing.
- Spec: `docs/specs/2026-07-22-pipeline-runs-and-verified-steps.md`.

---

### Task 1: Run columns on `tasks`

**Files:**
- Modify: `crates/core/src/store.rs` (SCHEMA const, and `Store::open`)
- Modify: `crates/core/src/model.rs:44-63` (`Task` struct + `new_now`)
- Test: `crates/core/tests/engine.rs`

**Interfaces:**
- Consumes: nothing.
- Produces: `Task.run_id: Option<String>`, `Task.step_name: Option<String>`; `Store::find_by_dedup_any(&str) -> rusqlite::Result<Option<Task>>`; `Store::tasks_in_run(&str) -> rusqlite::Result<Vec<Task>>`.

- [ ] **Step 1: Write the failing test**

In `crates/core/tests/engine.rs`:

```rust
#[test]
fn test_run_columns_roundtrip_and_lookup() {
    let store = Store::open_in_memory().unwrap();
    let mut t = Task::new_now("dummy", "echo", serde_json::json!({}), 1_000);
    t.run_id = Some("ep172".into());
    t.step_name = Some("publish_long".into());
    t.dedup_key = Some("ep172:publish_long".into());
    store.insert_task(&t).unwrap();

    let got = store.get_task(&t.id).unwrap().unwrap();
    assert_eq!(got.run_id.as_deref(), Some("ep172"));
    assert_eq!(got.step_name.as_deref(), Some("publish_long"));

    let in_run = store.tasks_in_run("ep172").unwrap();
    assert_eq!(in_run.len(), 1);

    // find_by_dedup_any must see TERMINAL tasks (this is what makes resume work,
    // unlike find_active_by_dedup which deliberately excludes them).
    let mut done = got.clone();
    done.status = TaskStatus::Succeeded;
    store.update_task(&done).unwrap();
    assert!(store.find_active_by_dedup("ep172:publish_long").unwrap().is_none());
    assert!(store.find_by_dedup_any("ep172:publish_long").unwrap().is_some());
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p pacewright-core test_run_columns_roundtrip_and_lookup`
Expected: FAIL to compile, `no field 'run_id' on type 'Task'`.

- [ ] **Step 3: Write minimal implementation**

In `crates/core/src/model.rs`, add to `Task` (after `dedup_key`):

```rust
    pub run_id: Option<String>,
    pub step_name: Option<String>,
```

and in `Task::new_now`, after `dedup_key: None,`:

```rust
            run_id: None,
            step_name: None,
```

In `crates/core/src/store.rs`, add to the `SCHEMA` const inside `CREATE TABLE tasks` (after `dedup_key TEXT,`):

```sql
    run_id TEXT,
    step_name TEXT,
```

Existing databases already have the table, so `CREATE TABLE IF NOT EXISTS` will not add columns. Add an idempotent migration. In `Store::open`, immediately after the `SCHEMA` execute:

```rust
        // Additive migration: CREATE TABLE IF NOT EXISTS won't alter an existing
        // tasks table, so add run columns when they're missing. Ignoring the
        // "duplicate column name" error is the idiomatic sqlite way to do this.
        for ddl in [
            "ALTER TABLE tasks ADD COLUMN run_id TEXT",
            "ALTER TABLE tasks ADD COLUMN step_name TEXT",
        ] {
            match conn.execute(ddl, []) {
                Ok(_) => {}
                Err(e) if e.to_string().contains("duplicate column name") => {}
                Err(e) => return Err(e),
            }
        }
        conn.execute(
            "CREATE UNIQUE INDEX IF NOT EXISTS idx_tasks_run_step \
             ON tasks(run_id, step_name) WHERE run_id IS NOT NULL",
            [],
        )?;
```

Extend `row_to_task` with `run_id: row.get("run_id")?,` and `step_name: row.get("step_name")?,`, and add both columns to the INSERT and UPDATE statements and their parameter lists (`store.rs:109` and `store.rs:125`).

Add the two lookups:

```rust
    /// Find a task by dedup key INCLUDING terminal ones. `find_active_by_dedup`
    /// deliberately excludes terminal tasks so a finished recurrence can be
    /// re-queued; resume needs the opposite, to see that a step already succeeded.
    pub fn find_by_dedup_any(&self, key: &str) -> rusqlite::Result<Option<Task>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row("SELECT * FROM tasks WHERE dedup_key=? LIMIT 1", params![key], Self::row_to_task)
            .optional()
    }

    pub fn tasks_in_run(&self, run_id: &str) -> rusqlite::Result<Vec<Task>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT * FROM tasks WHERE run_id=? ORDER BY created_at")?;
        let rows = stmt.query_map(params![run_id], Self::row_to_task)?;
        rows.collect()
    }
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p pacewright-core && cargo clippy -p pacewright-core -- -D warnings`
Expected: PASS, no warnings. All pre-existing tests still pass.

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/model.rs crates/core/src/store.rs crates/core/tests/engine.rs
git commit -m "feat(core): run_id/step_name on tasks + dedup lookup incl. terminal"
```

---

### Task 2: Result-reference resolution

**Files:**
- Create: `crates/core/src/refs.rs`
- Modify: `crates/core/src/lib.rs` (add `pub mod refs;`)
- Test: inline `#[cfg(test)]` in `crates/core/src/refs.rs`

**Interfaces:**
- Consumes: nothing.
- Produces: `pub fn resolve(params: &Value, vars: &Value, results: &HashMap<String, Value>) -> Result<Value, RefError>` and `pub enum RefError { Unresolved(String) }`. `results` maps `step_name -> that step's result JSON`.

- [ ] **Step 1: Write the failing test**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn results() -> HashMap<String, Value> {
        HashMap::from([("publish_long".to_string(), json!({"publish": {"video_id": "abc123"}}))])
    }

    #[test]
    fn resolves_step_results_and_vars() {
        let params = json!({
            "video_id": "{{ steps.publish_long.result.publish.video_id }}",
            "title": "EP{{ vars.episode_number }} {{ vars.title }}",
            "untouched": 7
        });
        let vars = json!({"episode_number": "172", "title": "Jane Doe"});
        let out = resolve(&params, &vars, &results()).unwrap();
        assert_eq!(out["video_id"], "abc123");
        assert_eq!(out["title"], "EP172 Jane Doe");
        assert_eq!(out["untouched"], 7);
    }

    #[test]
    fn a_whole_value_reference_keeps_its_json_type() {
        // Sole-reference params must not be stringified: a later step may need the array.
        let params = json!({"clips": "{{ steps.publish_long.result.publish }}"});
        let out = resolve(&params, &json!({}), &results()).unwrap();
        assert_eq!(out["clips"], json!({"video_id": "abc123"}));
    }

    #[test]
    fn unresolved_reference_is_an_error_not_a_literal() {
        // Sending a literal "{{ ... }}" to a browser is the failure mode we refuse.
        let params = json!({"x": "{{ steps.nope.result.y }}"});
        let err = resolve(&params, &json!({}), &results()).unwrap_err();
        assert!(matches!(err, RefError::Unresolved(ref s) if s.contains("steps.nope.result.y")));
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p pacewright-core refs::`
Expected: FAIL to compile, `unresolved module 'refs'`.

- [ ] **Step 3: Write minimal implementation**

Create `crates/core/src/refs.rs`:

```rust
//! Generic `{{ … }}` reference resolution for task params.
//!
//! Knows only about JSON: `steps.<step>.result.<path>` and `vars.<name>`. No adapter,
//! platform, or domain concepts, so any pipeline on any adapter gets this for free.
use serde_json::Value;
use std::collections::HashMap;

#[derive(Debug, thiserror::Error)]
pub enum RefError {
    #[error("unresolved reference: {0}")]
    Unresolved(String),
}

/// Matches a single `{{ … }}` occurrence, returning (start, end, inner-trimmed).
fn next_ref(s: &str) -> Option<(usize, usize, String)> {
    let start = s.find("{{")?;
    let rest = &s[start + 2..];
    let close = rest.find("}}")?;
    Some((start, start + 2 + close + 2, rest[..close].trim().to_string()))
}

fn lookup(expr: &str, vars: &Value, results: &HashMap<String, Value>) -> Option<Value> {
    let parts: Vec<&str> = expr.split('.').collect();
    match parts.as_slice() {
        ["vars", rest @ ..] => dig(vars, rest),
        ["steps", step, "result", rest @ ..] => dig(results.get(*step)?, rest),
        _ => None,
    }
}

fn dig<'a>(mut cur: &'a Value, path: &[&str]) -> Option<Value> {
    for p in path {
        cur = cur.get(p)?;
    }
    Some(cur.clone())
}

fn resolve_str(s: &str, vars: &Value, results: &HashMap<String, Value>) -> Result<Value, RefError> {
    // A string that is EXACTLY one reference keeps the referenced JSON type, so an
    // array or object survives into the next step instead of becoming a string.
    if let Some((0, end, expr)) = next_ref(s) {
        if end == s.len() {
            return lookup(&expr, vars, results).ok_or_else(|| RefError::Unresolved(expr));
        }
    }
    let mut out = String::new();
    let mut rem = s;
    while let Some((start, end, expr)) = next_ref(rem) {
        out.push_str(&rem[..start]);
        let v = lookup(&expr, vars, results).ok_or_else(|| RefError::Unresolved(expr))?;
        match v {
            Value::String(sv) => out.push_str(&sv),
            other => out.push_str(&other.to_string()),
        }
        rem = &rem[end..];
    }
    out.push_str(rem);
    Ok(Value::String(out))
}

pub fn resolve(params: &Value, vars: &Value, results: &HashMap<String, Value>) -> Result<Value, RefError> {
    Ok(match params {
        Value::String(s) => resolve_str(s, vars, results)?,
        Value::Array(a) => Value::Array(a.iter().map(|v| resolve(v, vars, results)).collect::<Result<_, _>>()?),
        Value::Object(o) => Value::Object(
            o.iter().map(|(k, v)| Ok((k.clone(), resolve(v, vars, results)?))).collect::<Result<_, RefError>>()?,
        ),
        other => other.clone(),
    })
}
```

Add `pub mod refs;` to `crates/core/src/lib.rs`. Confirm `thiserror` is in `crates/core/Cargo.toml`; if absent, add `thiserror = "1"`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p pacewright-core refs:: && cargo clippy -p pacewright-core -- -D warnings`
Expected: 3 passed, no warnings.

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/refs.rs crates/core/src/lib.rs crates/core/Cargo.toml
git commit -m "feat(core): generic {{steps.*.result}} / {{vars.*}} reference resolution"
```

---

### Task 3: Pipeline definition + KDL parsing

**Files:**
- Create: `crates/core/src/pipeline.rs`
- Modify: `crates/core/src/lib.rs` (add `pub mod pipeline;`)
- Test: inline `#[cfg(test)]` in `crates/core/src/pipeline.rs`

**Interfaces:**
- Consumes: nothing.
- Produces:

```rust
pub struct PipelineDef { pub name: String, pub pace_min_ms: i64, pub pace_max_ms: i64,
                         pub vars: Vec<VarDef>, pub steps: Vec<StepDef>, pub output: Value }
pub struct VarDef  { pub name: String, pub required: bool, pub default: Option<String> }
pub struct StepDef { pub name: String, pub recipe: String, pub params: Value,
                     pub after: Vec<String>, pub attempts: i64,
                     pub verify: Option<SubStep>, pub fallback: Option<SubStep> }
pub struct SubStep { pub recipe: String, pub params: Value }
pub fn parse_pipeline(kdl_src: &str) -> Result<PipelineDef, PipelineError>
```

- [ ] **Step 1: Write the failing test**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    const SRC: &str = r#"
pipeline "demo/two-step" {
    pace min="90s" max="5m"
    var "project_id" required=#true
    var "greeting" default="hi"
    step "first" recipe="dummy/echo" {
        params { project_id "{{ vars.project_id }}" }
        attempts 3
        verify recipe="dummy/check" { params { id "{{ steps.first.result.id }}" } }
    }
    step "second" recipe="dummy/echo" after="first" {
        params { note "{{ vars.greeting }}" }
    }
    output { final_id "{{ steps.first.result.id }}" }
}
"#;

    #[test]
    fn parses_steps_pace_vars_and_verify() {
        let p = parse_pipeline(SRC).unwrap();
        assert_eq!(p.name, "demo/two-step");
        assert_eq!(p.pace_min_ms, 90_000);
        assert_eq!(p.pace_max_ms, 300_000);

        assert_eq!(p.vars.len(), 2);
        assert!(p.vars[0].required);
        assert_eq!(p.vars[1].default.as_deref(), Some("hi"));

        assert_eq!(p.steps.len(), 2);
        let first = &p.steps[0];
        assert_eq!(first.recipe, "dummy/echo");
        assert_eq!(first.attempts, 3);
        assert_eq!(first.params["project_id"], "{{ vars.project_id }}");
        assert_eq!(first.verify.as_ref().unwrap().recipe, "dummy/check");
        assert!(first.after.is_empty());

        assert_eq!(p.steps[1].after, vec!["first".to_string()]);
        assert_eq!(p.output["final_id"], "{{ steps.first.result.id }}");
    }

    #[test]
    fn rejects_a_step_depending_on_an_unknown_step() {
        let src = r#"pipeline "x" { step "a" recipe="dummy/echo" after="ghost" { } }"#;
        assert!(parse_pipeline(src).is_err());
    }

    #[test]
    fn rejects_duplicate_step_names() {
        let src = r#"pipeline "x" { step "a" recipe="dummy/echo" { } step "a" recipe="dummy/echo" { } }"#;
        assert!(parse_pipeline(src).is_err());
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p pacewright-core pipeline::`
Expected: FAIL to compile, `unresolved module 'pipeline'`.

- [ ] **Step 3: Write minimal implementation**

Create `crates/core/src/pipeline.rs` with the structs from **Interfaces** above plus:

```rust
use kdl::{KdlDocument, KdlNode};
use serde_json::{Map, Value};

#[derive(Debug, thiserror::Error)]
pub enum PipelineError {
    #[error("kdl parse error: {0}")]
    Kdl(String),
    #[error("invalid pipeline: {0}")]
    Invalid(String),
}

/// "90s" / "5m" / "1500" (ms) -> milliseconds.
fn dur_ms(s: &str) -> Result<i64, PipelineError> {
    let s = s.trim();
    let (num, mult) = match s.chars().last() {
        Some('s') => (&s[..s.len() - 1], 1_000),
        Some('m') => (&s[..s.len() - 1], 60_000),
        Some('h') => (&s[..s.len() - 1], 3_600_000),
        _ => (s, 1),
    };
    num.trim().parse::<i64>().map(|n| n * mult)
        .map_err(|_| PipelineError::Invalid(format!("bad duration: {s}")))
}

fn first_str(n: &KdlNode) -> Option<String> {
    n.entries().iter().find(|e| e.name().is_none())
        .and_then(|e| e.value().as_string().map(|s| s.to_string()))
}

fn prop_str(n: &KdlNode, key: &str) -> Option<String> {
    n.entries().iter().find(|e| e.name().map(|k| k.value()) == Some(key))
        .and_then(|e| e.value().as_string().map(|s| s.to_string()))
}

fn prop_bool(n: &KdlNode, key: &str) -> Option<bool> {
    n.entries().iter().find(|e| e.name().map(|k| k.value()) == Some(key))
        .and_then(|e| e.value().as_bool())
}

/// A `params { k "v" ... }` block -> a flat JSON object of string values.
fn params_block(parent: &KdlNode) -> Value {
    let mut m = Map::new();
    if let Some(doc) = parent.children() {
        if let Some(p) = doc.nodes().iter().find(|n| n.name().value() == "params") {
            if let Some(pd) = p.children() {
                for n in pd.nodes() {
                    if let Some(v) = first_str(n) {
                        m.insert(n.name().value().to_string(), Value::String(v));
                    }
                }
            }
        }
    }
    Value::Object(m)
}

fn sub_step(parent: &KdlNode, kind: &str) -> Option<SubStep> {
    let doc = parent.children()?;
    let n = doc.nodes().iter().find(|n| n.name().value() == kind)?;
    Some(SubStep { recipe: prop_str(n, "recipe")?, params: params_block(n) })
}

pub fn parse_pipeline(src: &str) -> Result<PipelineDef, PipelineError> {
    let doc: KdlDocument = src.parse().map_err(|e| PipelineError::Kdl(format!("{e}")))?;
    let root = doc.nodes().iter().find(|n| n.name().value() == "pipeline")
        .ok_or_else(|| PipelineError::Invalid("no `pipeline` node".into()))?;
    let name = first_str(root).ok_or_else(|| PipelineError::Invalid("pipeline needs a name".into()))?;
    let body = root.children().ok_or_else(|| PipelineError::Invalid("pipeline needs a body".into()))?;

    let (mut pace_min_ms, mut pace_max_ms) = (0_i64, 0_i64);
    let (mut vars, mut steps) = (Vec::new(), Vec::<StepDef>::new());
    let mut output = Value::Object(Map::new());

    for n in body.nodes() {
        match n.name().value() {
            "pace" => {
                pace_min_ms = prop_str(n, "min").map(|s| dur_ms(&s)).transpose()?.unwrap_or(0);
                pace_max_ms = prop_str(n, "max").map(|s| dur_ms(&s)).transpose()?.unwrap_or(pace_min_ms);
            }
            "var" => vars.push(VarDef {
                name: first_str(n).ok_or_else(|| PipelineError::Invalid("var needs a name".into()))?,
                required: prop_bool(n, "required").unwrap_or(false),
                default: prop_str(n, "default"),
            }),
            "step" => {
                let sname = first_str(n).ok_or_else(|| PipelineError::Invalid("step needs a name".into()))?;
                if steps.iter().any(|s: &StepDef| s.name == sname) {
                    return Err(PipelineError::Invalid(format!("duplicate step: {sname}")));
                }
                steps.push(StepDef {
                    name: sname,
                    recipe: prop_str(n, "recipe")
                        .ok_or_else(|| PipelineError::Invalid("step needs recipe=".into()))?,
                    params: params_block(n),
                    after: prop_str(n, "after").map(|s| s.split(',').map(|x| x.trim().to_string()).collect())
                        .unwrap_or_default(),
                    attempts: n.children().and_then(|d| d.nodes().iter()
                        .find(|c| c.name().value() == "attempts")
                        .and_then(|c| c.entries().first().and_then(|e| e.value().as_integer())))
                        .unwrap_or(1) as i64,
                    verify: sub_step(n, "verify"),
                    fallback: sub_step(n, "fallback"),
                });
            }
            "output" => output = params_block_of(n),
            _ => {}
        }
    }

    // Referential integrity up front: a typo'd `after=` must fail at parse time,
    // not strand a run half-built at dispatch time.
    let names: Vec<&str> = steps.iter().map(|s| s.name.as_str()).collect();
    for s in &steps {
        for a in &s.after {
            if !names.contains(&a.as_str()) {
                return Err(PipelineError::Invalid(format!("step {} depends on unknown step {a}", s.name)));
            }
        }
    }
    if steps.is_empty() {
        return Err(PipelineError::Invalid("pipeline has no steps".into()));
    }
    Ok(PipelineDef { name, pace_min_ms, pace_max_ms, vars, steps, output })
}

/// `output { k "v" }` uses the same shape as `params` but is its own node.
fn params_block_of(n: &KdlNode) -> Value {
    let mut m = Map::new();
    if let Some(d) = n.children() {
        for c in d.nodes() {
            if let Some(v) = first_str(c) {
                m.insert(c.name().value().to_string(), Value::String(v));
            }
        }
    }
    Value::Object(m)
}
```

Add `pub mod pipeline;` to `crates/core/src/lib.rs`. Add `kdl` to `crates/core/Cargo.toml` matching the version already used by `adapter-recipe` (check its `Cargo.toml`; do not introduce a second `kdl` version).

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p pacewright-core pipeline:: && cargo clippy -p pacewright-core -- -D warnings`
Expected: 3 passed, no warnings.

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/pipeline.rs crates/core/src/lib.rs crates/core/Cargo.toml
git commit -m "feat(core): KDL pipeline definition parser"
```

---

### Task 4: Expanding a pipeline into tasks (with the verify gate)

**Files:**
- Create: `crates/core/src/run.rs`
- Modify: `crates/core/src/lib.rs` (add `pub mod run;`)
- Test: inline `#[cfg(test)]` in `crates/core/src/run.rs`

**Interfaces:**
- Consumes: `pipeline::{PipelineDef, StepDef}` (Task 3), `Store` + `Task` (Task 1).
- Produces: `pub fn expand(def: &PipelineDef, run_id: &str, vars: &Value, now_ms: i64) -> Result<Vec<Task>, RunError>`.

**Key rule:** a step with a `verify` produces two tasks, `<step>` and `<step>.verify`. `<step>.verify` depends on `<step>`. Any step listing `<step>` in `after=` depends on `<step>.verify`, NOT on `<step>`. That is the whole verification gate, expressed in existing primitives.

`depends_on` is a single `Option<String>`, so a step with multiple `after=` entries is rejected in this task (see Step 3 note) rather than silently honouring only one.

- [ ] **Step 1: Write the failing test**

```rust
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

    #[test]
    fn verify_task_is_inserted_and_gates_the_dependent() {
        let def = parse_pipeline(SRC).unwrap();
        let tasks = expand(&def, "run1", &json!({"a": "x"}), 1_000).unwrap();
        assert_eq!(tasks.len(), 3, "first, first.verify, second");

        let by = |n: &str| tasks.iter().find(|t| t.step_name.as_deref() == Some(n)).unwrap().clone();
        let first = by("first");
        let verify = by("first.verify");
        let second = by("second");

        assert_eq!(verify.depends_on, Some(first.id.clone()));
        // The gate: `second` waits on the VERIFY, never on the raw step.
        assert_eq!(second.depends_on, Some(verify.id.clone()));

        assert_eq!(first.run_id.as_deref(), Some("run1"));
        assert_eq!(first.dedup_key.as_deref(), Some("run1:first"));
        assert_eq!(verify.dedup_key.as_deref(), Some("run1:first.verify"));
    }

    #[test]
    fn a_step_without_verify_is_depended_on_directly() {
        let src = r#"pipeline "d" {
            step "a" recipe="dummy/echo" { }
            step "b" recipe="dummy/echo" after="a" { }
        }"#;
        let def = parse_pipeline(src).unwrap();
        let tasks = expand(&def, "r", &json!({}), 0).unwrap();
        assert_eq!(tasks.len(), 2);
        let a = tasks.iter().find(|t| t.step_name.as_deref() == Some("a")).unwrap();
        let b = tasks.iter().find(|t| t.step_name.as_deref() == Some("b")).unwrap();
        assert_eq!(b.depends_on, Some(a.id.clone()));
    }

    #[test]
    fn missing_required_var_is_rejected() {
        let src = r#"pipeline "d" { var "need" required=#true step "a" recipe="dummy/echo" { } }"#;
        let def = parse_pipeline(src).unwrap();
        assert!(expand(&def, "r", &json!({}), 0).is_err());
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p pacewright-core run::`
Expected: FAIL to compile, `unresolved module 'run'`.

- [ ] **Step 3: Write minimal implementation**

Create `crates/core/src/run.rs`:

```rust
//! Expanding a PipelineDef into ordinary tasks. Generic: the engine gains no
//! knowledge of what any step does, only how steps relate.
use crate::model::Task;
use crate::pipeline::PipelineDef;
use serde_json::{Map, Value};

#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error("missing required var: {0}")]
    MissingVar(String),
    #[error("step {0} lists multiple `after` deps; only one is supported")]
    MultiDep(String),
}

/// Split "adapter/action" into its parts; a bare name means action-on-recipe adapter.
fn split_recipe(r: &str) -> (String, String) {
    match r.split_once('/') {
        Some((a, b)) => (a.to_string(), b.to_string()),
        None => ("recipe".to_string(), r.to_string()),
    }
}

pub fn expand(def: &PipelineDef, run_id: &str, vars: &Value, now_ms: i64) -> Result<Vec<Task>, RunError> {
    // Apply declared defaults, then enforce required.
    let mut v: Map<String, Value> = vars.as_object().cloned().unwrap_or_default();
    for vd in &def.vars {
        if !v.contains_key(&vd.name) {
            if let Some(d) = &vd.default {
                v.insert(vd.name.clone(), Value::String(d.clone()));
            } else if vd.required {
                return Err(RunError::MissingVar(vd.name.clone()));
            }
        }
    }

    let mut out: Vec<Task> = Vec::new();
    // step name -> the task id a DEPENDENT should wait on (the verify when present).
    let mut gate: std::collections::HashMap<String, String> = std::collections::HashMap::new();

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
        if let Some(dep) = s.after.first() {
            t.depends_on = gate.get(dep).cloned();
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
                let vid = vt.id.clone();
                out.push(vt);
                // Dependents gate on the VERIFY: a step is not done until it is confirmed.
                gate.insert(s.name.clone(), vid);
            }
            None => {
                gate.insert(s.name.clone(), step_id);
            }
        }
    }
    Ok(out)
}
```

Add `pub mod run;` to `crates/core/src/lib.rs`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p pacewright-core run:: && cargo clippy -p pacewright-core -- -D warnings`
Expected: 3 passed, no warnings.

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/run.rs crates/core/src/lib.rs
git commit -m "feat(core): expand pipelines into tasks; verify task gates dependents"
```

---

### Task 5: Resume, and params resolved at dispatch

**Files:**
- Modify: `crates/core/src/run.rs` (add `start`)
- Modify: `crates/core/src/runner.rs` (resolve params before `adapter.execute`)
- Test: `crates/core/tests/engine.rs`

**Interfaces:**
- Consumes: `expand` (Task 4), `refs::resolve` (Task 2), `Store::{find_by_dedup_any, tasks_in_run}` (Task 1).
- Produces: `pub fn start(store: &Store, def: &PipelineDef, run_id: &str, vars: &Value, now_ms: i64) -> Result<Vec<Task>, RunError>` returning only the tasks it actually inserted.

- [ ] **Step 1: Write the failing test**

In `crates/core/tests/engine.rs`:

```rust
#[test]
fn test_resume_skips_succeeded_steps() {
    let store = Store::open_in_memory().unwrap();
    let src = r#"pipeline "d" {
        step "a" recipe="dummy/echo" { }
        step "b" recipe="dummy/echo" after="a" { }
    }"#;
    let def = pacewright_core::pipeline::parse_pipeline(src).unwrap();

    let first = pacewright_core::run::start(&store, &def, "r1", &serde_json::json!({}), 0).unwrap();
    assert_eq!(first.len(), 2);

    // Mark `a` succeeded, then re-start the same run.
    let mut a = store.tasks_in_run("r1").unwrap()
        .into_iter().find(|t| t.step_name.as_deref() == Some("a")).unwrap();
    a.status = TaskStatus::Succeeded;
    store.update_task(&a).unwrap();

    let again = pacewright_core::run::start(&store, &def, "r1", &serde_json::json!({}), 0).unwrap();
    assert!(again.iter().all(|t| t.step_name.as_deref() != Some("a")),
        "a already succeeded, it must not be recreated");
    // 3 rows = a, b, and the __vars sentinel added below. No duplicates on re-start.
    assert_eq!(store.tasks_in_run("r1").unwrap().len(), 3, "no duplicate rows");
    assert_eq!(
        store.tasks_in_run("r1").unwrap().iter()
            .filter(|t| t.step_name.as_deref() == Some("__vars")).count(),
        1, "the vars sentinel must not be re-inserted");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p pacewright-core test_resume_skips_succeeded_steps`
Expected: FAIL to compile, `no function 'start' in module 'run'`.

- [ ] **Step 3: Write minimal implementation**

Append to `crates/core/src/run.rs`:

```rust
use crate::model::TaskStatus;
use crate::store::Store;

/// Start or resume a run. Idempotent: a step whose task already exists and has
/// SUCCEEDED is never recreated, so re-running after a mid-pipeline failure
/// continues from the failed step and redoes nothing that worked.
pub fn start(
    store: &Store, def: &PipelineDef, run_id: &str, vars: &Value, now_ms: i64,
) -> Result<Vec<Task>, RunError> {
    let planned = expand(def, run_id, vars, now_ms)?;
    let mut inserted = Vec::new();
    for t in planned {
        let key = t.dedup_key.clone().unwrap_or_default();
        match store.find_by_dedup_any(&key) {
            Ok(Some(existing)) if existing.status == TaskStatus::Succeeded => continue,
            Ok(Some(_existing)) => continue, // already queued/running; leave it alone
            _ => {}
        }
        if store.insert_task(&t).is_ok() {
            inserted.push(t);
        }
    }
    Ok(inserted)
}
```

In `crates/core/src/runner.rs`, resolve params immediately before dispatch. Inside `execute_and_record`, replace the `adapter.execute(&ctx, &task.action, task.params.clone())` argument with a resolved value computed just above it:

```rust
    // Resolve {{ steps.*.result.* }} / {{ vars.* }} against this run's sibling
    // results at DISPATCH time (not enqueue time), because the referenced step
    // has only just produced its result.
    let params = match task.run_id.as_deref() {
        None => task.params.clone(),
        Some(rid) => {
            let mut results = std::collections::HashMap::new();
            let mut vars = serde_json::Value::Object(Default::default());
            for sib in store.tasks_in_run(rid)? {
                if let (Some(name), Some(res)) = (sib.step_name.clone(), sib.result.clone()) {
                    results.insert(name, res);
                }
                if sib.step_name.as_deref() == Some("__vars") {
                    vars = sib.params.clone();
                }
            }
            match crate::refs::resolve(&task.params, &vars, &results) {
                Ok(v) => v,
                Err(e) => {
                    // Refuse to send a literal "{{ … }}" to an adapter.
                    task.status = TaskStatus::Failed;
                    task.last_error = Some(e.to_string());
                    task.finished_at = Some(clock.now_ms());
                    task.updated_at = clock.now_ms();
                    store.update_task(&task)?;
                    return Ok(());
                }
            }
        }
    };
```

then pass `params` to `adapter.execute`.

For the run's vars to be visible, `start` must persist them. In `start`, before the loop, insert a sentinel task when absent:

```rust
    // Vars live in a terminal sentinel task so every step can reference them by
    // the same mechanism as step results, with no extra table.
    let vkey = format!("{run_id}:__vars");
    if matches!(store.find_by_dedup_any(&vkey), Ok(None)) {
        let mut vt = Task::new_now("noop", "vars", vars.clone(), now_ms);
        vt.run_id = Some(run_id.to_string());
        vt.step_name = Some("__vars".into());
        vt.dedup_key = Some(vkey);
        vt.status = TaskStatus::Succeeded;
        vt.finished_at = Some(now_ms);
        let _ = store.insert_task(&vt);
    }
```

Note the resume test asserts `tasks_in_run("r1").len() == 2`; update it to `3` to account for the `__vars` sentinel, and assert the sentinel is not re-inserted on the second `start`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p pacewright-core && cargo clippy --workspace -- -D warnings`
Expected: PASS, no warnings, no pre-existing test regressions.

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/run.rs crates/core/src/runner.rs crates/core/tests/engine.rs
git commit -m "feat(core): resumable runs + dispatch-time param resolution"
```

---

### Task 6: End-to-end run through the engine with `adapter-dummy`

**Files:**
- Test: `crates/daemon/tests/e2e.rs`

**Interfaces:**
- Consumes: everything from Tasks 1-5, plus the existing daemon engine tick.
- Produces: no new API. This is the regression test for the two silent no-ops.

- [ ] **Step 1: Write the failing test**

```rust
#[tokio::test]
async fn failing_verify_blocks_the_dependent_step() {
    // The regression test for the real incident: a step "succeeds" but did nothing.
    // Its verify fails, and the next step MUST NOT run.
    let (store, engine) = pipeline_test_engine().await;
    let src = r#"pipeline "d" {
        step "publish" recipe="dummy/echo" {
            verify recipe="dummy/always_fail" { }
        }
        step "after" recipe="dummy/echo" after="publish" { }
    }"#;
    let def = pacewright_core::pipeline::parse_pipeline(src).unwrap();
    pacewright_core::run::start(&store, &def, "r1", &serde_json::json!({}), 0).unwrap();

    engine.drain().await;

    let t = |n: &str| store.tasks_in_run("r1").unwrap()
        .into_iter().find(|t| t.step_name.as_deref() == Some(n)).unwrap();
    assert_eq!(t("publish").status, TaskStatus::Succeeded);
    assert_eq!(t("publish.verify").status, TaskStatus::Failed);
    assert_ne!(t("after").status, TaskStatus::Succeeded, "dependent must not run past a failed verify");
}

#[tokio::test]
async fn result_flows_from_one_step_into_the_next() {
    let (store, engine) = pipeline_test_engine().await;
    let src = r#"pipeline "d" {
        step "one" recipe="dummy/echo" { params { id "abc" } }
        step "two" recipe="dummy/echo" after="one" { params { got "{{ steps.one.result.id }}" } }
    }"#;
    let def = pacewright_core::pipeline::parse_pipeline(src).unwrap();
    pacewright_core::run::start(&store, &def, "r2", &serde_json::json!({}), 0).unwrap();

    engine.drain().await;

    let two = store.tasks_in_run("r2").unwrap()
        .into_iter().find(|t| t.step_name.as_deref() == Some("two")).unwrap();
    assert_eq!(two.result.unwrap()["got"], "abc", "dummy/echo returns its params");
}
```

`crates/daemon/tests/e2e.rs` has **no shared setup helper**: every test builds its own inline
(`Store::open_in_memory()`, an `AdapterRegistry` with `DummyAdapter::new()`, then
`Arc<Mutex<Engine::new(...)>>`, as at `e2e.rs:32-45`). Do **not** refactor the existing tests. Add a
local helper for the new ones only, mirroring that same shape:

```rust
/// Local to the pipeline tests. Mirrors the inline setup the other e2e tests use
/// (e2e.rs:32-45); deliberately not shared with them, to avoid touching passing tests.
async fn pipeline_test_engine() -> (Arc<Store>, Arc<Mutex<Engine>>) {
    let store = Arc::new(Store::open_in_memory().unwrap());
    let mut reg = AdapterRegistry::new();
    reg.register(Arc::new(DummyAdapter::new()));
    let engine = Arc::new(Mutex::new(Engine::new(
        store.clone(), Arc::new(reg), /* match the remaining Engine::new args at e2e.rs:40 */
    )));
    (store, engine)
}
```

Copy the remaining `Engine::new` arguments verbatim from `e2e.rs:40`. For draining, drive the tick
loop the way the existing tests do rather than assuming an `engine.drain()` method exists: if there
is no drain helper, loop `engine.lock().await.tick().await` until every task in the run reaches a
terminal status or a bounded iteration cap (say 50) is hit, then assert. Replace the
`engine.drain().await` lines in the tests above accordingly.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p pacewright-daemon failing_verify_blocks_the_dependent_step`
Expected: FAIL (dependent runs, or helper missing).

- [ ] **Step 3: Write minimal implementation**

No production code should be needed; Tasks 1-5 provide the behaviour. If the test fails, fix the defect it exposes rather than weakening the assertion. If `dummy/always_fail` returns a retryable rather than terminal error, give the verify task `max_attempts = 1` in the pipeline via `attempts 1` so the test does not wait on backoff.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --workspace && cargo clippy --workspace -- -D warnings`
Expected: PASS, no warnings.

- [ ] **Step 5: Commit**

```bash
git add crates/daemon/tests/e2e.rs
git commit -m "test(daemon): verify gates dependents; results flow between steps"
```

---

### Task 7: CLI `run` / `runs` / `show`

**Files:**
- Create: `crates/cli/src/run_cmd.rs`
- Modify: `crates/cli/src/main.rs` (register subcommands)
- Modify: `crates/proto/src/lib.rs` (request/response types)
- Modify: `crates/daemon/src/server.rs` (handle the new RPCs)

**Interfaces:**
- Consumes: `run::start`, `Store::tasks_in_run`, `pipeline::parse_pipeline`.
- Produces: CLI surface

```
pacewright run <pipeline> --run-id <id> --params '{"k":"v"}'
pacewright runs
pacewright show <run-id>
```

Pipelines are loaded from `~/.pacewright/recipes/pipelines/<name>.kdl`, where `<name>` is the pipeline's `name` with `/` replaced by `-`.

- [ ] **Step 1: Write the failing test**

In `crates/daemon/tests/e2e.rs`:

```rust
#[tokio::test]
async fn rpc_run_start_is_idempotent_and_show_reports_steps() {
    let (store, engine) = pipeline_test_engine().await;
    let src = r#"pipeline "d" { step "a" recipe="dummy/echo" { } }"#;
    let def = pacewright_core::pipeline::parse_pipeline(src).unwrap();

    let a = pacewright_core::run::start(&store, &def, "rx", &serde_json::json!({}), 0).unwrap();
    let b = pacewright_core::run::start(&store, &def, "rx", &serde_json::json!({}), 0).unwrap();
    assert_eq!(a.len(), 1);
    assert_eq!(b.len(), 0, "second start must add nothing");

    engine.drain().await;
    let steps = store.tasks_in_run("rx").unwrap();
    assert!(steps.iter().any(|t| t.step_name.as_deref() == Some("a")
        && t.status == TaskStatus::Succeeded));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p pacewright-daemon rpc_run_start_is_idempotent_and_show_reports_steps`
Expected: FAIL if `start` is not idempotent.

- [ ] **Step 3: Write minimal implementation**

Add to `crates/proto/src/lib.rs`, following the existing request/response enum style:

```rust
    RunStart { pipeline: String, run_id: String, params: serde_json::Value },
    RunList,
    RunShow { run_id: String },
```

In `crates/daemon/src/server.rs`, handle them: `RunStart` reads
`~/.pacewright/recipes/pipelines/<pipeline-with-slashes-as-dashes>.kdl`, calls `parse_pipeline`,
then `run::start`, and returns the inserted step names. `RunShow` returns `store.tasks_in_run(run_id)`
projected to `{step_name, status, attempts, last_error, result}`. `RunList` returns the distinct
`run_id`s with a per-run rollup of step statuses.

Create `crates/cli/src/run_cmd.rs` with `clap` subcommands mirroring those three RPCs, printing a
table for `show` with one row per step and a `kind` column derived from the step name: a
`*.verify` step prints as `verify`, `__vars` is hidden, everything else prints as `step`. Register
the module and subcommands in `crates/cli/src/main.rs` alongside the existing ones.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --workspace && cargo clippy --workspace -- -D warnings`
Expected: PASS, no warnings.

- [ ] **Step 5: Commit**

```bash
git add crates/cli/src/run_cmd.rs crates/cli/src/main.rs crates/proto/src/lib.rs crates/daemon/src/server.rs
git commit -m "feat(cli): pacewright run/runs/show for pipeline runs"
```

---

## Deferred to Phase 1b (explicitly NOT in this plan)

These are specified in `docs/specs/2026-07-22-pipeline-runs-and-verified-steps.md` but deliberately
left out so Phase 1 lands as working software:

- **`fallback` execution.** Task 3 parses it and Task 4 carries it, but nothing runs it yet. It needs
  a failure edge (`depends_on_failed`) in `scheduler.rs`, since the current scheduler only releases a
  dependent when its parent *succeeded*. Until that lands, a failing verify simply blocks the run,
  which is the safe behaviour.
- **Paced eligibility.** `pace min/max` is parsed and stored on `PipelineDef` but not yet applied to
  `next_eligible_at`. Steps currently run as soon as their dependency clears, still subject to the
  existing per-recipe `limit-key` caps.
- **`output` block resolution** into a final run report.
- **Multi-dependency steps.** `Task.depends_on` is a single `Option<String>`; `expand` rejects
  `after=` with more than one entry rather than silently honouring the first.

## Phase 2 (next plan, not this one)

`youtube/verify_video`, `youtube/verify_shorts`, `spotify/verify_episode`,
`riverside/verify_exports` as API-backed recipes, then the `podcast/episode` pipeline KDL.

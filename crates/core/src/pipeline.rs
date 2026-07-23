//! Declarative pipeline definitions.
//!
//! A pipeline names steps, their dependencies, their params, and (optionally) how to
//! verify each one. It references recipes by name and never encodes what a step *does*,
//! which is what keeps the engine platform-agnostic: all domain logic stays in recipes.
use serde_json::{Map, Value};

#[derive(Debug, thiserror::Error)]
pub enum PipelineError {
    #[error("not valid KDL: {0}")]
    Kdl(String),
    #[error("invalid pipeline: {0}")]
    Invalid(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct VarDef {
    pub name: String,
    pub required: bool,
    pub default: Option<String>,
}

/// A `verify` or `fallback` block: another recipe invocation with its own params.
#[derive(Debug, Clone, PartialEq)]
pub struct SubStep {
    pub recipe: String,
    pub params: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StepDef {
    pub name: String,
    pub recipe: String,
    pub params: Value,
    pub after: Vec<String>,
    pub attempts: i64,
    pub verify: Option<SubStep>,
    pub fallback: Option<SubStep>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PipelineDef {
    pub name: String,
    pub pace_min_ms: i64,
    pub pace_max_ms: i64,
    pub vars: Vec<VarDef>,
    pub steps: Vec<StepDef>,
    pub output: Value,
}

fn first_arg(n: &kdl::KdlNode) -> Option<&str> {
    n.entries().iter().find(|e| e.name().is_none()).and_then(|e| e.value().as_string())
}

fn first_int(n: &kdl::KdlNode) -> Option<i128> {
    n.entries().iter().find(|e| e.name().is_none()).and_then(|e| e.value().as_integer())
}

fn prop_str<'a>(n: &'a kdl::KdlNode, key: &str) -> Option<&'a str> {
    n.entries()
        .iter()
        .find(|e| e.name().map(kdl::KdlIdentifier::value) == Some(key))
        .and_then(|e| e.value().as_string())
}

fn prop_bool(n: &kdl::KdlNode, key: &str) -> Option<bool> {
    n.entries()
        .iter()
        .find(|e| e.name().map(kdl::KdlIdentifier::value) == Some(key))
        .and_then(|e| e.value().as_bool())
}

fn children(n: &kdl::KdlNode) -> impl Iterator<Item = &kdl::KdlNode> {
    n.children().into_iter().flat_map(|d| d.nodes().iter())
}

/// A block of `key "value"` lines -> a flat JSON object of strings.
fn kv_block(n: &kdl::KdlNode) -> Value {
    let mut m = Map::new();
    for c in children(n) {
        if let Some(v) = first_arg(c) {
            m.insert(c.name().value().to_string(), Value::String(v.to_string()));
        }
    }
    Value::Object(m)
}

/// The `params { … }` child of a step/verify/fallback node.
fn params_of(n: &kdl::KdlNode) -> Value {
    children(n)
        .find(|c| c.name().value() == "params")
        .map(kv_block)
        .unwrap_or_else(|| Value::Object(Map::new()))
}

fn sub_step(parent: &kdl::KdlNode, kind: &str) -> Result<Option<SubStep>, PipelineError> {
    let Some(n) = children(parent).find(|c| c.name().value() == kind) else { return Ok(None) };
    let recipe = prop_str(n, "recipe")
        .ok_or_else(|| PipelineError::Invalid(format!("{kind} needs recipe=")))?;
    Ok(Some(SubStep { recipe: recipe.to_string(), params: params_of(n) }))
}

/// "90s" / "5m" / "1h" / "1500" (bare = ms) -> milliseconds.
fn dur_ms(s: &str) -> Result<i64, PipelineError> {
    let s = s.trim();
    let bad = || PipelineError::Invalid(format!("bad duration: {s}"));
    let (num, mult) = match s.chars().last() {
        Some('s') => (&s[..s.len() - 1], 1_000),
        Some('m') => (&s[..s.len() - 1], 60_000),
        Some('h') => (&s[..s.len() - 1], 3_600_000),
        _ => (s, 1),
    };
    num.trim().parse::<i64>().map(|n| n * mult).map_err(|_| bad())
}

pub fn parse_pipeline(src: &str) -> Result<PipelineDef, PipelineError> {
    let doc: kdl::KdlDocument = src.parse().map_err(|e| PipelineError::Kdl(format!("{e}")))?;
    let root = doc
        .nodes()
        .iter()
        .find(|n| n.name().value() == "pipeline")
        .ok_or_else(|| PipelineError::Invalid("no `pipeline` node".into()))?;
    let name = first_arg(root)
        .ok_or_else(|| PipelineError::Invalid("pipeline needs a name".into()))?
        .to_string();

    let (mut pace_min_ms, mut pace_max_ms) = (0_i64, 0_i64);
    let mut vars: Vec<VarDef> = Vec::new();
    let mut steps: Vec<StepDef> = Vec::new();
    let mut output = Value::Object(Map::new());

    for n in children(root) {
        match n.name().value() {
            "pace" => {
                pace_min_ms = prop_str(n, "min").map(dur_ms).transpose()?.unwrap_or(0);
                pace_max_ms = prop_str(n, "max").map(dur_ms).transpose()?.unwrap_or(pace_min_ms);
                if pace_max_ms < pace_min_ms {
                    return Err(PipelineError::Invalid("pace max is below pace min".into()));
                }
            }
            "var" => vars.push(VarDef {
                name: first_arg(n)
                    .ok_or_else(|| PipelineError::Invalid("var needs a name".into()))?
                    .to_string(),
                required: prop_bool(n, "required").unwrap_or(false),
                default: prop_str(n, "default").map(str::to_string),
            }),
            "step" => {
                let sname = first_arg(n)
                    .ok_or_else(|| PipelineError::Invalid("step needs a name".into()))?
                    .to_string();
                if steps.iter().any(|s| s.name == sname) {
                    return Err(PipelineError::Invalid(format!("duplicate step: {sname}")));
                }
                if sname.ends_with(".verify") || sname == "__vars" {
                    // These names are synthesized by run expansion; a hand-written step
                    // using one would collide on the (run_id, step_name) unique index.
                    return Err(PipelineError::Invalid(format!("reserved step name: {sname}")));
                }
                steps.push(StepDef {
                    recipe: prop_str(n, "recipe")
                        .ok_or_else(|| {
                            PipelineError::Invalid(format!("step {sname} needs recipe="))
                        })?
                        .to_string(),
                    params: params_of(n),
                    after: prop_str(n, "after")
                        .map(|s| s.split(',').map(|x| x.trim().to_string()).collect())
                        .unwrap_or_default(),
                    attempts: children(n)
                        .find(|c| c.name().value() == "attempts")
                        .and_then(first_int)
                        .unwrap_or(1) as i64,
                    verify: sub_step(n, "verify")?,
                    fallback: sub_step(n, "fallback")?,
                    name: sname,
                });
            }
            "output" => output = kv_block(n),
            _ => {}
        }
    }

    if steps.is_empty() {
        return Err(PipelineError::Invalid("pipeline has no steps".into()));
    }
    // Referential integrity up front: a typo'd `after=` must fail at parse time rather
    // than strand a half-built run at dispatch time.
    let names: Vec<&str> = steps.iter().map(|s| s.name.as_str()).collect();
    for s in &steps {
        for a in &s.after {
            if !names.contains(&a.as_str()) {
                return Err(PipelineError::Invalid(format!(
                    "step {} depends on unknown step {a}",
                    s.name
                )));
            }
        }
    }
    Ok(PipelineDef { name, pace_min_ms, pace_max_ms, vars, steps, output })
}

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
        let src =
            r#"pipeline "x" { step "a" recipe="dummy/echo" { } step "a" recipe="dummy/echo" { } }"#;
        assert!(parse_pipeline(src).is_err());
    }

    #[test]
    fn rejects_reserved_step_names() {
        // `<step>.verify` and `__vars` are synthesized during expansion.
        let src = r#"pipeline "x" { step "a.verify" recipe="dummy/echo" { } }"#;
        assert!(parse_pipeline(src).is_err());
    }

    #[test]
    fn rejects_a_pipeline_with_no_steps() {
        assert!(parse_pipeline(r#"pipeline "x" { var "a" }"#).is_err());
    }

    #[test]
    fn durations_accept_s_m_h_and_bare_ms() {
        assert_eq!(dur_ms("90s").unwrap(), 90_000);
        assert_eq!(dur_ms("5m").unwrap(), 300_000);
        assert_eq!(dur_ms("1h").unwrap(), 3_600_000);
        assert_eq!(dur_ms("250").unwrap(), 250);
        assert!(dur_ms("soon").is_err());
    }
}

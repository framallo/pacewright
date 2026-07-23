//! Generic `{{ … }}` reference resolution for task params.
//!
//! Knows only about JSON: `steps.<step>.result.<path>` and `vars.<name>`. No adapter,
//! platform, or domain concept appears here, so any pipeline on any adapter gets result
//! passing for free. This exists because `depends_on` is pure status gating: a dependent
//! task is released when its parent succeeds but is handed nothing from it.
use serde_json::Value;
use std::collections::HashMap;

#[derive(Debug, thiserror::Error)]
pub enum RefError {
    #[error("unresolved reference: {0}")]
    Unresolved(String),
}

/// Locate the next `{{ … }}`, returning (start, end-exclusive, trimmed inner expression).
fn next_ref(s: &str) -> Option<(usize, usize, String)> {
    let start = s.find("{{")?;
    let rest = &s[start + 2..];
    let close = rest.find("}}")?;
    Some((start, start + 2 + close + 2, rest[..close].trim().to_string()))
}

fn dig(root: &Value, path: &[&str]) -> Option<Value> {
    let mut cur = root;
    for p in path {
        cur = cur.get(p)?;
    }
    Some(cur.clone())
}

fn lookup(expr: &str, vars: &Value, results: &HashMap<String, Value>) -> Option<Value> {
    let parts: Vec<&str> = expr.split('.').collect();
    match parts.as_slice() {
        ["vars", rest @ ..] => dig(vars, rest),
        ["steps", step, "result", rest @ ..] => dig(results.get(*step)?, rest),
        _ => None,
    }
}

fn resolve_str(s: &str, vars: &Value, results: &HashMap<String, Value>) -> Result<Value, RefError> {
    // A string that is EXACTLY one reference keeps the referenced JSON type, so an array
    // or object survives into the next step instead of being flattened to a string.
    if let Some((0, end, expr)) = next_ref(s) {
        if end == s.len() {
            return lookup(&expr, vars, results).ok_or(RefError::Unresolved(expr));
        }
    }
    let mut out = String::new();
    let mut rem = s;
    while let Some((start, end, expr)) = next_ref(rem) {
        out.push_str(&rem[..start]);
        match lookup(&expr, vars, results).ok_or(RefError::Unresolved(expr))? {
            Value::String(sv) => out.push_str(&sv),
            other => out.push_str(&other.to_string()),
        }
        rem = &rem[end..];
    }
    out.push_str(rem);
    Ok(Value::String(out))
}

/// Resolve every reference in `params`. An unresolvable reference is an error, never a
/// literal `{{ … }}` passed through to an adapter.
pub fn resolve(
    params: &Value, vars: &Value, results: &HashMap<String, Value>,
) -> Result<Value, RefError> {
    Ok(match params {
        Value::String(s) => resolve_str(s, vars, results)?,
        Value::Array(a) => {
            Value::Array(a.iter().map(|v| resolve(v, vars, results)).collect::<Result<_, _>>()?)
        }
        Value::Object(o) => Value::Object(
            o.iter()
                .map(|(k, v)| Ok((k.clone(), resolve(v, vars, results)?)))
                .collect::<Result<_, RefError>>()?,
        ),
        other => other.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn results() -> HashMap<String, Value> {
        HashMap::from([(
            "publish_long".to_string(),
            json!({"publish": {"video_id": "abc123", "tags": ["a", "b"]}}),
        )])
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
        let params = json!({"tags": "{{ steps.publish_long.result.publish.tags }}"});
        let out = resolve(&params, &json!({}), &results()).unwrap();
        assert_eq!(out["tags"], json!(["a", "b"]));
    }

    #[test]
    fn unresolved_reference_is_an_error_not_a_literal() {
        // Sending a literal "{{ … }}" to a browser is the failure mode we refuse.
        let params = json!({"x": "{{ steps.nope.result.y }}"});
        let err = resolve(&params, &json!({}), &results()).unwrap_err();
        assert!(matches!(err, RefError::Unresolved(ref s) if s.contains("steps.nope.result.y")));
    }

    #[test]
    fn nested_structures_are_resolved_throughout() {
        let params = json!({"outer": {"inner": ["{{ vars.a }}", 1]}});
        let out = resolve(&params, &json!({"a": "z"}), &HashMap::new()).unwrap();
        assert_eq!(out["outer"]["inner"][0], "z");
        assert_eq!(out["outer"]["inner"][1], 1);
    }
}

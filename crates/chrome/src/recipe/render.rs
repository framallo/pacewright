//! Output rendering: a minimal, non-Turing templater + file writing for `output` blocks.
//!
//! Scope is deliberately tiny (spec §5b): `{{ key }}` scalar substitution and Mustache-style
//! sections `{{#listkey}} … {{.}} … {{/listkey}}` to repeat a block over a `many` list. No
//! conditionals, expressions, or nesting. `json` outputs skip the template and serialize the
//! result map.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde_json::{Map, Value};

use crate::BoxError;
use crate::recipe::model::{Format, Recipe};

/// Look a scalar `key` up for interpolation: a string result value, then a non-string result
/// scalar, then a var, then the `now` builtin, else empty.
fn scalar(
    key: &str,
    vars: &BTreeMap<String, String>,
    result: &Map<String, Value>,
    now: &str,
) -> String {
    if key == "now" {
        return now.to_string();
    }
    if let Some(v) = result.get(key) {
        return match v {
            Value::String(s) => s.clone(),
            Value::Array(_) | Value::Object(_) | Value::Null => String::new(),
            other => other.to_string(),
        };
    }
    vars.get(key).cloned().unwrap_or_default()
}

/// Stringify a list item for a `{{.}}` placeholder.
fn item_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Replace `{{ key }}` scalars (leaving `{{#…}}`/`{{/…}}`/`{{.}}` for the section pass or, at the
/// top level, `{{.}}` untouched). `dot` supplies the value of `{{.}}` inside a section body.
fn render_scalars(
    tmpl: &str,
    vars: &BTreeMap<String, String>,
    result: &Map<String, Value>,
    now: &str,
    dot: Option<&str>,
) -> String {
    let mut out = String::with_capacity(tmpl.len());
    let mut rest = tmpl;
    while let Some(open) = rest.find("{{") {
        out.push_str(&rest[..open]);
        let after = &rest[open + 2..];
        let Some(close) = after.find("}}") else {
            out.push_str("{{");
            rest = after;
            continue;
        };
        let token = after[..close].trim();
        if token == "." {
            out.push_str(dot.unwrap_or(""));
        } else if token.is_empty() || token.starts_with('#') || token.starts_with('/') {
            // not a scalar — leave verbatim (section markers handled elsewhere)
            out.push_str(&rest[open..open + 2 + close + 2]);
        } else {
            out.push_str(&scalar(token, vars, result, now));
        }
        rest = &after[close + 2..];
    }
    out.push_str(rest);
    out
}

/// Expand a single (non-nested) `{{#key}}…{{/key}}` section by repeating its body over the list at
/// `result[key]`, rendering `{{.}}` per item. Returns the template with the first section expanded,
/// or `None` if there is no section.
fn expand_one_section(
    tmpl: &str,
    vars: &BTreeMap<String, String>,
    result: &Map<String, Value>,
    now: &str,
) -> Option<String> {
    let open_at = tmpl.find("{{#")?;
    let after_open = &tmpl[open_at + 3..];
    let name_end = after_open.find("}}")?;
    let key = after_open[..name_end].trim().to_string();
    let body_start = open_at + 3 + name_end + 2;

    let close_tag = format!("{{{{/{key}}}}}");
    let close_rel = tmpl[body_start..].find(&close_tag)?;
    let body = &tmpl[body_start..body_start + close_rel];
    let after_close = body_start + close_rel + close_tag.len();

    let items = result
        .get(&key)
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut expanded = String::new();
    for item in &items {
        expanded.push_str(&render_scalars(
            body,
            vars,
            result,
            now,
            Some(&item_string(item)),
        ));
    }

    Some(format!(
        "{}{}{}",
        &tmpl[..open_at],
        expanded,
        &tmpl[after_close..]
    ))
}

/// Render a markdown template: expand every section, then substitute remaining scalars.
pub fn render_template(
    tmpl: &str,
    vars: &BTreeMap<String, String>,
    result: &Map<String, Value>,
    now: &str,
) -> String {
    let mut current = tmpl.to_string();
    // Bounded loop: each pass expands one section; cap to avoid a pathological template.
    for _ in 0..1024 {
        match expand_one_section(&current, vars, result, now) {
            Some(next) => current = next,
            None => break,
        }
    }
    render_scalars(&current, vars, result, now, None)
}

/// RFC-3339 UTC timestamp (`YYYY-MM-DDThh:mm:ssZ`) from the wall clock — browser-edge I/O,
/// outside pacewright's Clock invariant. Civil date via Howard Hinnant's algorithm.
pub fn now_rfc3339() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let (y, m, d) = civil_from_days((secs / 86_400) as i64);
    let rem = secs % 86_400;
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

#[allow(clippy::cast_sign_loss)] // day-of-month and month are provably non-negative here
const fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Render and write every `output` block. Called as the run's last phase (only on a fully
/// successful run). Returns the written paths.
pub fn write_outputs(
    recipe: &Recipe,
    vars: &BTreeMap<String, String>,
    result: &Map<String, Value>,
) -> Result<Vec<String>, BoxError> {
    if recipe.outputs.is_empty() {
        return Ok(Vec::new());
    }
    let now = now_rfc3339();
    let mut written = Vec::new();
    for out in &recipe.outputs {
        let path = render_scalars(&out.path, vars, result, &now, None);
        let content = match out.format {
            Format::Json => serde_json::to_string_pretty(&Value::Object(result.clone()))?,
            Format::Markdown => {
                let tmpl = out.template.as_deref().unwrap_or_default();
                render_template(tmpl, vars, result, &now)
            }
        };
        let p = PathBuf::from(&path);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("creating {}: {e}", parent.display()))?;
        }
        std::fs::write(&p, content).map_err(|e| format!("writing {path}: {e}"))?;
        written.push(path);
    }
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::{civil_from_days, render_template, write_outputs};
    use crate::recipe::model::Recipe;
    use serde_json::{Map, Value, json};
    use std::collections::BTreeMap;

    fn result(pairs: Vec<(&str, Value)>) -> Map<String, Value> {
        pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect()
    }

    #[test]
    fn scalars_from_vars_and_result() {
        let vars: BTreeMap<String, String> = [("vault".to_string(), "/v".to_string())].into();
        let res = result(vec![("name", json!("Jane"))]);
        let out = render_template("# {{ name }} @ {{ vault }}", &vars, &res, "NOW");
        assert_eq!(out, "# Jane @ /v");
    }

    #[test]
    fn now_builtin_substitutes() {
        let out = render_template(
            "updated: {{ now }}",
            &BTreeMap::new(),
            &Map::new(),
            "2026-07-08T00:00:00Z",
        );
        assert_eq!(out, "updated: 2026-07-08T00:00:00Z");
    }

    #[test]
    fn section_repeats_over_list() {
        let res = result(vec![("stories", json!(["One", "Two", "Three"]))]);
        let out = render_template(
            "{{#stories}}- {{.}}\n{{/stories}}",
            &BTreeMap::new(),
            &res,
            "NOW",
        );
        assert_eq!(out, "- One\n- Two\n- Three\n");
    }

    #[test]
    fn empty_list_renders_nothing() {
        let res = result(vec![("stories", json!([]))]);
        let out = render_template(
            "a{{#stories}}- {{.}}{{/stories}}b",
            &BTreeMap::new(),
            &res,
            "NOW",
        );
        assert_eq!(out, "ab");
    }

    #[test]
    fn missing_scalar_is_empty() {
        let out = render_template("x{{ nope }}y", &BTreeMap::new(), &Map::new(), "NOW");
        assert_eq!(out, "xy");
    }

    #[test]
    fn civil_epoch_is_1970() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        // 2026-07-08 is 20642 days after the epoch.
        assert_eq!(civil_from_days(20_642), (2026, 7, 8));
    }

    #[test]
    fn write_outputs_json_and_markdown() {
        let dir = std::env::temp_dir().join(format!("recipe-out-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let vars: BTreeMap<String, String> =
            [("out".to_string(), dir.to_string_lossy().to_string())].into();
        let res = result(vec![("stories", json!(["A", "B"]))]);

        let src = r##"recipe "n/hn" {
                step { extract "stories" many=#true { locator css=".a" } }
                output "json" path="OUT/hn.json"
                output "markdown" path="OUT/hn.md" {
                    template #"# HN {{#stories}}- {{.}} {{/stories}}"#
                }
            }"##
        .replace("OUT", &dir.to_string_lossy());
        let recipe = Recipe::parse(&src).unwrap();
        let written = write_outputs(&recipe, &vars, &res).unwrap();
        assert_eq!(written.len(), 2);

        let jn = std::fs::read_to_string(dir.join("hn.json")).unwrap();
        assert!(jn.contains("\"A\""));
        let md = std::fs::read_to_string(dir.join("hn.md")).unwrap();
        assert_eq!(md, "# HN - A - B ");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

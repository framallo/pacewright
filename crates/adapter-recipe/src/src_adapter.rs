//! `RecipeSrcAdapter` — the built-in `recipe_src` adapter: run a recipe whose **source** travels in
//! the task params, for a controller that keeps its recipes in its own database (CazaFacturas
//! stores, versions and self-heals them as rows) and never installs anything under
//! `~/.pacewright/recipes/`.
//!
//! The daemon's `run_src` RPC is the only intended producer of these tasks; it shapes the params as
//!
//! ```json
//! {"__recipe_src": "<KDL>", "__name": "facturagas/facturar", "vars": {"rfc": "…"}}
//! ```
//!
//! with the recipe vars **nested** under `vars` so a var can never collide with the two reserved
//! keys. Pacing still works: `limit_keys_for_task` parses the recipe's `limit-key`s out of the
//! source (the registry's own metadata parser), so the engine paces and spends exactly as for an
//! installed recipe. `foreground` is honored the same way; the run is always accountless (the
//! shared page), matching what the RPC contract promises.

use async_trait::async_trait;
use pacewright_core::adapter::{Adapter, RunCtx};
use pacewright_core::model::{ActionSpec, AdapterError};
use serde_json::{json, Value};
use std::sync::Arc;

use crate::adapter::{result_from_envelope, warn_unexpected};
use crate::registry::parse_meta_from_src;
use crate::runner::{RecipeRunner, RunOpts};

/// The adapter's name (and the task's `adapter` column).
pub const RECIPE_SRC_ADAPTER: &str = "recipe_src";
/// Params key holding the KDL source.
pub const RECIPE_SRC_KEY: &str = "__recipe_src";
/// Params key holding the caller's display name for the recipe (logs/digest only).
pub const RECIPE_SRC_NAME_KEY: &str = "__name";
/// Params key under which the recipe vars are nested.
pub const RECIPE_SRC_VARS_KEY: &str = "vars";

pub struct RecipeSrcAdapter {
    runner: Arc<dyn RecipeRunner>,
}

/// The pieces of a `recipe_src/run` params object, validated.
struct SrcParams {
    src: String,
    name: String,
    vars_json: String,
}

fn parse_params(params: &Value) -> Result<SrcParams, AdapterError> {
    let obj = params.as_object().ok_or_else(|| {
        AdapterError::Terminal(format!(
            "recipe_src: params must be an object with `{RECIPE_SRC_KEY}`, got {params}"
        ))
    })?;
    let src = match obj.get(RECIPE_SRC_KEY) {
        Some(Value::String(s)) if !s.trim().is_empty() => s.clone(),
        _ => {
            return Err(AdapterError::Terminal(format!(
                "recipe_src: missing `{RECIPE_SRC_KEY}` (the recipe's KDL source) in params"
            )))
        }
    };
    let name = obj
        .get(RECIPE_SRC_NAME_KEY)
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| "<unnamed>".to_string());
    let vars_json = match obj.get(RECIPE_SRC_VARS_KEY) {
        None | Some(Value::Null) => "{}".to_string(),
        Some(v @ Value::Object(_)) => serde_json::to_string(v)
            .map_err(|e| AdapterError::Terminal(format!("serializing vars: {e}")))?,
        Some(other) => {
            return Err(AdapterError::Terminal(format!(
            "recipe_src: `{RECIPE_SRC_VARS_KEY}` must be a JSON object of recipe vars, got {other}"
        )))
        }
    };
    Ok(SrcParams {
        src,
        name,
        vars_json,
    })
}

impl RecipeSrcAdapter {
    pub fn new(runner: Arc<dyn RecipeRunner>) -> Self {
        Self { runner }
    }

    /// Build the task params the way the `run_src` RPC does, so the daemon and the tests cannot
    /// disagree about the shape.
    pub fn params(src: &str, name: Option<&str>, vars: Value) -> Value {
        json!({
            RECIPE_SRC_KEY: src,
            RECIPE_SRC_NAME_KEY: name,
            RECIPE_SRC_VARS_KEY: vars,
        })
    }
}

#[async_trait]
impl Adapter for RecipeSrcAdapter {
    fn name(&self) -> &str {
        RECIPE_SRC_ADAPTER
    }

    fn actions(&self) -> Vec<ActionSpec> {
        vec![ActionSpec {
            name: "run".into(),
            // Pacing keys come from each task's source — see `limit_keys_for_task`.
            limit_keys: vec![],
            params_schema: json!({
                RECIPE_SRC_KEY: "required — the recipe's KDL source",
                RECIPE_SRC_NAME_KEY: "optional — display name, e.g. \"facturagas/facturar\"",
                RECIPE_SRC_VARS_KEY: "optional — object of recipe vars",
            }),
            description: "Run a recipe handed over as KDL source (not installed on disk); pacing follows the source's `limit-key`s.".into(),
        }]
    }

    fn limit_keys_for_task(&self, action: &str, params: &Value) -> Vec<String> {
        if action != "run" {
            return vec![];
        }
        params
            .get(RECIPE_SRC_KEY)
            .and_then(Value::as_str)
            .and_then(|src| parse_meta_from_src(src).ok().flatten())
            .map(|m| m.limit_keys)
            .unwrap_or_default()
    }

    async fn execute(
        &self,
        ctx: &RunCtx,
        action: &str,
        params: Value,
    ) -> Result<Value, AdapterError> {
        if action != "run" {
            return Err(AdapterError::Terminal(format!(
                "recipe_src: unknown action `{action}` (expected `run`)"
            )));
        }
        let p = parse_params(&params)?;
        // Same run options the installed-recipe adapter derives from the registry: raise the tab
        // only if the source asks (`foreground #true`); accountless by contract. A source whose
        // metadata does not parse still runs — the engine reports the real error.
        let foreground = parse_meta_from_src(&p.src)
            .ok()
            .flatten()
            .map(|m| m.foreground)
            .unwrap_or(false);
        let opts = RunOpts {
            account: None,
            foreground,
        };
        let envelope = self.runner.run_src(&p.src, &p.vars_json, &opts).await?;
        warn_unexpected(&ctx.task_id, &p.name, &envelope);
        let result = result_from_envelope(&envelope)?;
        tracing::info!(task = %ctx.task_id, "ran source recipe `{}`", p.name);
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::fake::{FakeRecipeRunner, SRC_PATH};
    use pacewright_core::browser::NullBrowser;
    use std::path::Path;

    const SRC: &str = r#"recipe "facturagas/facturar" {
        limit-key "facturagas.facturar"
        foreground #true
        var "rfc" required=#true
        step { goto "https://facturagas.test/" }
    }"#;

    fn ctx() -> RunCtx {
        RunCtx {
            task_id: "t1".into(),
            browser: Arc::new(NullBrowser),
        }
    }

    #[tokio::test]
    async fn builds_vars_from_the_nested_params_and_runs_the_source() {
        let runner = Arc::new(FakeRecipeRunner::new(|p: &Path, vars: &str| {
            assert_eq!(p, Path::new(SRC_PATH));
            let v: Value = serde_json::from_str(vars).unwrap();
            // only the nested vars reach the recipe — never the reserved keys
            assert_eq!(v, json!({"rfc": "XAXX010101000", "periodo": "2026-09"}));
            Ok(json!({"ok": true, "result": {"folio": "F-1"}, "unexpected": []}))
        }));
        let a = RecipeSrcAdapter::new(runner.clone());
        let params = RecipeSrcAdapter::params(
            SRC,
            Some("facturagas/facturar"),
            json!({"rfc": "XAXX010101000", "periodo": "2026-09"}),
        );
        let out = a.execute(&ctx(), "run", params).await.unwrap();
        assert_eq!(out["folio"], "F-1");
        assert_eq!(runner.src_calls.lock().unwrap()[0], SRC);
        let call = runner.calls.lock().unwrap()[0].clone();
        assert!(
            call.2.foreground,
            "foreground #true in the source is honored"
        );
        assert_eq!(call.2.account, None, "always accountless");
    }

    #[tokio::test]
    async fn rejects_missing_source_without_running() {
        let runner = Arc::new(FakeRecipeRunner::ok(json!({"ok": true})));
        let a = RecipeSrcAdapter::new(runner.clone());
        for bad in [
            json!({"vars": {"rfc": "x"}}),
            json!({"__recipe_src": ""}),
            json!({"__recipe_src": 42}),
            json!("not an object"),
        ] {
            let err = a.execute(&ctx(), "run", bad.clone()).await.unwrap_err();
            assert!(
                matches!(err, AdapterError::Terminal(ref m) if m.contains("__recipe_src") || m.contains("params")),
                "{bad} → {err:?}"
            );
        }
        assert_eq!(runner.call_count(), 0, "nothing ran");
        // vars must be an object when present
        let err = a
            .execute(&ctx(), "run", json!({"__recipe_src": SRC, "vars": [1]}))
            .await
            .unwrap_err();
        assert!(matches!(err, AdapterError::Terminal(ref m) if m.contains("`vars`")));
        // and only `run` exists
        let err = a
            .execute(&ctx(), "nope", json!({"__recipe_src": SRC}))
            .await
            .unwrap_err();
        assert!(matches!(err, AdapterError::Terminal(ref m) if m.contains("unknown action")));
    }

    #[tokio::test]
    async fn missing_vars_default_to_an_empty_object() {
        let runner = Arc::new(FakeRecipeRunner::new(|_: &Path, vars: &str| {
            assert_eq!(vars, "{}");
            Ok(json!({"ok": true}))
        }));
        let a = RecipeSrcAdapter::new(runner);
        let out = a
            .execute(&ctx(), "run", json!({"__recipe_src": SRC}))
            .await
            .unwrap();
        assert_eq!(out, json!({}));
    }

    #[test]
    fn limit_keys_come_from_the_source() {
        let a = RecipeSrcAdapter::new(Arc::new(FakeRecipeRunner::ok(json!({}))));
        assert_eq!(a.name(), RECIPE_SRC_ADAPTER);
        assert_eq!(
            a.limit_keys_for_task("run", &RecipeSrcAdapter::params(SRC, None, json!({}))),
            vec!["facturagas.facturar".to_string()]
        );
        // no source / unparseable source / other action → no keys (and no panic)
        assert!(a.limit_keys_for_task("run", &json!({})).is_empty());
        assert!(a
            .limit_keys_for_task("run", &json!({"__recipe_src": "{{{"}))
            .is_empty());
        assert!(a
            .limit_keys_for_task("other", &RecipeSrcAdapter::params(SRC, None, json!({})))
            .is_empty());
        // the action-only answer stays empty: the key is per task, not per adapter
        assert!(a.limit_keys_for("run").is_empty());
    }

    #[tokio::test]
    async fn runner_failure_with_detail_propagates() {
        let runner = Arc::new(FakeRecipeRunner::new(|_: &Path, _: &str| {
            Err(AdapterError::RetryableWith {
                message: "locator not found".into(),
                detail: json!({"step_index": 0, "url": "https://facturagas.test/"}),
            })
        }));
        let a = RecipeSrcAdapter::new(runner);
        let err = a
            .execute(
                &ctx(),
                "run",
                RecipeSrcAdapter::params(SRC, None, json!({})),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, AdapterError::RetryableWith { .. }));
        assert_eq!(err.detail().unwrap()["step_index"], 0);
    }
}

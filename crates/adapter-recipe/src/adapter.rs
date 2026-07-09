//! `RecipeAdapter` — one pacewright `Adapter` backing every recipe under a given
//! `<adapter>` prefix. It replaces the hand-written per-site adapters (e.g.
//! `adapter-linkedin`): the site logic now lives in a `.kdl` recipe, and this thin
//! shim only *routes* an `(adapter, action)` to that recipe file, runs it via a
//! `RecipeRunner`, and maps the outcome to `Value` / `AdapterError`.
//!
//! Unlike a `BrowserHandle`-driven adapter, it does **not** touch `ctx.browser`: a
//! recipe runs entirely inside one `chrome-agent recipe run` process (so `locators.js`
//! persists across steps), which the `RecipeRunner` owns. `ctx` is used only for the
//! task id in logs.
//!
//! Pacing is unchanged: `limit_keys_for` reads each recipe's declared `limit-key`, so
//! the engine still decides *when* an action may spend, from `config.toml`.

use async_trait::async_trait;
use pacewright_core::adapter::{Adapter, RunCtx};
use pacewright_core::model::{ActionSpec, AdapterError};
use serde_json::{json, Value};
use std::sync::Arc;

use crate::registry::{RecipeMeta, RecipeRegistry};
use crate::runner::RecipeRunner;

pub struct RecipeAdapter {
    adapter: String,
    registry: Arc<RecipeRegistry>,
    runner: Arc<dyn RecipeRunner>,
}

impl RecipeAdapter {
    /// One adapter per distinct `<adapter>` prefix found in the registry. The daemon
    /// builds these from `RecipeRegistry::adapters()`.
    pub fn new(
        adapter: impl Into<String>,
        registry: Arc<RecipeRegistry>,
        runner: Arc<dyn RecipeRunner>,
    ) -> Self {
        Self {
            adapter: adapter.into(),
            registry,
            runner,
        }
    }

    fn resolve(&self, action: &str) -> Result<&RecipeMeta, AdapterError> {
        self.registry.get(&self.adapter, action).ok_or_else(|| {
            AdapterError::Terminal(format!(
                "no recipe `{}/{action}` installed (see `pcw recipe list`)",
                self.adapter
            ))
        })
    }
}

/// A human-readable params hint for `pcw adapters`, derived from the recipe's declared vars.
fn params_schema(meta: &RecipeMeta) -> Value {
    let mut obj = serde_json::Map::new();
    for v in &meta.vars {
        let mut hint = if v.required {
            "required".to_string()
        } else if v.has_default {
            "optional (has default)".to_string()
        } else {
            "optional".to_string()
        };
        if let Some(from) = &v.from {
            hint.push_str(&format!(" — job note field `{from}`"));
        }
        obj.insert(v.name.clone(), Value::String(hint));
    }
    Value::Object(obj)
}

#[async_trait]
impl Adapter for RecipeAdapter {
    fn name(&self) -> &str {
        &self.adapter
    }

    fn actions(&self) -> Vec<ActionSpec> {
        self.registry
            .actions_for(&self.adapter)
            .into_iter()
            .map(|m| ActionSpec {
                name: m.action.clone(),
                limit_keys: m.limit_keys.clone(),
                params_schema: params_schema(m),
                description: m
                    .description
                    .clone()
                    .unwrap_or_else(|| format!("run the `{}` recipe", m.name)),
            })
            .collect()
    }

    fn limit_keys_for(&self, action: &str) -> Vec<String> {
        self.registry
            .get(&self.adapter, action)
            .map(|m| m.limit_keys.clone())
            .unwrap_or_default()
    }

    async fn execute(
        &self,
        ctx: &RunCtx,
        action: &str,
        params: Value,
    ) -> Result<Value, AdapterError> {
        let meta = self.resolve(action)?;

        // The task's params ARE the recipe's vars (the job-runner already applied any
        // `from` aliases). They must be a JSON object to serialize as --vars-json.
        let vars_json = match &params {
            Value::Object(_) => serde_json::to_string(&params)
                .map_err(|e| AdapterError::Terminal(format!("serializing params: {e}")))?,
            Value::Null => "{}".to_string(),
            other => {
                return Err(AdapterError::Terminal(format!(
                    "params must be a JSON object of recipe vars, got {other}"
                )));
            }
        };

        // `auth` recipes get the operator's logged-in session; public recipes navigate cold.
        let envelope = self.runner.run(&meta.path, &vars_json, meta.auth).await?;

        // An `unexpected` run (e.g. a cardinality miss) is not a failure — surface it in the
        // log but return the (possibly under-delivered) result, matching engine semantics.
        if let Some(unexpected) = envelope.get("unexpected").and_then(Value::as_array) {
            if !unexpected.is_empty() {
                let msgs: Vec<&str> = unexpected.iter().filter_map(Value::as_str).collect();
                tracing::warn!(task = %ctx.task_id, "recipe `{}/{action}` unexpected: {}", self.adapter, msgs.join("; "));
            }
        }

        // `Response::Ok(Value)` must be a JSON object; a recipe's result map already is.
        match envelope.get("result") {
            Some(Value::Object(m)) => {
                tracing::info!(task = %ctx.task_id, "ran recipe `{}/{action}`", self.adapter);
                Ok(Value::Object(m.clone()))
            }
            // A result-less recipe (all side effects via `output` files) → an empty object.
            None | Some(Value::Null) => Ok(json!({})),
            Some(other) => Err(AdapterError::Terminal(format!(
                "recipe result was not an object: {other}"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::RecipeRegistry;
    use crate::runner::fake::FakeRecipeRunner;
    use pacewright_core::browser::NullBrowser;
    use std::path::Path;

    const LINKEDIN: &str = r#"recipe "linkedin/scrape_profile" {
        description "scrape a profile"
        limit-key "linkedin.profile_scrape"
        auth #true
        var "url" from="linkedin" required=#true
    }"#;

    fn reg_from(text: &str) -> Arc<RecipeRegistry> {
        let dir = std::env::temp_dir().join(format!(
            "pcw-adp-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("r.kdl"), text).unwrap();
        Arc::new(RecipeRegistry::load_dir(&dir))
    }

    fn ctx() -> RunCtx {
        RunCtx {
            task_id: "t1".into(),
            browser: Arc::new(NullBrowser),
        }
    }

    #[test]
    fn exposes_actions_and_limit_keys_from_recipes() {
        let a = RecipeAdapter::new(
            "linkedin",
            reg_from(LINKEDIN),
            Arc::new(FakeRecipeRunner::ok(json!({}))),
        );
        assert_eq!(a.name(), "linkedin");
        let acts = a.actions();
        assert_eq!(acts.len(), 1);
        assert_eq!(acts[0].name, "scrape_profile");
        assert_eq!(
            a.limit_keys_for("scrape_profile"),
            vec!["linkedin.profile_scrape".to_string()]
        );
        assert!(a.limit_keys_for("nope").is_empty());
    }

    #[tokio::test]
    async fn execute_runs_the_recipe_and_returns_the_result() {
        let reg = reg_from(LINKEDIN);
        let expected_path = reg.get("linkedin", "scrape_profile").unwrap().path.clone();
        let runner = Arc::new(FakeRecipeRunner::new(move |p: &Path, vars: &str| {
            assert_eq!(p, expected_path);
            // the job-runner already mapped `linkedin` → the recipe's `url` var
            assert!(vars.contains("\"url\""), "vars_json = {vars}");
            Ok(json!({"ok": true, "result": {"name": "Jane"}, "unexpected": []}))
        }));
        let a = RecipeAdapter::new("linkedin", reg, runner.clone());
        let out = a
            .execute(
                &ctx(),
                "scrape_profile",
                json!({"url": "https://linkedin.com/in/jane"}),
            )
            .await
            .unwrap();
        assert_eq!(out["name"], "Jane");
        assert_eq!(runner.call_count(), 1);
        // the recipe declared `auth #true`, so the runner was asked for the logged-in session.
        assert!(runner.calls.lock().unwrap()[0].2, "auth flag should propagate");
    }

    #[tokio::test]
    async fn unknown_action_is_terminal() {
        let a = RecipeAdapter::new(
            "linkedin",
            reg_from(LINKEDIN),
            Arc::new(FakeRecipeRunner::ok(json!({}))),
        );
        let err = a.execute(&ctx(), "nope", json!({})).await.unwrap_err();
        assert!(
            matches!(err, AdapterError::Terminal(ref m) if m.contains("no recipe")),
            "got {err:?}"
        );
    }

    #[tokio::test]
    async fn non_object_params_are_terminal() {
        let a = RecipeAdapter::new(
            "linkedin",
            reg_from(LINKEDIN),
            Arc::new(FakeRecipeRunner::ok(json!({}))),
        );
        let err = a
            .execute(&ctx(), "scrape_profile", json!("just a string"))
            .await
            .unwrap_err();
        assert!(matches!(err, AdapterError::Terminal(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn runner_failure_class_propagates() {
        let runner = Arc::new(FakeRecipeRunner::new(|_: &Path, _: &str| {
            Err(AdapterError::Retryable("locator not found".into()))
        }));
        let a = RecipeAdapter::new("linkedin", reg_from(LINKEDIN), runner);
        let err = a
            .execute(&ctx(), "scrape_profile", json!({}))
            .await
            .unwrap_err();
        assert!(matches!(err, AdapterError::Retryable(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn result_less_recipe_yields_empty_object() {
        // A recipe whose effects are all `output` files returns no `result`.
        let runner = Arc::new(FakeRecipeRunner::ok(json!({"ok": true, "unexpected": []})));
        let a = RecipeAdapter::new("linkedin", reg_from(LINKEDIN), runner);
        let out = a
            .execute(&ctx(), "scrape_profile", json!({}))
            .await
            .unwrap();
        assert_eq!(out, json!({}));
    }
}

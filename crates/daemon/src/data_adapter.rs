//! Built-in `data` adapter — persist and re-read task output as JSON datasets (see
//! [`pacewright_core::datastore`]). This is how a non-browser (or browser) pipeline *saves the
//! data*: a producer step emits an array, a `data/append` step accumulates it (deduped) into a named
//! dataset, and a later run's `data/read` step feeds it back — the JSON replacement for the fleet's
//! `sort -u` CSVs.
//!
//! `append` composes with everything: `scan -> data/append` grows a pool; `data/read -> fanout ->
//! claude_cli -> data/append` is the chunked-generation shape. Holds its own [`Datastore`] (captured
//! at construction, like the other built-ins) because writing files is outside the `Adapter` trait's
//! `RunCtx`.
use async_trait::async_trait;
use pacewright_core::adapter::{Adapter, RunCtx};
use pacewright_core::datastore::Datastore;
use pacewright_core::model::{ActionSpec, AdapterError};
use serde_json::{json, Value};

pub const DATA_ADAPTER: &str = "data";

pub struct DataAdapter {
    store: Datastore,
}

impl DataAdapter {
    pub fn new(store: Datastore) -> Self {
        DataAdapter { store }
    }
}

/// Coerce an `items` param into a row vec: an array is used as-is; a lone object becomes a
/// one-element batch; anything else is an error (so a typo'd ref fails loudly, not silently).
fn rows_from(items: &Value) -> Result<Vec<Value>, AdapterError> {
    match items {
        Value::Array(a) => Ok(a.clone()),
        Value::Object(_) => Ok(vec![items.clone()]),
        Value::Null => Ok(vec![]),
        _ => Err(AdapterError::Terminal(
            "data/append: `items` must be a JSON array or object".into(),
        )),
    }
}

#[async_trait]
impl Adapter for DataAdapter {
    fn name(&self) -> &str {
        DATA_ADAPTER
    }

    fn actions(&self) -> Vec<ActionSpec> {
        vec![
            ActionSpec {
                name: "append".into(),
                limit_keys: vec![],
                params_schema: json!({
                    "dataset": "string (e.g. leads/warm)",
                    "items": "array|object of rows to add",
                    "key?": "field to dedup on; omit to dedup on whole rows"
                }),
                description: "Append rows to a JSON dataset, de-duplicating (all-time). Returns {added, duplicates, total}.".into(),
            },
            ActionSpec {
                name: "read".into(),
                limit_keys: vec![],
                params_schema: json!({
                    "dataset": "string",
                    "limit?": "int cap on returned rows",
                    "chunk_size?": "int; when set, returns {chunks:[{index,items}]} instead of {items}"
                }),
                description: "Read a JSON dataset as {items,count} — or, with chunk_size, as {chunks} ready to fan out.".into(),
            },
        ]
    }

    async fn execute(
        &self,
        _ctx: &RunCtx,
        action: &str,
        params: Value,
    ) -> Result<Value, AdapterError> {
        let dataset = params
            .get("dataset")
            .and_then(Value::as_str)
            .ok_or_else(|| AdapterError::Terminal("data: `dataset` is required".into()))?;
        match action {
            "append" => {
                let items = rows_from(params.get("items").unwrap_or(&Value::Null))?;
                let key = params.get("key").and_then(Value::as_str);
                let r = self
                    .store
                    .append(dataset, &items, key)
                    .map_err(|e| AdapterError::Terminal(format!("data/append: {e}")))?;
                Ok(json!({
                    "dataset": dataset,
                    "added": r.added,
                    "duplicates": r.duplicates,
                    "total": r.total,
                }))
            }
            "read" => {
                let mut rows = self
                    .store
                    .read(dataset)
                    .map_err(|e| AdapterError::Terminal(format!("data/read: {e}")))?;
                if let Some(limit) = params.get("limit").and_then(Value::as_u64) {
                    rows.truncate(limit as usize);
                }
                match params.get("chunk_size").and_then(Value::as_u64) {
                    Some(sz) if sz > 0 => {
                        let chunks: Vec<Value> = rows
                            .chunks(sz as usize)
                            .enumerate()
                            .map(|(i, c)| json!({ "index": i, "items": c }))
                            .collect();
                        Ok(json!({ "dataset": dataset, "count": rows.len(), "chunks": chunks }))
                    }
                    _ => Ok(json!({ "dataset": dataset, "count": rows.len(), "items": rows })),
                }
            }
            other => Err(AdapterError::Terminal(format!(
                "data: unknown action `{other}` (expected `append` or `read`)"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn adapter() -> DataAdapter {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let id = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("pw-dataadapter-{}-{id}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        DataAdapter::new(Datastore::new(dir))
    }
    fn ctx() -> RunCtx {
        RunCtx {
            task_id: "t".into(),
            browser: std::sync::Arc::new(pacewright_core::browser::NullBrowser),
        }
    }

    #[tokio::test]
    async fn append_then_read_and_chunk() {
        let a = adapter();
        let ctx = ctx();
        let r = a
            .execute(
                &ctx,
                "append",
                json!({ "dataset": "d", "items": [{"h":"a"},{"h":"b"},{"h":"a"}], "key": "h" }),
            )
            .await
            .unwrap();
        assert_eq!(r["added"], 2);
        assert_eq!(r["duplicates"], 1);

        let read = a
            .execute(&ctx, "read", json!({ "dataset": "d" }))
            .await
            .unwrap();
        assert_eq!(read["count"], 2);
        assert_eq!(read["items"].as_array().unwrap().len(), 2);

        let chunked = a
            .execute(&ctx, "read", json!({ "dataset": "d", "chunk_size": 1 }))
            .await
            .unwrap();
        assert_eq!(chunked["chunks"].as_array().unwrap().len(), 2);
        assert_eq!(chunked["chunks"][0]["index"], 0);
    }

    #[tokio::test]
    async fn append_accepts_a_lone_object() {
        let a = adapter();
        let r = a
            .execute(&ctx(), "append", json!({ "dataset": "d", "items": {"h":"x"}, "key": "h" }))
            .await
            .unwrap();
        assert_eq!(r["added"], 1);
    }
}

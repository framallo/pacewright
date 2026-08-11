//! Built-in `pipeline` adapter — lets the declarative scheduler kick a fresh pipeline RUN on a
//! cadence.
//!
//! The scheduler's recurrence machinery re-queues a task per cron slot; an ordinary recipe task is
//! one action. A scan→draft→fanout pipeline is many tasks under a `run_id`, so we schedule a
//! `pipeline/start` task instead: each firing starts (or resumes) a run whose id is date-stamped, so
//! today's LinkedIn-comment round is `li-comment-20260810`, tomorrow's is `-20260811`, and re-firing
//! within a day is idempotent (`run::start` skips steps that already exist).
//!
//! It holds its own `Arc<Store>` + `Arc<dyn Clock>` (captured at construction, like `AgentAdapter`
//! holds its `Completer`), because starting a run means inserting tasks — which the `Adapter` trait's
//! `RunCtx` deliberately can't do.
use async_trait::async_trait;
use pacewright_core::adapter::{Adapter, RunCtx};
use pacewright_core::clock::Clock;
use pacewright_core::model::{ActionSpec, AdapterError};
use pacewright_core::store::Store;
use serde_json::{json, Value};
use std::sync::Arc;

pub const PIPELINE_ADAPTER: &str = "pipeline";

pub struct PipelineAdapter {
    store: Arc<Store>,
    clock: Arc<dyn Clock>,
}

impl PipelineAdapter {
    pub fn new(store: Arc<Store>, clock: Arc<dyn Clock>) -> Self {
        PipelineAdapter { store, clock }
    }
}

#[async_trait]
impl Adapter for PipelineAdapter {
    fn name(&self) -> &str {
        PIPELINE_ADAPTER
    }

    fn actions(&self) -> Vec<ActionSpec> {
        vec![ActionSpec {
            name: "start".into(),
            limit_keys: vec![],
            params_schema: json!({
                "pipeline": "string (e.g. linkedin/comment)",
                "run_prefix?": "string; run id is <prefix>-YYYYMMDD (default: pipeline name)",
                "params?": "object of pipeline vars"
            }),
            description: "Start (or resume) a fresh dated run of a pipeline. Scheduled daily, this is how a scan-then-act pipeline recurs.".into(),
        }]
    }

    async fn execute(
        &self,
        _ctx: &RunCtx,
        action: &str,
        params: Value,
    ) -> Result<Value, AdapterError> {
        if action != "start" {
            return Err(AdapterError::Terminal(format!(
                "pipeline: unknown action `{action}` (expected `start`)"
            )));
        }
        let pipeline = params
            .get("pipeline")
            .and_then(Value::as_str)
            .ok_or_else(|| AdapterError::Terminal("pipeline/start needs `pipeline`".into()))?;
        let run_prefix = params
            .get("run_prefix")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| pipeline.replace('/', "-"));
        let run_params = params.get("params").cloned().unwrap_or_else(|| json!({}));

        let now = self.clock.now_ms();
        let date = local_date(now);
        let run_id = format!("{run_prefix}-{date}");

        let file = pacewright_core::run::pipeline_path(&pacewright_core::run::home_dir(), pipeline);
        let src = std::fs::read_to_string(&file).map_err(|e| {
            AdapterError::Terminal(format!("cannot read pipeline {}: {e}", file.display()))
        })?;
        let def = pacewright_core::pipeline::parse_pipeline(&src)
            .map_err(|e| AdapterError::Terminal(format!("pipeline {pipeline} invalid: {e}")))?;
        let started = pacewright_core::run::start(&self.store, &def, &run_id, &run_params, now)
            .map_err(|e| AdapterError::Terminal(format!("start run {run_id}: {e}")))?;
        Ok(json!({
            "run_id": run_id,
            "pipeline": def.name,
            "created": started.iter().filter_map(|t| t.step_name.clone()).collect::<Vec<_>>(),
        }))
    }
}

/// Local-date `YYYYMMDD` for the run id, from an epoch-ms instant. Falls back to a stable string on
/// the (impossible-in-practice) out-of-range instant so a run id is always produced.
fn local_date(now_ms: i64) -> String {
    use chrono::TimeZone;
    match chrono::Local.timestamp_millis_opt(now_ms).single() {
        Some(dt) => dt.format("%Y%m%d").to_string(),
        None => "00000000".to_string(),
    }
}

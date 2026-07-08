use async_trait::async_trait;
use pacewright_core::adapter::{Adapter, RunCtx};
use pacewright_core::model::{ActionSpec, AdapterError};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Mutex;

#[derive(Default)]
pub struct DummyAdapter {
    // task_id -> attempts already made (for `flaky`)
    flaky_state: Mutex<HashMap<String, u64>>,
}

impl DummyAdapter {
    pub fn new() -> Self { Self::default() }
}

#[async_trait]
impl Adapter for DummyAdapter {
    fn name(&self) -> &str { "dummy" }

    fn actions(&self) -> Vec<ActionSpec> {
        vec![
            ActionSpec { name: "echo".into(), limit_keys: vec![], params_schema: Value::Null, description: "returns params".into() },
            ActionSpec { name: "slow".into(), limit_keys: vec![], params_schema: json!({"ms":"number"}), description: "sleeps ms".into() },
            ActionSpec { name: "flaky".into(), limit_keys: vec![], params_schema: json!({"fail_times":"number"}), description: "fails then succeeds".into() },
            ActionSpec { name: "always_fail".into(), limit_keys: vec![], params_schema: Value::Null, description: "terminal error".into() },
            ActionSpec { name: "rate_heavy".into(), limit_keys: vec!["dummy.capped".into()], params_schema: Value::Null, description: "spends dummy.capped".into() },
            ActionSpec { name: "panic".into(), limit_keys: vec![], params_schema: Value::Null, description: "deliberately panics, for exercising panic isolation".into() },
        ]
    }

    async fn execute(&self, ctx: &RunCtx, action: &str, params: Value) -> Result<Value, AdapterError> {
        match action {
            "echo" | "rate_heavy" => Ok(params),
            "slow" => {
                let ms = params.get("ms").and_then(|v| v.as_u64()).unwrap_or(0);
                tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
                Ok(json!({"slept_ms": ms}))
            }
            "always_fail" => Err(AdapterError::Terminal("always fails".into())),
            "panic" => panic!("dummy adapter deliberately panicked (action=panic, task={})", ctx.task_id),
            "flaky" => {
                let fail_times = params.get("fail_times").and_then(|v| v.as_u64()).unwrap_or(1);
                let mut st = self.flaky_state.lock().unwrap();
                let n = st.entry(ctx.task_id.clone()).or_insert(0);
                if *n < fail_times {
                    *n += 1;
                    Err(AdapterError::Retryable(format!("flaky attempt {}", *n)))
                } else {
                    Ok(json!({"succeeded_after": *n}))
                }
            }
            other => Err(AdapterError::Terminal(format!("unknown action {other}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(id: &str) -> RunCtx {
        RunCtx { task_id: id.into(), browser: std::sync::Arc::new(pacewright_core::browser::NullBrowser) }
    }

    #[tokio::test]
    async fn test_echo_and_always_fail() {
        let a = DummyAdapter::new();
        assert_eq!(a.execute(&ctx("t"), "echo", json!({"a":1})).await.unwrap(), json!({"a":1}));
        assert!(matches!(a.execute(&ctx("t"), "always_fail", Value::Null).await, Err(AdapterError::Terminal(_))));
    }

    #[tokio::test]
    async fn test_flaky_fails_then_succeeds() {
        let a = DummyAdapter::new();
        let p = json!({"fail_times": 2});
        assert!(a.execute(&ctx("x"), "flaky", p.clone()).await.is_err());
        assert!(a.execute(&ctx("x"), "flaky", p.clone()).await.is_err());
        assert!(a.execute(&ctx("x"), "flaky", p.clone()).await.is_ok());
    }

    #[tokio::test]
    async fn test_rate_heavy_declares_limit_key() {
        let a = DummyAdapter::new();
        assert_eq!(a.limit_keys_for("rate_heavy"), vec!["dummy.capped".to_string()]);
    }

    #[tokio::test]
    #[should_panic(expected = "dummy adapter deliberately panicked")]
    async fn test_panic_action_panics() {
        let a = DummyAdapter::new();
        // Exercises the raw adapter behavior in isolation (no catch_unwind here);
        // the runner-level isolation is covered in pacewright-core's runner/engine tests.
        let _ = a.execute(&ctx("t"), "panic", Value::Null).await;
    }
}

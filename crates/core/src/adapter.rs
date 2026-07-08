use crate::model::{ActionSpec, AdapterError};
use async_trait::async_trait;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;

pub struct RunCtx {
    pub task_id: String,
    // M2+: pub browser: BrowserHandle
}

#[async_trait]
pub trait Adapter: Send + Sync {
    fn name(&self) -> &str;
    fn actions(&self) -> Vec<ActionSpec>;
    async fn execute(&self, ctx: &RunCtx, action: &str, params: Value) -> Result<Value, AdapterError>;

    /// Which daily-limit keys the given action spends. Default: read from `actions()`.
    fn limit_keys_for(&self, action: &str) -> Vec<String> {
        self.actions()
            .into_iter()
            .find(|a| a.name == action)
            .map(|a| a.limit_keys)
            .unwrap_or_default()
    }
}

#[derive(Default, Clone)]
pub struct AdapterRegistry {
    map: HashMap<String, Arc<dyn Adapter>>,
}

impl AdapterRegistry {
    pub fn new() -> Self { Self::default() }
    pub fn register(&mut self, a: Arc<dyn Adapter>) { self.map.insert(a.name().to_string(), a); }
    pub fn get(&self, name: &str) -> Option<Arc<dyn Adapter>> { self.map.get(name).cloned() }
    pub fn all(&self) -> Vec<Arc<dyn Adapter>> { self.map.values().cloned().collect() }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeAdapter;
    #[async_trait]
    impl Adapter for FakeAdapter {
        fn name(&self) -> &str { "fake" }
        fn actions(&self) -> Vec<ActionSpec> {
            vec![ActionSpec { name: "go".into(), limit_keys: vec!["fake.go".into()], params_schema: Value::Null, description: "".into() }]
        }
        async fn execute(&self, _ctx: &RunCtx, _action: &str, _params: Value) -> Result<Value, AdapterError> {
            Ok(Value::Null)
        }
    }

    #[tokio::test]
    async fn test_registry_and_limit_keys() {
        let mut reg = AdapterRegistry::new();
        reg.register(Arc::new(FakeAdapter));
        let a = reg.get("fake").unwrap();
        assert_eq!(a.name(), "fake");
        assert_eq!(a.limit_keys_for("go"), vec!["fake.go".to_string()]);
        assert!(a.limit_keys_for("missing").is_empty());
        let ctx = RunCtx { task_id: "t1".into() };
        assert_eq!(a.execute(&ctx, "go", Value::Null).await.unwrap(), Value::Null);
    }
}

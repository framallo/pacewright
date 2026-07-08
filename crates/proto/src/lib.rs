use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AddTaskReq {
    pub adapter: String,
    pub action: String,
    pub params: Value,
    #[serde(default)] pub scheduled_for: Option<i64>,
    #[serde(default)] pub recurrence: Option<String>,
    #[serde(default)] pub depends_on: Option<String>,
    #[serde(default)] pub priority: Option<i64>,
    #[serde(default)] pub dedup_key: Option<String>,
    #[serde(default)] pub max_attempts: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "method", content = "params", rename_all = "snake_case")]
pub enum Request {
    Add(AddTaskReq),
    Get { id: String },
    List { #[serde(default)] status: Option<String>, #[serde(default)] adapter: Option<String>, #[serde(default)] limit: Option<i64> },
    Cancel { id: String },
    RunNow { id: String, #[serde(default)] force: bool },
    Pause { scope: String },
    Resume { scope: String },
    Limits,
    Adapters,
    Status,
    Subscribe,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    Ok(Value),
    Error { message: String },
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_add_request_roundtrips() {
        let req = Request::Add(AddTaskReq {
            adapter: "dummy".into(), action: "echo".into(), params: serde_json::json!({"a":1}),
            scheduled_for: None, recurrence: None, depends_on: None, priority: Some(2), dedup_key: None, max_attempts: None,
        });
        let s = serde_json::to_string(&req).unwrap();
        let back: Request = serde_json::from_str(&s).unwrap();
        assert_eq!(req, back);
    }
    #[test]
    fn test_list_tagged_shape() {
        let s = serde_json::to_string(&Request::List { status: Some("pending".into()), adapter: None, limit: Some(10) }).unwrap();
        assert!(s.contains("\"method\":\"list\""));
    }
    #[test]
    fn test_response_ok() {
        let s = serde_json::to_string(&Response::Ok(serde_json::json!({"x":1}))).unwrap();
        assert!(s.contains("\"type\":\"ok\""));
    }
}

use anyhow::Result;
use pacewright_proto::{Request, Response};
use std::path::Path;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

pub async fn call(sock: &Path, req: Request) -> Result<Response> {
    let stream = UnixStream::connect(sock).await?;
    let (r, mut w) = stream.into_split();
    let mut line = serde_json::to_string(&req)?;
    line.push('\n');
    w.write_all(line.as_bytes()).await?;
    let mut reader = BufReader::new(r).lines();
    let resp_line = reader
        .next_line()
        .await?
        .ok_or_else(|| anyhow::anyhow!("no response"))?;
    Ok(serde_json::from_str(&resp_line)?)
}

#[cfg(test)]
mod tests {
    use pacewright_proto::{AddTaskReq, Request};
    #[test]
    fn test_build_add_request() {
        let req = Request::Add(AddTaskReq {
            adapter: "dummy".into(),
            action: "echo".into(),
            params: serde_json::json!({}),
            scheduled_for: None,
            recurrence: None,
            depends_on: None,
            priority: None,
            dedup_key: None,
            max_attempts: None,
        });
        let s = serde_json::to_string(&req).unwrap();
        assert!(s.contains("\"method\":\"add\""));
    }
}

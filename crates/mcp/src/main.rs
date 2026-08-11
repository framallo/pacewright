//! `pacewright-mcp` — a stdio MCP server that exposes the pacewright daemon to an MCP client
//! (e.g. Claude). It speaks newline-delimited JSON-RPC 2.0 on stdin/stdout and bridges each
//! `tools/call` to the daemon's Unix socket using the same `pacewright_proto::Request`/`Response`
//! the CLI uses. Logs go to stderr so stdout stays a clean protocol stream.

mod tools;

use anyhow::Result;
use pacewright_proto::{Request, Response};
use serde_json::{json, Value};
use std::path::PathBuf;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

/// The MCP protocol version this server implements; also the fallback when a client's
/// `initialize` omits `protocolVersion`. We echo the client's version when it sends one.
const PROTOCOL_VERSION: &str = "2025-06-18";

fn sock_path() -> PathBuf {
    if let Ok(p) = std::env::var("PACEWRIGHT_SOCK") {
        return PathBuf::from(p);
    }
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()))
        .join(".pacewright")
        .join("pw.sock")
}

/// One round-trip to the daemon: send a request line, read the single response line.
async fn daemon_call(req: Request) -> Result<Response> {
    let stream = UnixStream::connect(sock_path()).await?;
    let (r, mut w) = stream.into_split();
    let mut line = serde_json::to_string(&req)?;
    line.push('\n');
    w.write_all(line.as_bytes()).await?;
    let mut reader = BufReader::new(r).lines();
    let resp_line = reader
        .next_line()
        .await?
        .ok_or_else(|| anyhow::anyhow!("no response from daemon"))?;
    Ok(serde_json::from_str(&resp_line)?)
}

/// Wrap a daemon response as an MCP `tools/call` result: pretty JSON as a text content block,
/// with `isError` set when the daemon returned an error variant.
fn tool_result(resp: Response) -> Value {
    let is_error = matches!(resp, Response::Error { .. });
    let text = serde_json::to_string_pretty(&resp)
        .unwrap_or_else(|e| format!("{{\"serialize_error\":\"{e}\"}}"));
    json!({
        "content": [{ "type": "text", "text": text }],
        "isError": is_error,
    })
}

/// Handle one MCP method call, returning the JSON-RPC `result` value (or an `Err` that the caller
/// turns into a JSON-RPC error). Notifications are handled by the caller (no id → no reply).
async fn handle_method(method: &str, params: &Value) -> Result<Value, String> {
    match method {
        "initialize" => {
            let version = params
                .get("protocolVersion")
                .and_then(|v| v.as_str())
                .unwrap_or(PROTOCOL_VERSION)
                .to_string();
            Ok(json!({
                "protocolVersion": version,
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "pacewright-mcp", "version": env!("CARGO_PKG_VERSION") },
            }))
        }
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tools::tool_catalog() })),
        "tools/call" => {
            let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let args = params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            let req = tools::build_request(name, &args)?;
            match daemon_call(req).await {
                Ok(resp) => Ok(tool_result(resp)),
                // A transport failure (daemon down) is surfaced as a tool error, not a protocol
                // error, so the model sees an actionable message instead of the call aborting.
                Err(e) => Ok(json!({
                    "content": [{ "type": "text", "text": format!("daemon call failed: {e}\n(is pacewrightd running? socket: {})", sock_path().display()) }],
                    "isError": true,
                })),
            }
        }
        other => Err(format!("method not found: {other}")),
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut stdout = tokio::io::stdout();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let msg: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("pacewright-mcp: ignoring unparseable line: {e}");
                continue;
            }
        };
        let id = msg.get("id").cloned();
        let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let params = msg.get("params").cloned().unwrap_or_else(|| json!({}));

        let result = handle_method(method, &params).await;

        // Requests carry an `id` and must be answered; notifications (no id) never are.
        let Some(id) = id else { continue };
        let reply = match result {
            Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            Err(message) => {
                json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32601, "message": message } })
            }
        };
        let mut out = serde_json::to_string(&reply)?;
        out.push('\n');
        stdout.write_all(out.as_bytes()).await?;
        stdout.flush().await?;
    }
    Ok(())
}

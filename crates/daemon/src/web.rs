//! The local web dashboard — the same control plane as the CLI/TUI, in a browser.
//!
//! An axum HTTP server bound to `127.0.0.1` only (operator console, not a network service). It runs
//! alongside the Unix-socket RPC server and shares the one `Arc<Server>`, so the browser, CLI, and
//! TUI all drive the same engine. It adds **no** scheduling/limit logic: `POST /api` funnels a
//! `proto::Request` straight through `server::handle_request` (the exact socket dispatch), and
//! `GET /ws` pushes a full snapshot once a second, each field built from the existing read RPCs.

use crate::server::{handle_request, Server};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::{Html, IntoResponse};
use axum::routing::{get, post};
use axum::{Json, Router};
use pacewright_proto::{Request, Response};
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

/// Run one read RPC through the shared dispatch and return its `Ok` body (or an `{error}` object,
/// so a single failing pane never sinks the whole snapshot).
async fn rpc_value(srv: &Server, req: Request) -> Value {
    match handle_request(srv, req).await {
        Response::Ok(v) => v,
        Response::Error { message } => json!({ "error": message }),
    }
}

/// The full dashboard state: what the Feed / Schedule / Limits panes render, in one payload.
pub async fn snapshot(srv: &Server) -> Value {
    let status = rpc_value(srv, Request::Status).await;
    let tasks = rpc_value(
        srv,
        Request::List {
            status: None,
            adapter: None,
            limit: Some(100),
        },
    )
    .await;
    let schedules = rpc_value(srv, Request::ScheduleList).await;
    let limits = rpc_value(srv, Request::Limits).await;
    // Cached auth status — `AuthList` reads the cache (no per-account check), so it's cheap enough
    // to carry in the 1 s snapshot and lets the Accounts pane flip live during a login.
    let accounts = rpc_value(srv, Request::AuthList).await;
    // Newer feature panes: escalations (call-Claude outbox), datasets (saved JSON), the dedup
    // ledger, and the Claude Max/Pro subscription status.
    let escalations = rpc_value(srv, Request::Escalations { drain: false }).await;
    let datasets = rpc_value(srv, Request::DataList).await;
    let ledger = rpc_value(srv, Request::LedgerStats).await;
    let anthropic = rpc_value(srv, Request::AnthropicStatus).await;
    json!({
        "status": status,
        "tasks": tasks.get("tasks").cloned().unwrap_or(json!([])),
        "schedules": schedules.get("schedules").cloned().unwrap_or(json!([])),
        "schedule_errors": schedules.get("errors").cloned().unwrap_or(json!([])),
        "limits": limits,
        "accounts": accounts.get("accounts").cloned().unwrap_or(json!([])),
        "escalations": escalations.get("escalations").cloned().unwrap_or(json!([])),
        "datasets": datasets.get("datasets").cloned().unwrap_or(json!([])),
        "ledger": ledger.get("scopes").cloned().unwrap_or(json!([])),
        "anthropic": anthropic,
    })
}

/// Build the router. Split from `serve_web` so tests can exercise it without binding a socket.
pub fn app(srv: Arc<Server>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/next", get(next_index))
        .route("/api", post(api))
        .route("/ws", get(ws_upgrade))
        .with_state(srv)
}

async fn index() -> impl IntoResponse {
    Html(include_str!("web/index.html"))
}

/// The feature dashboard (escalations · datasets · ledger · subscription · fleet), served at
/// `/next` alongside the original `/` control plane.
async fn next_index() -> impl IntoResponse {
    Html(include_str!("web/next.html"))
}

/// The whole control plane: a `proto::Request` in, a `proto::Response` out — identical to the
/// socket path, so the browser can do anything the CLI can.
async fn api(State(srv): State<Arc<Server>>, Json(req): Json<Request>) -> Json<Response> {
    Json(handle_request(&srv, req).await)
}

async fn ws_upgrade(State(srv): State<Arc<Server>>, ws: WebSocketUpgrade) -> impl IntoResponse {
    ws.on_upgrade(move |socket| push_snapshots(socket, srv))
}

/// Push a snapshot immediately on connect, then once a second, until the client goes away.
async fn push_snapshots(mut socket: WebSocket, srv: Arc<Server>) {
    let mut interval = tokio::time::interval(Duration::from_secs(1));
    loop {
        interval.tick().await; // fires immediately on the first iteration, then every 1 s
        let snap = snapshot(&srv).await;
        let Ok(txt) = serde_json::to_string(&snap) else {
            continue;
        };
        if socket.send(Message::Text(txt)).await.is_err() {
            break; // client disconnected
        }
    }
}

/// Serve the dashboard on `addr` (localhost). Spawned by `main`; returns only on a bind/serve error.
pub async fn serve_web(srv: Arc<Server>, addr: SocketAddr) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("pacewright dashboard on http://{addr}");
    axum::serve(listener, app(srv)).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::test_support::{test_server, test_server_with_schedule};
    use pacewright_proto::AddTaskReq;

    #[tokio::test]
    async fn snapshot_bundles_the_four_panes() {
        let srv = test_server().await;
        let snap = snapshot(&srv).await;
        // every pane the UI needs is present, even on a fresh engine
        assert!(snap["status"]["pending"].is_number());
        assert!(snap["tasks"].is_array());
        assert!(snap["schedules"].is_array());
        assert!(snap["limits"]["counters"].is_array());
        assert!(snap["accounts"].is_array());
    }

    #[tokio::test]
    async fn snapshot_reflects_an_added_task() {
        let srv = test_server().await;
        handle_request(
            &srv,
            Request::Add(AddTaskReq {
                adapter: "dummy".into(),
                action: "echo".into(),
                params: json!({"a": 1}),
                scheduled_for: None,
                recurrence: None,
                depends_on: None,
                priority: None,
                dedup_key: None,
                max_attempts: None,
            }),
        )
        .await;
        let snap = snapshot(&srv).await;
        assert_eq!(snap["tasks"][0]["action"], "echo");
    }

    #[tokio::test]
    async fn snapshot_reflects_an_applied_schedule() {
        let srv =
            test_server_with_schedule("[[task]]\nid = \"t1\"\nrecipe = \"dummy/echo\"\n").await;
        handle_request(&srv, Request::ScheduleApply { prune: false }).await;
        let snap = snapshot(&srv).await;
        assert_eq!(snap["schedules"][0]["id"], "t1");
        assert_eq!(snap["schedules"][0]["enabled"], true);
    }
}

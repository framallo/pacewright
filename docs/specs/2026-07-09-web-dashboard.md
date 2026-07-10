# Local web dashboard — the scheduler control plane in a browser

Status: **in progress** (2026-07-09). Follow-up to the declarative scheduler
(`2026-07-09-declarative-scheduler.md`, §10 "Deferred"). Pre-approved shape (AskUserQuestion):
**localhost web dashboard · full control plane · clean functional dashboard**.

The TUI already renders Feed / Schedule / Limits and toggles schedules over the daemon's RPCs. The
dashboard is the same control plane in a browser: a single localhost page that live-updates and
drives every RPC. seo-os-inspired centerpiece — a **catalog of recurrent tasks you switch on/off**.

---

## 1. Where it runs

An **axum HTTP server inside the daemon**, bound to `127.0.0.1` only (never `0.0.0.0` — this is an
operator console for the machine running the daemon, not a network service). Address from
`PACEWRIGHT_WEB_ADDR` (default `127.0.0.1:7878`); set it empty to disable the web server entirely.
It runs **alongside** the existing Unix-socket RPC server and the 1 s tick loop — `main.rs` spawns
`web::serve_web(srv, addr)` and then awaits `server::serve(srv, sock)`. Both share the one
`Arc<Server>`, so the browser, the CLI, and the TUI all act on the same engine.

## 2. Reusing the dispatch — no new backend logic

The daemon already funnels every action through `handle_request(&Server, Request) -> Response`. The
web layer reuses it verbatim:

- `POST /api` — body is a `proto::Request` JSON, response is a `proto::Response` JSON. This is the
  whole control plane: `Add`, `Cancel`, `RunNow`, `Pause`, `Resume`, `ScheduleApply/Enable/Disable`,
  `SetLimit`, etc. No per-endpoint routes — one envelope, same as the socket.
- Not a single line of scheduling/limit logic is duplicated in the web module.

## 3. Live updates — snapshot push over WebSocket

- `GET /ws` — a WebSocket that pushes a **full snapshot** JSON every 1 s (matching the tick), and one
  immediately on connect. No client polling; no change-hub plumbing through the engine (a 1 s
  server-driven snapshot is simpler and the payload is small).
- A snapshot bundles what the three panes need, each built by calling `handle_request` for the
  existing read RPCs and assembling the results:
  `{ status: Status, tasks: List(limit 100), schedules: ScheduleList, limits: Limits }`.
- The client applies each snapshot to whichever pane is visible; control actions (`POST /api`) take
  effect and the next snapshot reflects them (≤1 s), so the UI needs no optimistic bookkeeping.

## 4. The page — one self-contained file

`GET /` serves a single HTML document with **inlined CSS + vanilla JS** (`include_str!` an
`index.html`; no Node build, no external assets, no CDN — matches "Rust-only"). Layout mirrors the
TUI so the three surfaces stay conceptually identical:

- **Feed** — the task list (id · adapter/action · status · attempts · last error), newest first,
  with per-row **Cancel** and **Run now**. A header shows pending/running counts and a global
  **Pause/Resume all**.
- **Schedule** — the catalog: each entry as a card/row with a **toggle switch** (enabled), recipe,
  when (every/at/on-apply), next fire, live status. An **Apply** button (with a **prune** checkbox)
  reconciles the files. This is the centerpiece.
- **Limits** — per-key `count / cap`, last-spent, plus a small inline editor that issues `SetLimit`
  (daily cap · min gap · jitter · active window).

"Clean functional dashboard": system font stack, a light/dark-aware neutral palette, no framework,
readable tables, a clear on/off switch. Accessible (labelled controls, keyboard-usable).

## 5. Files

- `crates/daemon/src/web.rs` — the axum `Router`, the three handlers (`GET /`, `POST /api`,
  `GET /ws`), the snapshot builder (`async fn snapshot(&Server) -> Value`), and
  `pub async fn serve_web(srv: Arc<Server>, addr: SocketAddr)`.
- `crates/daemon/src/web/index.html` — the embedded SPA (inlined CSS + JS).
- `crates/daemon/src/lib.rs` — `pub mod web;`.
- `crates/daemon/src/main.rs` — parse `PACEWRIGHT_WEB_ADDR`, spawn `serve_web` (skip if empty/unparseable), log the URL.
- `crates/daemon/Cargo.toml` + root `Cargo.toml` — add `axum` (feature `ws`).

## 6. Testing

- **snapshot builder** — over an in-memory `Server` (the `test_server()` helper), assert the
  snapshot bundles `status`/`tasks`/`schedules`/`limits` and reflects an added task + an applied
  schedule. Pure async, no socket.
- **`POST /api` parity** — a request routed through the web path yields the same `Response` as the
  socket path for a representative RPC (e.g. `Add` then `Get`).
- The page itself is static; no JS test harness in this Rust-only milestone (the control logic it
  drives is all covered server-side). `cargo test --workspace` + `clippy -D warnings` stay green;
  only new files are rustfmt-formatted (no blanket-fmt of existing hand-formatted files).

## 7. Non-goals (this milestone)

- No auth / TLS — localhost-only, single operator. (A future networked mode would add both.)
- No historical charts / metrics beyond current counters.
- No live log streaming per task (the Feed's `last_error` + status is enough; task event history
  stays behind `pcw get`).

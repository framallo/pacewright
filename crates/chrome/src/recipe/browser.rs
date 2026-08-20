//! `RecipeBrowser` — the engine's only coupling to a browser. A tiny trait (generic bound, no
//! `async-trait`/`dyn`) so the engine's logic is unit-testable over a `FakeBrowser` while the real
//! `CdpBrowser` drives chrome-agent's CDP client. The locator runtime (`__pw`) is injected once;
//! `resolve`/`extract` call into it.
//!
//! Runs on the main task (`block_on`), never spawned across threads — futures need not be `Send`.
#![allow(clippy::future_not_send)]

use std::cell::RefCell;
use std::rc::Rc;

use serde_json::{Value, json};

use crate::BoxError;

type Cdp = crate::cdp::client::CdpClient;

/// Settled navigation info.
#[derive(Debug, Clone)]
pub struct NavInfo {
    pub url: String,
    pub title: String,
}

/// The result of `__pw.resolve(spec)` that the engine consumes (the runtime also returns a
/// `count`, unused by the read path — cardinality comes from `extract`).
#[derive(Debug, Clone)]
pub struct Resolved {
    pub found: bool,
    pub text: Option<String>,
}

/// The outcome of an in-page `fetch` (a `request` step): HTTP status and the response body text.
#[derive(Debug, Clone)]
pub struct HttpResponse {
    pub status: u64,
    pub body: String,
}

/// The outcome of a native `api` request: status, body text, and the response headers (a recipe can
/// capture a header such as LinkedIn's `x-restli-id`, which carries the created share URN).
#[derive(Debug, Clone)]
pub struct ApiResponse {
    pub status: u64,
    pub body: String,
    pub headers: Vec<(String, String)>,
}

/// The browser operations a recipe run needs: read path (resolve/extract), write verbs
/// (click/fill/select/upload), and cookie-authenticated HTTP (`request`).
#[allow(async_fn_in_trait)] // engine runs on the main task (block_on), never spawned across threads
pub trait RecipeBrowser {
    /// Register a script to run on every new document (and the current one).
    async fn inject_init_script(&self, js: &str) -> Result<(), BoxError>;
    /// Navigate and settle; returns the settled url/title.
    async fn goto(&self, url: &str) -> Result<NavInfo, BoxError>;
    /// Reload the current page (CDP `Page.reload`, never a fresh navigation) and settle; returns the
    /// settled url/title. Renders SPAs that paint blank on a first `goto` (e.g. the Spotify wizard).
    async fn reload(&self) -> Result<NavInfo, BoxError>;
    /// Resolve a locator spec via `__pw.resolve`.
    async fn resolve(&self, spec: &Value) -> Result<Resolved, BoxError>;
    /// Extract a locator's text (or list of texts) via `__pw.extract`.
    async fn extract(&self, spec: &Value, many: bool) -> Result<Value, BoxError>;
    /// Evaluate a boolean JS expression (used for regex tripwire conditions).
    async fn eval_bool(&self, js: &str) -> Result<bool, BoxError>;

    /// Run an async in-page JS routine (awaitPromise) and return its value as JSON.
    async fn eval(&self, js: &str) -> Result<Value, BoxError>;
    /// Capture a PNG screenshot, returned base64-encoded.
    async fn screenshot(&self) -> Result<String, BoxError>;
    /// Click the element a locator resolves to (real CDP input on the resolved element).
    async fn click(&self, spec: &Value) -> Result<(), BoxError>;
    /// Fill a text input/textarea the locator resolves to.
    async fn fill(&self, spec: &Value, value: &str) -> Result<(), BoxError>;
    /// Trusted text-insert into a rich / `contenteditable` editor (e.g. Slate.js): focus, select all
    /// its contents, then a single trusted `Input.insertText` of `value`.
    async fn insert(&self, spec: &Value, value: &str) -> Result<(), BoxError>;
    /// Choose an option (by value or visible text) of the `<select>` the locator resolves to.
    async fn select(&self, spec: &Value, value: &str) -> Result<(), BoxError>;
    /// Set the files of the `<input type=file>` the locator resolves to.
    async fn upload(&self, spec: &Value, paths: &[String]) -> Result<(), BoxError>;
    /// Perform an HTTP request from the page context (`fetch(…, {credentials:"include"})`), so the
    /// browser's authenticated session cookies are sent.
    async fn request(
        &self,
        method: &str,
        url: &str,
        headers: &[(String, String)],
        body: Option<&str>,
    ) -> Result<HttpResponse, BoxError>;
    /// Native HTTP request (no browser session, no cookies): a direct TLS call for the `api` step.
    /// `headers` already includes any `Authorization: Bearer …` the engine derived from `bearer=`.
    async fn api_request(
        &self,
        method: &str,
        url: &str,
        headers: &[(String, String)],
        body: Option<&str>,
    ) -> Result<ApiResponse, BoxError>;
    /// Native browser download of `url` to `out` (follows cross-origin signed redirects that an
    /// in-page fetch can't). `Ok(true)` = saved; `Ok(false)` = not ready yet (a same-origin 4xx
    /// preflight); `Err` = hard failure.
    async fn download(&self, url: &str, out: &str, timeout_secs: u64) -> Result<bool, BoxError>;
    /// Multi-tab control. `action`:
    /// - `"follow"` — after a click that opened a new tab, attach to it and route subsequent steps
    ///   there (identified by `opener_id`, else a `url_contains` substring, else the lone new page).
    /// - `"back"` — return to the previous tab (pop the follow stack).
    /// - `"close"` — close the current followed tab, then return to the previous.
    async fn tab(&self, action: &str, url_contains: Option<&str>) -> Result<(), BoxError>;
}

/// The real browser: drives chrome-agent's `CdpClient`. The *active* page client is interior-mutable
/// so a `tab "follow"` step can attach to a newly-opened tab and route subsequent steps there; the
/// engine runs on the main task (never `Send`-spawned), so `RefCell`/`Rc` across `.await` is fine.
pub struct CdpBrowser<'a> {
    /// The currently-active page client (main page, or a followed tab once `tab "follow"` runs).
    active: RefCell<Rc<Cdp>>,
    /// The target id of the active page (used to exclude it when hunting for a newly-opened tab).
    active_target: RefCell<String>,
    /// A stack of (client, target_id) to pop on `tab "back"` / `tab "close"`.
    prev: RefCell<Vec<(Rc<Cdp>, String)>>,
    timeout_secs: u64,
    /// Browser-level connection (can `Target.getTargets`); required for `tab` steps.
    browser_client: Option<&'a Cdp>,
    /// HTTP endpoint (`/json/list`) to resolve a new tab's page-ws; required for `tab` steps.
    http_endpoint: Option<String>,
    stealth: bool,
}

impl<'a> CdpBrowser<'a> {
    /// Full constructor. `browser_client`/`http_endpoint` enable `tab` steps; pass `None` for a
    /// single-page run (a `tab` step then errors clearly instead of misbehaving).
    pub fn new(
        client: Cdp,
        target_id: String,
        timeout_secs: u64,
        browser_client: Option<&'a Cdp>,
        http_endpoint: Option<String>,
        stealth: bool,
    ) -> Self {
        Self {
            active: RefCell::new(Rc::new(client)),
            active_target: RefCell::new(target_id),
            prev: RefCell::new(Vec::new()),
            timeout_secs,
            browser_client,
            http_endpoint,
            stealth,
        }
    }

    /// The active page client (Rc clone, so the `RefCell` borrow is released before any `.await`).
    pub fn active_client(&self) -> Rc<Cdp> {
        self.active.borrow().clone()
    }

    /// Resolve a locator to a unique CSS selector via `__pw.mark`, which tags the resolved element
    /// with a `data-pw-recipe` attribute. Errors if the locator resolves nothing.
    async fn mark(&self, spec: &Value) -> Result<String, BoxError> {
        let c = self.active_client();
        let v = crate::commands::eval::run_raw(&c, &format!("__pw.mark({spec})")).await?;
        v.as_str()
            .map(str::to_string)
            .ok_or_else(|| "locator resolved no element to act on".into())
    }

    /// Attach to a newly-opened tab and make it the active page. Polls `Target.getTargets` for a
    /// page whose `opener_id` is the active page (the tab our click opened), else — if given — one
    /// whose url contains `url_contains`, else the lone new page. Injects the `__pw` runtime so
    /// resolve/extract/click work in the followed tab.
    async fn follow(&self, url_contains: Option<&str>) -> Result<(), BoxError> {
        let browser_client = self
            .browser_client
            .ok_or("tab follow: recipe was run without a browser-level connection")?;
        let http = self
            .http_endpoint
            .as_deref()
            .ok_or("tab follow: no HTTP endpoint to resolve the new tab")?;
        let active_id = self.active_target.borrow().clone();

        let mut chosen: Option<String> = None;
        for _ in 0..40 {
            let res: crate::cdp::types::GetTargetsResult =
                browser_client.call("Target.getTargets", json!({})).await?;
            let pages: Vec<&crate::cdp::types::TargetInfo> = res
                .target_infos
                .iter()
                .filter(|t| t.target_type == "page" && t.target_id != active_id)
                .filter(|t| {
                    !t.url.starts_with("devtools://") && !t.url.starts_with("chrome-extension://")
                })
                .collect();
            if let Some(t) = pages
                .iter()
                .find(|t| t.opener_id.as_deref() == Some(active_id.as_str()))
            {
                chosen = Some(t.target_id.clone());
                break;
            }
            if let Some(sub) = url_contains
                && let Some(t) = pages.iter().find(|t| t.url.contains(sub))
            {
                chosen = Some(t.target_id.clone());
                break;
            }
            if url_contains.is_none() && pages.len() == 1 {
                chosen = Some(pages[0].target_id.clone());
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
        let new_id = chosen.ok_or("tab follow: no new tab appeared within timeout")?;

        let new_client = crate::run_helpers::connect_page(http, &new_id, self.stealth).await?;
        // Inject the locator runtime into the followed tab (on future docs and the current one).
        new_client.enable("Page").await?;
        new_client
            .send(
                "Page.addScriptToEvaluateOnNewDocument",
                json!({ "source": crate::recipe::engine::LOCATORS_JS }),
            )
            .await?;
        crate::commands::eval::run_raw(&new_client, crate::recipe::engine::LOCATORS_JS).await?;

        let old = self.active_client();
        self.prev.borrow_mut().push((old, active_id));
        *self.active.borrow_mut() = Rc::new(new_client);
        *self.active_target.borrow_mut() = new_id;
        Ok(())
    }

    /// Return to the previous tab (pop the follow stack). No-op-safe error if the stack is empty.
    async fn back(&self) -> Result<(), BoxError> {
        let (client, target) = self
            .prev
            .borrow_mut()
            .pop()
            .ok_or("tab back: no previous tab on the stack")?;
        *self.active.borrow_mut() = client;
        *self.active_target.borrow_mut() = target;
        Ok(())
    }

    /// Close the current followed tab (via the browser-level connection), then return to the previous.
    async fn close(&self) -> Result<(), BoxError> {
        let target = self.active_target.borrow().clone();
        if let Some(bc) = self.browser_client {
            let _: Value = bc
                .call("Target.closeTarget", json!({ "targetId": target }))
                .await
                .unwrap_or_default();
        }
        self.back().await
    }
}

impl RecipeBrowser for CdpBrowser<'_> {
    async fn inject_init_script(&self, js: &str) -> Result<(), BoxError> {
        let c = self.active_client();
        c.enable("Page").await?;
        // Runs on every future document (survives navigations)…
        c.send(
            "Page.addScriptToEvaluateOnNewDocument",
            json!({ "source": js }),
        )
        .await?;
        // …and once now, so `__pw` exists on the document already loaded (if any).
        crate::commands::eval::run_raw(&c, js).await?;
        Ok(())
    }

    async fn goto(&self, url: &str) -> Result<NavInfo, BoxError> {
        let c = self.active_client();
        let r = crate::commands::goto::run(&c, url, self.timeout_secs, &[]).await?;
        Ok(NavInfo {
            url: r.url,
            title: r.title,
        })
    }

    async fn reload(&self) -> Result<NavInfo, BoxError> {
        let c = self.active_client();
        c.enable("Page").await?;
        // A real CDP reload (not a fresh Page.navigate): some SPAs paint blank on first load but
        // render on reload of an already-open tab. `ignoreCache:false` = a normal (warm) reload.
        c.send("Page.reload", json!({ "ignoreCache": false }))
            .await?;
        // Wait for the load event, then let the SPA settle (mirrors goto's post-load stabilization).
        let _ = c
            .wait_for_event(
                "Page.loadEventFired",
                std::time::Duration::from_secs(self.timeout_secs.max(1)),
            )
            .await;
        let _ = crate::commands::eval::run_raw(
            &c,
            "(async () => new Promise(resolve => { \
               let t = setTimeout(resolve, 3000); \
               const o = new MutationObserver(() => { clearTimeout(t); \
                 t = setTimeout(() => { o.disconnect(); resolve(); }, 200); }); \
               o.observe(document.body || document.documentElement, { childList: true, subtree: true }); \
             }))()",
        )
        .await;
        let info =
            crate::commands::eval::run_raw(&c, "({ url: location.href, title: document.title })")
                .await?;
        Ok(NavInfo {
            url: info
                .get("url")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            title: info
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        })
    }

    async fn resolve(&self, spec: &Value) -> Result<Resolved, BoxError> {
        let c = self.active_client();
        let v = crate::commands::eval::run_raw(&c, &format!("__pw.resolve({spec})")).await?;
        Ok(Resolved {
            found: v.get("found").and_then(Value::as_bool).unwrap_or(false),
            text: v.get("text").and_then(Value::as_str).map(str::to_string),
        })
    }

    async fn extract(&self, spec: &Value, many: bool) -> Result<Value, BoxError> {
        let c = self.active_client();
        crate::commands::eval::run_raw(&c, &format!("__pw.extract({spec}, {many})")).await
    }

    async fn eval_bool(&self, js: &str) -> Result<bool, BoxError> {
        let c = self.active_client();
        let v = crate::commands::eval::run_raw(&c, js).await?;
        Ok(v.as_bool().unwrap_or(false))
    }

    async fn eval(&self, js: &str) -> Result<Value, BoxError> {
        let c = self.active_client();
        crate::commands::eval::run_raw(&c, js).await
    }

    async fn screenshot(&self) -> Result<String, BoxError> {
        let c = self.active_client();
        let v: Value = c
            .call("Page.captureScreenshot", json!({ "format": "png" }))
            .await?;
        Ok(v.get("data")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string())
    }

    async fn click(&self, spec: &Value) -> Result<(), BoxError> {
        let sel = self.mark(spec).await?;
        let c = self.active_client();
        crate::element::click_selector(&c, &sel)
            .await
            .map_err(|e| format!("click failed: {e}"))?;
        Ok(())
    }

    async fn fill(&self, spec: &Value, value: &str) -> Result<(), BoxError> {
        let sel = self.mark(spec).await?;
        let c = self.active_client();
        crate::element::fill_selector(&c, &sel, value)
            .await
            .map_err(|e| format!("fill failed: {e}"))?;
        Ok(())
    }

    async fn insert(&self, spec: &Value, value: &str) -> Result<(), BoxError> {
        let sel = self.mark(spec).await?;
        let c = self.active_client();
        crate::element::insert_text_selector(&c, &sel, value)
            .await
            .map_err(|e| format!("insert failed: {e}"))?;
        Ok(())
    }

    async fn select(&self, spec: &Value, value: &str) -> Result<(), BoxError> {
        let sel = self.mark(spec).await?;
        let c = self.active_client();
        crate::element::select_option_selector(&c, &sel, value)
            .await
            .map_err(|e| format!("select failed: {e}"))?;
        Ok(())
    }

    async fn upload(&self, spec: &Value, paths: &[String]) -> Result<(), BoxError> {
        let sel = self.mark(spec).await?;
        let c = self.active_client();
        crate::element::set_file_input_selector(&c, &sel, paths)
            .await
            .map_err(|e| format!("upload failed: {e}"))?;
        Ok(())
    }

    async fn request(
        &self,
        method: &str,
        url: &str,
        headers: &[(String, String)],
        body: Option<&str>,
    ) -> Result<HttpResponse, BoxError> {
        let hdr_obj: serde_json::Map<String, Value> = headers
            .iter()
            .map(|(k, v)| (k.clone(), Value::String(v.clone())))
            .collect();
        let init = json!({
            "method": method,
            "headers": hdr_obj,
            "body": body,
            "credentials": "include",
        });
        // Async IIFE: fetch with the session cookies, then read the body as text. `awaitPromise`
        // (in eval::run_raw) resolves the promise browser-side.
        let js = format!(
            "(async () => {{ const r = await fetch({url}, {init}); \
             const b = await r.text(); return {{ status: r.status, body: b }}; }})()",
            url = serde_json::to_string(url)?,
        );
        let c = self.active_client();
        let v = crate::commands::eval::run_raw(&c, &js).await?;
        Ok(HttpResponse {
            status: v.get("status").and_then(Value::as_u64).unwrap_or(0),
            body: v
                .get("body")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        })
    }

    async fn api_request(
        &self,
        method: &str,
        url: &str,
        headers: &[(String, String)],
        body: Option<&str>,
    ) -> Result<ApiResponse, BoxError> {
        native_api(method, url, headers, body)
    }

    async fn download(&self, url: &str, out: &str, timeout_secs: u64) -> Result<bool, BoxError> {
        let c = self.active_client();
        match crate::commands::download::run_native(&c, url, Some(out), timeout_secs).await {
            Ok(_) => Ok(true),
            // A "not rendered yet" preflight is not a failure — signal retry with Ok(false).
            Err(e)
                if e.downcast_ref::<crate::commands::download::NotReady>()
                    .is_some() =>
            {
                Ok(false)
            }
            Err(e) => Err(e),
        }
    }

    async fn tab(&self, action: &str, url_contains: Option<&str>) -> Result<(), BoxError> {
        match action {
            "follow" => self.follow(url_contains).await,
            "back" | "main" => self.back().await,
            "close" => self.close().await,
            other => Err(format!("unknown tab action `{other}` (use follow|back|close)").into()),
        }
    }
}

/// A `RecipeBrowser` for browser-free (`api`-only) recipes: it performs native `api_request`s and
/// refuses every page operation, so the runner can skip launching Chrome entirely. Selected when
/// [`crate::recipe::model::Recipe::needs_browser`] is false.
#[derive(Debug, Default, Clone, Copy)]
pub struct NativeBrowser;

impl NativeBrowser {
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

fn no_browser<T>(op: &str) -> Result<T, BoxError> {
    Err(
        format!("recipe step `{op}` needs a browser, but this run is browser-less (api-only)")
            .into(),
    )
}

impl RecipeBrowser for NativeBrowser {
    async fn inject_init_script(&self, _js: &str) -> Result<(), BoxError> {
        Ok(()) // no page to inject into; harmless no-op
    }
    async fn goto(&self, _url: &str) -> Result<NavInfo, BoxError> {
        no_browser("goto")
    }
    async fn reload(&self) -> Result<NavInfo, BoxError> {
        no_browser("reload")
    }
    async fn resolve(&self, _spec: &Value) -> Result<Resolved, BoxError> {
        no_browser("resolve")
    }
    async fn extract(&self, _spec: &Value, _many: bool) -> Result<Value, BoxError> {
        no_browser("extract")
    }
    async fn eval_bool(&self, _js: &str) -> Result<bool, BoxError> {
        no_browser("expect")
    }
    async fn eval(&self, _js: &str) -> Result<Value, BoxError> {
        no_browser("eval")
    }
    async fn screenshot(&self) -> Result<String, BoxError> {
        no_browser("screenshot")
    }
    async fn click(&self, _spec: &Value) -> Result<(), BoxError> {
        no_browser("click")
    }
    async fn fill(&self, _spec: &Value, _value: &str) -> Result<(), BoxError> {
        no_browser("fill")
    }
    async fn insert(&self, _spec: &Value, _value: &str) -> Result<(), BoxError> {
        no_browser("insert")
    }
    async fn select(&self, _spec: &Value, _value: &str) -> Result<(), BoxError> {
        no_browser("select")
    }
    async fn upload(&self, _spec: &Value, _paths: &[String]) -> Result<(), BoxError> {
        no_browser("upload")
    }
    async fn request(
        &self,
        _method: &str,
        _url: &str,
        _headers: &[(String, String)],
        _body: Option<&str>,
    ) -> Result<HttpResponse, BoxError> {
        // In-page `request` rides session cookies → inherently needs a browser. Use `api` instead.
        no_browser("request")
    }
    async fn api_request(
        &self,
        method: &str,
        url: &str,
        headers: &[(String, String)],
        body: Option<&str>,
    ) -> Result<ApiResponse, BoxError> {
        native_api(method, url, headers, body)
    }
    async fn download(&self, _url: &str, _out: &str, _timeout_secs: u64) -> Result<bool, BoxError> {
        no_browser("download")
    }
    async fn tab(&self, _action: &str, _url_contains: Option<&str>) -> Result<(), BoxError> {
        no_browser("tab")
    }
}

/// Native TLS HTTP for the `api` step. Gated behind the non-default `api` feature so default builds
/// (incl. the static-musl release binaries) stay pure-Rust with no TLS stack. `ureq` is blocking;
/// the recipe engine runs sequentially, so a direct call is fine (no runtime to starve).
#[cfg(feature = "api")]
fn native_api(
    method: &str,
    url: &str,
    headers: &[(String, String)],
    body: Option<&str>,
) -> Result<ApiResponse, BoxError> {
    use ureq::http;
    // `http_status_as_error(false)` so a 4xx/5xx still returns a response (status captured for the
    // engine's `expect-status` check) instead of an opaque error.
    let agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(std::time::Duration::from_secs(30)))
        .build()
        .new_agent();
    let mut builder = http::Request::builder().method(method).uri(url);
    for (k, v) in headers {
        builder = builder.header(k.as_str(), v.as_str());
    }
    let request = builder
        .body(body.unwrap_or_default().to_string())
        .map_err(|e| format!("building api request: {e}"))?;
    let mut resp = agent
        .run(request)
        .map_err(|e| format!("api request failed: {e}"))?;
    let status = u64::from(resp.status().as_u16());
    let hdrs = resp
        .headers()
        .iter()
        .filter_map(|(k, v)| {
            v.to_str()
                .ok()
                .map(|s| (k.as_str().to_string(), s.to_string()))
        })
        .collect();
    let text = resp
        .body_mut()
        .read_to_string()
        .map_err(|e| format!("reading api response body: {e}"))?;
    Ok(ApiResponse {
        status,
        body: text,
        headers: hdrs,
    })
}

/// Stub when the `api` feature is off: the step parses (recipes stay portable) but can't execute.
#[cfg(not(feature = "api"))]
fn native_api(
    _method: &str,
    _url: &str,
    _headers: &[(String, String)],
    _body: Option<&str>,
) -> Result<ApiResponse, BoxError> {
    Err(
        "chrome-agent was built without the `api` feature (native HTTPS disabled); \
         rebuild with `--features api`"
            .into(),
    )
}

#[cfg(test)]
pub mod fake {
    use super::{ApiResponse, BoxError, HttpResponse, NavInfo, RecipeBrowser, Resolved, Value};
    use std::cell::RefCell;

    type Resolver = Box<dyn Fn(&Value) -> Resolved>;
    type Extractor = Box<dyn Fn(&Value, bool) -> Value>;
    type BoolEval = Box<dyn Fn(&str) -> bool>;
    type JsonEval = Box<dyn Fn(&str) -> Value>;
    type Responder = Box<dyn Fn(&str, &str, Option<&str>) -> HttpResponse>;
    type ApiResponder = Box<dyn Fn(&str, &str, &[(String, String)], Option<&str>) -> ApiResponse>;

    /// A recorded write verb or HTTP request, for asserting the engine drove the browser correctly.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum Action {
        Reload,
        Click,
        Fill(String),
        Insert(String),
        Select(String),
        Upload(Vec<String>),
        Download { url: String, out: String },
        Request { method: String, url: String },
        Api { method: String, url: String },
        Tab { action: String },
    }

    /// A scriptable `RecipeBrowser` for engine unit tests. Records calls; responds to
    /// resolve/extract/eval/request via injected closures.
    pub struct FakeBrowser {
        pub resolver: Resolver,
        pub extractor: Extractor,
        pub bool_eval: BoolEval,
        pub json_eval: JsonEval,
        pub responder: Responder,
        pub api_responder: ApiResponder,
        pub injected: RefCell<Vec<String>>,
        pub gotos: RefCell<Vec<String>>,
        pub shots: RefCell<usize>,
        pub actions: RefCell<Vec<Action>>,
    }

    impl FakeBrowser {
        /// Defaults: everything resolves (found, count 1), extract returns null, conditions false,
        /// requests return `200 {}`.
        pub fn new() -> Self {
            Self {
                resolver: Box::new(|_| Resolved {
                    found: true,
                    text: Some("x".into()),
                }),
                extractor: Box::new(|_, _| Value::Null),
                bool_eval: Box::new(|_| false),
                json_eval: Box::new(|_| Value::Null),
                responder: Box::new(|_, _, _| HttpResponse {
                    status: 200,
                    body: "{}".into(),
                }),
                api_responder: Box::new(|_, _, _, _| ApiResponse {
                    status: 200,
                    body: "{}".into(),
                    headers: Vec::new(),
                }),
                injected: RefCell::new(Vec::new()),
                gotos: RefCell::new(Vec::new()),
                shots: RefCell::new(0),
                actions: RefCell::new(Vec::new()),
            }
        }
        pub fn resolver(mut self, f: impl Fn(&Value) -> Resolved + 'static) -> Self {
            self.resolver = Box::new(f);
            self
        }
        pub fn extractor(mut self, f: impl Fn(&Value, bool) -> Value + 'static) -> Self {
            self.extractor = Box::new(f);
            self
        }
        pub fn bool_eval(mut self, f: impl Fn(&str) -> bool + 'static) -> Self {
            self.bool_eval = Box::new(f);
            self
        }
        pub fn json_eval(mut self, f: impl Fn(&str) -> Value + 'static) -> Self {
            self.json_eval = Box::new(f);
            self
        }
        pub fn responder(
            mut self,
            f: impl Fn(&str, &str, Option<&str>) -> HttpResponse + 'static,
        ) -> Self {
            self.responder = Box::new(f);
            self
        }
        pub fn api_responder(
            mut self,
            f: impl Fn(&str, &str, &[(String, String)], Option<&str>) -> ApiResponse + 'static,
        ) -> Self {
            self.api_responder = Box::new(f);
            self
        }
    }

    impl RecipeBrowser for FakeBrowser {
        async fn inject_init_script(&self, js: &str) -> Result<(), BoxError> {
            self.injected.borrow_mut().push(js.to_string());
            Ok(())
        }
        async fn goto(&self, url: &str) -> Result<NavInfo, BoxError> {
            self.gotos.borrow_mut().push(url.to_string());
            Ok(NavInfo {
                url: url.to_string(),
                title: "fake".into(),
            })
        }
        async fn reload(&self) -> Result<NavInfo, BoxError> {
            self.actions.borrow_mut().push(Action::Reload);
            Ok(NavInfo {
                url: "about:reloaded".into(),
                title: "fake".into(),
            })
        }
        async fn resolve(&self, spec: &Value) -> Result<Resolved, BoxError> {
            Ok((self.resolver)(spec))
        }
        async fn extract(&self, spec: &Value, many: bool) -> Result<Value, BoxError> {
            Ok((self.extractor)(spec, many))
        }
        async fn eval_bool(&self, js: &str) -> Result<bool, BoxError> {
            Ok((self.bool_eval)(js))
        }
        async fn eval(&self, js: &str) -> Result<Value, BoxError> {
            Ok((self.json_eval)(js))
        }
        async fn screenshot(&self) -> Result<String, BoxError> {
            *self.shots.borrow_mut() += 1;
            Ok("ZmFrZQ==".into())
        }
        async fn click(&self, _spec: &Value) -> Result<(), BoxError> {
            self.actions.borrow_mut().push(Action::Click);
            Ok(())
        }
        async fn fill(&self, _spec: &Value, value: &str) -> Result<(), BoxError> {
            self.actions.borrow_mut().push(Action::Fill(value.into()));
            Ok(())
        }
        async fn insert(&self, _spec: &Value, value: &str) -> Result<(), BoxError> {
            self.actions.borrow_mut().push(Action::Insert(value.into()));
            Ok(())
        }
        async fn select(&self, _spec: &Value, value: &str) -> Result<(), BoxError> {
            self.actions.borrow_mut().push(Action::Select(value.into()));
            Ok(())
        }
        async fn upload(&self, _spec: &Value, paths: &[String]) -> Result<(), BoxError> {
            self.actions
                .borrow_mut()
                .push(Action::Upload(paths.to_vec()));
            Ok(())
        }
        async fn download(
            &self,
            url: &str,
            out: &str,
            _timeout_secs: u64,
        ) -> Result<bool, BoxError> {
            self.actions.borrow_mut().push(Action::Download {
                url: url.to_string(),
                out: out.to_string(),
            });
            Ok(true)
        }
        async fn tab(&self, action: &str, _url_contains: Option<&str>) -> Result<(), BoxError> {
            self.actions.borrow_mut().push(Action::Tab {
                action: action.to_string(),
            });
            Ok(())
        }
        async fn request(
            &self,
            method: &str,
            url: &str,
            _headers: &[(String, String)],
            body: Option<&str>,
        ) -> Result<HttpResponse, BoxError> {
            self.actions.borrow_mut().push(Action::Request {
                method: method.to_string(),
                url: url.to_string(),
            });
            Ok((self.responder)(method, url, body))
        }
        async fn api_request(
            &self,
            method: &str,
            url: &str,
            headers: &[(String, String)],
            body: Option<&str>,
        ) -> Result<ApiResponse, BoxError> {
            self.actions.borrow_mut().push(Action::Api {
                method: method.to_string(),
                url: url.to_string(),
            });
            Ok((self.api_responder)(method, url, headers, body))
        }
    }
}

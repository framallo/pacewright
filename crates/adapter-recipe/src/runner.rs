//! `RecipeRunner` — the seam that actually *runs* a recipe: a whole recipe in **one**
//! chrome-agent process, so the injected `locators.js` runtime persists across the
//! recipe's steps (the exact limitation the per-verb `CliBrowser` could not satisfy).
//!
//! The trait is the testable boundary: `CliRecipeRunner` shells the real
//! `chrome-agent recipe run …`; adapter tests drive a fake. Everything about the
//! child-process contract — the pinned `--browser`/`--page`, the `{"ok":…}` envelope,
//! and recovering the pacewright error *class* from the child's error string — is
//! isolated here so the adapter stays pure result→`Value` / exit→`AdapterError` glue.

use async_trait::async_trait;
use pacewright_core::model::AdapterError;
use pacewright_core::secrets::SecretStore;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// `~/.pacewright/secrets.json` — the OAuth secret store (mode 0600).
fn secrets_path() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default())
        .join(".pacewright")
        .join("secrets.json")
}

/// The provider (recipe-name prefix) of a recipe source: `recipe "acme/post"` → `"acme"`.
fn recipe_provider(src: &str) -> Option<String> {
    let after = src.split("recipe ").nth(1)?.trim_start();
    let inner = after.strip_prefix('"')?;
    let name = &inner[..inner.find('"')?];
    name.split('/').next().map(str::to_string)
}

/// True if the recipe declares `var "<name>"`.
fn declares_var(src: &str, name: &str) -> bool {
    src.contains(&format!("var \"{name}\""))
}

/// Merge OAuth secrets into `vars_json` for a recipe that declares a `token` var: a currently-valid
/// access token for the recipe's provider (plus its `author_urn`) from `store`. Best-effort — returns
/// `vars_json` unchanged if the recipe needs no token, the provider has no valid token, or the JSON
/// won't parse (the recipe then fails clearly on the missing var). Never overwrites a value already
/// present in `vars_json` (an explicit param wins).
fn inject_oauth_vars(src: &str, vars_json: &str, store: &SecretStore, now_ms: i64) -> String {
    if !declares_var(src, "token") {
        return vars_json.to_string();
    }
    let Some(provider) = recipe_provider(src) else {
        return vars_json.to_string();
    };
    // 5-minute skew: only inject a token with real runway left.
    let Some(token) = store.valid_access_token(&provider, now_ms, 300_000) else {
        return vars_json.to_string();
    };
    let Ok(Value::Object(mut obj)) = serde_json::from_str::<Value>(vars_json) else {
        return vars_json.to_string();
    };
    obj.entry("token".to_string())
        .or_insert_with(|| Value::String(token.to_string()));
    if let Some(urn) = store.get(&provider).and_then(|r| r.author_urn.clone()) {
        obj.entry("author_urn".to_string())
            .or_insert(Value::String(urn));
    }
    serde_json::to_string(&Value::Object(obj)).unwrap_or_else(|_| vars_json.to_string())
}

/// How one recipe run should be driven. A struct rather than positional flags because the
/// old `(auth: bool, account: Option<&str>)` pair no longer described anything: attaching to the
/// live Chrome removed the throwaway profile that `auth` chose cookie-copying for, leaving it dead.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunOpts {
    /// `auth account="<name>"` — run in the account's own **tab** of the attached Chrome (the
    /// session the human established there via `pcw auth login`). `None` → the shared page.
    pub account: Option<String>,
    /// `foreground #true` — raise the tab (`--activate`) for the run. Chrome throttles background
    /// tabs, which stalls a heavy render.
    pub foreground: bool,
}

/// Page (tab) name for one run: the account's own tab, or `<base>-<unique>` so the tab is created
/// for this run and closed after it. The suffix is nanoseconds plus a process-wide counter, so two
/// runs started in the same instant on two Chromes still get distinct names.
pub fn run_page_name(opts: &RunOpts, base: &str) -> String {
    if let Some(account) = opts.account.as_deref() {
        return account.to_string();
    }
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("{base}-{nanos:x}-{seq}")
}

impl RunOpts {
    pub fn account(name: impl Into<String>) -> Self {
        Self {
            account: Some(name.into()),
            foreground: false,
        }
    }
    pub fn foreground(mut self, on: bool) -> Self {
        self.foreground = on;
        self
    }
}

/// Runs a recipe file and returns its run envelope, or the mapped failure class.
#[async_trait]
pub trait RecipeRunner: Send + Sync {
    /// Run `recipe_path` binding `vars_json` (a JSON **object** string), per `opts`.
    ///
    /// On success returns the run envelope `{"ok":true,"result":{…},"unexpected":[…]}`; on failure
    /// the `AdapterError` whose class is recovered from the child's error output.
    async fn run(
        &self,
        recipe_path: &Path,
        vars_json: &str,
        opts: &RunOpts,
    ) -> Result<Value, AdapterError>;

    /// Run a recipe from its **source** text rather than a file — for recipes that live in a
    /// database row (the `recipe_src` adapter), not under `~/.pacewright/recipes/`.
    ///
    /// Default: spool `src` to a temp file and [`RecipeRunner::run`] it, so a path-only runner
    /// (the `chrome-agent` CLI) still works. The in-process runner overrides this and never
    /// touches the disk.
    async fn run_src(
        &self,
        src: &str,
        vars_json: &str,
        opts: &RunOpts,
    ) -> Result<Value, AdapterError> {
        let path = std::env::temp_dir().join(format!(
            "pacewright-recipe-src-{}-{}.kdl",
            std::process::id(),
            uuid_like()
        ));
        std::fs::write(&path, src)
            .map_err(|e| AdapterError::Terminal(format!("spooling recipe source: {e}")))?;
        let out = self.run(&path, vars_json, opts).await;
        let _ = std::fs::remove_file(&path);
        out
    }
}

/// A unique-enough suffix for a spool file name without pulling a uuid crate in here.
fn uuid_like() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    format!("{nanos:x}")
}

/// Inline a `download` step's files by `base64` when they are at most this big (bytes); larger
/// files are reported by `path` + `size` only. 4 MiB covers every CFDI/PDF a tax portal hands back
/// while keeping a task row far from the SQLite blob limits.
pub const DOWNLOAD_INLINE_MAX_BYTES: u64 = 4 * 1024 * 1024;

/// The `downloads` object of a run envelope: `{"<key>": {"path", "size", "base64"?}}`, where
/// `base64` is present only up to [`DOWNLOAD_INLINE_MAX_BYTES`]. A file that cannot be read back
/// reports `{"path", "error"}` instead — a missing file is evidence, not a reason to fail a run
/// that already succeeded. Empty when the recipe downloaded nothing.
pub fn downloads_json(downloads: &[pacewright_chrome::recipe::engine::Download]) -> Value {
    use base64::Engine as _;
    let mut obj = serde_json::Map::new();
    for d in downloads {
        let entry = match std::fs::metadata(&d.path) {
            Ok(meta) => {
                let size = meta.len();
                let mut e = serde_json::json!({ "path": d.path, "size": size });
                if size <= DOWNLOAD_INLINE_MAX_BYTES {
                    match std::fs::read(&d.path) {
                        Ok(bytes) => {
                            e["base64"] = Value::String(
                                base64::engine::general_purpose::STANDARD.encode(bytes),
                            );
                        }
                        Err(err) => e["error"] = Value::String(format!("reading: {err}")),
                    }
                }
                e
            }
            Err(err) => serde_json::json!({ "path": d.path, "error": format!("stat: {err}") }),
        };
        obj.insert(d.key.clone(), entry);
    }
    Value::Object(obj)
}

/// The endpoint of the always-on Chrome. Re-exported from core so the two callers of chrome-agent
/// cannot drift onto different endpoints.
pub use pacewright_core::browser::{default_connect_endpoint, DEFAULT_CHROME_CONNECT};

/// The chrome-agent binary pacewright shells out to (`CHROME_AGENT_BIN`, default `chrome-agent`).
pub fn chrome_agent_bin() -> String {
    std::env::var("CHROME_AGENT_BIN").unwrap_or_else(|_| "chrome-agent".to_string())
}

/// Preflight: does `<bin> recipe` exist? Recipes + pipelines shell out to `chrome-agent recipe run`;
/// a chrome-agent built WITHOUT the recipe engine (plain upstream, or the fork's `main` before the
/// engine was merged) makes every recipe task fail cryptically. The daemon calls this at boot so it
/// warns once, loudly, instead of failing per task. Runs `<bin> recipe --help` and checks exit 0.
pub fn recipe_subcommand_available(bin: &str) -> bool {
    std::process::Command::new(bin)
        .args(["recipe", "--help"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// The real runner:
/// `chrome-agent --connect <endpoint> --browser pacewright --page <account> recipe run <file>`.
///
/// **Attaches** to the operator's always-on, non-headless Chrome (launched by launchd on
/// `--remote-debugging-port`, never by chrome-agent) rather than launching one. Launching is
/// what breaks auth: acme walls a CDP-launched profile and revokes `li_at`, and Google
/// refuses sign-in on one outright.
///
/// Sites are separated by named **tabs**, not by browser profiles — `account` picks the page.
/// `--browser` is still pinned even though `--connect` identifies the browser: it is the
/// `sessions.json` bookkeeping key, and leaving it at `default` invites the shared-`default`
/// hijack another chrome-agent consumer can walk into.
///
/// `--stealth` is retained: CDP attach still leaves `navigator.webdriver` true, which is itself
/// a detection signal. `--headed`/`--copy-cookies` are gone — the attached profile IS the live
/// session, and the real Chrome is visible by definition.
pub struct CliRecipeRunner {
    bin: String,
    timeout_secs: u64,
    stealth: bool,
    connect: Option<String>,
    browser_name: String,
    page_name: String,
}

impl Default for CliRecipeRunner {
    fn default() -> Self {
        Self::new()
    }
}

impl CliRecipeRunner {
    pub fn new() -> Self {
        Self {
            bin: std::env::var("CHROME_AGENT_BIN").unwrap_or_else(|_| "chrome-agent".to_string()),
            // A recipe drives many steps in one process; give it more room than a single verb.
            timeout_secs: 180,
            stealth: true,
            connect: default_connect_endpoint(),
            // Keep in lockstep with pacewright_browser::DEFAULT_{BROWSER,PAGE}_NAME.
            browser_name: "pacewright".to_string(),
            page_name: "pacewright".to_string(),
        }
    }
    pub fn bin(mut self, bin: impl Into<String>) -> Self {
        self.bin = bin.into();
        self
    }
    pub fn timeout_secs(mut self, s: u64) -> Self {
        self.timeout_secs = s;
        self
    }
    pub fn stealth(mut self, on: bool) -> Self {
        self.stealth = on;
        self
    }
    /// Endpoint of the always-on Chrome to attach to (`http://127.0.0.1:9222` or `auto`).
    pub fn connect(mut self, endpoint: impl Into<String>) -> Self {
        self.connect = Some(endpoint.into());
        self
    }
    pub fn browser_name(mut self, name: impl Into<String>) -> Self {
        self.browser_name = name.into();
        self
    }
    pub fn page_name(mut self, name: impl Into<String>) -> Self {
        self.page_name = name.into();
        self
    }

    /// The tab a run drives. An `auth account` recipe keeps its named tab (that is where the human
    /// signed in). Everything else gets a page name that is unique to this run, so chrome-agent
    /// always CREATES the tab and, because it created it, closes it when the run ends (success or
    /// failure). Reusing one shared name ("pacewright") adopted whatever tab carried that name from
    /// an earlier, interrupted run and then never closed it: that is how X compose boxes were left
    /// open on screen after a killed run.
    fn run_page(&self, opts: &RunOpts) -> String {
        run_page_name(opts, &self.page_name)
    }

    fn args(&self, recipe_path: &Path, vars_json: &str, page: &str, opts: &RunOpts) -> Vec<String> {
        // The inversion: an account no longer selects a *browser* (its own launched profile) but a
        // named *tab* inside the one attached Chrome. Verified live 2026-07-16 — named pages are
        // real, independent tabs and driving one does not clobber another. Public/shared recipes
        // get a per-run tab (see run_page).
        let mut v = vec![
            "--json".to_string(),
            "--timeout".to_string(),
            self.timeout_secs.to_string(),
            "--browser".to_string(),
            self.browser_name.clone(),
            "--page".to_string(),
            page.to_string(),
        ];
        // Attach to the always-on Chrome. Global flag → must precede the subcommand.
        if let Some(endpoint) = &self.connect {
            v.push("--connect".into());
            v.push(endpoint.clone());
        }
        // Raise the tab only when the recipe asked: exactly one tab can be foreground, and
        // stealing focus on the operator's Mac is not a thing to do by default.
        if opts.foreground {
            v.push("--activate".into());
        }
        if self.stealth {
            v.push("--stealth".into());
        }
        v.push("recipe".into());
        v.push("run".into());
        v.push(recipe_path.to_string_lossy().into_owned());
        v.push("--vars-json".into());
        v.push(vars_json.to_string());
        v
    }
}

#[async_trait]
impl RecipeRunner for CliRecipeRunner {
    async fn run(
        &self,
        recipe_path: &Path,
        vars_json: &str,
        opts: &RunOpts,
    ) -> Result<Value, AdapterError> {
        // Inject OAuth secrets (token/author_urn) for token-based recipes from the 0600 secret store,
        // just before spawning — so scheduled API posts fire headless with no auth wall. Best-effort:
        // a recipe that needs no token, or a provider with no valid token, passes through untouched.
        let src = std::fs::read_to_string(recipe_path).unwrap_or_default();
        let store = SecretStore::load(secrets_path()).unwrap_or_default();
        let now_ms = chrono::Utc::now().timestamp_millis();
        let injected = inject_oauth_vars(&src, vars_json, &store, now_ms);
        let page = self.run_page(opts);
        let args = self.args(recipe_path, &injected, &page, opts);
        let outcome = self.spawn_once(&args).await;
        // If the run failed because this page's cached CDP target is stale (its tab was closed
        // since chrome-agent recorded it), prune the page and run once more — chrome-agent then
        // opens a fresh tab. A closed tab would otherwise be a silent, permanent task failure. The
        // page is opts.account (its own tab) or the shared default page. See is_stale_page_target.
        if is_stale_outcome(&outcome) {
            pacewright_core::browser::prune_stale_page(&self.browser_name, &page);
            return self.spawn_once(&args).await;
        }
        outcome
    }
}

impl CliRecipeRunner {
    async fn spawn_once(&self, args: &[String]) -> Result<Value, AdapterError> {
        let out = tokio::process::Command::new(&self.bin)
            .args(args)
            .output()
            .await
            .map_err(|e| AdapterError::Terminal(format!("cannot spawn `{}`: {e}", self.bin)))?;
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        interpret_output(out.status.success(), &stdout, &stderr)
    }
}

/// A recipe run's failure is a stale page target if its error message is one — whether it came back
/// classed (interpret_output tags an untagged chrome-agent error Terminal) or raw.
fn is_stale_outcome(outcome: &Result<Value, AdapterError>) -> bool {
    match outcome {
        Err(e) => pacewright_core::browser::is_stale_page_target(&e.to_string()),
        Ok(_) => false,
    }
}

/// In-process recipe runner: links the vendored chrome-agent (`pacewright_chrome`) and drives the
/// attached Chrome directly over CDP — no `chrome-agent` subprocess, PATH, or symlink. Same behavior
/// contract as [`CliRecipeRunner`] (OAuth-var injection, `{ok,result,unexpected}` envelope, error
/// classification), just linked instead of shelled.
///
/// The recipe engine holds `Rc<CdpClient>`, so its future is `!Send` and can't run on pacewright's
/// spawned (Send) task. We isolate it on a dedicated current-thread runtime + `LocalSet` and return
/// only the Send result (`Value`/`AdapterError`) over a oneshot — the standard !Send bridge.
pub struct NativeRecipeRunner {
    pool: crate::pool::ChromePool,
    browser_name: String,
    page_name: String,
    stealth: bool,
    timeout_secs: u64,
    solver: Option<Arc<dyn pacewright_chrome::recipe::engine::Solver + Send + Sync>>,
}

impl Default for NativeRecipeRunner {
    fn default() -> Self {
        Self::new()
    }
}

impl NativeRecipeRunner {
    pub fn new() -> Self {
        Self {
            pool: crate::pool::ChromePool::new(pacewright_core::browser::default_connect_pool()),
            browser_name: "pacewright".to_string(),
            page_name: "pacewright".to_string(),
            stealth: true,
            timeout_secs: 180,
            solver: None,
        }
    }
    /// Attach to exactly one Chrome: a pool of one, which is the original behavior.
    pub fn connect(mut self, endpoint: impl Into<String>) -> Self {
        self.pool = crate::pool::ChromePool::single(endpoint);
        self
    }
    /// Attach across several Chromes. An account still lands on its own one (that is where its
    /// session is); accountless runs spread, so the pool size is the parallelism.
    pub fn pool(mut self, pool: crate::pool::ChromePool) -> Self {
        self.pool = pool;
        self
    }
    pub fn timeout_secs(mut self, s: u64) -> Self {
        self.timeout_secs = s;
        self
    }
    /// Wire a challenge solver (pacewright passes a Claude-vision solver), enabling recipe `solve`
    /// steps to work around captchas. Without it, a `solve` step fails terminal.
    pub fn with_solver(
        mut self,
        solver: Arc<dyn pacewright_chrome::recipe::engine::Solver + Send + Sync>,
    ) -> Self {
        self.solver = Some(solver);
        self
    }
}

#[async_trait]
impl RecipeRunner for NativeRecipeRunner {
    async fn run(
        &self,
        recipe_path: &Path,
        vars_json: &str,
        opts: &RunOpts,
    ) -> Result<Value, AdapterError> {
        let src = std::fs::read_to_string(recipe_path).map_err(|e| {
            AdapterError::Terminal(format!("reading {}: {e}", recipe_path.display()))
        })?;
        self.run_src(&src, vars_json, opts).await
    }

    /// Run a recipe from its **source** rather than a path — in-process, no spool file.
    ///
    /// This is the real body; [`RecipeRunner::run`] reads the file and delegates here. Splitting it
    /// this way also removed a double read that was here from the start: the source was loaded to
    /// inject OAuth vars, thrown away, and the *path* handed to the worker — which opened and read
    /// the very same file again.
    async fn run_src(
        &self,
        src: &str,
        vars_json: &str,
        opts: &RunOpts,
    ) -> Result<Value, AdapterError> {
        // Inject OAuth secrets exactly as the CLI path did, then resolve to a var map — all on the
        // caller thread (Send), before crossing to the recipe worker.
        let store = SecretStore::load(secrets_path()).unwrap_or_default();
        let now_ms = chrono::Utc::now().timestamp_millis();
        let injected = inject_oauth_vars(src, vars_json, &store, now_ms);
        let vars = pacewright_chrome::api::parse_vars(&[], Some(&injected))
            .map_err(|e| AdapterError::Terminal(format!("recipe vars: {e}")))?;

        // Lease a Chrome for this run. The lease lives until the end of this function, so the slot
        // is not handed to another run while this recipe is driving its tab.
        let lease = self.pool.lease(opts.account.as_deref()).await;
        let connect = lease.endpoint().to_string();
        // Per-slot bookkeeping name: chrome-agent caches a page's CDP target id under
        // `--browser <name>`, so N Chromes under one name would look up each other's tabs.
        let browser_name = lease.browser_name(&self.browser_name);
        // Same rule as the CLI runner: account tab, or a per-run tab that gets created and closed.
        let page = run_page_name(opts, &self.page_name);
        let activate = opts.foreground;

        // First attempt, then the same stale-page recovery the CLI runner already had. The native
        // path never got it, and the pool made it matter: more Chromes means more cached tabs, and
        // a tab closed since chrome-agent recorded it is otherwise a permanent task failure.
        let outcome = self
            .attached(&connect, &browser_name, &page, src, vars.clone(), activate)
            .await;
        if is_stale_outcome(&outcome) {
            pacewright_core::browser::prune_stale_page(&browser_name, &page);
            return self
                .attached(&connect, &browser_name, &page, src, vars, activate)
                .await;
        }
        outcome
    }
}

impl NativeRecipeRunner {
    /// One attach-and-run on a dedicated current-thread runtime.
    ///
    /// The recipe engine holds `Rc<CdpClient>`, so its future is `!Send` and cannot run on
    /// pacewright's spawned task: it is isolated on its own thread + `LocalSet` and only the Send
    /// result crosses back over a oneshot.
    #[allow(clippy::too_many_arguments)]
    async fn attached(
        &self,
        connect: &str,
        browser_name: &str,
        page: &str,
        src: &str,
        vars: std::collections::BTreeMap<String, String>,
        activate: bool,
    ) -> Result<Value, AdapterError> {
        // Todo lo que cruza al worker es dueño de sus datos: el future del motor es `!Send` y
        // corre en otro hilo, así que no puede quedarse con préstamos de este.
        let (connect, browser_name, page, src) = (
            connect.to_string(),
            browser_name.to_string(),
            page.to_string(),
            src.to_string(),
        );
        let stealth = self.stealth;
        let timeout_secs = self.timeout_secs;
        let solver = self.solver.clone();
        let (tx, rx) = tokio::sync::oneshot::channel::<Result<Value, AdapterError>>();
        std::thread::spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    let _ = tx.send(Err(AdapterError::Terminal(format!("recipe runtime: {e}"))));
                    return;
                }
            };
            let local = tokio::task::LocalSet::new();
            let res = local.block_on(&rt, async move {
                // Coerce the Send+Sync Arc (needed to cross the thread) down to the bare
                // `&dyn Solver` the engine expects; dropping the auto-trait bounds is a valid unsize.
                let solver_ref: Option<&dyn pacewright_chrome::recipe::engine::Solver> =
                    match &solver {
                        Some(s) => Some(&**s),
                        None => None,
                    };
                let at = pacewright_chrome::api::RecipeAttach {
                    connect: &connect,
                    browser: &browser_name,
                    page: &page,
                    stealth,
                    timeout_secs,
                    activate,
                };
                match pacewright_chrome::api::run_recipe_attached_src(&at, &src, vars, solver_ref)
                    .await
                {
                    Ok(o) => {
                        let mut env = serde_json::json!({
                            "ok": true, "result": o.result, "unexpected": o.unexpected,
                        });
                        if !o.downloads.is_empty() {
                            env["downloads"] = downloads_json(&o.downloads);
                        }
                        Ok(env)
                    }
                    // The class comes from the `[Class]` tag as before; the page state captured
                    // at the failure rides along so the engine can persist it in `task.result`.
                    Err(e) => Err(match e.failure {
                        Some(detail) => classify(&e.message).with_detail(detail),
                        None => classify(&e.message),
                    }),
                }
            });
            let _ = tx.send(res);
        });
        rx.await.map_err(|_| {
            AdapterError::Terminal("recipe worker thread ended without a result".into())
        })?
    }
}

/// Turn a finished `recipe run` into a result envelope or a classed error. Pure, so the
/// child-process contract is unit-tested without spawning anything.
pub fn interpret_output(success: bool, stdout: &str, stderr: &str) -> Result<Value, AdapterError> {
    match last_json(stdout) {
        Some(v) => {
            // chrome-agent signals recipe failure in-band as {"ok":false,"error":"[Class] …"}.
            if v.get("ok").and_then(Value::as_bool) == Some(false) {
                let msg = v
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("recipe run failed");
                // A child that also reports where it died (`"failure": {…}`) gets that persisted
                // exactly like the in-process runner's capture.
                Err(match v.get("failure") {
                    Some(detail) if !detail.is_null() => classify(msg).with_detail(detail.clone()),
                    _ => classify(msg),
                })
            } else {
                Ok(v)
            }
        }
        // No JSON at all: a crash/usage error before the envelope was printed.
        None => {
            let detail = first_nonempty(stderr, stdout);
            if success {
                Err(AdapterError::Terminal(format!(
                    "recipe run produced no JSON envelope: {detail}"
                )))
            } else {
                Err(AdapterError::Terminal(format!(
                    "recipe run failed: {detail}"
                )))
            }
        }
    }
}

/// Recover the pacewright error class from a chrome-agent recipe error string. The engine
/// formats `RecipeError` as `"[Terminal] …" / "[Retryable] …" / "[RateLimited] …"`, so the
/// leading `[Class]` tag is the wire contract.
///
/// `RateLimited` has no absolute defer time on the recipe side (the recipe format can't name
/// one), and adapters have no `Clock`, so it degrades to `Retryable` (the runner backs off).
/// pacewright's real rate-limiting is the config-driven limits engine (declared `limit-key`s),
/// not this signal.
pub fn classify(msg: &str) -> AdapterError {
    // A dead always-on Chrome is untagged (chrome-agent fails before the recipe engine can class
    // it), so it would land in the catch-all below as an opaque Terminal. Name it instead: it is
    // now the single most likely browser failure, since pacewright never launches a browser.
    if let Some(explained) = pacewright_core::browser::explain_connect_failure(msg) {
        return AdapterError::Terminal(explained);
    }
    if let Some(rest) = msg.strip_prefix("[Terminal]") {
        AdapterError::Terminal(rest.trim().to_string())
    } else if let Some(rest) = msg.strip_prefix("[Retryable]") {
        AdapterError::Retryable(rest.trim().to_string())
    } else if let Some(rest) = msg.strip_prefix("[RateLimited]") {
        AdapterError::Retryable(format!("rate-limited: {}", rest.trim()))
    } else {
        // No class tag → an unclassed failure (bad recipe, missing var, spawn issue): Terminal.
        AdapterError::Terminal(msg.to_string())
    }
}

/// Scan from the bottom for the last parseable top-level JSON object. chrome-agent prints
/// info lines (e.g. "Copied cookies …") before its JSON.
fn last_json(s: &str) -> Option<Value> {
    for line in s.lines().rev() {
        let t = line.trim();
        if t.starts_with('{') {
            if let Ok(v) = serde_json::from_str::<Value>(t) {
                return Some(v);
            }
        }
    }
    None
}

fn first_nonempty<'a>(a: &'a str, b: &'a str) -> &'a str {
    if a.trim().is_empty() {
        b.trim()
    } else {
        a.trim()
    }
}

#[cfg(test)]
pub mod fake {
    use super::{AdapterError, Path, RecipeRunner, RunOpts, Value};
    use async_trait::async_trait;
    use std::sync::Mutex;

    type Responder = Box<dyn Fn(&Path, &str) -> Result<Value, AdapterError> + Send + Sync>;

    /// One recorded `run` call: (recipe path, vars_json, opts). A `run_src` call records the
    /// pseudo-path [`SRC_PATH`] here and its source in `src_calls`.
    pub type Call = (String, String, RunOpts);

    /// The path a `run_src` call is recorded under (and handed to the responder).
    pub const SRC_PATH: &str = "<src>";

    /// A scriptable `RecipeRunner` for adapter tests. Records (path, vars_json, opts) calls
    /// and returns whatever the injected closure produces.
    pub struct FakeRecipeRunner {
        responder: Responder,
        pub calls: Mutex<Vec<Call>>,
        /// The recipe sources handed to `run_src`, in order.
        pub src_calls: Mutex<Vec<String>>,
    }

    impl FakeRecipeRunner {
        pub fn new(
            f: impl Fn(&Path, &str) -> Result<Value, AdapterError> + Send + Sync + 'static,
        ) -> Self {
            Self {
                responder: Box::new(f),
                calls: Mutex::new(Vec::new()),
                src_calls: Mutex::new(Vec::new()),
            }
        }
        /// Always succeed with the given envelope.
        pub fn ok(envelope: Value) -> Self {
            Self::new(move |_, _| Ok(envelope.clone()))
        }
        pub fn call_count(&self) -> usize {
            self.calls.lock().unwrap().len()
        }
    }

    #[async_trait]
    impl RecipeRunner for FakeRecipeRunner {
        async fn run(
            &self,
            recipe_path: &Path,
            vars_json: &str,
            opts: &RunOpts,
        ) -> Result<Value, AdapterError> {
            self.calls.lock().unwrap().push((
                recipe_path.to_string_lossy().into_owned(),
                vars_json.to_string(),
                opts.clone(),
            ));
            (self.responder)(recipe_path, vars_json)
        }

        async fn run_src(
            &self,
            src: &str,
            vars_json: &str,
            opts: &RunOpts,
        ) -> Result<Value, AdapterError> {
            self.src_calls.lock().unwrap().push(src.to_string());
            self.calls.lock().unwrap().push((
                SRC_PATH.to_string(),
                vars_json.to_string(),
                opts.clone(),
            ));
            (self.responder)(Path::new(SRC_PATH), vars_json)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn store_with_token() -> SecretStore {
        let mut s = SecretStore::default();
        s.set_app("acme", "cid", "csec");
        // valid for a long time from now_ms=1000 below
        s.set_tokens("acme", "TOK", None, 10_000_000_000, None)
            .unwrap();
        s.set_author_urn("acme", "urn:li:person:ME").unwrap();
        s
    }

    const POST_SRC: &str = r#"recipe "acme/post_with_mentions" {
        var "token" required=#true
        var "author_urn" required=#true
        var "commentary" required=#true
    }"#;

    #[test]
    fn recipe_provider_extracts_prefix() {
        assert_eq!(recipe_provider(POST_SRC).as_deref(), Some("acme"));
        assert_eq!(
            recipe_provider(r#"recipe "globex/list_projects" {}"#).as_deref(),
            Some("globex")
        );
    }

    #[test]
    fn inject_adds_token_and_author_urn() {
        let out = inject_oauth_vars(
            POST_SRC,
            r#"{"commentary":"hi"}"#,
            &store_with_token(),
            1000,
        );
        let obj: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(obj["token"], "TOK");
        assert_eq!(obj["author_urn"], "urn:li:person:ME");
        assert_eq!(obj["commentary"], "hi");
    }

    #[test]
    fn inject_is_noop_when_recipe_declares_no_token() {
        let src = r#"recipe "globex/list_projects" { var "production_id" }"#;
        let vars = r#"{"production_id":"x"}"#;
        assert_eq!(
            inject_oauth_vars(src, vars, &store_with_token(), 1000),
            vars
        );
    }

    #[test]
    fn inject_does_not_overwrite_explicit_token() {
        let out = inject_oauth_vars(
            POST_SRC,
            r#"{"token":"EXPLICIT"}"#,
            &store_with_token(),
            1000,
        );
        let obj: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(obj["token"], "EXPLICIT");
    }

    #[test]
    fn inject_noop_when_no_valid_token() {
        // Empty store → no token for acme → vars unchanged (recipe fails on missing var later).
        let vars = r#"{"commentary":"hi"}"#;
        assert_eq!(
            inject_oauth_vars(POST_SRC, vars, &SecretStore::default(), 1000),
            vars
        );
    }

    #[test]
    fn ok_envelope_passes_through() {
        let out = "Copied cookies from Chrome\n{\"ok\":true,\"result\":{\"stories\":[\"a\"]},\"unexpected\":[]}\n";
        let v = interpret_output(true, out, "").unwrap();
        assert_eq!(v["result"]["stories"][0], "a");
    }

    #[test]
    fn in_band_failure_recovers_the_class() {
        let out = "{\"ok\":false,\"error\":\"[Retryable] locator not found within 10000ms\"}";
        let err = interpret_output(false, out, "").unwrap_err();
        assert!(
            matches!(err, AdapterError::Retryable(ref m) if m.contains("locator not found")),
            "got {err:?}"
        );
    }

    #[test]
    fn classifies_each_tag() {
        assert!(
            matches!(classify("[Terminal] bad var"), AdapterError::Terminal(m) if m == "bad var")
        );
        assert!(matches!(classify("[Retryable] down"), AdapterError::Retryable(m) if m == "down"));
        // rate-limited degrades to retryable (no absolute defer time available)
        assert!(
            matches!(classify("[RateLimited] slow down"), AdapterError::Retryable(m) if m.contains("slow down"))
        );
        // untagged → terminal
        assert!(matches!(classify("kaboom"), AdapterError::Terminal(m) if m == "kaboom"));
    }

    #[test]
    fn in_band_failure_object_becomes_the_error_detail() {
        let out = json!({"ok": false, "error": "[Terminal] auth wall", "failure": {"step_index": 1, "url": "https://x/login"}}).to_string();
        let err = interpret_output(false, &out, "").unwrap_err();
        assert!(
            matches!(err, AdapterError::TerminalWith { ref message, .. } if message == "auth wall"),
            "got {err:?}"
        );
        assert_eq!(err.detail().unwrap()["step_index"], 1);
    }

    #[test]
    fn downloads_json_inlines_small_files_and_reports_missing_ones() {
        use pacewright_chrome::recipe::engine::Download;
        let dir =
            std::env::temp_dir().join(format!("pcw-dl-{}-{}", std::process::id(), uuid_like()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("factura.xml");
        std::fs::write(&f, b"<cfdi/>").unwrap();
        let v = downloads_json(&[
            Download {
                key: "xml".into(),
                path: f.to_string_lossy().into_owned(),
            },
            Download {
                key: "gone".into(),
                path: dir.join("nope.pdf").to_string_lossy().into_owned(),
            },
        ]);
        assert_eq!(v["xml"]["size"], 7);
        assert_eq!(v["xml"]["base64"], "PGNmZGkvPg==");
        assert!(v["gone"]["error"].as_str().unwrap().contains("stat"));
        assert!(v["gone"].get("base64").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn default_run_src_spools_to_a_file_and_runs_it() {
        // The trait default serves path-only runners: the source lands in a temp file that is
        // gone again after the run.
        struct PathOnly(Mutex<Option<String>>);
        #[async_trait]
        impl RecipeRunner for PathOnly {
            async fn run(
                &self,
                recipe_path: &Path,
                _vars_json: &str,
                _opts: &RunOpts,
            ) -> Result<Value, AdapterError> {
                *self.0.lock().unwrap() = Some(std::fs::read_to_string(recipe_path).unwrap());
                Ok(json!({"ok": true, "path": recipe_path.to_string_lossy()}))
            }
        }
        use std::sync::Mutex;
        let r = PathOnly(Mutex::new(None));
        let v = r
            .run_src("recipe \"a/b\" {}", "{}", &RunOpts::default())
            .await
            .unwrap();
        assert_eq!(r.0.lock().unwrap().as_deref(), Some("recipe \"a/b\" {}"));
        assert!(
            !Path::new(v["path"].as_str().unwrap()).exists(),
            "spool file removed"
        );
    }

    #[test]
    fn no_json_is_terminal() {
        let err = interpret_output(false, "usage: chrome-agent …", "some stderr").unwrap_err();
        assert!(
            matches!(err, AdapterError::Terminal(ref m) if m.contains("some stderr")),
            "got {err:?}"
        );
    }

    #[test]
    fn cli_runner_builds_the_pinned_invocation() {
        let r = CliRecipeRunner::new().timeout_secs(90);
        // a public recipe → a per-run tab named after the base page, no account tab.
        let args = r.args(
            Path::new("/r/hn.kdl"),
            r#"{"url":"u"}"#,
            &r.run_page(&RunOpts::default()),
            &RunOpts::default(),
        );
        // pinned browser+page, stealth, then the subcommand + vars-json
        let find = |f: &str| args.iter().position(|a| a == f).expect("flag present");
        assert_eq!(args[find("--browser") + 1], "pacewright");
        assert!(
            args[find("--page") + 1].starts_with("pacewright-"),
            "per-run tab derives from the base page: {args:?}"
        );
        assert!(args.contains(&"--stealth".to_string()));
        assert!(!args.contains(&"--copy-cookies".to_string()));
        assert!(args.contains(&"recipe".to_string()) && args.contains(&"run".to_string()));
        assert_eq!(args[find("--vars-json") + 1], r#"{"url":"u"}"#);
        // global flags precede the subcommand
        assert!(find("--browser") < find("recipe"));
    }

    #[test]
    fn recipe_preflight_is_false_for_a_missing_binary() {
        assert!(!recipe_subcommand_available(
            "chrome-agent-definitely-not-installed-xyz"
        ));
    }

    #[test]
    fn cli_runner_attaches_and_never_launches() {
        // The whole point of the refactor: pacewright ATTACHES to the operator's real, always-on
        // Chrome instead of letting chrome-agent launch one. A launched browser is a bot signal —
        // acme walls the profile and revokes `li_at`; Google refuses sign-in outright.
        let r = CliRecipeRunner::new().connect("http://127.0.0.1:9222");
        let args = r.args(
            Path::new("/r/li.kdl"),
            "{}",
            &r.run_page(&RunOpts::account("acme-account")),
            &RunOpts::account("acme-account"),
        );
        let find = |f: &str| {
            args.iter()
                .position(|a| a == f)
                .unwrap_or_else(|| panic!("{f} absent: {args:?}"))
        };
        assert_eq!(args[find("--connect") + 1], "http://127.0.0.1:9222");
        // --connect is a global flag → must precede the `recipe` subcommand.
        assert!(find("--connect") < find("recipe"));
    }

    #[test]
    fn cli_runner_attached_drops_headed_and_copy_cookies() {
        // Both flags existed only to make a launched throwaway Chromium resemble a real signed-in
        // Chrome. Attached, the profile IS the session: there is nothing to copy, and the real
        // Chrome is visible by definition. Passing either is now meaningless at best.
        let r = CliRecipeRunner::new().connect("http://127.0.0.1:9222");
        for opts in [RunOpts::account("acme-account"), RunOpts::default()] {
            let args = r.args(Path::new("/r/x.kdl"), "{}", &r.run_page(&opts), &opts);
            assert!(
                !args.contains(&"--headed".to_string()),
                "attached must not pass --headed: {args:?}"
            );
            assert!(
                !args.contains(&"--copy-cookies".to_string()),
                "attached has nothing to copy: {args:?}"
            );
        }
    }

    #[test]
    fn cli_runner_maps_account_to_page_not_browser() {
        // The core inversion. Was: account → its own launched browser profile. Now: one shared
        // attached browser, and the account picks a named TAB inside it. Verified live on
        // 2026-07-16 — three named pages coexist as three real tabs and do not clobber each other.
        let r = CliRecipeRunner::new().connect("http://127.0.0.1:9222");
        let li = r.args(
            Path::new("/r/li.kdl"),
            "{}",
            &r.run_page(&RunOpts::account("acme-account")),
            &RunOpts::account("acme-account"),
        );
        let rv = r.args(
            Path::new("/r/rv.kdl"),
            "{}",
            &r.run_page(&RunOpts::account("globex-account")),
            &RunOpts::account("globex-account"),
        );
        let page = |a: &Vec<String>| a[a.iter().position(|x| x == "--page").unwrap() + 1].clone();
        let browser =
            |a: &Vec<String>| a[a.iter().position(|x| x == "--browser").unwrap() + 1].clone();
        assert_eq!(page(&li), "acme-account");
        assert_eq!(page(&rv), "globex-account");
        assert_ne!(page(&li), page(&rv), "each account gets its own tab");
        // ...but they share ONE browser now.
        assert_eq!(
            browser(&li),
            browser(&rv),
            "one attached Chrome for every account"
        );
        // A public/shared recipe gets a tab of its own per run — created for the run, closed
        // after it — named after the runner's default page, never the bare shared name.
        let pubrec = r.args(
            Path::new("/r/hn.kdl"),
            "{}",
            &r.run_page(&RunOpts::default()),
            &RunOpts::default(),
        );
        assert!(page(&pubrec).starts_with("pacewright-"), "{pubrec:?}");
        let again = r.run_page(&RunOpts::default());
        assert_ne!(page(&pubrec), again, "every run gets a fresh tab name");
    }

    #[test]
    fn cli_runner_activates_only_foreground_recipes() {
        // One attached Chrome has exactly ONE foreground tab. Chrome throttles the rest, which is
        // what stalls a heavy render — so a recipe that needs to be watched must raise its tab.
        let r = CliRecipeRunner::new().connect("http://127.0.0.1:9222");
        let fg = r.args(
            Path::new("/r/rv.kdl"),
            "{}",
            &r.run_page(&RunOpts::account("globex-account").foreground(true)),
            &RunOpts::account("globex-account").foreground(true),
        );
        assert!(
            fg.contains(&"--activate".to_string()),
            "foreground recipe must raise its tab: {fg:?}"
        );
        // --activate is a global flag → must precede the `recipe` subcommand.
        let apos = fg.iter().position(|a| a == "--activate").unwrap();
        let rpos = fg.iter().position(|a| a == "recipe").unwrap();
        assert!(
            apos < rpos,
            "--activate must precede the subcommand: {fg:?}"
        );

        // Default off: raising a window steals focus on the operator's real Mac, and most recipes
        // (scrapes, API polls) have no reason to.
        let bg = r.args(
            Path::new("/r/li.kdl"),
            "{}",
            &r.run_page(&RunOpts::account("acme-account")),
            &RunOpts::account("acme-account"),
        );
        assert!(
            !bg.contains(&"--activate".to_string()),
            "background recipe must not steal focus: {bg:?}"
        );
    }

    #[test]
    fn cli_runner_pins_a_named_browser_even_when_attached() {
        // Under --connect the endpoint identifies the browser and `--browser` is only the
        // sessions.json bookkeeping key — but it MUST still be pinned. Observed 2026-07-16:
        // omitting it filed our pages under the browser key `default`, which is exactly the
        // shared-`default` hijack another chrome-agent consumer can walk into.
        let r = CliRecipeRunner::new().connect("http://127.0.0.1:9222");
        let args = r.args(
            Path::new("/r/li.kdl"),
            "{}",
            &r.run_page(&RunOpts::account("acme-account")),
            &RunOpts::account("acme-account"),
        );
        let browser = &args[args.iter().position(|a| a == "--browser").unwrap() + 1];
        assert_eq!(browser, "pacewright");
        assert_ne!(browser, "default", "never the shared default browser key");
    }

    #[tokio::test]
    async fn missing_binary_is_terminal() {
        let r = CliRecipeRunner::new().bin("definitely-not-real-xyz");
        let err = r
            .run(Path::new("/r/x.kdl"), "{}", &RunOpts::default())
            .await
            .unwrap_err();
        assert!(matches!(err, AdapterError::Terminal(_)), "got {err:?}");
    }

    #[test]
    fn unexpected_marker_is_still_ok() {
        // A cardinality miss is `unexpected`, not a failure — the envelope stays ok:true.
        let out = json!({"ok": true, "result": {"stories": []}, "unexpected": ["stories: expected ≥1, got 0"]}).to_string();
        let v = interpret_output(true, &out, "").unwrap();
        assert_eq!(v["unexpected"][0], "stories: expected ≥1, got 0");
    }
}

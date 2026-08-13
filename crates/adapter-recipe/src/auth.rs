//! The auth subsystem — establishing and inspecting the logged-in session each account recipe
//! needs. It owns the per-account **status cache** and orchestrates **interactive login**.
//!
//! Sessions live in **persistent per-account browser profiles** (named after the account), so an
//! authed recipe runs in the same profile the human logged into — no cookie snapshot, no staleness.
//!
//! - **Check** (`recheck`): run the account recipe (whose steps ARE the signed-in check) headless in
//!   the account profile. `Ok` → signed in; a `Terminal` failure (the recipe's `expect on-fail`) →
//!   signed out; a `Retryable`/launch error → unknown (we couldn't tell).
//! - **Login** (`login`): open a **headed** browser in the account profile at the recipe's
//!   `login-url` so the operator signs in by hand, then poll the check until it passes.
//!
//! Lives in `adapter-recipe` (not `core`) because it needs the `RecipeRegistry` + `RecipeRunner`;
//! the daemon wires it behind the `Auth*` RPCs, mirroring how it wires `schedule`.

use crate::registry::RecipeRegistry;
use crate::runner::{default_connect_endpoint, RecipeRunner, RunOpts, DEFAULT_CHROME_CONNECT};
use async_trait::async_trait;
use pacewright_core::model::AdapterError;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Opens a headed browser for interactive login. The seam is a trait so the daemon drives real
/// Chrome via chrome-agent while tests use a fake.
#[async_trait]
pub trait LoginLauncher: Send + Sync {
    /// Open a headed browser in the persistent profile `account` at `login_url` for the human to
    /// sign in. Returns once the window is open — it does NOT wait for the login to complete.
    async fn open(&self, account: &str, login_url: &str) -> Result<(), String>;
}

/// The real launcher:
/// `chrome-agent --connect <endpoint> --browser pacewright --page <account> --activate goto <url>`.
///
/// Despite the name, it no longer *launches* anything — it attaches to the operator's always-on
/// Chrome and raises the account's tab. That distinction is the whole fix: a chrome-agent-launched
/// browser is a bot signal, so the old launcher burnt the very session it was trying to establish
/// (acme walls the profile and revokes `li_at`) and Google refused sign-in on it outright.
pub struct CliLoginLauncher {
    bin: String,
    connect: String,
    browser_name: String,
}

impl Default for CliLoginLauncher {
    fn default() -> Self {
        Self::new()
    }
}

impl CliLoginLauncher {
    pub fn new() -> Self {
        Self {
            bin: std::env::var("CHROME_AGENT_BIN").unwrap_or_else(|_| "chrome-agent".to_string()),
            connect: default_connect_endpoint()
                .unwrap_or_else(|| DEFAULT_CHROME_CONNECT.to_string()),
            browser_name: "pacewright".to_string(),
        }
    }
    pub fn bin(mut self, bin: impl Into<String>) -> Self {
        self.bin = bin.into();
        self
    }
    pub fn connect(mut self, endpoint: impl Into<String>) -> Self {
        self.connect = endpoint.into();
        self
    }

    /// The chrome-agent invocation for a sign-in tab. The page is the **account's own tab** — the
    /// same one the account recipe's check/run reuse (`CliRecipeRunner::args`), so login, sign-in,
    /// and check all share one tab and the check reads the very page the human logged into.
    /// `--activate` raises it (a background tab would navigate invisibly — "nothing opened").
    /// No `--headed`: the attached Chrome is visible by definition.
    fn login_args<'a>(&'a self, account: &'a str, login_url: &'a str) -> Vec<&'a str> {
        vec![
            "--connect",
            &self.connect,
            "--browser",
            &self.browser_name,
            "--page",
            account,
            "--activate",
            "--stealth",
            "goto",
            login_url,
        ]
    }
}

#[async_trait]
impl LoginLauncher for CliLoginLauncher {
    async fn open(&self, account: &str, login_url: &str) -> Result<(), String> {
        let status = tokio::process::Command::new(&self.bin)
            .args(self.login_args(account, login_url))
            .status()
            .await
            .map_err(|e| format!("cannot spawn `{}`: {e}", self.bin))?;
        if status.success() {
            Ok(())
        } else {
            Err(format!("chrome-agent goto exited with {status}"))
        }
    }
}

/// In-process login launcher: links the vendored chrome-agent and attaches directly, so `pcw auth
/// login` needs no `chrome-agent` binary. Attaches to the always-on Chrome, navigates the account's
/// tab to the login URL, and raises the window. Same !Send bridge as [`NativeRecipeRunner`].
pub struct NativeLoginLauncher {
    connect: String,
    browser_name: String,
    stealth: bool,
}

impl Default for NativeLoginLauncher {
    fn default() -> Self {
        Self::new()
    }
}

impl NativeLoginLauncher {
    pub fn new() -> Self {
        Self {
            connect: default_connect_endpoint().unwrap_or_else(|| DEFAULT_CHROME_CONNECT.to_string()),
            browser_name: "pacewright".to_string(),
            stealth: true,
        }
    }
    pub fn connect(mut self, endpoint: impl Into<String>) -> Self {
        self.connect = endpoint.into();
        self
    }
}

#[async_trait]
impl LoginLauncher for NativeLoginLauncher {
    async fn open(&self, account: &str, login_url: &str) -> Result<(), String> {
        let connect = self.connect.clone();
        let browser = self.browser_name.clone();
        let page = account.to_string();
        let url = login_url.to_string();
        let stealth = self.stealth;
        let (tx, rx) = tokio::sync::oneshot::channel::<Result<(), String>>();
        std::thread::spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                Ok(rt) => rt,
                Err(e) => {
                    let _ = tx.send(Err(format!("login runtime: {e}")));
                    return;
                }
            };
            let local = tokio::task::LocalSet::new();
            let res = local.block_on(&rt, async move {
                let at = pacewright_chrome::api::RecipeAttach {
                    connect: &connect,
                    browser: &browser,
                    page: &page,
                    stealth,
                    timeout_secs: 30,
                    activate: true,
                };
                pacewright_chrome::api::open_page(&at, &url)
                    .await
                    .map_err(|e| e.to_string())
            });
            let _ = tx.send(res);
        });
        rx.await
            .map_err(|_| "login worker thread ended without a result".to_string())?
    }
}

/// Cached signed-in state for one account. `signed_in = None` = unknown (never checked, or the check
/// was unreachable). `logging_in` = an interactive login is in flight.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AccountStatus {
    pub signed_in: Option<bool>,
    pub last_checked_ms: Option<i64>,
    pub logging_in: bool,
}

/// One row of the auth catalog: an account, where to log in, the recipes that use it, and its
/// cached status.
#[derive(Clone, Debug)]
pub struct AccountInfo {
    pub account: String,
    pub login_url: Option<String>,
    pub recipes: Vec<String>,
    pub status: AccountStatus,
}

/// Owns the status cache and login orchestration for every account recipe in the registry.
pub struct AuthManager {
    registry: Arc<RecipeRegistry>,
    runner: Arc<dyn RecipeRunner>,
    launcher: Arc<dyn LoginLauncher>,
    status: Mutex<HashMap<String, AccountStatus>>,
}

impl AuthManager {
    pub fn new(
        registry: Arc<RecipeRegistry>,
        runner: Arc<dyn RecipeRunner>,
        launcher: Arc<dyn LoginLauncher>,
    ) -> Self {
        Self {
            registry,
            runner,
            launcher,
            status: Mutex::new(HashMap::new()),
        }
    }

    /// The account catalog with cached status — the read side of `AuthList`. No checks are run here
    /// (status comes from the cache), so it's cheap enough to include in the 1 s web snapshot.
    pub fn list(&self) -> Vec<AccountInfo> {
        let cache = self.status.lock().unwrap();
        self.registry
            .accounts()
            .into_iter()
            .map(|acct| {
                let name = acct.action.clone();
                let recipes = self
                    .registry
                    .recipes_for_account(&name)
                    .into_iter()
                    .map(|m| m.name.clone())
                    .collect();
                AccountInfo {
                    login_url: acct.login_url.clone(),
                    recipes,
                    status: cache.get(&name).cloned().unwrap_or_default(),
                    account: name,
                }
            })
            .collect()
    }

    fn set(&self, account: &str, f: impl FnOnce(&mut AccountStatus)) {
        let mut cache = self.status.lock().unwrap();
        f(cache.entry(account.to_string()).or_default());
    }

    /// Run the account recipe (its steps are the signed-in check) headless in the account profile,
    /// map the outcome to `signed_in`, and cache it. `now_ms` stamps `last_checked_ms`.
    pub async fn recheck(&self, account: &str, now_ms: i64) -> Result<AccountStatus, String> {
        let Some(meta) = self.registry.account(account) else {
            return Err(format!("no account recipe `accounts/{account}` installed"));
        };
        let path = meta.path.clone();
        // The signed-in check runs in the account's own tab, in the background: it must not steal
        // focus from whatever the operator is doing, and reading a session needs no foreground.
        let outcome = self
            .runner
            .run(&path, "{}", &RunOpts::account(account))
            .await;
        let signed_in = match outcome {
            Ok(_) => Some(true),
            // The recipe's `expect on-fail="terminal"` fires when signed out.
            Err(AdapterError::Terminal(_)) => Some(false),
            // Launch/navigation hiccup — we genuinely couldn't tell.
            Err(AdapterError::Retryable(_) | AdapterError::RateLimited { .. }) => None,
        };
        self.set(account, |s| {
            s.signed_in = signed_in;
            s.last_checked_ms = Some(now_ms);
            // A recheck concludes any in-flight login: read the session, drop "logging in…".
            s.logging_in = false;
        });
        Ok(self
            .status
            .lock()
            .unwrap()
            .get(account)
            .cloned()
            .unwrap_or_default())
    }

    /// Open a headed login window for `account` and mark it `logging_in`. Does not block on the
    /// human and, deliberately, does NOT start any automated recheck: driving the browser while the
    /// operator signs in would navigate the very profile being logged into (spawning tabs, breaking
    /// bot-sensitive sign-ins like acme). The operator triggers `recheck` when done, which reads
    /// the session and clears `logging_in`.
    pub async fn login(&self, account: &str) -> Result<(), String> {
        let Some(meta) = self.registry.account(account) else {
            return Err(format!("no account recipe `accounts/{account}` installed"));
        };
        // Open the account's HOME (its check's landing page), not the raw login form: signed in →
        // the operator sees the app; signed out → the app redirects them to sign in. Fall back to
        // `login-url` only when the recipe declares no `goto` home.
        let Some(url) = meta.home_url.clone().or_else(|| meta.login_url.clone()) else {
            return Err(format!(
                "account `{account}` declares no home (`goto`) or `login-url`"
            ));
        };
        self.launcher.open(account, &url).await?;
        self.set(account, |s| s.logging_in = true);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::fake::FakeRecipeRunner;
    use serde_json::json;
    use std::path::Path;

    struct FakeLauncher {
        pub opened: Mutex<Vec<(String, String)>>,
    }
    #[async_trait]
    impl LoginLauncher for FakeLauncher {
        async fn open(&self, account: &str, login_url: &str) -> Result<(), String> {
            self.opened
                .lock()
                .unwrap()
                .push((account.into(), login_url.into()));
            Ok(())
        }
    }

    fn registry_with_account() -> Arc<RecipeRegistry> {
        let dir = std::env::temp_dir().join(format!(
            "pcw-auth-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(dir.join("accounts")).unwrap();
        std::fs::write(
            dir.join("accounts/globex-account.kdl"),
            "recipe \"accounts/globex-account\" { login-url \"https://globex.example/login\"\n step { goto \"https://globex.example/dashboard\" } }",
        )
        .unwrap();
        std::fs::write(
            dir.join("rv.kdl"),
            "recipe \"globex/generate_clips\" { auth account=\"globex-account\"\n var \"project_id\" required=#true }",
        )
        .unwrap();
        Arc::new(RecipeRegistry::load_dir(&dir))
    }

    fn mgr(runner: Arc<dyn RecipeRunner>) -> (Arc<AuthManager>, Arc<FakeLauncher>) {
        let launcher = Arc::new(FakeLauncher {
            opened: Mutex::new(vec![]),
        });
        let m = Arc::new(AuthManager::new(
            registry_with_account(),
            runner,
            launcher.clone(),
        ));
        (m, launcher)
    }

    #[tokio::test]
    async fn list_reports_accounts_recipes_and_unknown_before_any_check() {
        let (m, _) = mgr(Arc::new(FakeRecipeRunner::ok(json!({"ok": true}))));
        let list = m.list();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].account, "globex-account");
        assert_eq!(
            list[0].login_url.as_deref(),
            Some("https://globex.example/login")
        );
        assert_eq!(list[0].recipes, vec!["globex/generate_clips".to_string()]);
        assert_eq!(list[0].status.signed_in, None, "unknown before any check");
    }

    #[tokio::test]
    async fn recheck_ok_run_is_signed_in() {
        let runner = Arc::new(FakeRecipeRunner::new(|p: &Path, _| {
            // the check runs the account recipe in its own profile
            assert!(p.to_string_lossy().contains("globex-account"));
            Ok(json!({"ok": true, "unexpected": []}))
        }));
        let (m, _) = mgr(runner);
        let st = m.recheck("globex-account", 1000).await.unwrap();
        assert_eq!(st.signed_in, Some(true));
        assert_eq!(st.last_checked_ms, Some(1000));
        assert_eq!(m.list()[0].status.signed_in, Some(true));
    }

    #[tokio::test]
    async fn recheck_terminal_failure_is_signed_out_retryable_is_unknown() {
        let signed_out = Arc::new(FakeRecipeRunner::new(|_: &Path, _| {
            Err(AdapterError::Terminal("signed out".into()))
        }));
        let (m, _) = mgr(signed_out);
        assert_eq!(
            m.recheck("globex-account", 5).await.unwrap().signed_in,
            Some(false)
        );

        let flaky = Arc::new(FakeRecipeRunner::new(|_: &Path, _| {
            Err(AdapterError::Retryable("nav timeout".into()))
        }));
        let (m2, _) = mgr(flaky);
        assert_eq!(
            m2.recheck("globex-account", 6).await.unwrap().signed_in,
            None
        );
    }

    #[tokio::test]
    async fn login_opens_home_not_the_login_form_and_marks_logging_in() {
        // Login opens the account's HOME (the check's `goto` target), not the raw login form: a
        // signed-in operator lands on the app and sees they're in; signed out, the app bounces them
        // to sign in. Opening `/login` unconditionally shows a login form even when already signed in.
        let (m, launcher) = mgr(Arc::new(FakeRecipeRunner::ok(json!({"ok": true}))));
        m.login("globex-account").await.unwrap();
        let opened = launcher.opened.lock().unwrap();
        assert_eq!(
            opened[0],
            (
                "globex-account".to_string(),
                "https://globex.example/dashboard".to_string()
            )
        );
        assert!(m.list()[0].status.logging_in);
    }

    #[tokio::test]
    async fn recheck_clears_logging_in() {
        // A recheck is a definitive status read — the login interaction has concluded, so the
        // account is no longer "logging in…". This is what flips a just-logged-in account green:
        // the human signs in undisturbed (no browser-driving poll fights the login window), then a
        // single `recheck` reads the session and clears the flag. Without this, a manual recheck
        // after login would report "signed in" yet still show "logging in…" forever.
        let (m, _) = mgr(Arc::new(FakeRecipeRunner::ok(json!({"ok": true}))));
        m.login("globex-account").await.unwrap();
        assert!(m.list()[0].status.logging_in, "login marks logging_in");
        let st = m.recheck("globex-account", 1000).await.unwrap();
        assert!(!st.logging_in, "recheck clears logging_in");
        assert_eq!(st.signed_in, Some(true));
    }

    #[test]
    fn login_attaches_and_never_launches_a_browser() {
        // THE FIX. `auth login` used to LAUNCH a headed browser per account, and launching is what
        // breaks sign-in: acme walls a CDP-launched profile and revokes `li_at` (so logging in
        // burnt the very session it was establishing), and Google refuses sign-in on one outright
        // — which is why `auth login` for such an account hung at "logging in…" forever.
        // Now it attaches to the always-on Chrome the human already uses and just brings the
        // account's tab forward for them to sign into.
        let l = CliLoginLauncher::new();
        let args = l.login_args("acme-account", "https://x/login");
        let pos = |f: &str| {
            args.iter()
                .position(|a| *a == f)
                .unwrap_or_else(|| panic!("{f} absent: {args:?}"))
        };
        assert_eq!(args[pos("--connect") + 1], DEFAULT_CHROME_CONNECT);
        assert!(
            !args.contains(&"--headed"),
            "attached Chrome is visible by definition: {args:?}"
        );
    }

    #[test]
    fn login_activates_the_accounts_own_tab() {
        // The window must be raised, or a background tab navigates invisibly and reads as "nothing
        // opened". The tab must be the account's own — the SAME one the signed-in check and every
        // authed recipe run reuse (see `CliRecipeRunner::args`), so the check reads the very page
        // the human logged into rather than spawning a second tab that navigates away.
        let l = CliLoginLauncher::new();
        let args = l.login_args("acme-account", "https://x/login");
        assert!(
            args.contains(&"--activate"),
            "must raise the window: {args:?}"
        );
        assert_eq!(
            args[args.iter().position(|a| *a == "--page").unwrap() + 1],
            "acme-account"
        );
        assert_eq!(args.last(), Some(&"https://x/login"));
        // and each account lands on its own tab, not a shared `main`
        let l2 = CliLoginLauncher::new();
        let rv = l2.login_args("globex-account", "https://y/login");
        assert_eq!(
            rv[rv.iter().position(|a| *a == "--page").unwrap() + 1],
            "globex-account"
        );
    }

    #[tokio::test]
    async fn login_unknown_account_errors() {
        let (m, _) = mgr(Arc::new(FakeRecipeRunner::ok(json!({"ok": true}))));
        assert!(m.login("nope").await.is_err());
        assert!(m.recheck("nope", 1).await.is_err());
    }
}

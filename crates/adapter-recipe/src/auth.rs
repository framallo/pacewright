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
use crate::runner::RecipeRunner;
use async_trait::async_trait;
use pacewright_core::model::AdapterError;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Opens a headed browser for interactive login. The seam is a trait so the daemon drives real
/// Chrome via chrome-agent while tests use a fake.
#[async_trait]
pub trait LoginLauncher: Send + Sync {
    /// Open a headed browser in the persistent profile `account` at `login_url` for the human to
    /// sign in. Returns once the window is open — it does NOT wait for the login to complete.
    async fn open(&self, account: &str, login_url: &str) -> Result<(), String>;
}

/// The real launcher: `chrome-agent --browser <account> --page main --headed goto <login_url>`.
/// The named browser persists, so the window stays open for the operator and the recipe's later
/// checks/runs reuse the same profile + session.
pub struct CliLoginLauncher {
    bin: String,
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
        }
    }
    pub fn bin(mut self, bin: impl Into<String>) -> Self {
        self.bin = bin.into();
        self
    }
}

#[async_trait]
impl LoginLauncher for CliLoginLauncher {
    async fn open(&self, account: &str, login_url: &str) -> Result<(), String> {
        let status = tokio::process::Command::new(&self.bin)
            .args([
                "--browser",
                account,
                "--page",
                "main",
                "--headed",
                "--stealth",
                "goto",
                login_url,
            ])
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
        let outcome = self.runner.run(&path, "{}", true, Some(account)).await;
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
        });
        Ok(self.status.lock().unwrap().get(account).cloned().unwrap_or_default())
    }

    /// Open a headed login window for `account` and mark it `logging_in`. Does not block on the
    /// human; callers spawn `poll_until_signed_in` (or the operator triggers `recheck`) to confirm.
    pub async fn login(&self, account: &str) -> Result<(), String> {
        let Some(meta) = self.registry.account(account) else {
            return Err(format!("no account recipe `accounts/{account}` installed"));
        };
        let Some(url) = meta.login_url.clone() else {
            return Err(format!("account `{account}` declares no `login-url`"));
        };
        self.launcher.open(account, &url).await?;
        self.set(account, |s| s.logging_in = true);
        Ok(())
    }

    /// Bounded poll after a login: recheck every `every` until signed in or `deadline` rechecks pass,
    /// then clear `logging_in`. Uses injected `now` for the `last_checked_ms` stamp so it stays
    /// wall-clock-free where it matters. Spawned detached by the daemon.
    pub async fn poll_until_signed_in(
        self: Arc<Self>,
        account: String,
        every: Duration,
        max_rechecks: u32,
        now: Arc<dyn Fn() -> i64 + Send + Sync>,
    ) {
        for _ in 0..max_rechecks {
            tokio::time::sleep(every).await;
            if let Ok(st) = self.recheck(&account, now()).await {
                if st.signed_in == Some(true) {
                    break;
                }
            }
        }
        self.set(&account, |s| s.logging_in = false);
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
            self.opened.lock().unwrap().push((account.into(), login_url.into()));
            Ok(())
        }
    }

    fn registry_with_account() -> Arc<RecipeRegistry> {
        let dir = std::env::temp_dir().join(format!(
            "pcw-auth-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir_all(dir.join("accounts")).unwrap();
        std::fs::write(
            dir.join("accounts/prevetted-riverside.kdl"),
            "recipe \"accounts/prevetted-riverside\" { login-url \"https://riverside.com/login\"\n step { goto \"https://riverside.com/dashboard\" } }",
        )
        .unwrap();
        std::fs::write(
            dir.join("rv.kdl"),
            "recipe \"riverside/generate_magic_clips\" { auth account=\"prevetted-riverside\"\n var \"project_id\" required=#true }",
        )
        .unwrap();
        Arc::new(RecipeRegistry::load_dir(&dir))
    }

    fn mgr(runner: Arc<dyn RecipeRunner>) -> (Arc<AuthManager>, Arc<FakeLauncher>) {
        let launcher = Arc::new(FakeLauncher { opened: Mutex::new(vec![]) });
        let m = Arc::new(AuthManager::new(registry_with_account(), runner, launcher.clone()));
        (m, launcher)
    }

    #[tokio::test]
    async fn list_reports_accounts_recipes_and_unknown_before_any_check() {
        let (m, _) = mgr(Arc::new(FakeRecipeRunner::ok(json!({"ok": true}))));
        let list = m.list();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].account, "prevetted-riverside");
        assert_eq!(list[0].login_url.as_deref(), Some("https://riverside.com/login"));
        assert_eq!(list[0].recipes, vec!["riverside/generate_magic_clips".to_string()]);
        assert_eq!(list[0].status.signed_in, None, "unknown before any check");
    }

    #[tokio::test]
    async fn recheck_ok_run_is_signed_in() {
        let runner = Arc::new(FakeRecipeRunner::new(|p: &Path, _| {
            // the check runs the account recipe in its own profile
            assert!(p.to_string_lossy().contains("prevetted-riverside"));
            Ok(json!({"ok": true, "unexpected": []}))
        }));
        let (m, _) = mgr(runner);
        let st = m.recheck("prevetted-riverside", 1000).await.unwrap();
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
        assert_eq!(m.recheck("prevetted-riverside", 5).await.unwrap().signed_in, Some(false));

        let flaky = Arc::new(FakeRecipeRunner::new(|_: &Path, _| {
            Err(AdapterError::Retryable("nav timeout".into()))
        }));
        let (m2, _) = mgr(flaky);
        assert_eq!(m2.recheck("prevetted-riverside", 6).await.unwrap().signed_in, None);
    }

    #[tokio::test]
    async fn login_opens_the_window_and_marks_logging_in() {
        let (m, launcher) = mgr(Arc::new(FakeRecipeRunner::ok(json!({"ok": true}))));
        m.login("prevetted-riverside").await.unwrap();
        let opened = launcher.opened.lock().unwrap();
        assert_eq!(opened[0], ("prevetted-riverside".to_string(), "https://riverside.com/login".to_string()));
        assert!(m.list()[0].status.logging_in);
    }

    #[tokio::test]
    async fn login_unknown_account_errors() {
        let (m, _) = mgr(Arc::new(FakeRecipeRunner::ok(json!({"ok": true}))));
        assert!(m.login("nope").await.is_err());
        assert!(m.recheck("nope", 1).await.is_err());
    }
}

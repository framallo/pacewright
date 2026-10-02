//! `RecipeRegistry` — enumerate the installed `*.kdl` recipes and expose just the
//! metadata pacewright needs to *route and pace* them, without running anything.
//!
//! A recipe's full model (steps/locators/outputs) lives in and is executed by the
//! chrome-agent fork. pacewright only needs three things off each recipe, all readable
//! by a shallow KDL walk (no chrome-agent process required at boot):
//!
//! - its **name** `"<adapter>/<action>"` → the `(adapter, action)` the RPC surface uses
//!   (`pcw add acme scrape_profile`), so the existing CLI is unchanged;
//! - its **`limit-key`s** → declared pacing, enforced by the engine, not the adapter;
//! - its **`var`s** (name, optional `from` alias, required/default) → so the vault
//!   job-runner can bind a note's frontmatter fields to the recipe's vars.
//!
//! Parsing is deliberately lenient at the directory level: a file that isn't a valid
//! recipe is skipped (logged), never fatal — one bad recipe must not blind the daemon
//! to the good ones.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// A declared recipe variable, as pacewright needs it for job binding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecipeVar {
    /// The recipe's own var name (what `--vars-json` is keyed by).
    pub name: String,
    /// Optional source-field alias: a job note's `<from>` frontmatter field binds to
    /// this var (`var "url" from="acme"` ← note's `acme:` field).
    pub from: Option<String>,
    pub required: bool,
    pub has_default: bool,
}

impl RecipeVar {
    /// The frontmatter field a job note should supply for this var: the `from` alias
    /// if present, else the var's own name.
    pub fn source_field(&self) -> &str {
        self.from.as_deref().unwrap_or(&self.name)
    }
}

/// The routing/pacing metadata for one installed recipe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecipeMeta {
    /// Full recipe name, `"<adapter>/<action>"`.
    pub name: String,
    /// `name` before the first `/`.
    pub adapter: String,
    /// `name` after the first `/`.
    pub action: String,
    /// Absolute path to the `.kdl` file (what `recipe run` is pointed at).
    pub path: PathBuf,
    pub limit_keys: Vec<String>,
    pub vars: Vec<RecipeVar>,
    /// The recipe's `description`, if any (for `actions()` / `pcw adapters`).
    pub description: Option<String>,
    /// `auth #true` — the recipe needs the operator's logged-in Chrome session, so the runner
    /// copies cookies before navigating. Absent/`#false` = a public recipe that runs without a
    /// signed-in browser (the default). Keeps auth-vs-public an explicit, declarative property.
    pub auth: bool,
    /// `auth account="<name>"` — the recipe runs in the persistent per-account browser profile
    /// `<name>` (established via `pcw auth login`), instead of copying the everyday Chrome cookies.
    /// `None` with `auth=true` = the legacy `auth #true` (shared `pacewright` profile + copy).
    pub account: Option<String>,
    /// `foreground #true` — the recipe needs its tab **in front** while it runs, not merely open.
    /// Chrome throttles background tabs (timers, rAF), which is what stalls a heavy render.
    ///
    /// Only matters now that every site shares one attached Chrome, where exactly one tab can be
    /// foreground; the old model gave each account its own window. Default off: raising a window
    /// steals focus on the operator's real Mac, so a recipe must ask for it.
    ///
    /// This does NOT need a scheduling mutex — the daemon's tick loop awaits each task before
    /// claiming the next (`server.rs`), so tasks never overlap and two recipes cannot fight over
    /// the foreground. If that loop is ever made concurrent, this flag is where the contention
    /// lands: see `foreground_serialization_is_load_bearing` in `daemon/tests/e2e.rs`.
    pub foreground: bool,
    /// `login-url "…"` — only on **account recipes** (name prefix `accounts/`): the raw sign-in
    /// form. Kept as a fallback; login prefers `home_url`. `None` on normal recipes.
    pub login_url: Option<String>,
    /// The account recipe's **home** — its check's first `step { goto "…" }` (the authenticated
    /// landing page). Login opens THIS: signed in → the operator sees the app; signed out → the app
    /// redirects them to sign in. Falls back to `login_url` when the recipe has no `goto` step.
    pub home_url: Option<String>,
}

impl RecipeMeta {
    /// An account (login) recipe — its steps are a signed-in check and it carries a `login-url`.
    /// Routed to the auth subsystem, never registered as a runnable task adapter.
    pub fn is_account(&self) -> bool {
        self.adapter == "accounts"
    }
    /// The account name this recipe is bound to (for a normal recipe: its `auth account`; for an
    /// account recipe: its own `action`, i.e. `accounts/<name>` → `<name>`).
    pub fn account_name(&self) -> Option<String> {
        if self.is_account() {
            Some(self.action.clone())
        } else {
            self.account.clone()
        }
    }
}

/// The pseudo-path recorded on metadata parsed from a source string (no file behind it).
pub const INLINE_PATH: &str = "<inline>";

/// [`parse_meta`] for a recipe that has no file — a database row handed to `run_src`. The
/// `path` is [`INLINE_PATH`]; everything else (name, `limit-key`s, `foreground`, `auth`, vars)
/// reads exactly as from a file, so pacing rules apply the same to both.
pub fn parse_meta_from_src(text: &str) -> Result<Option<RecipeMeta>, String> {
    parse_meta(text, Path::new(INLINE_PATH))
}

/// Parse a recipe file's routing metadata. `Ok(None)` = valid KDL but not a recipe (or a
/// recipe whose name isn't `<adapter>/<action>`); skip it. `Err` = not valid KDL at all.
pub fn parse_meta(text: &str, path: &Path) -> Result<Option<RecipeMeta>, String> {
    let doc: kdl::KdlDocument = text.parse().map_err(|e| format!("not valid KDL: {e}"))?;
    let Some(node) = doc.nodes().iter().find(|n| n.name().value() == "recipe") else {
        return Ok(None);
    };
    let Some(name) = first_arg(node) else {
        return Err("`recipe` node is missing its \"<name>\" argument".to_string());
    };
    // The name must split into exactly `<adapter>/<action>` for RPC routing.
    let Some((adapter, action)) = name.split_once('/') else {
        return Ok(None);
    };
    if adapter.is_empty() || action.is_empty() || action.contains('/') {
        return Ok(None);
    }

    let mut limit_keys = Vec::new();
    let mut vars = Vec::new();
    let mut description = None;
    let mut auth = false;
    let mut account = None;
    let mut login_url = None;
    let mut foreground = false;
    for child in children(node) {
        match child.name().value() {
            "limit-key" => {
                if let Some(k) = first_arg(child) {
                    limit_keys.push(k.to_string());
                }
            }
            "description" => description = first_arg(child).map(str::to_string),
            "var" => {
                if let Some(v) = parse_var(child) {
                    vars.push(v);
                }
            }
            // `auth` / `auth #true` → needs a session; `auth #false` → opt out;
            // `auth account="X"` → needs the session of the `X` account profile.
            "auth" => {
                account = prop_str(child, "account").map(str::to_string);
                auth = account.is_some() || first_bool(child).unwrap_or(true);
            }
            // `login-url "…"` — only meaningful on account recipes.
            "login-url" => login_url = first_arg(child).map(str::to_string),
            // `foreground` / `foreground #true` → the tab must be raised while the recipe runs.
            "foreground" => foreground = first_bool(child).unwrap_or(true),
            _ => {}
        }
    }

    Ok(Some(RecipeMeta {
        name: name.to_string(),
        adapter: adapter.to_string(),
        action: action.to_string(),
        path: path.to_path_buf(),
        limit_keys,
        vars,
        description,
        auth,
        account,
        foreground,
        login_url,
        home_url: first_goto_url(node),
    }))
}

fn parse_var(node: &kdl::KdlNode) -> Option<RecipeVar> {
    let name = first_arg(node)?.to_string();
    Some(RecipeVar {
        name,
        from: prop_str(node, "from").map(str::to_string),
        required: prop_bool(node, "required").unwrap_or(false),
        has_default: prop_str(node, "default").is_some(),
    })
}

/// The URL of the recipe's first `step { goto "…" }` — an account recipe's home/landing page.
/// Steps are scanned in document order; the first `goto` found wins. `None` if no step navigates.
fn first_goto_url(recipe: &kdl::KdlNode) -> Option<String> {
    for step in children(recipe).filter(|c| c.name().value() == "step") {
        if let Some(goto) = children(step).find(|c| c.name().value() == "goto") {
            return first_arg(goto).map(str::to_string);
        }
    }
    None
}

fn first_arg(node: &kdl::KdlNode) -> Option<&str> {
    node.entries()
        .iter()
        .find(|e| e.name().is_none())
        .and_then(|e| e.value().as_string())
}

/// The node's first unnamed argument as a bool (e.g. the `#true` in `auth #true`).
fn first_bool(node: &kdl::KdlNode) -> Option<bool> {
    node.entries()
        .iter()
        .find(|e| e.name().is_none())
        .and_then(|e| e.value().as_bool())
}

fn prop_str<'a>(node: &'a kdl::KdlNode, key: &str) -> Option<&'a str> {
    node.entries()
        .iter()
        .find(|e| e.name().map(kdl::KdlIdentifier::value) == Some(key))
        .and_then(|e| e.value().as_string())
}

fn prop_bool(node: &kdl::KdlNode, key: &str) -> Option<bool> {
    node.entries()
        .iter()
        .find(|e| e.name().map(kdl::KdlIdentifier::value) == Some(key))
        .and_then(|e| e.value().as_bool())
}

fn children(node: &kdl::KdlNode) -> impl Iterator<Item = &kdl::KdlNode> {
    node.children().into_iter().flat_map(|d| d.nodes().iter())
}

/// All installed recipes, keyed by full name. Built once from the recipes dir at daemon
/// boot (and by the vault job-runner) — cheap to clone the metadata out of.
#[derive(Debug, Clone, Default)]
pub struct RecipeRegistry {
    by_name: BTreeMap<String, RecipeMeta>,
}

impl RecipeRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Load every `*.kdl` under `dir` (recursively), skipping non-recipe / invalid files.
    /// A missing dir is an empty registry, not an error (a fresh box has no recipes yet).
    pub fn load_dir(dir: &Path) -> Self {
        let mut reg = Self::new();
        let mut files = Vec::new();
        find_kdl(dir, &mut files);
        for f in files {
            let text = match std::fs::read_to_string(&f) {
                Ok(t) => t,
                Err(e) => {
                    tracing::warn!("recipe registry: cannot read {}: {e}", f.display());
                    continue;
                }
            };
            match parse_meta(&text, &f) {
                Ok(Some(meta)) => {
                    if let Some(prev) = reg.by_name.get(&meta.name) {
                        tracing::warn!(
                            "recipe registry: `{}` from {} shadows {} — keys collide on name",
                            meta.name,
                            f.display(),
                            prev.path.display()
                        );
                    }
                    reg.by_name.insert(meta.name.clone(), meta);
                }
                Ok(None) => {}
                Err(e) => tracing::warn!("recipe registry: skip {}: {e}", f.display()),
            }
        }
        reg
    }

    pub fn get(&self, adapter: &str, action: &str) -> Option<&RecipeMeta> {
        self.by_name
            .values()
            .find(|m| m.adapter == adapter && m.action == action)
    }

    /// Distinct adapter prefixes present, sorted — one `RecipeAdapter` is registered per.
    /// Excludes the `accounts` prefix: account (login) recipes are routed to the auth subsystem,
    /// never registered as runnable task adapters.
    pub fn adapters(&self) -> Vec<String> {
        let mut v: Vec<String> = self
            .by_name
            .values()
            .filter(|m| !m.is_account())
            .map(|m| m.adapter.clone())
            .collect();
        v.sort_unstable();
        v.dedup();
        v
    }

    /// Every account (login) recipe, in name order.
    pub fn accounts(&self) -> Vec<&RecipeMeta> {
        self.by_name.values().filter(|m| m.is_account()).collect()
    }

    /// The account (login) recipe for `<name>` (i.e. `accounts/<name>`), if installed.
    pub fn account(&self, name: &str) -> Option<&RecipeMeta> {
        self.by_name
            .values()
            .find(|m| m.is_account() && m.action == name)
    }

    /// The runnable recipes bound to account `<name>` via `auth account="<name>"`, in name order.
    pub fn recipes_for_account(&self, name: &str) -> Vec<&RecipeMeta> {
        self.by_name
            .values()
            .filter(|m| !m.is_account() && m.account.as_deref() == Some(name))
            .collect()
    }

    /// Every recipe under one adapter prefix (its `action`s), in name order.
    pub fn actions_for(&self, adapter: &str) -> Vec<&RecipeMeta> {
        self.by_name
            .values()
            .filter(|m| m.adapter == adapter)
            .collect()
    }

    pub fn is_empty(&self) -> bool {
        self.by_name.is_empty()
    }

    pub fn len(&self) -> usize {
        self.by_name.len()
    }
}

/// Recursively collect `*.kdl` under `dir`. Silent on a missing dir (empty registry).
fn find_kdl(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().is_some_and(|n| n == ".git") {
                continue;
            }
            find_kdl(&path, out);
        } else if path.extension().is_some_and(|e| e == "kdl") {
            out.push(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ACME: &str = r#"recipe "acme/scrape_profile" {
        description "scrape a profile"
        limit-key "acme.profile_scrape"
        var "url" from="acme" required=#true doc="profile URL"
        var "vault"
        var "slug" default="unknown"
    }"#;

    #[test]
    fn parses_routing_metadata() {
        let m = parse_meta(ACME, Path::new("/r/x.kdl")).unwrap().unwrap();
        assert_eq!(m.name, "acme/scrape_profile");
        assert_eq!(m.adapter, "acme");
        assert_eq!(m.action, "scrape_profile");
        assert_eq!(m.limit_keys, vec!["acme.profile_scrape".to_string()]);
        assert_eq!(m.description.as_deref(), Some("scrape a profile"));
        assert_eq!(m.vars.len(), 3);
        let url = &m.vars[0];
        assert_eq!(url.name, "url");
        assert_eq!(url.from.as_deref(), Some("acme"));
        assert!(url.required);
        assert_eq!(url.source_field(), "acme");
        // an unaliased var maps by its own name
        assert_eq!(m.vars[1].source_field(), "vault");
        assert!(m.vars[2].has_default && !m.vars[2].required);
        // no `auth` node → a public recipe by default
        assert!(!m.auth);
    }

    #[test]
    fn parse_meta_from_src_reads_pacing_without_a_file() {
        let m = parse_meta_from_src(
            "recipe \"facturagas/facturar\" {\n  limit-key \"facturagas.facturar\"\n  foreground #true\n}",
        )
        .unwrap()
        .unwrap();
        assert_eq!(m.adapter, "facturagas");
        assert_eq!(m.action, "facturar");
        assert_eq!(m.limit_keys, vec!["facturagas.facturar".to_string()]);
        assert!(m.foreground);
        assert_eq!(m.path, Path::new(INLINE_PATH));
        assert!(parse_meta_from_src("not kdl {{{").is_err());
        assert!(parse_meta_from_src("other \"x\" {}").unwrap().is_none());
    }

    #[test]
    fn account_recipes_route_to_auth_not_task_adapters() {
        let dir = std::env::temp_dir().join(format!(
            "pcw-acct-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(dir.join("accounts")).unwrap();
        // an account (login) recipe + a normal recipe bound to it
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
        let reg = RecipeRegistry::load_dir(&dir);

        // the account recipe is NOT a task adapter
        assert!(!reg.adapters().contains(&"accounts".to_string()));
        assert!(reg.adapters().contains(&"globex".to_string()));
        // it IS discoverable as an account, with its login url
        let accounts = reg.accounts();
        assert_eq!(accounts.len(), 1);
        assert_eq!(
            accounts[0].account_name().as_deref(),
            Some("globex-account")
        );
        assert_eq!(
            accounts[0].login_url.as_deref(),
            Some("https://globex.example/login")
        );
        // `home_url` = the check's first `goto` (the authenticated landing). Login opens THIS so a
        // signed-in operator sees the app instead of a pointless login form; signed out, the app
        // redirects them to sign in anyway.
        assert_eq!(
            accounts[0].home_url.as_deref(),
            Some("https://globex.example/dashboard")
        );
        assert!(reg.account("globex-account").is_some());
        // the normal recipe is bound to the account
        let rv = reg.get("globex", "generate_clips").unwrap();
        assert!(rv.auth && rv.account.as_deref() == Some("globex-account"));
        let bound = reg.recipes_for_account("globex-account");
        assert_eq!(bound.len(), 1);
        assert_eq!(bound[0].name, "globex/generate_clips");
    }

    #[test]
    fn auth_node_marks_a_recipe_as_needing_a_session() {
        let authed = parse_meta(r#"recipe "acme/dm" { auth #true }"#, Path::new("/r/a.kdl"))
            .unwrap()
            .unwrap();
        assert!(authed.auth);
        // explicit opt-out stays public
        let public = parse_meta(r#"recipe "news/hn" { auth #false }"#, Path::new("/r/p.kdl"))
            .unwrap()
            .unwrap();
        assert!(!public.auth);
    }

    #[test]
    fn non_recipe_and_bad_name_are_skipped_not_errors() {
        assert_eq!(parse_meta("other \"x\" {}", Path::new("/x")).unwrap(), None);
        // a recipe name without <adapter>/<action> can't be routed → skipped
        assert_eq!(
            parse_meta(r#"recipe "flat" {}"#, Path::new("/x")).unwrap(),
            None
        );
    }

    #[test]
    fn invalid_kdl_is_an_error() {
        assert!(parse_meta("recipe \"x\" { within={bad} }", Path::new("/x")).is_err());
    }

    fn scratch() -> PathBuf {
        let p = std::env::temp_dir().join(format!("pcw-reg-test-{}", uuid_like()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    // A tiny unique-ish suffix without pulling the uuid crate into this crate's deps.
    fn uuid_like() -> String {
        use std::time::{SystemTime, UNIX_EPOCH};
        format!(
            "{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        )
    }

    #[test]
    fn load_dir_enumerates_and_routes() {
        let dir = scratch();
        std::fs::create_dir_all(dir.join("acme")).unwrap();
        std::fs::write(dir.join("acme/scrape.kdl"), ACME).unwrap();
        std::fs::write(
            dir.join("hn.kdl"),
            r#"recipe "news/hackernews" { limit-key "news.hn" }"#,
        )
        .unwrap();
        std::fs::write(dir.join("junk.kdl"), "not a recipe { a 1 }").unwrap();
        std::fs::write(dir.join("readme.md"), "hi").unwrap();

        let reg = RecipeRegistry::load_dir(&dir);
        assert_eq!(reg.len(), 2);
        assert_eq!(reg.adapters(), vec!["acme", "news"]);
        assert!(reg.get("acme", "scrape_profile").is_some());
        assert!(reg.get("news", "hackernews").is_some());
        assert!(reg.get("acme", "nope").is_none());
        assert_eq!(reg.actions_for("acme").len(), 1);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_dir_is_empty_not_error() {
        let reg = RecipeRegistry::load_dir(Path::new("/nonexistent/pcw/recipes"));
        assert!(reg.is_empty());
        assert!(reg.adapters().is_empty());
    }
}

#[cfg(test)]
mod foreground_tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn parses_foreground_flag() {
        // `foreground #true` — the recipe needs its tab actually in FRONT while it runs. Chrome
        // throttles background tabs, which is what stalls a heavy render.
        let m = parse_meta(
            "recipe \"globex/heavy_job\" { foreground #true }",
            Path::new("/r/rv.kdl"),
        )
        .unwrap()
        .unwrap();
        assert!(m.foreground);
    }

    #[test]
    fn foreground_defaults_off_and_can_be_explicit() {
        // Absent → off. Raising a window steals focus on the operator's real Mac, so a recipe must
        // ASK for it; most (API polls, scrapes) never should.
        let off = parse_meta("recipe \"acme/whoami\" { }", Path::new("/r/li.kdl"))
            .unwrap()
            .unwrap();
        assert!(!off.foreground);
        // `foreground #false` → explicitly off.
        let explicit = parse_meta(
            "recipe \"acme/whoami\" { foreground #false }",
            Path::new("/r/li.kdl"),
        )
        .unwrap()
        .unwrap();
        assert!(!explicit.foreground);
    }
}

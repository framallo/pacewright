//! `RecipeRegistry` — enumerate the installed `*.kdl` recipes and expose just the
//! metadata pacewright needs to *route and pace* them, without running anything.
//!
//! A recipe's full model (steps/locators/outputs) lives in and is executed by the
//! chrome-agent fork. pacewright only needs three things off each recipe, all readable
//! by a shallow KDL walk (no chrome-agent process required at boot):
//!
//! - its **name** `"<adapter>/<action>"` → the `(adapter, action)` the RPC surface uses
//!   (`pcw add linkedin scrape_profile`), so the existing CLI is unchanged;
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
    /// this var (`var "url" from="linkedin"` ← note's `linkedin:` field).
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

fn first_arg(node: &kdl::KdlNode) -> Option<&str> {
    node.entries()
        .iter()
        .find(|e| e.name().is_none())
        .and_then(|e| e.value().as_string())
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
    pub fn adapters(&self) -> Vec<String> {
        let mut v: Vec<String> = self.by_name.values().map(|m| m.adapter.clone()).collect();
        v.sort_unstable();
        v.dedup();
        v
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

    const LINKEDIN: &str = r#"recipe "linkedin/scrape_profile" {
        description "scrape a profile"
        limit-key "linkedin.profile_scrape"
        var "url" from="linkedin" required=#true doc="profile URL"
        var "vault"
        var "slug" default="unknown"
    }"#;

    #[test]
    fn parses_routing_metadata() {
        let m = parse_meta(LINKEDIN, Path::new("/r/x.kdl"))
            .unwrap()
            .unwrap();
        assert_eq!(m.name, "linkedin/scrape_profile");
        assert_eq!(m.adapter, "linkedin");
        assert_eq!(m.action, "scrape_profile");
        assert_eq!(m.limit_keys, vec!["linkedin.profile_scrape".to_string()]);
        assert_eq!(m.description.as_deref(), Some("scrape a profile"));
        assert_eq!(m.vars.len(), 3);
        let url = &m.vars[0];
        assert_eq!(url.name, "url");
        assert_eq!(url.from.as_deref(), Some("linkedin"));
        assert!(url.required);
        assert_eq!(url.source_field(), "linkedin");
        // an unaliased var maps by its own name
        assert_eq!(m.vars[1].source_field(), "vault");
        assert!(m.vars[2].has_default && !m.vars[2].required);
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
        std::fs::create_dir_all(dir.join("linkedin")).unwrap();
        std::fs::write(dir.join("linkedin/scrape.kdl"), LINKEDIN).unwrap();
        std::fs::write(
            dir.join("hn.kdl"),
            r#"recipe "news/hackernews" { limit-key "news.hn" }"#,
        )
        .unwrap();
        std::fs::write(dir.join("junk.kdl"), "not a recipe { a 1 }").unwrap();
        std::fs::write(dir.join("readme.md"), "hi").unwrap();

        let reg = RecipeRegistry::load_dir(&dir);
        assert_eq!(reg.len(), 2);
        assert_eq!(reg.adapters(), vec!["linkedin", "news"]);
        assert!(reg.get("linkedin", "scrape_profile").is_some());
        assert!(reg.get("news", "hackernews").is_some());
        assert!(reg.get("linkedin", "nope").is_none());
        assert_eq!(reg.actions_for("linkedin").len(), 1);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_dir_is_empty_not_error() {
        let reg = RecipeRegistry::load_dir(Path::new("/nonexistent/pcw/recipes"));
        assert!(reg.is_empty());
        assert!(reg.adapters().is_empty());
    }
}

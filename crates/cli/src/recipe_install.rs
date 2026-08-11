//! `pacewright recipe add / list` — install KDL recipes from a GitHub repo.
//!
//! Recipes are distributed *data* (see the recipe spec §3): they live outside the
//! engine repo, in the operator's `~/.pacewright/recipes/` dir. This installs them
//! there from a GitHub repo via a shallow `git clone`, validating each `.kdl` and
//! recording provenance (repo + pinned commit SHA) so installs are reproducible and
//! listable.
//!
//! It is a purely client-side, local-filesystem operation — no daemon round-trip.

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Command;

/// A parsed `owner/repo[@ref][#subdir]` source. Kept separate from any IO so it's
/// unit-testable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoSource {
    pub owner: String,
    pub repo: String,
    /// Branch / tag / SHA to clone; `None` = the repo's default branch.
    pub git_ref: Option<String>,
    /// Restrict the search to this subdirectory of the repo.
    pub subdir: Option<String>,
}

impl RepoSource {
    pub fn url(&self) -> String {
        format!("https://github.com/{}/{}", self.owner, self.repo)
    }
    /// Filesystem-safe install dir name under the recipes root.
    pub fn dir_name(&self) -> String {
        format!("{}__{}", self.owner, self.repo)
    }
}

/// Parse `owner/repo`, `github.com/owner/repo`, or `https://github.com/owner/repo[.git]`,
/// each optionally suffixed with `@<ref>` and/or `#<subdir>`.
pub fn parse_source(spec: &str) -> Result<RepoSource> {
    let spec = spec.trim();
    if spec.is_empty() {
        bail!("empty repo spec");
    }
    // `#subdir` first (a path never contains '#'); then `@ref` (none of the supported
    // URL forms contain '@' except as the ref delimiter).
    let (rest, subdir) = match spec.split_once('#') {
        Some((r, s)) => (r, Some(s.trim_matches('/').to_string())),
        None => (spec, None),
    };
    let (repo_part, git_ref) = match rest.split_once('@') {
        Some((r, g)) => (r, Some(g.to_string())),
        None => (rest, None),
    };

    let norm = repo_part
        .trim()
        .strip_prefix("https://")
        .or_else(|| repo_part.trim().strip_prefix("http://"))
        .unwrap_or(repo_part.trim());
    let norm = norm.strip_prefix("github.com/").unwrap_or(norm);
    let norm = norm.strip_suffix('/').unwrap_or(norm);
    let norm = norm.strip_suffix(".git").unwrap_or(norm);

    let segs: Vec<&str> = norm.split('/').filter(|s| !s.is_empty()).collect();
    if segs.len() != 2 {
        bail!("expected owner/repo (optionally github.com/owner/repo or a full https URL), got `{spec}`");
    }
    Ok(RepoSource {
        owner: segs[0].to_string(),
        repo: segs[1].to_string(),
        git_ref,
        subdir: subdir.filter(|s| !s.is_empty()),
    })
}

/// If `text` is a valid recipe, return its declared `name`. `Ok(None)` means the file
/// parses as KDL but has no top-level `recipe "<name>"` node (skip it, don't fail the
/// whole install). `Err` means it isn't valid KDL at all.
pub fn recipe_name_from_kdl(text: &str) -> Result<Option<String>> {
    let doc: kdl::KdlDocument = text.parse().map_err(|e| anyhow!("not valid KDL: {e}"))?;
    for node in doc.nodes() {
        if node.name().value() == "recipe" {
            let name = node
                .entries()
                .iter()
                .find(|e| e.name().is_none()) // first positional arg
                .and_then(|e| e.value().as_string())
                .ok_or_else(|| anyhow!("`recipe` node is missing its \"<name>\" argument"))?;
            return Ok(Some(name.to_string()));
        }
    }
    Ok(None)
}

// ---- provenance manifest ---------------------------------------------------

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Manifest {
    #[serde(default, rename = "source")]
    pub sources: Vec<SourceEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceEntry {
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_ref: Option<String>,
    pub sha: String,
    pub dir: String,
    /// The recipe names this source installed (as declared in each `.kdl`).
    pub recipes: Vec<String>,
}

fn manifest_path(recipes_dir: &Path) -> PathBuf {
    recipes_dir.join(".sources.toml")
}

pub fn load_manifest(recipes_dir: &Path) -> Result<Manifest> {
    let p = manifest_path(recipes_dir);
    match std::fs::read_to_string(&p) {
        Ok(s) => toml::from_str(&s).with_context(|| format!("parsing {}", p.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Manifest::default()),
        Err(e) => Err(e).with_context(|| format!("reading {}", p.display())),
    }
}

fn save_manifest(recipes_dir: &Path, m: &Manifest) -> Result<()> {
    std::fs::create_dir_all(recipes_dir)?;
    let s = toml::to_string_pretty(m)?;
    std::fs::write(manifest_path(recipes_dir), s)?;
    Ok(())
}

// ---- git ------------------------------------------------------------------

fn git(args: &[&str]) -> Result<std::process::Output> {
    Command::new("git")
        .args(args)
        .output()
        .map_err(|e| anyhow!("failed to run git (is it installed?): {e}"))
}

fn shallow_clone(src: &RepoSource, dest: &Path) -> Result<String> {
    let mut args = vec!["clone", "--depth", "1"];
    if let Some(r) = &src.git_ref {
        args.push("--branch");
        args.push(r);
    }
    let url = src.url();
    args.push(&url);
    let dest_s = dest.to_string_lossy().to_string();
    args.push(&dest_s);

    let out = git(&args)?;
    if !out.status.success() {
        bail!(
            "git clone of {} failed: {}",
            url,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    // Pin the exact commit for reproducibility.
    let rev = git(&["-C", &dest_s, "rev-parse", "HEAD"])?;
    if !rev.status.success() {
        bail!("could not resolve HEAD commit after clone");
    }
    Ok(String::from_utf8_lossy(&rev.stdout).trim().to_string())
}

// ---- commands -------------------------------------------------------------

pub(crate) fn recipes_root() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME not set")?;
    Ok(PathBuf::from(home).join(".pacewright").join("recipes"))
}

/// Recursively collect `*.kdl` under `dir`.
fn find_kdl(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            // skip the clone's .git
            if path.file_name().map(|n| n == ".git").unwrap_or(false) {
                continue;
            }
            find_kdl(&path, out)?;
        } else if path.extension().map(|e| e == "kdl").unwrap_or(false) {
            out.push(path);
        }
    }
    Ok(())
}

/// `pacewright recipe add <spec>`.
pub fn add(spec: &str) -> Result<()> {
    let src = parse_source(spec)?;
    let recipes_dir = recipes_root()?;

    // Clone into a unique temp dir.
    let tmp = std::env::temp_dir().join(format!("pacewright-recipe-{}", uuid::Uuid::new_v4()));
    let sha = shallow_clone(&src, &tmp)?;
    // Ensure cleanup even on early return.
    let _guard = TempGuard(tmp.clone());

    install_from_dir(&src, &tmp, &sha, &recipes_dir)
}

/// Everything after the clone: discover, validate, copy, record. Split out from `add`
/// so it's testable against a local directory without any network/git.
pub fn install_from_dir(
    src: &RepoSource,
    cloned_root: &Path,
    sha: &str,
    recipes_dir: &Path,
) -> Result<()> {
    let search_root = match &src.subdir {
        Some(s) => cloned_root.join(s),
        None => cloned_root.to_path_buf(),
    };
    if !search_root.exists() {
        bail!(
            "subdir `{}` not found in {}",
            src.subdir.as_deref().unwrap_or(""),
            src.url()
        );
    }

    let mut kdl_files = Vec::new();
    find_kdl(&search_root, &mut kdl_files)?;
    if kdl_files.is_empty() {
        bail!(
            "no .kdl recipes found in {}{}",
            src.url(),
            src.subdir
                .as_deref()
                .map(|s| format!(" (#{s})"))
                .unwrap_or_default()
        );
    }

    // Validate; collect (name, source-relative-path, bytes).
    let mut valid: Vec<(String, PathBuf, String)> = Vec::new();
    let mut skipped = 0usize;
    for f in &kdl_files {
        let text = std::fs::read_to_string(f)?;
        match recipe_name_from_kdl(&text) {
            Ok(Some(name)) => {
                let rel = f.strip_prefix(&search_root).unwrap_or(f).to_path_buf();
                valid.push((name, rel, text));
            }
            Ok(None) => {
                skipped += 1;
            }
            Err(e) => {
                eprintln!(
                    "  skip {}: {e}",
                    f.file_name().unwrap_or_default().to_string_lossy()
                );
                skipped += 1;
            }
        }
    }
    if valid.is_empty() {
        bail!(
            "found {} .kdl file(s) but none are valid recipes",
            kdl_files.len()
        );
    }

    // Warn on names already installed by a *different* source.
    let manifest = load_manifest(recipes_dir)?;
    let dir_name = src.dir_name();
    for (name, _, _) in &valid {
        for existing in &manifest.sources {
            if existing.dir != dir_name && existing.recipes.iter().any(|r| r == name) {
                eprintln!("  warning: recipe `{name}` is also provided by {} — the registry keys on recipe name, so this will collide", existing.url);
            }
        }
    }

    // Install into <recipes_dir>/<owner>__<repo>/, replacing any prior copy from this source.
    let install_dir = recipes_dir.join(&dir_name);
    if install_dir.exists() {
        std::fs::remove_dir_all(&install_dir)?;
    }
    for (_, rel, text) in &valid {
        let dest = install_dir.join(rel);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&dest, text)?;
    }

    // Record provenance (replace any prior entry for this source dir).
    let mut manifest = manifest;
    manifest.sources.retain(|s| s.dir != dir_name);
    manifest.sources.push(SourceEntry {
        url: src.url(),
        git_ref: src.git_ref.clone(),
        sha: sha.to_string(),
        dir: dir_name.clone(),
        recipes: valid.iter().map(|(n, _, _)| n.clone()).collect(),
    });
    save_manifest(recipes_dir, &manifest)?;

    println!(
        "Installed {} recipe(s) from {} @ {}",
        valid.len(),
        src.url(),
        &sha[..sha.len().min(12)]
    );
    for (name, rel, _) in &valid {
        println!("  {name}  ({})", rel.display());
    }
    if skipped > 0 {
        println!("  ({skipped} non-recipe/invalid .kdl file(s) skipped)");
    }
    println!("into {}", install_dir.display());
    Ok(())
}

/// `pacewright recipe list`.
pub fn list() -> Result<()> {
    let recipes_dir = recipes_root()?;
    let manifest = load_manifest(&recipes_dir)?;
    if manifest.sources.is_empty() {
        println!("No recipes installed. Add some with: pacewright recipe add <owner/repo>");
        return Ok(());
    }
    for s in &manifest.sources {
        let r = s
            .git_ref
            .as_deref()
            .map(|r| format!(" ({r})"))
            .unwrap_or_default();
        println!("{}{}  @ {}", s.url, r, &s.sha[..s.sha.len().min(12)]);
        for name in &s.recipes {
            println!("  {name}");
        }
    }
    Ok(())
}

/// Best-effort temp-dir cleanup.
struct TempGuard(PathBuf);
impl Drop for TempGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_owner_repo_forms() {
        let want = RepoSource {
            owner: "o".into(),
            repo: "r".into(),
            git_ref: None,
            subdir: None,
        };
        assert_eq!(parse_source("o/r").unwrap(), want);
        assert_eq!(parse_source("github.com/o/r").unwrap(), want);
        assert_eq!(parse_source("https://github.com/o/r").unwrap(), want);
        assert_eq!(parse_source("https://github.com/o/r.git").unwrap(), want);
        assert_eq!(parse_source("https://github.com/o/r/").unwrap(), want);
    }

    #[test]
    fn parses_ref_and_subdir() {
        let s = parse_source("o/r@v1.2#recipes/acme").unwrap();
        assert_eq!(s.owner, "o");
        assert_eq!(s.repo, "r");
        assert_eq!(s.git_ref.as_deref(), Some("v1.2"));
        assert_eq!(s.subdir.as_deref(), Some("recipes/acme"));
        // ref on a full URL too
        assert_eq!(
            parse_source("https://github.com/o/r@main")
                .unwrap()
                .git_ref
                .as_deref(),
            Some("main")
        );
        // subdir only
        assert_eq!(
            parse_source("o/r#sub").unwrap().subdir.as_deref(),
            Some("sub")
        );
    }

    #[test]
    fn rejects_malformed() {
        assert!(parse_source("").is_err());
        assert!(parse_source("justone").is_err());
        assert!(parse_source("a/b/c/d").is_err());
    }

    #[test]
    fn url_and_dir_name() {
        let s = parse_source("acme/recipes").unwrap();
        assert_eq!(s.url(), "https://github.com/acme/recipes");
        assert_eq!(s.dir_name(), "acme__recipes");
    }

    #[test]
    fn extracts_recipe_name() {
        let kdl = "recipe \"acme/scrape_profile\" {\n  description \"x\"\n}\n";
        assert_eq!(
            recipe_name_from_kdl(kdl).unwrap().as_deref(),
            Some("acme/scrape_profile")
        );
    }

    #[test]
    fn non_recipe_kdl_is_none_not_error() {
        let kdl = "something \"else\" {\n  a 1\n}\n";
        assert_eq!(recipe_name_from_kdl(kdl).unwrap(), None);
    }

    #[test]
    fn recipe_without_name_arg_errors() {
        assert!(recipe_name_from_kdl("recipe {\n}\n").is_err());
    }

    #[test]
    fn invalid_kdl_errors() {
        assert!(recipe_name_from_kdl("recipe \"x\" { within={bad} }").is_err());
    }

    /// Scratch dir under the system temp root, cleaned up by `TempGuard`.
    fn scratch() -> PathBuf {
        let p = std::env::temp_dir().join(format!("pcw-recipe-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    const VALID_RECIPE: &str = "recipe \"acme/scrape_profile\" {\n  description \"x\"\n}\n";

    #[test]
    fn install_from_dir_copies_validates_and_records() {
        // A fake clone: one valid recipe (nested), one non-recipe kdl, one junk file.
        let clone = scratch();
        let _cg = TempGuard(clone.clone());
        std::fs::create_dir_all(clone.join("recipes/acme")).unwrap();
        std::fs::write(clone.join("recipes/acme/scrape_profile.kdl"), VALID_RECIPE).unwrap();
        std::fs::write(clone.join("notes.kdl"), "something \"else\" { a 1 }\n").unwrap();
        std::fs::write(clone.join("README.md"), "hi").unwrap();

        let recipes_dir = scratch();
        let _rg = TempGuard(recipes_dir.clone());
        let src = parse_source("acme/recipes").unwrap();
        install_from_dir(&src, &clone, "deadbeefcafefeed", &recipes_dir).unwrap();

        // Copied under <recipes_dir>/acme__recipes/, preserving the source-relative path.
        let dest = recipes_dir.join("acme__recipes/recipes/acme/scrape_profile.kdl");
        assert!(
            dest.exists(),
            "recipe should be copied to {}",
            dest.display()
        );
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), VALID_RECIPE);
        // The non-recipe kdl is not installed.
        assert!(!recipes_dir.join("acme__recipes/notes.kdl").exists());

        // Provenance recorded: repo, pinned SHA, and the discovered recipe name.
        let m = load_manifest(&recipes_dir).unwrap();
        assert_eq!(m.sources.len(), 1);
        let e = &m.sources[0];
        assert_eq!(e.url, "https://github.com/acme/recipes");
        assert_eq!(e.sha, "deadbeefcafefeed");
        assert_eq!(e.dir, "acme__recipes");
        assert_eq!(e.recipes, vec!["acme/scrape_profile".to_string()]);
    }

    #[test]
    fn install_from_dir_is_idempotent_and_replaces_prior_copy() {
        let clone = scratch();
        let _cg = TempGuard(clone.clone());
        std::fs::write(clone.join("a.kdl"), VALID_RECIPE).unwrap();
        let recipes_dir = scratch();
        let _rg = TempGuard(recipes_dir.clone());
        let src = parse_source("acme/recipes").unwrap();

        install_from_dir(&src, &clone, "sha1", &recipes_dir).unwrap();
        // A stale file left in the install dir from a prior (larger) install must be pruned.
        std::fs::write(recipes_dir.join("acme__recipes/stale.kdl"), VALID_RECIPE).unwrap();
        install_from_dir(&src, &clone, "sha2", &recipes_dir).unwrap();

        assert!(
            !recipes_dir.join("acme__recipes/stale.kdl").exists(),
            "stale file should be pruned"
        );
        // Single source entry, updated to the new SHA (not duplicated).
        let m = load_manifest(&recipes_dir).unwrap();
        assert_eq!(m.sources.len(), 1);
        assert_eq!(m.sources[0].sha, "sha2");
    }

    #[test]
    fn install_from_dir_errors_when_no_valid_recipes() {
        let clone = scratch();
        let _cg = TempGuard(clone.clone());
        std::fs::write(clone.join("notes.kdl"), "something \"else\" { a 1 }\n").unwrap();
        let recipes_dir = scratch();
        let _rg = TempGuard(recipes_dir.clone());
        let src = parse_source("acme/recipes").unwrap();

        let err = install_from_dir(&src, &clone, "sha", &recipes_dir).unwrap_err();
        assert!(
            err.to_string().contains("none are valid recipes"),
            "got: {err}"
        );
        // Nothing recorded when the install fails.
        assert!(load_manifest(&recipes_dir).unwrap().sources.is_empty());
    }

    #[test]
    fn install_from_dir_honors_subdir() {
        let clone = scratch();
        let _cg = TempGuard(clone.clone());
        std::fs::create_dir_all(clone.join("recipes")).unwrap();
        std::fs::write(clone.join("recipes/keep.kdl"), VALID_RECIPE).unwrap();
        std::fs::write(clone.join("outside.kdl"), VALID_RECIPE).unwrap();
        let recipes_dir = scratch();
        let _rg = TempGuard(recipes_dir.clone());
        let src = parse_source("acme/recipes#recipes").unwrap();

        install_from_dir(&src, &clone, "sha", &recipes_dir).unwrap();
        // Only the in-subdir recipe is installed, and its path is relative to the subdir.
        assert!(recipes_dir.join("acme__recipes/keep.kdl").exists());
        assert!(!recipes_dir.join("acme__recipes/outside.kdl").exists());
    }

    #[test]
    fn install_from_dir_errors_on_missing_subdir() {
        let clone = scratch();
        let _cg = TempGuard(clone.clone());
        std::fs::write(clone.join("a.kdl"), VALID_RECIPE).unwrap();
        let recipes_dir = scratch();
        let _rg = TempGuard(recipes_dir.clone());
        let src = parse_source("acme/recipes#nope").unwrap();

        let err = install_from_dir(&src, &clone, "sha", &recipes_dir).unwrap_err();
        assert!(err.to_string().contains("nope"), "got: {err}");
    }

    /// Live git smoke test: proves `shallow_clone` (git clone --depth 1 + HEAD pin) and
    /// discovery run against a real public repo. Uses a tiny repo so it's fast and the
    /// discovery outcome is deterministic (no .kdl → clean bail). Ignored by default;
    /// run with `cargo test -p pacewright-cli -- --ignored live_clone`.
    #[test]
    #[ignore]
    fn live_clone_and_discover() {
        let src = parse_source("octocat/Hello-World").unwrap();
        let tmp = scratch();
        let _g = TempGuard(tmp.clone());
        std::fs::remove_dir_all(&tmp).unwrap(); // git clone wants a non-existent dest
        let sha = shallow_clone(&src, &tmp).unwrap();
        assert_eq!(
            sha.len(),
            40,
            "HEAD should pin to a full 40-char SHA, got {sha:?}"
        );

        // No recipes in that repo → discovery bails cleanly rather than panicking.
        let recipes_dir = scratch();
        let _rg = TempGuard(recipes_dir.clone());
        let err = install_from_dir(&src, &tmp, &sha, &recipes_dir).unwrap_err();
        assert!(
            err.to_string().contains("no .kdl recipes found"),
            "got: {err}"
        );
    }

    #[test]
    fn manifest_roundtrips() {
        let m = Manifest {
            sources: vec![SourceEntry {
                url: "https://github.com/o/r".into(),
                git_ref: Some("main".into()),
                sha: "abc123".into(),
                dir: "o__r".into(),
                recipes: vec!["acme/scrape_profile".into()],
            }],
        };
        let s = toml::to_string_pretty(&m).unwrap();
        let back: Manifest = toml::from_str(&s).unwrap();
        assert_eq!(back.sources.len(), 1);
        assert_eq!(
            back.sources[0].recipes,
            vec!["acme/scrape_profile".to_string()]
        );
    }
}

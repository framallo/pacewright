//! `pacewright recipe job <note.md>` — the vault job-runner.
//!
//! The unit of work is a **job note**: an Obsidian note whose YAML frontmatter both
//! names a recipe and supplies its vars. Running the job resolves the named recipe,
//! binds the note's frontmatter fields to the recipe's `var`s (honoring `from` aliases),
//! and enqueues a **paced** task so the run obeys the daily caps — the whole reason
//! pacewright, not a raw script, drives it. The recipe's `output` blocks then write the
//! markdown/JSON note back into the vault.
//!
//! Frontmatter parsing lives here (pacewright owns the YAML; the chrome-agent engine
//! stays YAML-free and takes vars as `--vars-json`). Job notes are flat `key: value`
//! records, so a tiny scalar reader is enough — no `serde_yaml` dependency.

use anyhow::{anyhow, bail, Context, Result};
use pacewright_adapter_recipe::registry::{RecipeMeta, RecipeRegistry};
use pacewright_proto::{AddTaskReq, Request};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;

/// The context vars pacewright injects so a recipe can write back into the vault. Only
/// those a recipe actually declares (and hasn't already been given by frontmatter) are
/// bound, so params stay minimal.
struct JobContext {
    vault: String,
    out_dir: String,
    slug: String,
    note: String,
}

/// Everything needed to enqueue the job, resolved from the note + registry.
#[derive(Debug)]
pub struct BuiltJob {
    pub recipe_name: String,
    pub adapter: String,
    pub action: String,
    pub params: Value,
    pub dedup_key: String,
}

/// Split a note into its frontmatter map. Requires a leading `---` fenced block of flat
/// `key: value` scalar lines; quotes around a value are stripped. Comments (`#`) and blank
/// lines are ignored. A note without frontmatter is an error (it isn't a job).
pub fn parse_frontmatter(text: &str) -> Result<BTreeMap<String, String>> {
    let mut lines = text.lines();
    // The very first non-empty line must be the opening fence.
    loop {
        match lines.next() {
            Some(l) if l.trim().is_empty() => {}
            Some(l) if l.trim() == "---" => break,
            _ => bail!("note has no YAML frontmatter (expected a leading `---` block)"),
        }
    }
    let mut map = BTreeMap::new();
    for line in lines {
        let t = line.trim_end();
        if t.trim() == "---" {
            return Ok(map);
        }
        let trimmed = t.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let (key, value) = trimmed
            .split_once(':')
            .ok_or_else(|| anyhow!("frontmatter line is not `key: value`: {trimmed:?}"))?;
        let key = key.trim();
        if key.is_empty() {
            bail!("frontmatter has an empty key: {trimmed:?}");
        }
        map.insert(key.to_string(), unquote(value.trim()).to_string());
    }
    bail!("frontmatter block was not closed with a trailing `---`")
}

fn unquote(s: &str) -> &str {
    let bytes = s.as_bytes();
    if s.len() >= 2
        && (bytes[0] == b'"' && bytes[s.len() - 1] == b'"'
            || bytes[0] == b'\'' && bytes[s.len() - 1] == b'\'')
    {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

/// Bind the recipe's declared vars from frontmatter (+ injected context), erroring if a
/// required var has no source. Returns the params object the paced task carries — keyed by
/// the recipe's own var names, ready to hand to `--vars-json`.
fn bind_vars(meta: &RecipeMeta, fm: &BTreeMap<String, String>, ctx: &JobContext) -> Result<Value> {
    let mut params = serde_json::Map::new();
    for var in &meta.vars {
        // 1) a frontmatter field (the `from` alias if any, else the var's own name)
        if let Some(v) = fm.get(var.source_field()) {
            params.insert(var.name.clone(), Value::String(v.clone()));
            continue;
        }
        // 2) an injected context var, by the recipe's var name
        let injected = match var.name.as_str() {
            "vault" => Some(ctx.vault.clone()),
            "out_dir" => Some(ctx.out_dir.clone()),
            "slug" => Some(ctx.slug.clone()),
            "note" => Some(ctx.note.clone()),
            _ => None,
        };
        if let Some(v) = injected {
            params.insert(var.name.clone(), Value::String(v));
            continue;
        }
        // 3) unbound: only an error if the recipe *requires* it (a default fills the rest)
        if var.required {
            bail!(
                "recipe `{}` requires var `{}` but the note has no `{}:` field",
                meta.name,
                var.name,
                var.source_field()
            );
        }
    }
    Ok(Value::Object(params))
}

/// Resolve a job note into an enqueue-ready `BuiltJob`. `vault` overrides the vault root
/// (defaults to the note's parent directory); `out_dir` defaults to the vault.
pub fn build_job(
    note_path: &Path,
    registry: &RecipeRegistry,
    vault: Option<&Path>,
) -> Result<BuiltJob> {
    let text = std::fs::read_to_string(note_path)
        .with_context(|| format!("reading job note {}", note_path.display()))?;
    let fm = parse_frontmatter(&text)?;

    let recipe_name = fm
        .get("recipe")
        .ok_or_else(|| anyhow!("job note is missing a `recipe:` field naming the recipe to run"))?;
    let (adapter, action) = recipe_name
        .split_once('/')
        .ok_or_else(|| anyhow!("`recipe: {recipe_name}` must be `<adapter>/<action>`"))?;
    let meta = registry
        .get(adapter, action)
        .ok_or_else(|| anyhow!("no recipe `{recipe_name}` installed (see `pcw recipe list`)"))?;

    let note_abs = std::fs::canonicalize(note_path).unwrap_or_else(|_| note_path.to_path_buf());
    let vault_dir = vault
        .map(Path::to_path_buf)
        .or_else(|| note_abs.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| Path::new(".").to_path_buf());
    let slug = fm.get("slug").cloned().unwrap_or_else(|| {
        note_path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default()
    });
    let ctx = JobContext {
        vault: vault_dir.to_string_lossy().into_owned(),
        out_dir: vault_dir.to_string_lossy().into_owned(),
        slug,
        note: note_abs.to_string_lossy().into_owned(),
    };

    let params = bind_vars(meta, &fm, &ctx)?;
    Ok(BuiltJob {
        recipe_name: recipe_name.clone(),
        adapter: adapter.to_string(),
        action: action.to_string(),
        params,
        // Dedup on the note path so re-running the same note doesn't double-enqueue.
        dedup_key: format!("job:{}", note_abs.to_string_lossy()),
    })
}

impl BuiltJob {
    /// The paced enqueue request for this job.
    pub fn into_add_request(self) -> Request {
        Request::Add(AddTaskReq {
            adapter: self.adapter,
            action: self.action,
            params: self.params,
            scheduled_for: None,
            recurrence: None,
            depends_on: None,
            priority: None,
            dedup_key: Some(self.dedup_key),
            max_attempts: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn scratch_note(name: &str, body: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "pcw-job-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(name);
        std::fs::write(&p, body).unwrap();
        p
    }

    fn registry_with(recipe: &str) -> RecipeRegistry {
        let dir = std::env::temp_dir().join(format!(
            "pcw-job-reg-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("r.kdl"), recipe).unwrap();
        RecipeRegistry::load_dir(&dir)
    }

    const RECIPE: &str = r#"recipe "acme/scrape_profile" {
        limit-key "acme.profile_scrape"
        var "url" from="acme" required=#true
        var "vault"
        var "slug"
    }"#;

    #[test]
    fn parses_flat_frontmatter() {
        let fm = parse_frontmatter(
            "---\ntype: guest\nrecipe: acme/scrape_profile\nacme: \"https://x/in/j\"\nslug: jane\n---\nbody\n",
        )
        .unwrap();
        assert_eq!(fm.get("recipe").unwrap(), "acme/scrape_profile");
        assert_eq!(fm.get("acme").unwrap(), "https://x/in/j");
        assert_eq!(fm.get("slug").unwrap(), "jane");
    }

    #[test]
    fn rejects_note_without_frontmatter() {
        assert!(parse_frontmatter("just a note, no fences").is_err());
        assert!(parse_frontmatter("---\nkey: v\n(never closed)").is_err());
    }

    #[test]
    fn binds_from_alias_and_injects_context() {
        let reg = registry_with(RECIPE);
        let note = scratch_note(
            "jane-doe.md",
            "---\nrecipe: acme/scrape_profile\nacme: https://www.acme.example/in/jane/\n---\n",
        );
        let job = build_job(&note, &reg, Some(Path::new("/vault"))).unwrap();
        assert_eq!(job.adapter, "acme");
        assert_eq!(job.action, "scrape_profile");
        // `acme` frontmatter → the recipe's `url` var (via `from`)
        assert_eq!(job.params["url"], "https://www.acme.example/in/jane/");
        // `vault` context injected because the recipe declares it
        assert_eq!(job.params["vault"], "/vault");
        // `slug` defaulted from the note filename
        assert_eq!(job.params["slug"], "jane-doe");
        // deduped on the note path
        assert!(job.dedup_key.starts_with("job:"));
        assert!(job.dedup_key.contains("jane-doe.md"));

        std::fs::remove_dir_all(note.parent().unwrap()).ok();
    }

    #[test]
    fn frontmatter_slug_overrides_the_filename() {
        let reg = registry_with(RECIPE);
        let note = scratch_note(
            "note.md",
            "---\nrecipe: acme/scrape_profile\nacme: https://x/in/j\nslug: custom-slug\n---\n",
        );
        let job = build_job(&note, &reg, None).unwrap();
        assert_eq!(job.params["slug"], "custom-slug");
        std::fs::remove_dir_all(note.parent().unwrap()).ok();
    }

    #[test]
    fn missing_required_var_is_an_error() {
        let reg = registry_with(RECIPE);
        // no `acme:` field → the required `url` var is unbindable
        let note = scratch_note("bad.md", "---\nrecipe: acme/scrape_profile\n---\n");
        let err = build_job(&note, &reg, None).unwrap_err();
        assert!(err.to_string().contains("requires var `url`"), "got: {err}");
        std::fs::remove_dir_all(note.parent().unwrap()).ok();
    }

    #[test]
    fn unknown_recipe_is_an_error() {
        let reg = registry_with(RECIPE);
        let note = scratch_note("x.md", "---\nrecipe: nope/missing\nacme: u\n---\n");
        let err = build_job(&note, &reg, None).unwrap_err();
        assert!(err.to_string().contains("no recipe"), "got: {err}");
        std::fs::remove_dir_all(note.parent().unwrap()).ok();
    }

    #[test]
    fn note_without_recipe_field_is_an_error() {
        let reg = registry_with(RECIPE);
        let note = scratch_note("x.md", "---\ntype: guest\n---\n");
        let err = build_job(&note, &reg, None).unwrap_err();
        assert!(err.to_string().contains("recipe:"), "got: {err}");
        std::fs::remove_dir_all(note.parent().unwrap()).ok();
    }

    #[test]
    fn builds_a_deduped_add_request() {
        let reg = registry_with(RECIPE);
        let note = scratch_note(
            "j.md",
            "---\nrecipe: acme/scrape_profile\nacme: https://x/in/j\n---\n",
        );
        let job = build_job(&note, &reg, None).unwrap();
        let req = job.into_add_request();
        let s = serde_json::to_string(&req).unwrap();
        assert!(s.contains("\"method\":\"add\""));
        assert!(s.contains("scrape_profile"));
        assert!(s.contains("dedup_key"));
        std::fs::remove_dir_all(note.parent().unwrap()).ok();
    }
}

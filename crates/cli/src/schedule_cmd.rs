//! `pcw schedule check` — offline validation of the schedule files.
//!
//! The other `schedule` subcommands (list/apply/enable/disable) are daemon round-trips
//! handled in `main.rs`; `check` is purely local (parse + validate against the installed
//! recipes, no daemon), like `recipe check`.

use anyhow::{bail, Result};
use pacewright_adapter_recipe::{schedule, RecipeRegistry};
use std::path::Path;

/// Validate every `*.toml` in `schedules_dir` against the recipes in `recipes_dir`:
/// parseable, unique ids, recipes resolve, required params present, cron/`at` parse.
pub fn check(schedules_dir: &Path, recipes_dir: &Path) -> Result<()> {
    let (entries, load_errs) = schedule::load_dir(schedules_dir);
    let registry = RecipeRegistry::load_dir(recipes_dir);
    let mut errors = load_errs;
    errors.extend(schedule::validate(&entries, &registry));

    if errors.is_empty() {
        println!(
            "ok: {} schedule task(s) valid in {}",
            entries.len(),
            schedules_dir.display()
        );
        Ok(())
    } else {
        eprintln!("{} schedule error(s):", errors.len());
        for e in &errors {
            eprintln!("  {e}");
        }
        bail!("schedule validation failed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn scratch(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "pcw-sched-cmd-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn recipes() -> PathBuf {
        let d = scratch("recipes");
        std::fs::write(
            d.join("hn.kdl"),
            "recipe \"news/hackernews\" {\n  var \"url\" required=#true\n}\n",
        )
        .unwrap();
        d
    }

    #[test]
    fn check_passes_on_a_valid_schedule() {
        let sched = scratch("ok");
        std::fs::write(
            sched.join("s.toml"),
            "[[task]]\nid=\"hn\"\nrecipe=\"news/hackernews\"\nevery=\"0 9 * * *\"\nparams={ url = \"u\" }\n",
        )
        .unwrap();
        assert!(check(&sched, &recipes()).is_ok());
    }

    #[test]
    fn check_fails_on_missing_param_and_unknown_recipe() {
        let sched = scratch("bad");
        std::fs::write(
            sched.join("s.toml"),
            "[[task]]\nid=\"a\"\nrecipe=\"news/hackernews\"\n[[task]]\nid=\"b\"\nrecipe=\"no/such\"\n",
        )
        .unwrap();
        assert!(check(&sched, &recipes()).is_err());
    }

    #[test]
    fn check_fails_on_malformed_toml() {
        let sched = scratch("malformed");
        std::fs::write(sched.join("s.toml"), "[[task]\nid = broken").unwrap();
        assert!(check(&sched, &recipes()).is_err());
    }
}

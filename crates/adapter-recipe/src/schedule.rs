//! Declarative scheduler — turn `~/.pacewright/schedules/*.toml` into paced tasks.
//!
//! The model (see `docs/specs/2026-07-09-declarative-scheduler.md`): a **recipe** is *how*
//! to execute (shared), a **task** is a *specific case* (its params), and a **schedule** is
//! *when* + whether it's enabled. Schedules live in their own TOML files, never in the recipe
//! — one recipe serves many cases on many cadences.
//!
//! This module lives in `adapter-recipe` (not `core`) because validation needs the
//! [`RecipeRegistry`]; `core` owns only the store tables + the reconcile primitives.
//!
//! - [`load_dir`] / [`parse_file`] — read entries from TOML.
//! - [`validate`] — offline checks (unique ids, recipe resolves, required vars, cron/`at`).
//! - [`reconcile`] — desired-state: make the queue match the effectively-enabled entries.

use std::path::{Path, PathBuf};

use pacewright_core::clock::Clock;
use pacewright_core::model::{Task, TaskEvent, TaskStatus};
use pacewright_core::runner::next_occurrence_ms;
use pacewright_core::store::Store;
use serde::Deserialize;
use serde_json::Value;

use crate::registry::RecipeRegistry;

/// When a scheduled task runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Timing {
    /// Recurring on a cron pattern (croner 5- or 6-field).
    Every(String),
    /// One-shot at an absolute time (epoch ms).
    At(i64),
    /// One-shot, made eligible the moment it's applied.
    OnApply,
}

/// One declared scheduled task: a recipe + its params + when + its default enabled state.
#[derive(Debug, Clone)]
pub struct ScheduleEntry {
    pub id: String,
    pub recipe: String,
    pub params: serde_json::Map<String, Value>,
    pub timing: Timing,
    pub priority: Option<i64>,
    pub max_attempts: Option<i64>,
    /// The file-declared default; a runtime override in `schedule_state` wins over it.
    pub enabled: bool,
    /// The file this entry came from (for error messages).
    pub source: PathBuf,
}

impl ScheduleEntry {
    /// The dedup key that ties this entry to its live queue task across reconciles + firings.
    pub fn dedup_key(&self) -> String {
        format!("schedule:{}", self.id)
    }

    /// `(adapter, action)` from the recipe name; `None` if it isn't `<adapter>/<action>`.
    pub fn route(&self) -> Option<(&str, &str)> {
        self.recipe.split_once('/')
    }

    /// The next time this entry fires after `now_ms`, for display. Recurring → next cron
    /// slot; one-shot `at` → its time if still future; on-apply → none.
    pub fn next_fire(&self, now_ms: i64) -> Option<i64> {
        match &self.timing {
            Timing::Every(cron) => next_occurrence_ms(cron, now_ms),
            Timing::At(ms) => (*ms > now_ms).then_some(*ms),
            Timing::OnApply => None,
        }
    }
}

// ---- parsing ---------------------------------------------------------------

#[derive(Deserialize)]
struct RawFile {
    #[serde(default)]
    task: Vec<RawEntry>,
}

#[derive(Deserialize)]
struct RawEntry {
    id: String,
    recipe: String,
    #[serde(default)]
    params: Option<toml::Value>,
    #[serde(default)]
    every: Option<String>,
    #[serde(default)]
    at: Option<String>,
    #[serde(default)]
    priority: Option<i64>,
    #[serde(default)]
    max_attempts: Option<i64>,
    #[serde(default = "default_true")]
    enabled: bool,
}

fn default_true() -> bool {
    true
}

/// Parse one schedule file's text into entries. `Err` = the file itself is malformed
/// (bad TOML, an entry with both `every` and `at`, or an unparseable `at`).
pub fn parse_file(text: &str, source: &Path) -> Result<Vec<ScheduleEntry>, String> {
    let raw: RawFile = toml::from_str(text).map_err(|e| format!("{}: {e}", source.display()))?;
    let mut out = Vec::with_capacity(raw.task.len());
    for r in raw.task {
        let timing = match (r.every.as_deref(), r.at.as_deref()) {
            (Some(_), Some(_)) => {
                return Err(format!(
                    "{}: task `{}` sets both `every` and `at` (pick one)",
                    source.display(),
                    r.id
                ));
            }
            (Some(cron), None) => Timing::Every(cron.to_string()),
            (None, Some(at)) => Timing::At(parse_at(at).map_err(|e| {
                format!(
                    "{}: task `{}` has an invalid `at`: {e}",
                    source.display(),
                    r.id
                )
            })?),
            (None, None) => Timing::OnApply,
        };
        let params = match r.params {
            Some(v) => match serde_json::to_value(v) {
                Ok(Value::Object(m)) => m,
                Ok(_) => {
                    return Err(format!(
                        "{}: task `{}` `params` must be a table",
                        source.display(),
                        r.id
                    ));
                }
                Err(e) => return Err(format!("{}: task `{}` params: {e}", source.display(), r.id)),
            },
            None => serde_json::Map::new(),
        };
        out.push(ScheduleEntry {
            id: r.id,
            recipe: r.recipe,
            params,
            timing,
            priority: r.priority,
            max_attempts: r.max_attempts,
            enabled: r.enabled,
            source: source.to_path_buf(),
        });
    }
    Ok(out)
}

/// Parse an `at` value: RFC3339 (with offset), else a naive `YYYY-MM-DDTHH:MM:SS`
/// interpreted in the local timezone. Returns epoch millis.
fn parse_at(s: &str) -> Result<i64, String> {
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        return Ok(dt.timestamp_millis());
    }
    use chrono::TimeZone;
    let naive = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S")
        .map_err(|_| format!("expected RFC3339 or YYYY-MM-DDTHH:MM:SS, got {s:?}"))?;
    chrono::Local
        .from_local_datetime(&naive)
        .single()
        .map(|dt| dt.timestamp_millis())
        .ok_or_else(|| format!("ambiguous local time {s:?}"))
}

/// Load every `*.toml` directly under `dir`. Returns the parsed entries plus a list of
/// file-level errors (a bad file is skipped, not fatal — one typo mustn't hide the rest).
/// A missing dir yields no entries and no errors.
pub fn load_dir(dir: &Path) -> (Vec<ScheduleEntry>, Vec<String>) {
    let mut entries = Vec::new();
    let mut errors = Vec::new();
    let read = match std::fs::read_dir(dir) {
        Ok(r) => r,
        Err(_) => return (entries, errors),
    };
    let mut files: Vec<PathBuf> = read
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "toml"))
        .collect();
    files.sort();
    for f in files {
        match std::fs::read_to_string(&f) {
            Ok(text) => match parse_file(&text, &f) {
                Ok(mut es) => entries.append(&mut es),
                Err(e) => errors.push(e),
            },
            Err(e) => errors.push(format!("{}: {e}", f.display())),
        }
    }
    (entries, errors)
}

// ---- validation ------------------------------------------------------------

/// Engine-provided adapters that a schedule may reference even though they are not recipes. Kept in
/// lockstep with the built-ins the daemon registers in `build_adapter_registry`.
pub const BUILTIN_ADAPTERS: &[&str] =
    &["dummy", "agent", "claude", "claude_cli", "pipeline", "data", "http"];

/// Partition entries into the **valid** ones (safe to reconcile) and a list of
/// human-readable errors for the invalid ones. Checks: ids are globally unique, each recipe
/// resolves, its required vars are all present in `params`, and cron patterns parse. An entry
/// with any error is excluded from `valid` (so `apply` enqueues the good ones and reports the
/// bad, rather than an all-or-nothing failure).
pub fn partition(
    entries: &[ScheduleEntry],
    registry: &RecipeRegistry,
) -> (Vec<ScheduleEntry>, Vec<String>) {
    let mut valid = Vec::new();
    let mut errors = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for e in entries {
        let mut errs = Vec::new();
        if !seen.insert(e.id.as_str()) {
            errs.push(format!("duplicate schedule id `{}`", e.id));
        }
        match e.route() {
            None => errs.push(format!(
                "task `{}`: recipe `{}` is not `<adapter>/<action>`",
                e.id, e.recipe
            )),
            // Built-in engine adapters (`agent`/`claude` reasoning, `pipeline` launcher, `dummy`)
            // aren't recipes, so the RecipeRegistry can't see them. They're always registered by the
            // daemon, so accept them without a recipe-existence or required-var check (the adapter
            // validates its own params at runtime).
            Some((adapter, _)) if BUILTIN_ADAPTERS.contains(&adapter) => {}
            Some((adapter, action)) => match registry.get(adapter, action) {
                None => errs.push(format!(
                    "task `{}`: no recipe `{}` installed",
                    e.id, e.recipe
                )),
                Some(meta) => {
                    for var in &meta.vars {
                        if var.required && !e.params.contains_key(&var.name) {
                            errs.push(format!(
                                "task `{}`: recipe `{}` requires param `{}`",
                                e.id, e.recipe, var.name
                            ));
                        }
                    }
                }
            },
        }
        if let Timing::Every(cron) = &e.timing {
            if next_occurrence_ms(cron, 0).is_none() {
                errs.push(format!("task `{}`: invalid cron `{}`", e.id, cron));
            }
        }
        if errs.is_empty() {
            valid.push(e.clone());
        } else {
            errors.extend(errs);
        }
    }
    (valid, errors)
}

/// Offline validation: the errors from [`partition`] (empty ⇒ every entry is valid).
pub fn validate(entries: &[ScheduleEntry], registry: &RecipeRegistry) -> Vec<String> {
    partition(entries, registry).1
}

// ---- reconcile -------------------------------------------------------------

/// What a reconcile pass did, by schedule-entry id.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ReconcileReport {
    pub created: Vec<String>,
    pub updated: Vec<String>,
    pub canceled: Vec<String>,
}

/// Make the queue match the effectively-enabled schedule entries. Runs on daemon boot, on
/// `schedule apply`, and after each enable/disable. Keyed on `dedup_key = "schedule:<id>"`:
/// enable an entry with no live task ⇒ enqueue; a changed enabled entry ⇒ update in place; a
/// disabled entry (or, under `prune`, a deleted one) with a live task ⇒ cancel it.
///
/// Effective-enabled = a runtime override in `schedule_state` if present, else the entry's
/// declared default.
pub fn reconcile(
    store: &Store,
    clock: &dyn Clock,
    entries: &[ScheduleEntry],
    prune: bool,
) -> rusqlite::Result<ReconcileReport> {
    let now = clock.now_ms();
    let overrides = store.schedule_state_all()?;
    let mut report = ReconcileReport::default();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    for e in entries {
        let Some((adapter, action)) = e.route() else {
            continue; // unroutable — validation surfaces this; skip in reconcile
        };
        let dedup = e.dedup_key();
        seen.insert(dedup.clone());
        let effective = overrides.get(&e.id).copied().unwrap_or(e.enabled);
        let existing = store.find_active_by_dedup(&dedup)?;

        match (effective, existing) {
            (true, None) => {
                let scheduled_for = match &e.timing {
                    Timing::At(ms) => *ms,
                    Timing::Every(cron) => next_occurrence_ms(cron, now).unwrap_or(now),
                    Timing::OnApply => now,
                };
                let mut t = Task::new_now(
                    adapter,
                    action,
                    Value::Object(build_params(e)),
                    scheduled_for,
                );
                t.recurrence = recurrence_of(e);
                t.dedup_key = Some(dedup);
                t.priority = e.priority.unwrap_or(0);
                if let Some(m) = e.max_attempts {
                    t.max_attempts = m;
                }
                store.insert_task(&t)?;
                store.append_event(&TaskEvent {
                    task_id: t.id.clone(),
                    at: now,
                    from_status: None,
                    to_status: t.status,
                    detail: serde_json::json!({ "scheduled": e.id }),
                })?;
                report.created.push(e.id.clone());
            }
            (true, Some(mut t)) => {
                let want_params = build_params(e);
                let want_rec = recurrence_of(e);
                let want_prio = e.priority.unwrap_or(0);
                let want_max = e.max_attempts.unwrap_or(t.max_attempts);
                if t.params != Value::Object(want_params.clone())
                    || t.recurrence != want_rec
                    || t.priority != want_prio
                    || t.max_attempts != want_max
                {
                    t.params = Value::Object(want_params);
                    t.recurrence = want_rec;
                    t.priority = want_prio;
                    t.max_attempts = want_max;
                    t.updated_at = now;
                    store.update_task(&t)?;
                    report.updated.push(e.id.clone());
                }
            }
            (false, Some(t)) => {
                cancel(store, t, now, &e.id)?;
                report.canceled.push(e.id.clone());
            }
            (false, None) => {}
        }
    }

    if prune {
        for t in store.list_active_by_dedup_prefix("schedule:")? {
            let dedup = t.dedup_key.clone().unwrap_or_default();
            if !seen.contains(&dedup) {
                let id = dedup
                    .strip_prefix("schedule:")
                    .unwrap_or(&dedup)
                    .to_string();
                cancel(store, t, now, &id)?;
                report.canceled.push(id);
            }
        }
    }

    Ok(report)
}

fn recurrence_of(e: &ScheduleEntry) -> Option<String> {
    match &e.timing {
        Timing::Every(c) => Some(c.clone()),
        _ => None,
    }
}

/// The entry's params as a JSON object, expanding a leading `~/` in string values to $HOME
/// so a recipe receives absolute paths.
fn build_params(e: &ScheduleEntry) -> serde_json::Map<String, Value> {
    let home = std::env::var("HOME").ok();
    e.params
        .iter()
        .map(|(k, v)| {
            let v = match (v.as_str(), &home) {
                (Some(s), Some(h)) if s.starts_with("~/") => {
                    Value::String(format!("{h}/{}", &s[2..]))
                }
                _ => v.clone(),
            };
            (k.clone(), v)
        })
        .collect()
}

fn cancel(store: &Store, mut t: Task, now: i64, entry_id: &str) -> rusqlite::Result<()> {
    let prev = t.status;
    t.status = TaskStatus::Canceled;
    t.finished_at = Some(now);
    t.updated_at = now;
    store.update_task(&t)?;
    store.append_event(&TaskEvent {
        task_id: t.id.clone(),
        at: now,
        from_status: Some(prev),
        to_status: TaskStatus::Canceled,
        detail: serde_json::json!({ "schedule_disabled": entry_id }),
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pacewright_core::clock::TestClock;

    fn registry(recipe: &str) -> RecipeRegistry {
        let dir = std::env::temp_dir().join(format!(
            "pcw-sched-reg-{}-{}",
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

    const RECIPE: &str = r#"recipe "news/hackernews" {
        limit-key "news.hn"
        var "url" required=#true
        var "out_dir"
    }"#;

    fn p(id: &str) -> PathBuf {
        PathBuf::from(format!("/s/{id}.toml"))
    }

    #[test]
    fn parses_every_at_and_on_apply() {
        let text = r#"
            [[task]]
            id = "a"
            recipe = "news/hackernews"
            every = "0 9 * * *"
            params = { url = "u", out_dir = "~/vault" }

            [[task]]
            id = "b"
            recipe = "news/hackernews"
            at = "2026-07-10T09:00:00"
            params = { url = "u" }

            [[task]]
            id = "c"
            recipe = "news/hackernews"
            params = { url = "u" }
            enabled = false
        "#;
        let es = parse_file(text, &p("f")).unwrap();
        assert_eq!(es.len(), 3);
        assert!(matches!(es[0].timing, Timing::Every(_)));
        assert!(matches!(es[1].timing, Timing::At(_)));
        assert_eq!(es[2].timing, Timing::OnApply);
        assert!(!es[2].enabled);
        assert_eq!(es[0].params.get("url").unwrap(), "u");
    }

    #[test]
    fn rejects_every_and_at_together_and_bad_at() {
        assert!(parse_file(
            "[[task]]\nid=\"a\"\nrecipe=\"news/hackernews\"\nevery=\"0 9 * * *\"\nat=\"2026-07-10T09:00:00\"",
            &p("f")
        )
        .is_err());
        assert!(parse_file(
            "[[task]]\nid=\"a\"\nrecipe=\"news/hackernews\"\nat=\"not-a-time\"",
            &p("f")
        )
        .is_err());
    }

    #[test]
    fn validate_catches_dup_id_unknown_recipe_missing_var_bad_cron() {
        let reg = registry(RECIPE);
        let mk = |id: &str, recipe: &str, timing: Timing, params: &[(&str, &str)]| ScheduleEntry {
            id: id.into(),
            recipe: recipe.into(),
            params: params
                .iter()
                .map(|(k, v)| ((*k).to_string(), Value::String((*v).into())))
                .collect(),
            timing,
            priority: None,
            max_attempts: None,
            enabled: true,
            source: p(id),
        };
        let entries = vec![
            mk("dup", "news/hackernews", Timing::OnApply, &[("url", "u")]),
            mk("dup", "news/hackernews", Timing::OnApply, &[("url", "u")]), // duplicate id
            mk("x", "no/such", Timing::OnApply, &[("url", "u")]),           // unknown recipe
            mk("y", "news/hackernews", Timing::OnApply, &[]), // missing required `url`
            mk(
                "z",
                "news/hackernews",
                Timing::Every("nonsense".into()),
                &[("url", "u")],
            ), // bad cron
        ];
        let errs = validate(&entries, &reg);
        assert!(errs.iter().any(|e| e.contains("duplicate")), "{errs:?}");
        assert!(errs.iter().any(|e| e.contains("no recipe")), "{errs:?}");
        assert!(
            errs.iter().any(|e| e.contains("requires param `url`")),
            "{errs:?}"
        );
        assert!(errs.iter().any(|e| e.contains("invalid cron")), "{errs:?}");
    }

    fn entry(id: &str, timing: Timing, enabled: bool) -> ScheduleEntry {
        ScheduleEntry {
            id: id.into(),
            recipe: "news/hackernews".into(),
            params: [("url".to_string(), Value::String("u".into()))]
                .into_iter()
                .collect(),
            timing,
            priority: None,
            max_attempts: None,
            enabled,
            source: p(id),
        }
    }

    #[test]
    fn reconcile_creates_updates_and_cancels() {
        let store = Store::open_in_memory().unwrap();
        let clock = TestClock::new(1_000);

        // enabled entry → created as a live task
        let e = entry("hn", Timing::OnApply, true);
        let r = reconcile(&store, &clock, std::slice::from_ref(&e), false).unwrap();
        assert_eq!(r.created, vec!["hn".to_string()]);
        let t = store.find_active_by_dedup("schedule:hn").unwrap().unwrap();
        assert_eq!(t.adapter, "news");
        assert_eq!(t.action, "hackernews");
        assert_eq!(t.params["url"], "u");

        // re-reconcile unchanged → no-op
        let r = reconcile(&store, &clock, std::slice::from_ref(&e), false).unwrap();
        assert!(r.created.is_empty() && r.updated.is_empty());

        // changed params → update in place
        let mut e2 = e.clone();
        e2.params
            .insert("out_dir".into(), Value::String("/tmp".into()));
        let r = reconcile(&store, &clock, std::slice::from_ref(&e2), false).unwrap();
        assert_eq!(r.updated, vec!["hn".to_string()]);
        assert_eq!(
            store
                .find_active_by_dedup("schedule:hn")
                .unwrap()
                .unwrap()
                .params["out_dir"],
            "/tmp"
        );

        // runtime-disable → cancel the live task
        store.schedule_state_set("hn", false, 2_000).unwrap();
        let r = reconcile(&store, &clock, std::slice::from_ref(&e2), false).unwrap();
        assert_eq!(r.canceled, vec!["hn".to_string()]);
        assert!(store.find_active_by_dedup("schedule:hn").unwrap().is_none());

        // runtime-enable again → re-created
        store.schedule_state_set("hn", true, 3_000).unwrap();
        let r = reconcile(&store, &clock, std::slice::from_ref(&e2), false).unwrap();
        assert_eq!(r.created, vec!["hn".to_string()]);
    }

    #[test]
    fn disabled_by_default_is_not_queued() {
        let store = Store::open_in_memory().unwrap();
        let clock = TestClock::new(1_000);
        let e = entry("off", Timing::OnApply, false);
        let r = reconcile(&store, &clock, &[e], false).unwrap();
        assert!(r.created.is_empty());
        assert!(store
            .find_active_by_dedup("schedule:off")
            .unwrap()
            .is_none());
    }

    #[test]
    fn prune_cancels_removed_entries() {
        let store = Store::open_in_memory().unwrap();
        let clock = TestClock::new(1_000);
        let a = entry("a", Timing::OnApply, true);
        let b = entry("b", Timing::OnApply, true);
        reconcile(&store, &clock, &[a.clone(), b.clone()], false).unwrap();
        assert!(store.find_active_by_dedup("schedule:a").unwrap().is_some());

        // b removed from the files; prune cancels its live task, keeps a
        let r = reconcile(&store, &clock, std::slice::from_ref(&a), true).unwrap();
        assert_eq!(r.canceled, vec!["b".to_string()]);
        assert!(store.find_active_by_dedup("schedule:a").unwrap().is_some());
        assert!(store.find_active_by_dedup("schedule:b").unwrap().is_none());

        // without prune, a stale entry would linger
        let store2 = Store::open_in_memory().unwrap();
        reconcile(&store2, &clock, &[a.clone(), b], false).unwrap();
        reconcile(&store2, &clock, std::slice::from_ref(&a), false).unwrap();
        assert!(store2.find_active_by_dedup("schedule:b").unwrap().is_some());
    }

    #[test]
    fn every_schedules_at_next_cron_slot_with_recurrence() {
        let store = Store::open_in_memory().unwrap();
        let clock = TestClock::new(0);
        let e = entry("cron", Timing::Every("0 0 * * * *".into()), true); // top of each hour
        reconcile(&store, &clock, &[e], false).unwrap();
        let t = store
            .find_active_by_dedup("schedule:cron")
            .unwrap()
            .unwrap();
        assert_eq!(t.recurrence.as_deref(), Some("0 0 * * * *"));
        assert!(
            t.scheduled_for > 0,
            "first run is the next cron slot, not now"
        );
    }
}

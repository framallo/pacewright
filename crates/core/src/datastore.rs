//! A tiny JSON dataset store — pacewright's answer to "save the data, not as a CSV".
//!
//! Each dataset is one JSON file under `~/.pacewright/data/<name>.json` holding an **array of
//! objects**. [`append`] adds rows, de-duplicating on a chosen key field so a job can run daily and
//! accumulate a growing, resumable set (the x-harvest handle pool, the Apollo host list, the
//! personalize-hooks output) without the `sort -u` / grep-the-CSV dance every script reimplemented.
//!
//! Deliberately not SQLite: these are append-mostly, human-readable, git-diffable exports that other
//! tools (and the operator) read directly. The engine's durable state stays in the SQLite store;
//! this is for *task output data*.
//!
//! Clock-free and dependency-light: writes are atomic (temp + rename). A dataset name is validated
//! to a safe slug so a task param can't escape the data dir.
use serde_json::Value;
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum DataError {
    #[error("invalid dataset name `{0}` (use letters, digits, -_. and /)")]
    BadName(String),
    #[error("io: {0}")]
    Io(String),
    #[error("dataset {0} is not a JSON array")]
    NotArray(String),
}

/// A JSON-file dataset store rooted at a directory.
#[derive(Debug, Clone)]
pub struct Datastore {
    dir: PathBuf,
}

/// Outcome of an [`Datastore::append`]: how many rows were new vs already present, and the new total.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AppendReport {
    pub added: usize,
    pub duplicates: usize,
    pub total: usize,
}

/// A dataset name is a safe relative slug: segments of `[A-Za-z0-9._-]`, optionally `/`-nested, no
/// `..`, no leading `/`. Keeps a task param from writing outside the data dir.
fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('/')
        && name.split('/').all(|seg| {
            !seg.is_empty()
                && seg != ".."
                && seg
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        })
}

impl Datastore {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    fn path(&self, name: &str) -> Result<PathBuf, DataError> {
        if !valid_name(name) {
            return Err(DataError::BadName(name.to_string()));
        }
        Ok(self.dir.join(format!("{name}.json")))
    }

    /// Read a dataset's rows (empty if it doesn't exist yet).
    pub fn read(&self, name: &str) -> Result<Vec<Value>, DataError> {
        let path = self.path(name)?;
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(DataError::Io(e.to_string())),
        };
        match serde_json::from_slice::<Value>(&bytes) {
            Ok(Value::Array(a)) => Ok(a),
            Ok(_) => Err(DataError::NotArray(name.to_string())),
            Err(e) => Err(DataError::Io(format!("parsing {}: {e}", path.display()))),
        }
    }

    pub fn count(&self, name: &str) -> Result<usize, DataError> {
        Ok(self.read(name)?.len())
    }

    /// Every dataset in the store as `(name, row_count)`, sorted by name. Names are the `/`-nested
    /// slugs (file stems), so `leads/warm.json` lists as `leads/warm`.
    pub fn list(&self) -> Vec<(String, usize)> {
        fn walk(base: &Path, dir: &Path, out: &mut Vec<(String, usize)>) {
            let Ok(rd) = std::fs::read_dir(dir) else {
                return;
            };
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    walk(base, &p, out);
                } else if p.extension().is_some_and(|x| x == "json") {
                    if let Ok(rel) = p.strip_prefix(base) {
                        let name = rel.with_extension("").to_string_lossy().replace('\\', "/");
                        let count = std::fs::read(&p)
                            .ok()
                            .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
                            .and_then(|v| v.as_array().map(|a| a.len()))
                            .unwrap_or(0);
                        out.push((name, count));
                    }
                }
            }
        }
        let mut out = Vec::new();
        walk(&self.dir, &self.dir, &mut out);
        out.sort();
        out
    }

    /// Append `items` to a dataset, de-duplicating. With `key = Some(field)`, two rows collide when
    /// that field's value matches (an existing row or another new one); with `key = None`, on full
    /// value equality. Existing rows win; among new rows the first occurrence wins. Idempotent, so a
    /// re-run of the same producer adds nothing. Returns what changed.
    pub fn append(
        &self,
        name: &str,
        items: &[Value],
        key: Option<&str>,
    ) -> Result<AppendReport, DataError> {
        let path = self.path(name)?;
        let mut rows = self.read(name)?;

        // Seed the seen-set from existing rows so we never re-add.
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        for r in &rows {
            if let Some(k) = dedup_token(r, key) {
                seen.insert(k);
            }
        }

        let (mut added, mut duplicates) = (0usize, 0usize);
        for item in items {
            match dedup_token(item, key) {
                Some(tok) => {
                    if seen.insert(tok) {
                        rows.push(item.clone());
                        added += 1;
                    } else {
                        duplicates += 1;
                    }
                }
                // A keyed row missing the key field is dropped rather than silently un-deduped.
                None if key.is_some() => duplicates += 1,
                None => {
                    rows.push(item.clone());
                    added += 1;
                }
            }
        }

        if added > 0 {
            write_atomic(&path, &Value::Array(rows.clone()))?;
        }
        Ok(AppendReport {
            added,
            duplicates,
            total: rows.len(),
        })
    }
}

/// The dedup token for a row: the chosen key field rendered to a stable string, or (no key) the
/// whole value. `None` means a keyed row lacked the field.
fn dedup_token(v: &Value, key: Option<&str>) -> Option<String> {
    match key {
        None => Some(v.to_string()),
        Some(k) => v.get(k).map(|f| match f {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        }),
    }
}
fn write_atomic(path: &Path, value: &Value) -> Result<(), DataError> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| DataError::Io(e.to_string()))?;
    }
    let tmp = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec_pretty(value).map_err(|e| DataError::Io(e.to_string()))?;
    std::fs::write(&tmp, bytes).map_err(|e| DataError::Io(e.to_string()))?;
    std::fs::rename(&tmp, path).map_err(|e| DataError::Io(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tmp() -> Datastore {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let id = N.fetch_add(1, Ordering::Relaxed);
        let d = std::env::temp_dir().join(format!("pw-data-{}-{id}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        Datastore::new(d)
    }

    #[test]
    fn append_dedups_on_key_across_calls() {
        let ds = tmp();
        let r1 = ds
            .append(
                "demo/pool",
                &[json!({"h": "a"}), json!({"h": "b"})],
                Some("h"),
            )
            .unwrap();
        assert_eq!((r1.added, r1.total), (2, 2));
        // Re-run with an overlap: only the new one lands.
        let r2 = ds
            .append(
                "demo/pool",
                &[json!({"h": "b"}), json!({"h": "c"})],
                Some("h"),
            )
            .unwrap();
        assert_eq!((r2.added, r2.duplicates, r2.total), (1, 1, 3));
        let rows = ds.read("demo/pool").unwrap();
        let hs: Vec<&str> = rows.iter().filter_map(|r| r["h"].as_str()).collect();
        assert_eq!(hs, vec!["a", "b", "c"]);
    }

    #[test]
    fn dedups_within_one_batch_too() {
        let ds = tmp();
        let r = ds
            .append("d", &[json!({"h": "a"}), json!({"h": "a"})], Some("h"))
            .unwrap();
        assert_eq!((r.added, r.duplicates), (1, 1));
    }

    #[test]
    fn no_key_dedups_on_whole_value() {
        let ds = tmp();
        let r = ds
            .append(
                "d",
                &[json!({"x": 1}), json!({"x": 1}), json!({"x": 2})],
                None,
            )
            .unwrap();
        assert_eq!(r.added, 2);
    }

    #[test]
    fn read_missing_is_empty_and_list_reports_counts() {
        let ds = tmp();
        assert!(ds.read("nope").unwrap().is_empty());
        ds.append("a", &[json!({"k": 1})], Some("k")).unwrap();
        ds.append("nested/b", &[json!({"k": 1}), json!({"k": 2})], Some("k"))
            .unwrap();
        let mut listed = ds.list();
        listed.sort();
        assert_eq!(
            listed,
            vec![("a".to_string(), 1), ("nested/b".to_string(), 2)]
        );
    }

    #[test]
    fn rejects_unsafe_names() {
        let ds = tmp();
        assert!(ds.read("../escape").is_err());
        assert!(ds.append("/abs", &[], None).is_err());
        assert!(ds.read("a/../b").is_err());
    }
}

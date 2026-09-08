use crate::model::{Task, TaskEvent, TaskStatus};
use parking_lot::Mutex;
use rusqlite::{params, Connection, OptionalExtension};

pub struct Store {
    conn: Mutex<Connection>,
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS tasks (
    id TEXT PRIMARY KEY,
    adapter TEXT NOT NULL,
    action TEXT NOT NULL,
    params TEXT NOT NULL,
    status TEXT NOT NULL,
    scheduled_for INTEGER NOT NULL,
    next_eligible_at INTEGER,
    priority INTEGER NOT NULL DEFAULT 0,
    recurrence TEXT,
    depends_on TEXT,
    dedup_key TEXT,
    run_id TEXT,
    step_name TEXT,
    pace_ms INTEGER,
    dep_on_failure INTEGER NOT NULL DEFAULT 0,
    escalation TEXT,
    pause_scope_on_failure INTEGER NOT NULL DEFAULT 0,
    attempts INTEGER NOT NULL DEFAULT 0,
    max_attempts INTEGER NOT NULL DEFAULT 3,
    last_error TEXT,
    result TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    finished_at INTEGER
);
CREATE INDEX IF NOT EXISTS idx_tasks_status ON tasks(status);
CREATE INDEX IF NOT EXISTS idx_tasks_dedup ON tasks(dedup_key) WHERE dedup_key IS NOT NULL;
CREATE TABLE IF NOT EXISTS task_events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    task_id TEXT NOT NULL,
    at INTEGER NOT NULL,
    from_status TEXT,
    to_status TEXT NOT NULL,
    detail TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_events_task ON task_events(task_id);
CREATE TABLE IF NOT EXISTS limit_counters (
    limit_key TEXT NOT NULL,
    window_date TEXT NOT NULL,
    count INTEGER NOT NULL DEFAULT 0,
    last_spent_at INTEGER,
    PRIMARY KEY (limit_key, window_date)
);
-- Runtime enable/disable overrides for schedule entries (id = the schedule entry id).
-- The schedule file declares a default; a row here wins over it until changed.
CREATE TABLE IF NOT EXISTS schedule_state (
    entry_id TEXT PRIMARY KEY,
    enabled INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
-- Runtime pacing overrides that win over config.toml until changed (set_limit).
CREATE TABLE IF NOT EXISTS limit_overrides (
    limit_key TEXT PRIMARY KEY,
    daily_cap INTEGER NOT NULL,
    min_gap_ms INTEGER NOT NULL,
    jitter REAL NOT NULL,
    active_start_min INTEGER NOT NULL,
    active_end_min INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
-- Paused scopes, persisted so a daemon RESTART re-asserts them instead of silently resuming.
-- `all` pauses the whole engine; any other value pauses that adapter.
CREATE TABLE IF NOT EXISTS paused_scopes (
    scope TEXT PRIMARY KEY NOT NULL
);
-- All-time dedup ledger (R7): a target, once acted on within a scope, is never touched again.
-- Replaces the per-script text ledgers (mm-pitched-all.log, .apollo-revealed.txt, "grep the CSV").
CREATE TABLE IF NOT EXISTS touched (
    scope TEXT NOT NULL,
    target_id TEXT NOT NULL,
    first_touched_at INTEGER NOT NULL,
    PRIMARY KEY (scope, target_id)
);
"#;

/// `CREATE TABLE IF NOT EXISTS` will not alter an existing `tasks` table, so add the
/// run columns when they are missing. Swallowing "duplicate column name" is the
/// idiomatic sqlite way to make an additive migration idempotent.
fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    for ddl in [
        "ALTER TABLE tasks ADD COLUMN run_id TEXT",
        "ALTER TABLE tasks ADD COLUMN step_name TEXT",
        "ALTER TABLE tasks ADD COLUMN pace_ms INTEGER",
        "ALTER TABLE tasks ADD COLUMN dep_on_failure INTEGER NOT NULL DEFAULT 0",
        "ALTER TABLE tasks ADD COLUMN escalation TEXT",
        "ALTER TABLE tasks ADD COLUMN pause_scope_on_failure INTEGER NOT NULL DEFAULT 0",
        "ALTER TABLE tasks ADD COLUMN fanout TEXT",
        "ALTER TABLE tasks ADD COLUMN touch_scope TEXT",
        "ALTER TABLE tasks ADD COLUMN touch_id TEXT",
    ] {
        match conn.execute(ddl, []) {
            Ok(_) => {}
            Err(e) if e.to_string().contains("duplicate column name") => {}
            Err(e) => return Err(e),
        }
    }
    conn.execute(
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_tasks_run_step \
         ON tasks(run_id, step_name) WHERE run_id IS NOT NULL",
        [],
    )?;
    Ok(())
}

impl Store {
    pub fn open(path: &str) -> rusqlite::Result<Store> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.execute_batch(SCHEMA)?;
        migrate(&conn)?;
        Ok(Store {
            conn: Mutex::new(conn),
        })
    }
    pub fn open_in_memory() -> rusqlite::Result<Store> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        migrate(&conn)?;
        Ok(Store {
            conn: Mutex::new(conn),
        })
    }

    fn row_to_task(row: &rusqlite::Row) -> rusqlite::Result<Task> {
        let status: String = row.get("status")?;
        let params: String = row.get("params")?;
        let result: Option<String> = row.get("result")?;
        Ok(Task {
            id: row.get("id")?,
            adapter: row.get("adapter")?,
            action: row.get("action")?,
            params: serde_json::from_str(&params).unwrap_or(serde_json::Value::Null),
            status: status_from_str(&status),
            scheduled_for: row.get("scheduled_for")?,
            next_eligible_at: row.get("next_eligible_at")?,
            priority: row.get("priority")?,
            recurrence: row.get("recurrence")?,
            depends_on: row.get("depends_on")?,
            dedup_key: row.get("dedup_key")?,
            run_id: row.get("run_id")?,
            step_name: row.get("step_name")?,
            pace_ms: row.get("pace_ms")?,
            dep_on_failure: row.get::<_, i64>("dep_on_failure")? != 0,
            escalation: row.get("escalation")?,
            pause_scope_on_failure: row.get::<_, i64>("pause_scope_on_failure")? != 0,
            fanout: row
                .get::<_, Option<String>>("fanout")?
                .and_then(|s| serde_json::from_str(&s).ok()),
            touch_scope: row.get("touch_scope")?,
            touch_id: row.get("touch_id")?,
            attempts: row.get("attempts")?,
            max_attempts: row.get("max_attempts")?,
            last_error: row.get("last_error")?,
            result: result.and_then(|s| serde_json::from_str(&s).ok()),
            created_at: row.get("created_at")?,
            updated_at: row.get("updated_at")?,
            finished_at: row.get("finished_at")?,
        })
    }

    pub fn insert_task(&self, t: &Task) -> rusqlite::Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO tasks (id,adapter,action,params,status,scheduled_for,next_eligible_at,priority,recurrence,depends_on,dedup_key,run_id,step_name,pace_ms,dep_on_failure,escalation,attempts,max_attempts,last_error,result,created_at,updated_at,finished_at,pause_scope_on_failure,fanout,touch_scope,touch_id)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23,?24,?25,?26,?27)",
            params![
                t.id, t.adapter, t.action, serde_json::to_string(&t.params).unwrap(),
                t.status.as_str(), t.scheduled_for, t.next_eligible_at, t.priority,
                t.recurrence, t.depends_on, t.dedup_key, t.run_id, t.step_name, t.pace_ms,
                t.dep_on_failure as i64, t.escalation, t.attempts, t.max_attempts,
                t.last_error, t.result.as_ref().map(|v| serde_json::to_string(v).unwrap()),
                t.created_at, t.updated_at, t.finished_at, t.pause_scope_on_failure as i64,
                t.fanout.as_ref().map(|v| serde_json::to_string(v).unwrap()),
                t.touch_scope, t.touch_id
            ],
        )?;
        Ok(())
    }

    pub fn update_task(&self, t: &Task) -> rusqlite::Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "UPDATE tasks SET adapter=?2,action=?3,params=?4,status=?5,scheduled_for=?6,next_eligible_at=?7,priority=?8,recurrence=?9,depends_on=?10,dedup_key=?11,run_id=?12,step_name=?13,pace_ms=?14,dep_on_failure=?15,escalation=?16,attempts=?17,max_attempts=?18,last_error=?19,result=?20,created_at=?21,updated_at=?22,finished_at=?23,pause_scope_on_failure=?24,fanout=?25,touch_scope=?26,touch_id=?27 WHERE id=?1",
            params![
                t.id, t.adapter, t.action, serde_json::to_string(&t.params).unwrap(),
                t.status.as_str(), t.scheduled_for, t.next_eligible_at, t.priority,
                t.recurrence, t.depends_on, t.dedup_key, t.run_id, t.step_name, t.pace_ms,
                t.dep_on_failure as i64, t.escalation, t.attempts, t.max_attempts,
                t.last_error, t.result.as_ref().map(|v| serde_json::to_string(v).unwrap()),
                t.created_at, t.updated_at, t.finished_at, t.pause_scope_on_failure as i64,
                t.fanout.as_ref().map(|v| serde_json::to_string(v).unwrap()),
                t.touch_scope, t.touch_id
            ],
        )?;
        Ok(())
    }

    /// All-time dedup ledger (R7): has `(scope, target_id)` ever been acted on?
    pub fn is_touched(&self, scope: &str, target_id: &str) -> rusqlite::Result<bool> {
        let conn = self.conn.lock();
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM touched WHERE scope=?1 AND target_id=?2",
            params![scope, target_id],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    /// Record `(scope, target_id)` as touched. Idempotent (`INSERT OR IGNORE`); returns whether
    /// this call was the first touch (a new row), so a caller can log the transition once.
    pub fn mark_touched(
        &self,
        scope: &str,
        target_id: &str,
        now_ms: i64,
    ) -> rusqlite::Result<bool> {
        let conn = self.conn.lock();
        let n = conn.execute(
            "INSERT OR IGNORE INTO touched(scope, target_id, first_touched_at) VALUES (?1,?2,?3)",
            params![scope, target_id, now_ms],
        )?;
        Ok(n > 0)
    }

    /// How many targets have been touched in a scope (for the digest / caps introspection).
    pub fn touched_count(&self, scope: &str) -> rusqlite::Result<i64> {
        let conn = self.conn.lock();
        conn.query_row(
            "SELECT COUNT(*) FROM touched WHERE scope=?1",
            params![scope],
            |r| r.get(0),
        )
    }

    /// Every scope in the ledger with its touched-target count, sorted by scope. Powers the
    /// dashboard's Ledger pane and `digest` introspection.
    pub fn touched_scopes(&self) -> rusqlite::Result<Vec<(String, i64)>> {
        let conn = self.conn.lock();
        let mut stmt =
            conn.prepare("SELECT scope, COUNT(*) FROM touched GROUP BY scope ORDER BY scope")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
        rows.collect()
    }

    /// Persist a paused scope (`INSERT OR IGNORE`, idempotent). Persisted, not just in memory, so a
    /// daemon restart re-asserts it rather than silently resuming — the Jul-23 incident.
    pub fn pause_scope(&self, scope: &str) -> rusqlite::Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT OR IGNORE INTO paused_scopes(scope) VALUES (?1)",
            params![scope],
        )?;
        Ok(())
    }

    /// Remove a paused scope (resume). No-op if absent.
    pub fn resume_scope(&self, scope: &str) -> rusqlite::Result<()> {
        let conn = self.conn.lock();
        conn.execute("DELETE FROM paused_scopes WHERE scope=?1", params![scope])?;
        Ok(())
    }

    /// Every currently-paused scope, sorted for stable output.
    pub fn paused_scopes(&self) -> rusqlite::Result<Vec<String>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare("SELECT scope FROM paused_scopes ORDER BY scope")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        rows.collect()
    }

    /// Find a task by dedup key INCLUDING terminal ones. `find_active_by_dedup`
    /// deliberately excludes terminal tasks so a finished recurrence can be re-queued;
    /// resume needs the opposite, to see that a step already succeeded.
    pub fn find_by_dedup_any(&self, key: &str) -> rusqlite::Result<Option<Task>> {
        let conn = self.conn.lock();
        conn.query_row(
            "SELECT * FROM tasks WHERE dedup_key=?1 LIMIT 1",
            params![key],
            Self::row_to_task,
        )
        .optional()
    }

    pub fn tasks_in_run(&self, run_id: &str) -> rusqlite::Result<Vec<Task>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare("SELECT * FROM tasks WHERE run_id=?1 ORDER BY created_at")?;
        let rows = stmt.query_map(params![run_id], Self::row_to_task)?;
        rows.collect()
    }

    pub fn get_task(&self, id: &str) -> rusqlite::Result<Option<Task>> {
        let conn = self.conn.lock();
        conn.query_row(
            "SELECT * FROM tasks WHERE id=?1",
            params![id],
            Self::row_to_task,
        )
        .optional()
    }

    pub fn list_tasks(
        &self,
        status: Option<TaskStatus>,
        limit: i64,
    ) -> rusqlite::Result<Vec<Task>> {
        let conn = self.conn.lock();
        let mut out = Vec::new();
        match status {
            Some(s) => {
                let mut stmt = conn.prepare(
                    "SELECT * FROM tasks WHERE status=?1 ORDER BY created_at DESC LIMIT ?2",
                )?;
                let rows = stmt.query_map(params![s.as_str(), limit], Self::row_to_task)?;
                for r in rows {
                    out.push(r?);
                }
            }
            None => {
                let mut stmt =
                    conn.prepare("SELECT * FROM tasks ORDER BY created_at DESC LIMIT ?1")?;
                let rows = stmt.query_map(params![limit], Self::row_to_task)?;
                for r in rows {
                    out.push(r?);
                }
            }
        }
        Ok(out)
    }

    pub fn tasks_in_status(&self, status: TaskStatus) -> rusqlite::Result<Vec<Task>> {
        self.list_tasks(Some(status), i64::MAX)
    }

    pub fn find_active_by_dedup(&self, key: &str) -> rusqlite::Result<Option<Task>> {
        let conn = self.conn.lock();
        // Exclude terminal statuses derived from `TaskStatus::is_terminal` (single
        // source of truth) rather than hardcoding the set in SQL — add a status and
        // this stays correct. Bind `key` + each terminal string positionally.
        let terminal = TaskStatus::terminal_strs();
        let placeholders = terminal.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let sql = format!(
            "SELECT * FROM tasks WHERE dedup_key=? AND status NOT IN ({placeholders}) LIMIT 1"
        );
        let mut binds: Vec<&str> = Vec::with_capacity(1 + terminal.len());
        binds.push(key);
        binds.extend(terminal.iter().copied());
        conn.query_row(&sql, rusqlite::params_from_iter(binds), Self::row_to_task)
            .optional()
    }

    pub fn append_event(&self, e: &TaskEvent) -> rusqlite::Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO task_events (task_id,at,from_status,to_status,detail) VALUES (?1,?2,?3,?4,?5)",
            params![e.task_id, e.at, e.from_status.map(|s| s.as_str()), e.to_status.as_str(), serde_json::to_string(&e.detail).unwrap()],
        )?;
        Ok(())
    }

    pub fn events_for(&self, task_id: &str) -> rusqlite::Result<Vec<TaskEvent>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare("SELECT task_id,at,from_status,to_status,detail FROM task_events WHERE task_id=?1 ORDER BY id ASC")?;
        let rows = stmt.query_map(params![task_id], |row| {
            let from: Option<String> = row.get("from_status")?;
            let to: String = row.get("to_status")?;
            let detail: String = row.get("detail")?;
            Ok(TaskEvent {
                task_id: row.get("task_id")?,
                at: row.get("at")?,
                from_status: from.map(|s| status_from_str(&s)),
                to_status: status_from_str(&to),
                detail: serde_json::from_str(&detail).unwrap_or(serde_json::Value::Null),
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn counter_get(&self, key: &str, date: &str) -> rusqlite::Result<(i64, Option<i64>)> {
        let conn = self.conn.lock();
        let res = conn.query_row(
            "SELECT count,last_spent_at FROM limit_counters WHERE limit_key=?1 AND window_date=?2",
            params![key, date], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Option<i64>>(1)?)),
        ).optional()?;
        Ok(res.unwrap_or((0, None)))
    }

    pub fn counter_spend(&self, key: &str, date: &str, at_ms: i64) -> rusqlite::Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO limit_counters (limit_key,window_date,count,last_spent_at) VALUES (?1,?2,1,?3)
             ON CONFLICT(limit_key,window_date) DO UPDATE SET count=count+1, last_spent_at=?3",
            params![key, date, at_ms],
        )?;
        Ok(())
    }

    /// All limit counters recorded for a given local date, ordered by key.
    /// Returns `(limit_key, count, last_spent_at)` tuples.
    pub fn list_counters(&self, date: &str) -> rusqlite::Result<Vec<(String, i64, Option<i64>)>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT limit_key, count, last_spent_at FROM limit_counters WHERE window_date=?1 ORDER BY limit_key",
        )?;
        let rows = stmt.query_map(params![date], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, Option<i64>>(2)?,
            ))
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    // ---- schedule enable/disable overrides ----------------------------------

    /// Upsert the runtime enabled override for a schedule entry.
    pub fn schedule_state_set(
        &self,
        entry_id: &str,
        enabled: bool,
        at_ms: i64,
    ) -> rusqlite::Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO schedule_state (entry_id,enabled,updated_at) VALUES (?1,?2,?3)
             ON CONFLICT(entry_id) DO UPDATE SET enabled=?2, updated_at=?3",
            params![entry_id, i64::from(enabled), at_ms],
        )?;
        Ok(())
    }

    /// Every runtime enabled override, keyed by entry id.
    pub fn schedule_state_all(&self) -> rusqlite::Result<std::collections::HashMap<String, bool>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare("SELECT entry_id, enabled FROM schedule_state")?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? != 0))
        })?;
        let mut out = std::collections::HashMap::new();
        for r in rows {
            let (k, v) = r?;
            out.insert(k, v);
        }
        Ok(out)
    }

    // ---- runtime pacing overrides (set_limit) -------------------------------

    /// Upsert a runtime pacing override for a limit key.
    pub fn limit_override_set(
        &self,
        key: &str,
        cfg: &crate::config::LimitConfig,
        at_ms: i64,
    ) -> rusqlite::Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO limit_overrides (limit_key,daily_cap,min_gap_ms,jitter,active_start_min,active_end_min,updated_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7)
             ON CONFLICT(limit_key) DO UPDATE SET daily_cap=?2,min_gap_ms=?3,jitter=?4,active_start_min=?5,active_end_min=?6,updated_at=?7",
            params![key, cfg.daily_cap, cfg.min_gap_ms, cfg.jitter, cfg.active_start_min, cfg.active_end_min, at_ms],
        )?;
        Ok(())
    }

    /// Every runtime pacing override, as `(key, config)` pairs.
    pub fn limit_overrides_all(
        &self,
    ) -> rusqlite::Result<Vec<(String, crate::config::LimitConfig)>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT limit_key,daily_cap,min_gap_ms,jitter,active_start_min,active_end_min FROM limit_overrides ORDER BY limit_key",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                crate::config::LimitConfig {
                    daily_cap: r.get(1)?,
                    min_gap_ms: r.get(2)?,
                    jitter: r.get(3)?,
                    active_start_min: r.get(4)?,
                    active_end_min: r.get(5)?,
                    // spread is a config.toml-only feature and is not persisted in the override
                    // table; runtime overrides carry no spread. Default to off.
                    spread_ms: 0,
                },
            ))
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Active (non-terminal) tasks whose `dedup_key` starts with `prefix` — the schedule
    /// reconciler uses this (prefix `"schedule:"`) to find its live tasks for pruning.
    pub fn list_active_by_dedup_prefix(&self, prefix: &str) -> rusqlite::Result<Vec<Task>> {
        let conn = self.conn.lock();
        let terminal = TaskStatus::terminal_strs();
        let placeholders = terminal.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let sql = format!(
            "SELECT * FROM tasks WHERE dedup_key LIKE ? || '%' AND status NOT IN ({placeholders}) ORDER BY dedup_key"
        );
        let mut stmt = conn.prepare(&sql)?;
        let mut binds: Vec<&str> = Vec::with_capacity(1 + terminal.len());
        binds.push(prefix);
        binds.extend(terminal.iter().copied());
        let rows = stmt.query_map(rusqlite::params_from_iter(binds), Self::row_to_task)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }
}

fn status_from_str(s: &str) -> TaskStatus {
    match s {
        "pending" => TaskStatus::Pending,
        "blocked" => TaskStatus::Blocked,
        "deferred" => TaskStatus::Deferred,
        "running" => TaskStatus::Running,
        "succeeded" => TaskStatus::Succeeded,
        "failed" => TaskStatus::Failed,
        "canceled" => TaskStatus::Canceled,
        _ => TaskStatus::Pending,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Task;

    #[test]
    fn test_insert_get_update_roundtrip() {
        let s = Store::open_in_memory().unwrap();
        let mut t = Task::new_now("dummy", "echo", serde_json::json!({"x":1}), 1000);
        s.insert_task(&t).unwrap();
        let got = s.get_task(&t.id).unwrap().unwrap();
        assert_eq!(got.action, "echo");
        t.status = TaskStatus::Succeeded;
        t.result = Some(serde_json::json!({"ok":true}));
        s.update_task(&t).unwrap();
        let got2 = s.get_task(&t.id).unwrap().unwrap();
        assert_eq!(got2.status, TaskStatus::Succeeded);
        assert_eq!(got2.result, Some(serde_json::json!({"ok":true})));
    }

    #[test]
    fn test_dedup_only_matches_active() {
        let s = Store::open_in_memory().unwrap();
        let mut t = Task::new_now("dummy", "echo", serde_json::json!({}), 1000);
        t.dedup_key = Some("k1".into());
        s.insert_task(&t).unwrap();
        assert!(s.find_active_by_dedup("k1").unwrap().is_some());
        t.status = TaskStatus::Succeeded;
        s.update_task(&t).unwrap();
        assert!(s.find_active_by_dedup("k1").unwrap().is_none());
    }

    #[test]
    fn test_counter_spend_and_rollover() {
        let s = Store::open_in_memory().unwrap();
        s.counter_spend("acme.post", "2026-07-07", 100).unwrap();
        s.counter_spend("acme.post", "2026-07-07", 200).unwrap();
        let (c, last) = s.counter_get("acme.post", "2026-07-07").unwrap();
        assert_eq!(c, 2);
        assert_eq!(last, Some(200));
        // different day is a fresh counter
        assert_eq!(s.counter_get("acme.post", "2026-07-08").unwrap(), (0, None));
    }

    #[test]
    fn test_list_counters_for_date() {
        let s = Store::open_in_memory().unwrap();
        s.counter_spend("acme.post", "2026-07-07", 100).unwrap();
        s.counter_spend("acme.post", "2026-07-07", 200).unwrap();
        s.counter_spend("email.send", "2026-07-07", 150).unwrap();
        s.counter_spend("email.send", "2026-07-08", 50).unwrap();

        let rows = s.list_counters("2026-07-07").unwrap();
        assert_eq!(rows.len(), 2);
        // ordered by limit_key
        assert_eq!(rows[0], ("acme.post".to_string(), 2, Some(200)));
        assert_eq!(rows[1], ("email.send".to_string(), 1, Some(150)));

        let other_day = s.list_counters("2026-07-08").unwrap();
        assert_eq!(other_day, vec![("email.send".to_string(), 1, Some(50))]);

        assert!(s.list_counters("2026-01-01").unwrap().is_empty());
    }

    #[test]
    fn test_schedule_state_override_roundtrip() {
        let s = Store::open_in_memory().unwrap();
        assert!(s.schedule_state_all().unwrap().is_empty());
        s.schedule_state_set("hn-digest", false, 100).unwrap();
        s.schedule_state_set("scrape-jane", true, 100).unwrap();
        s.schedule_state_set("hn-digest", true, 200).unwrap(); // upsert wins
        let all = s.schedule_state_all().unwrap();
        assert_eq!(all.get("hn-digest"), Some(&true));
        assert_eq!(all.get("scrape-jane"), Some(&true));
    }

    #[test]
    fn test_limit_override_roundtrip() {
        use crate::config::LimitConfig;
        let s = Store::open_in_memory().unwrap();
        let cfg = LimitConfig {
            daily_cap: 5,
            min_gap_ms: 60_000,
            jitter: 0.25,
            active_start_min: 540,
            active_end_min: 1080,
            spread_ms: 0,
        };
        s.limit_override_set("acme.post", &cfg, 100).unwrap();
        let all = s.limit_overrides_all().unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].0, "acme.post");
        assert_eq!(all[0].1, cfg);
        // upsert replaces
        let cfg2 = LimitConfig {
            daily_cap: 10,
            ..cfg
        };
        s.limit_override_set("acme.post", &cfg2, 200).unwrap();
        assert_eq!(s.limit_overrides_all().unwrap()[0].1.daily_cap, 10);
    }

    #[test]
    fn test_active_by_dedup_prefix() {
        let s = Store::open_in_memory().unwrap();
        let mut a = Task::new_now("dummy", "echo", serde_json::json!({}), 1000);
        a.dedup_key = Some("schedule:one".into());
        s.insert_task(&a).unwrap();
        let mut b = Task::new_now("dummy", "echo", serde_json::json!({}), 1000);
        b.dedup_key = Some("schedule:two".into());
        s.insert_task(&b).unwrap();
        let mut other = Task::new_now("dummy", "echo", serde_json::json!({}), 1000);
        other.dedup_key = Some("job:x".into());
        s.insert_task(&other).unwrap();

        let mut found = s.list_active_by_dedup_prefix("schedule:").unwrap();
        found.sort_by(|x, y| x.dedup_key.cmp(&y.dedup_key));
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].dedup_key.as_deref(), Some("schedule:one"));

        // terminal tasks drop out of the active set
        b.status = TaskStatus::Canceled;
        s.update_task(&b).unwrap();
        assert_eq!(s.list_active_by_dedup_prefix("schedule:").unwrap().len(), 1);
    }

    #[test]
    fn test_events_ordered() {
        let s = Store::open_in_memory().unwrap();
        let t = Task::new_now("dummy", "echo", serde_json::json!({}), 1000);
        s.insert_task(&t).unwrap();
        s.append_event(&TaskEvent {
            task_id: t.id.clone(),
            at: 1,
            from_status: None,
            to_status: TaskStatus::Pending,
            detail: serde_json::json!({}),
        })
        .unwrap();
        s.append_event(&TaskEvent {
            task_id: t.id.clone(),
            at: 2,
            from_status: Some(TaskStatus::Pending),
            to_status: TaskStatus::Running,
            detail: serde_json::json!({}),
        })
        .unwrap();
        let evs = s.events_for(&t.id).unwrap();
        assert_eq!(evs.len(), 2);
        assert_eq!(evs[1].to_status, TaskStatus::Running);
    }
}

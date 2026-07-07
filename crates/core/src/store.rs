use crate::model::{Task, TaskEvent, TaskStatus};
use rusqlite::{params, Connection, OptionalExtension};
use std::sync::Mutex;

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
    attempts INTEGER NOT NULL DEFAULT 0,
    max_attempts INTEGER NOT NULL DEFAULT 3,
    last_error TEXT,
    result TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    finished_at INTEGER
);
CREATE INDEX IF NOT EXISTS idx_tasks_status ON tasks(status);
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
"#;

impl Store {
    pub fn open(path: &str) -> rusqlite::Result<Store> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.execute_batch(SCHEMA)?;
        Ok(Store { conn: Mutex::new(conn) })
    }
    pub fn open_in_memory() -> rusqlite::Result<Store> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        Ok(Store { conn: Mutex::new(conn) })
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
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO tasks (id,adapter,action,params,status,scheduled_for,next_eligible_at,priority,recurrence,depends_on,dedup_key,attempts,max_attempts,last_error,result,created_at,updated_at,finished_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18)",
            params![
                t.id, t.adapter, t.action, serde_json::to_string(&t.params).unwrap(),
                t.status.as_str(), t.scheduled_for, t.next_eligible_at, t.priority,
                t.recurrence, t.depends_on, t.dedup_key, t.attempts, t.max_attempts,
                t.last_error, t.result.as_ref().map(|v| serde_json::to_string(v).unwrap()),
                t.created_at, t.updated_at, t.finished_at
            ],
        )?;
        Ok(())
    }

    pub fn update_task(&self, t: &Task) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE tasks SET adapter=?2,action=?3,params=?4,status=?5,scheduled_for=?6,next_eligible_at=?7,priority=?8,recurrence=?9,depends_on=?10,dedup_key=?11,attempts=?12,max_attempts=?13,last_error=?14,result=?15,created_at=?16,updated_at=?17,finished_at=?18 WHERE id=?1",
            params![
                t.id, t.adapter, t.action, serde_json::to_string(&t.params).unwrap(),
                t.status.as_str(), t.scheduled_for, t.next_eligible_at, t.priority,
                t.recurrence, t.depends_on, t.dedup_key, t.attempts, t.max_attempts,
                t.last_error, t.result.as_ref().map(|v| serde_json::to_string(v).unwrap()),
                t.created_at, t.updated_at, t.finished_at
            ],
        )?;
        Ok(())
    }

    pub fn get_task(&self, id: &str) -> rusqlite::Result<Option<Task>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row("SELECT * FROM tasks WHERE id=?1", params![id], Self::row_to_task).optional()
    }

    pub fn list_tasks(&self, status: Option<TaskStatus>, limit: i64) -> rusqlite::Result<Vec<Task>> {
        let conn = self.conn.lock().unwrap();
        let mut out = Vec::new();
        match status {
            Some(s) => {
                let mut stmt = conn.prepare("SELECT * FROM tasks WHERE status=?1 ORDER BY created_at DESC LIMIT ?2")?;
                let rows = stmt.query_map(params![s.as_str(), limit], Self::row_to_task)?;
                for r in rows { out.push(r?); }
            }
            None => {
                let mut stmt = conn.prepare("SELECT * FROM tasks ORDER BY created_at DESC LIMIT ?1")?;
                let rows = stmt.query_map(params![limit], Self::row_to_task)?;
                for r in rows { out.push(r?); }
            }
        }
        Ok(out)
    }

    pub fn tasks_in_status(&self, status: TaskStatus) -> rusqlite::Result<Vec<Task>> {
        self.list_tasks(Some(status), i64::MAX)
    }

    pub fn find_active_by_dedup(&self, key: &str) -> rusqlite::Result<Option<Task>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT * FROM tasks WHERE dedup_key=?1 AND status NOT IN ('succeeded','failed','canceled') LIMIT 1",
            params![key], Self::row_to_task,
        ).optional()
    }

    pub fn append_event(&self, e: &TaskEvent) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO task_events (task_id,at,from_status,to_status,detail) VALUES (?1,?2,?3,?4,?5)",
            params![e.task_id, e.at, e.from_status.map(|s| s.as_str()), e.to_status.as_str(), serde_json::to_string(&e.detail).unwrap()],
        )?;
        Ok(())
    }

    pub fn events_for(&self, task_id: &str) -> rusqlite::Result<Vec<TaskEvent>> {
        let conn = self.conn.lock().unwrap();
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
        for r in rows { out.push(r?); }
        Ok(out)
    }

    pub fn counter_get(&self, key: &str, date: &str) -> rusqlite::Result<(i64, Option<i64>)> {
        let conn = self.conn.lock().unwrap();
        let res = conn.query_row(
            "SELECT count,last_spent_at FROM limit_counters WHERE limit_key=?1 AND window_date=?2",
            params![key, date], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Option<i64>>(1)?)),
        ).optional()?;
        Ok(res.unwrap_or((0, None)))
    }

    pub fn counter_spend(&self, key: &str, date: &str, at_ms: i64) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO limit_counters (limit_key,window_date,count,last_spent_at) VALUES (?1,?2,1,?3)
             ON CONFLICT(limit_key,window_date) DO UPDATE SET count=count+1, last_spent_at=?3",
            params![key, date, at_ms],
        )?;
        Ok(())
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
        s.counter_spend("linkedin.post", "2026-07-07", 100).unwrap();
        s.counter_spend("linkedin.post", "2026-07-07", 200).unwrap();
        let (c, last) = s.counter_get("linkedin.post", "2026-07-07").unwrap();
        assert_eq!(c, 2);
        assert_eq!(last, Some(200));
        // different day is a fresh counter
        assert_eq!(s.counter_get("linkedin.post", "2026-07-08").unwrap(), (0, None));
    }

    #[test]
    fn test_events_ordered() {
        let s = Store::open_in_memory().unwrap();
        let t = Task::new_now("dummy", "echo", serde_json::json!({}), 1000);
        s.insert_task(&t).unwrap();
        s.append_event(&TaskEvent { task_id: t.id.clone(), at: 1, from_status: None, to_status: TaskStatus::Pending, detail: serde_json::json!({}) }).unwrap();
        s.append_event(&TaskEvent { task_id: t.id.clone(), at: 2, from_status: Some(TaskStatus::Pending), to_status: TaskStatus::Running, detail: serde_json::json!({}) }).unwrap();
        let evs = s.events_for(&t.id).unwrap();
        assert_eq!(evs.len(), 2);
        assert_eq!(evs[1].to_status, TaskStatus::Running);
    }
}

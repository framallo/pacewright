use crate::clock::Clock;
use crate::model::{Task, TaskStatus};
use crate::rng::Rng;
use crate::store::Store;

pub fn select_runnable(store: &Store, clock: &dyn Clock) -> rusqlite::Result<Vec<Task>> {
    let now = clock.now_ms();
    let mut candidates: Vec<Task> = Vec::new();
    for status in [TaskStatus::Pending, TaskStatus::Deferred] {
        for t in store.tasks_in_status(status)? {
            if t.scheduled_for > now {
                continue;
            }
            if let Some(nea) = t.next_eligible_at {
                if nea > now {
                    continue;
                }
            }
            if let Some(dep) = &t.depends_on {
                // A fallback waits for its dependency to FAIL; everything else for success.
                let want = if t.dep_on_failure {
                    TaskStatus::Failed
                } else {
                    TaskStatus::Succeeded
                };
                match store.get_task(dep)? {
                    Some(d) if d.status == want => {}
                    _ => continue,
                }
            }
            candidates.push(t);
        }
    }
    candidates.sort_by(|a, b| {
        b.priority
            .cmp(&a.priority)
            .then(a.scheduled_for.cmp(&b.scheduled_for))
    });
    Ok(candidates)
}

pub fn resolve_blocked(store: &Store, clock: &dyn Clock, rng: &dyn Rng) -> rusqlite::Result<()> {
    let now = clock.now_ms();
    for mut t in store.tasks_in_status(TaskStatus::Blocked)? {
        let Some(dep) = t.depends_on.clone() else {
            continue;
        };
        let want = if t.dep_on_failure {
            TaskStatus::Failed
        } else {
            TaskStatus::Succeeded
        };
        match store.get_task(&dep)? {
            Some(d) if d.status == want => {
                t.status = TaskStatus::Pending;
                // Humanized pause between pipeline steps: a released task waits a
                // jittered spell before it is eligible, so a run does not fire its
                // steps back to back the instant each dependency clears.
                if let Some(pace) = t.pace_ms {
                    if pace > 0 {
                        t.next_eligible_at = Some(now + rng.jitter(pace, 0.35).max(0));
                    }
                }
                t.updated_at = now;
                store.update_task(&t)?;
            }
            // A failed dependency is only FINAL once any escalation backstopping it has
            // also finished unsuccessfully. Failing dependents the instant a verify fails
            // would race the adjudicator and discard a run it was about to rescue.
            Some(d)
                if !t.dep_on_failure
                    && matches!(d.status, TaskStatus::Failed | TaskStatus::Canceled)
                    && match d.escalation.as_deref() {
                        None => true,
                        Some(esc) => matches!(
                            store.get_task(esc)?.map(|x| x.status),
                            Some(TaskStatus::Failed) | Some(TaskStatus::Canceled) | None
                        ),
                    } =>
            {
                t.status = TaskStatus::Failed;
                t.last_error = Some(format!("dependency {} ended {}", dep, d.status.as_str()));
                t.finished_at = Some(now);
                t.updated_at = now;
                store.update_task(&t)?;
            }
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::TestClock;

    #[test]
    fn test_selects_due_and_orders_by_priority() {
        let store = Store::open_in_memory().unwrap();
        let clock = TestClock::new(1000);
        let mut a = Task::new_now("dummy", "echo", serde_json::json!({}), 500);
        a.priority = 1;
        let mut b = Task::new_now("dummy", "echo", serde_json::json!({}), 400);
        b.priority = 5;
        let c = Task::new_now("dummy", "echo", serde_json::json!({}), 2000); // future, excluded
        store.insert_task(&a).unwrap();
        store.insert_task(&b).unwrap();
        store.insert_task(&c).unwrap();
        let got = select_runnable(&store, &clock).unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].id, b.id); // higher priority first
    }

    #[test]
    fn test_excludes_not_yet_eligible_deferred() {
        let store = Store::open_in_memory().unwrap();
        let clock = TestClock::new(1000);
        let mut d = Task::new_now("dummy", "echo", serde_json::json!({}), 500);
        d.status = TaskStatus::Deferred;
        d.next_eligible_at = Some(5000);
        store.insert_task(&d).unwrap();
        assert!(select_runnable(&store, &clock).unwrap().is_empty());
        clock.set(5000);
        assert_eq!(select_runnable(&store, &clock).unwrap().len(), 1);
    }

    #[test]
    fn test_depends_on_gating_and_resolve() {
        let store = Store::open_in_memory().unwrap();
        let clock = TestClock::new(1000);
        let mut parent = Task::new_now("dummy", "echo", serde_json::json!({}), 100);
        let mut child = Task::new_now("dummy", "echo", serde_json::json!({}), 100);
        child.status = TaskStatus::Blocked;
        child.depends_on = Some(parent.id.clone());
        store.insert_task(&parent).unwrap();
        store.insert_task(&child).unwrap();
        // blocked child is not selected, and stays blocked while parent pending
        resolve_blocked(&store, &clock, &crate::rng::TestRng::fixed(0)).unwrap();
        assert_eq!(
            store.get_task(&child.id).unwrap().unwrap().status,
            TaskStatus::Blocked
        );
        // parent succeeds -> child becomes pending -> selectable
        parent.status = TaskStatus::Succeeded;
        store.update_task(&parent).unwrap();
        resolve_blocked(&store, &clock, &crate::rng::TestRng::fixed(0)).unwrap();
        assert_eq!(
            store.get_task(&child.id).unwrap().unwrap().status,
            TaskStatus::Pending
        );
        assert_eq!(select_runnable(&store, &clock).unwrap().len(), 1);
    }
}

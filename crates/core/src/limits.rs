use crate::clock::Clock;
use crate::config::Config;
use crate::rng::Rng;
use crate::store::Store;
use chrono::{DateTime, Local, TimeZone, Timelike, Utc};

#[derive(Debug, Clone, PartialEq)]
pub enum LimitDecision {
    Allow,
    Defer { until_ms: i64, reason: String },
}

/// Resolve a `chrono::LocalResult` without ever panicking. Prefers the
/// unambiguous `Single` result; on a DST-ambiguous local time (`Ambiguous`)
/// falls back to the earlier of the two instants; on a DST-nonexistent local
/// time (`None`) falls back to interpreting the naive wall-clock time as UTC
/// as a last-resort, always-defined default.
fn resolve_local<F>(
    result: chrono::LocalResult<DateTime<Local>>,
    fallback_naive: F,
) -> DateTime<Local>
where
    F: FnOnce() -> chrono::NaiveDateTime,
{
    match result {
        chrono::LocalResult::Single(dt) => dt,
        chrono::LocalResult::Ambiguous(earliest, _latest) => earliest,
        chrono::LocalResult::None => {
            // Nonexistent local time (spring-forward gap): interpret the
            // naive wall-clock time as UTC rather than panicking.
            DateTime::<Utc>::from_naive_utc_and_offset(fallback_naive(), Utc).with_timezone(&Local)
        }
    }
}

/// Convert epoch milliseconds to a local `DateTime`, never panicking. Falls
/// back to interpreting the timestamp as UTC if the local-time conversion is
/// somehow undefined (this conversion is normally always `Single` since it
/// starts from an absolute instant, but we guard it defensively anyway).
fn local_from_millis(ms: i64) -> DateTime<Local> {
    match Local.timestamp_millis_opt(ms) {
        chrono::LocalResult::Single(dt) => dt,
        chrono::LocalResult::Ambiguous(earliest, _latest) => earliest,
        chrono::LocalResult::None => DateTime::<Utc>::from_timestamp_millis(ms)
            .unwrap_or_else(|| DateTime::<Utc>::from_timestamp_millis(0).unwrap())
            .with_timezone(&Local),
    }
}

pub fn local_date_str(now_ms: i64) -> String {
    let dt = local_from_millis(now_ms);
    dt.format("%Y-%m-%d").to_string()
}

pub fn minutes_since_local_midnight(now_ms: i64) -> i32 {
    let dt = local_from_millis(now_ms);
    (dt.hour() * 60 + dt.minute()) as i32
}

fn next_local_midnight_ms(now_ms: i64) -> i64 {
    let dt = local_from_millis(now_ms);
    let next = (dt + chrono::Duration::days(1))
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .unwrap();
    resolve_local(Local.from_local_datetime(&next), || next).timestamp_millis()
}

/// Deterministic per-`(key, date)` time offset in `[0, spread_ms]`, added to a window-open
/// deferral so a daily-capped drip fires at a slightly different clock time each day.
///
/// Stability is the whole point: on any given local date every backlog candidate for the same
/// limit key must resolve to the SAME release time, so exactly one fires at `window_open + offset`.
/// Drawing from the shared `rng` per-check would give each candidate its own offset and the
/// earliest would always win (min-bias clustering right back at window open). Hashing
/// `(key, date)` with a fixed seed gives a stable, rng-free value that varies day to day.
///
/// Returns `0` when `spread_ms <= 0`, making spread-off behavior byte-identical to before.
fn day_spread_offset_ms(key: &str, date: &str, spread_ms: i64) -> i64 {
    if spread_ms <= 0 {
        return 0;
    }
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    // Length-prefix-free but delimited: hashing the two &str separately (Hash for str folds in the
    // length) keeps ("ab","c") distinct from ("a","bc").
    key.hash(&mut h);
    date.hash(&mut h);
    // Map the 64-bit hash uniformly into the inclusive range [0, spread_ms].
    let span = (spread_ms as u64).saturating_add(1);
    (h.finish() % span) as i64
}

fn local_time_at_minute_ms(now_ms: i64, minute_of_day: i32) -> i64 {
    let dt = local_from_millis(now_ms);
    let base = dt
        .date_naive()
        .and_hms_opt((minute_of_day / 60) as u32, (minute_of_day % 60) as u32, 0)
        .unwrap();
    resolve_local(Local.from_local_datetime(&base), || base).timestamp_millis()
}

pub fn check_limits(
    store: &Store,
    cfg: &Config,
    clock: &dyn Clock,
    rng: &dyn Rng,
    keys: &[String],
) -> rusqlite::Result<LimitDecision> {
    let now = clock.now_ms();
    let date = local_date_str(now);
    let now_min = minutes_since_local_midnight(now);
    let mut worst: Option<(i64, String)> = None;
    let consider = |until: i64, reason: String, worst: &mut Option<(i64, String)>| match worst {
        Some((u, _)) if *u >= until => {}
        _ => *worst = Some((until, reason)),
    };

    for key in keys {
        let lc = cfg.limit_for(key);
        let (count, last_spent) = store.counter_get(key, &date)?;

        // 1. daily cap -> defer to next local midnight
        if count >= lc.daily_cap {
            consider(
                next_local_midnight_ms(now),
                format!("over_cap:{key}"),
                &mut worst,
            );
            continue;
        }
        // 2. active hours -> defer to window open (today or next day), plus a stable per-day
        //    spread offset so a daily-capped drip does not fire at the exact same clock time
        //    every day. The offset is 0 when spread is unset (byte-identical to before).
        if now_min < lc.active_start_min {
            let offset = day_spread_offset_ms(key, &date, lc.spread_ms);
            consider(
                local_time_at_minute_ms(now, lc.active_start_min) + offset,
                format!("before_active:{key}"),
                &mut worst,
            );
            continue;
        }
        if now_min >= lc.active_end_min {
            // Deferring to tomorrow's window open: derive the offset from tomorrow's local date
            // so it matches what a `before_active` check will compute once that day arrives.
            let next_mid = next_local_midnight_ms(now);
            let offset = day_spread_offset_ms(key, &local_date_str(next_mid), lc.spread_ms);
            let open_next = local_time_at_minute_ms(next_mid, lc.active_start_min) + offset;
            consider(open_next, format!("after_active:{key}"), &mut worst);
            continue;
        }
        // 3. min gap (jittered) since last spend
        if let Some(last) = last_spent {
            let gap = rng.jitter(lc.min_gap_ms, lc.jitter);
            let earliest = last + gap;
            if now < earliest {
                consider(earliest, format!("too_soon:{key}"), &mut worst);
            }
        }
    }

    Ok(match worst {
        Some((until, reason)) => LimitDecision::Defer {
            until_ms: until,
            reason,
        },
        None => LimitDecision::Allow,
    })
}

pub fn spend_limits(store: &Store, clock: &dyn Clock, keys: &[String]) -> rusqlite::Result<()> {
    let now = clock.now_ms();
    let date = local_date_str(now);
    for key in keys {
        store.counter_spend(key, &date, now)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::TestClock;
    use crate::config::Config;
    use crate::rng::TestRng;

    // A fixed instant: 2026-07-07 12:00 local. We compute via chrono to stay tz-independent.
    fn noon_ms() -> i64 {
        let naive = chrono::NaiveDate::from_ymd_opt(2026, 7, 7)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap();
        Local
            .from_local_datetime(&naive)
            .single()
            .unwrap()
            .timestamp_millis()
    }

    fn cfg() -> Config {
        Config::from_toml(
            r#"
[limits."dummy.capped"]
daily_cap = 3
min_gap = "8m"
jitter = 0.0
active = "09:00-18:00"
"#,
        )
        .unwrap()
    }

    #[test]
    fn test_allow_when_under_everything() {
        let store = Store::open_in_memory().unwrap();
        let clock = TestClock::new(noon_ms());
        let rng = TestRng::fixed(8 * 60_000);
        let d = check_limits(&store, &cfg(), &clock, &rng, &["dummy.capped".into()]).unwrap();
        assert_eq!(d, LimitDecision::Allow);
    }

    #[test]
    fn test_defer_over_cap_to_next_midnight() {
        let store = Store::open_in_memory().unwrap();
        let clock = TestClock::new(noon_ms());
        let date = local_date_str(noon_ms());
        for _ in 0..3 {
            store
                .counter_spend("dummy.capped", &date, noon_ms())
                .unwrap();
        }
        let rng = TestRng::fixed(0);
        let d = check_limits(&store, &cfg(), &clock, &rng, &["dummy.capped".into()]).unwrap();
        match d {
            LimitDecision::Defer { until_ms, reason } => {
                assert!(until_ms > noon_ms());
                assert!(reason.starts_with("over_cap"));
            }
            _ => panic!("expected defer"),
        }
    }

    #[test]
    fn test_defer_too_soon_after_recent_spend() {
        let store = Store::open_in_memory().unwrap();
        let clock = TestClock::new(noon_ms());
        // last spend 2 minutes ago; gap is 8m
        store
            .counter_spend(
                "dummy.capped",
                &local_date_str(noon_ms()),
                noon_ms() - 2 * 60_000,
            )
            .unwrap();
        let rng = TestRng::fixed(8 * 60_000);
        let d = check_limits(&store, &cfg(), &clock, &rng, &["dummy.capped".into()]).unwrap();
        match d {
            LimitDecision::Defer { until_ms, reason } => {
                assert_eq!(until_ms, noon_ms() - 2 * 60_000 + 8 * 60_000);
                assert!(reason.starts_with("too_soon"));
            }
            _ => panic!("expected defer"),
        }
    }

    // 2026-07-07 07:00 local — before a 09:00 window open.
    fn morning_ms() -> i64 {
        let naive = chrono::NaiveDate::from_ymd_opt(2026, 7, 7)
            .unwrap()
            .and_hms_opt(7, 0, 0)
            .unwrap();
        Local
            .from_local_datetime(&naive)
            .single()
            .unwrap()
            .timestamp_millis()
    }

    fn cfg_with_spread() -> Config {
        Config::from_toml(
            r#"
[limits."dummy.capped"]
daily_cap = 3
min_gap = "8m"
jitter = 0.0
active = "09:00-18:00"
spread = "45m"
"#,
        )
        .unwrap()
    }

    #[test]
    fn test_before_active_release_is_offset_within_spread_and_stable() {
        let store = Store::open_in_memory().unwrap();
        let clock = TestClock::new(morning_ms());
        let rng = TestRng::fixed(0);
        let cfg = cfg_with_spread();
        let key = "dummy.capped".to_string();

        // window open at 09:00 on the same local day as `morning_ms()`
        let window_open = local_time_at_minute_ms(morning_ms(), 9 * 60);
        let spread_ms = 45 * 60_000i64;

        let d1 = check_limits(&store, &cfg, &clock, &rng, std::slice::from_ref(&key)).unwrap();
        let until1 = match &d1 {
            LimitDecision::Defer { until_ms, reason } => {
                assert!(reason.starts_with("before_active"));
                *until_ms
            }
            _ => panic!("expected before_active defer"),
        };

        // Offset from the exact window open, and inside [open, open+spread].
        assert!(until1 > window_open, "expected a nonzero spread offset");
        assert!(until1 <= window_open + spread_ms);

        // Stable: a second check on the same (key, date) yields the identical release time —
        // this is what guarantees exactly one backlog candidate fires per day at window+offset.
        let d2 = check_limits(&store, &cfg, &clock, &rng, &[key]).unwrap();
        let until2 = match d2 {
            LimitDecision::Defer { until_ms, .. } => until_ms,
            _ => panic!("expected before_active defer"),
        };
        assert_eq!(until1, until2);

        // And spread=0 collapses back to the exact window open (byte-identical to old behavior).
        assert_eq!(
            day_spread_offset_ms("dummy.capped", &local_date_str(morning_ms()), 0),
            0
        );
    }

    #[test]
    fn test_permissive_key_always_allows() {
        let store = Store::open_in_memory().unwrap();
        let clock = TestClock::new(noon_ms());
        let rng = TestRng::fixed(0);
        // key not in config -> permissive
        let d = check_limits(
            &store,
            &Config::default(),
            &clock,
            &rng,
            &["unknown.key".into()],
        )
        .unwrap();
        assert_eq!(d, LimitDecision::Allow);
    }

    #[test]
    fn test_local_time_helpers_do_not_panic_for_normal_input() {
        // Proves the non-panicking resolve path compiles and behaves sanely
        // for an ordinary (non-DST-boundary) timestamp. Constructing a real
        // DST-ambiguous/nonexistent local time portably (tz-independent) is
        // impractical in a unit test, so this covers the normal case only.
        let now = noon_ms();
        let date = local_date_str(now);
        assert_eq!(date.len(), 10); // "YYYY-MM-DD"
        let mins = minutes_since_local_midnight(now);
        assert!((0..1440).contains(&mins));
        let next_midnight = next_local_midnight_ms(now);
        assert!(next_midnight > now);
        let at_minute = local_time_at_minute_ms(now, 9 * 60);
        assert!(at_minute > 0);
    }
}

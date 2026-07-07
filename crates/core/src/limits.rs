use crate::clock::Clock;
use crate::config::Config;
use crate::rng::Rng;
use crate::store::Store;
use chrono::{Local, TimeZone, Timelike};

#[derive(Debug, Clone, PartialEq)]
pub enum LimitDecision {
    Allow,
    Defer { until_ms: i64, reason: String },
}

pub fn local_date_str(now_ms: i64) -> String {
    let dt = Local.timestamp_millis_opt(now_ms).single().unwrap();
    dt.format("%Y-%m-%d").to_string()
}

pub fn minutes_since_local_midnight(now_ms: i64) -> i32 {
    let dt = Local.timestamp_millis_opt(now_ms).single().unwrap();
    (dt.hour() * 60 + dt.minute()) as i32
}

fn next_local_midnight_ms(now_ms: i64) -> i64 {
    let dt = Local.timestamp_millis_opt(now_ms).single().unwrap();
    let next = (dt + chrono::Duration::days(1)).date_naive().and_hms_opt(0, 0, 0).unwrap();
    Local.from_local_datetime(&next).single().unwrap().timestamp_millis()
}

fn local_time_at_minute_ms(now_ms: i64, minute_of_day: i32) -> i64 {
    let dt = Local.timestamp_millis_opt(now_ms).single().unwrap();
    let base = dt.date_naive().and_hms_opt((minute_of_day / 60) as u32, (minute_of_day % 60) as u32, 0).unwrap();
    Local.from_local_datetime(&base).single().unwrap().timestamp_millis()
}

pub fn check_limits(
    store: &Store, cfg: &Config, clock: &dyn Clock, rng: &dyn Rng, keys: &[String],
) -> rusqlite::Result<LimitDecision> {
    let now = clock.now_ms();
    let date = local_date_str(now);
    let now_min = minutes_since_local_midnight(now);
    let mut worst: Option<(i64, String)> = None;
    let mut consider = |until: i64, reason: String, worst: &mut Option<(i64, String)>| {
        match worst {
            Some((u, _)) if *u >= until => {}
            _ => *worst = Some((until, reason)),
        }
    };

    for key in keys {
        let lc = cfg.limit_for(key);
        let (count, last_spent) = store.counter_get(key, &date)?;

        // 1. daily cap -> defer to next local midnight
        if count >= lc.daily_cap {
            consider(next_local_midnight_ms(now), format!("over_cap:{key}"), &mut worst);
            continue;
        }
        // 2. active hours -> defer to window open (today or next day)
        if now_min < lc.active_start_min {
            consider(local_time_at_minute_ms(now, lc.active_start_min), format!("before_active:{key}"), &mut worst);
            continue;
        }
        if now_min >= lc.active_end_min {
            let open_next = local_time_at_minute_ms(next_local_midnight_ms(now), lc.active_start_min);
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
        Some((until, reason)) => LimitDecision::Defer { until_ms: until, reason },
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
        let naive = chrono::NaiveDate::from_ymd_opt(2026, 7, 7).unwrap().and_hms_opt(12, 0, 0).unwrap();
        Local.from_local_datetime(&naive).single().unwrap().timestamp_millis()
    }

    fn cfg() -> Config {
        Config::from_toml(r#"
[limits."dummy.capped"]
daily_cap = 3
min_gap = "8m"
jitter = 0.0
active = "09:00-18:00"
"#).unwrap()
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
        for _ in 0..3 { store.counter_spend("dummy.capped", &date, noon_ms()).unwrap(); }
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
        store.counter_spend("dummy.capped", &local_date_str(noon_ms()), noon_ms() - 2 * 60_000).unwrap();
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

    #[test]
    fn test_permissive_key_always_allows() {
        let store = Store::open_in_memory().unwrap();
        let clock = TestClock::new(noon_ms());
        let rng = TestRng::fixed(0);
        // key not in config -> permissive
        let d = check_limits(&store, &Config::default(), &clock, &rng, &["unknown.key".into()]).unwrap();
        assert_eq!(d, LimitDecision::Allow);
    }
}

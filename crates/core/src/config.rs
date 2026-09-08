use anyhow::{anyhow, Result};
use serde::Deserialize;
use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq)]
pub struct LimitConfig {
    pub daily_cap: i64,
    pub min_gap_ms: i64,
    pub jitter: f64,
    pub active_start_min: i32,
    pub active_end_min: i32,
    /// Per-day time spread (ms) added to a window-open deferral so a daily-capped drip fires at a
    /// slightly different clock time each day instead of exactly at the window open. `0` = off
    /// (byte-identical to pre-spread behavior). The offset is derived deterministically per
    /// `(limit_key, local_date)` in `limits.rs`, never drawn from the shared rng.
    pub spread_ms: i64,
}

impl LimitConfig {
    pub fn permissive() -> Self {
        LimitConfig {
            daily_cap: i64::MAX,
            min_gap_ms: 0,
            jitter: 0.0,
            active_start_min: 0,
            active_end_min: 1440,
            spread_ms: 0,
        }
    }

    /// Build a `LimitConfig` from the human-friendly parts a `set_limit` carries — the same
    /// forms `config.toml` accepts (`min_gap="8m"`, `active="09:00-18:00"`). Omitted parts
    /// take permissive defaults.
    pub fn from_parts(
        daily_cap: Option<i64>,
        min_gap: Option<&str>,
        jitter: Option<f64>,
        active: Option<&str>,
    ) -> Result<LimitConfig> {
        let (astart, aend) = match active {
            Some(a) => parse_active(a)?,
            None => (0, 1440),
        };
        Ok(LimitConfig {
            daily_cap: daily_cap.unwrap_or(i64::MAX),
            min_gap_ms: match min_gap {
                Some(g) => parse_duration_ms(g)?,
                None => 0,
            },
            jitter: jitter.unwrap_or(0.0),
            active_start_min: astart,
            active_end_min: aend,
            // `set_limit` (the runtime-override path) does not carry spread; spread is a
            // config.toml feature for daily drips. Defaults to off here.
            spread_ms: 0,
        })
    }
}

#[derive(Debug, Clone, Default)]
pub struct Config {
    pub limits: HashMap<String, LimitConfig>,
    /// Every Chrome to attach to, in declaration order (`http://127.0.0.1:9222`, or `auto`).
    /// Empty = not configured; the browser layer falls back to `browser::DEFAULT_CHROME_CONNECT`.
    ///
    /// A **list**, because one Chrome and many Chromes are the same mechanism at different sizes:
    /// the original single-browser operator has a one-element pool and needs no config change.
    pub browser_pool: Vec<String>,
}

impl Config {
    /// The one endpoint, for callers that predate the pool. `None` when unconfigured.
    pub fn browser_connect(&self) -> Option<&str> {
        self.browser_pool.first().map(String::as_str)
    }
}

#[derive(Deserialize)]
struct RawConfig {
    #[serde(default)]
    limits: HashMap<String, RawLimit>,
    #[serde(default)]
    browser: Option<RawBrowser>,
}
/// Unknown keys are ignored by design (serde's default): `idle_timeout` lived here until the
/// browser reaper was retired, and an operator upgrading with it still in `config.toml` must not
/// hit a hard failure on daemon boot.
#[derive(Deserialize)]
struct RawBrowser {
    /// One endpoint (`"http://127.0.0.1:9222"`, `"ws://…"`, `"auto"`) or a list of them.
    #[serde(default)]
    connect: Option<Connect>,
}

/// `connect` takes a string or a list of strings. Untagged so the existing one-line form keeps
/// parsing unchanged — an operator's `config.toml` needs no edit to stay on one Chrome.
#[derive(Deserialize)]
#[serde(untagged)]
enum Connect {
    One(String),
    Many(Vec<String>),
}

impl Connect {
    fn into_pool(self) -> Vec<String> {
        match self {
            Connect::One(s) => vec![s],
            Connect::Many(v) => v,
        }
        .into_iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
    }
}
#[derive(Deserialize)]
struct RawLimit {
    #[serde(default)]
    daily_cap: Option<i64>,
    #[serde(default)]
    min_gap: Option<String>,
    #[serde(default)]
    jitter: Option<f64>,
    #[serde(default)]
    active: Option<String>,
    #[serde(default)]
    spread: Option<String>,
}

fn parse_duration_ms(s: &str) -> Result<i64> {
    let s = s.trim();
    let (num, mult) = if let Some(v) = s.strip_suffix("ms") {
        (v, 1)
    } else if let Some(v) = s.strip_suffix('s') {
        (v, 1000)
    } else if let Some(v) = s.strip_suffix('m') {
        (v, 60_000)
    } else if let Some(v) = s.strip_suffix('h') {
        (v, 3_600_000)
    } else {
        (s, 1)
    };
    Ok(num
        .trim()
        .parse::<i64>()
        .map_err(|_| anyhow!("bad duration {s}"))?
        * mult)
}

fn parse_active(s: &str) -> Result<(i32, i32)> {
    let (a, b) = s
        .split_once('-')
        .ok_or_else(|| anyhow!("bad active window {s}"))?;
    let to_min = |hm: &str| -> Result<i32> {
        let (h, m) = hm
            .trim()
            .split_once(':')
            .ok_or_else(|| anyhow!("bad time {hm}"))?;
        Ok(h.trim().parse::<i32>()? * 60 + m.trim().parse::<i32>()?)
    };
    Ok((to_min(a)?, to_min(b)?))
}

impl Config {
    pub fn from_toml(s: &str) -> Result<Config> {
        let raw: RawConfig = toml::from_str(s)?;
        let mut limits = HashMap::new();
        for (k, v) in raw.limits {
            let (astart, aend) = match v.active {
                Some(a) => parse_active(&a)?,
                None => (0, 1440),
            };
            limits.insert(
                k,
                LimitConfig {
                    daily_cap: v.daily_cap.unwrap_or(i64::MAX),
                    min_gap_ms: match v.min_gap {
                        Some(g) => parse_duration_ms(&g)?,
                        None => 0,
                    },
                    jitter: v.jitter.unwrap_or(0.0),
                    active_start_min: astart,
                    active_end_min: aend,
                    spread_ms: match v.spread {
                        Some(sp) => parse_duration_ms(&sp)?,
                        None => 0,
                    },
                },
            );
        }
        let browser_pool = raw
            .browser
            .and_then(|b| b.connect)
            .map(Connect::into_pool)
            .unwrap_or_default();
        Ok(Config {
            limits,
            browser_pool,
        })
    }

    pub fn limit_for(&self, key: &str) -> LimitConfig {
        self.limits
            .get(key)
            .cloned()
            .unwrap_or_else(LimitConfig::permissive)
    }

    /// Set (or replace) a limit at runtime — used by `set_limit` to layer a persisted
    /// override over what `config.toml` declared. In-memory only; the daemon persists it.
    pub fn set_limit(&mut self, key: impl Into<String>, cfg: LimitConfig) {
        self.limits.insert(key.into(), cfg);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_parse_full_config() {
        let toml = r#"
[limits."dummy.capped"]
daily_cap = 3
min_gap = "8m"
jitter = 0.5
active = "09:00-18:00"
"#;
        let c = Config::from_toml(toml).unwrap();
        let l = c.limit_for("dummy.capped");
        assert_eq!(l.daily_cap, 3);
        assert_eq!(l.min_gap_ms, 8 * 60_000);
        assert_eq!(l.jitter, 0.5);
        assert_eq!(l.active_start_min, 540);
        assert_eq!(l.active_end_min, 1080);
    }
    #[test]
    fn test_browser_connect_parses_and_defaults() {
        // Absent → empty pool (the browser layer falls back to DEFAULT_CHROME_CONNECT).
        assert!(Config::from_toml("").unwrap().browser_pool.is_empty());
        assert_eq!(Config::from_toml("").unwrap().browser_connect(), None);
        // An explicit endpoint overrides, so the port can move without a rebuild.
        let c = Config::from_toml("[browser]\nconnect = \"http://127.0.0.1:9333\"\n").unwrap();
        assert_eq!(c.browser_pool, ["http://127.0.0.1:9333"]);
        assert_eq!(c.browser_connect(), Some("http://127.0.0.1:9333"));
        // chrome-agent's own discovery mode is a legal value too.
        let c = Config::from_toml("[browser]\nconnect = \"auto\"\n").unwrap();
        assert_eq!(c.browser_connect(), Some("auto"));
    }

    #[test]
    fn test_browser_connect_accepts_a_pool() {
        // The scale case: many Chromes for throughput. Same key, a list instead of a string, so
        // the one-Chrome operator's config keeps parsing byte for byte unchanged.
        let c = Config::from_toml(
            "[browser]\nconnect = [\"http://127.0.0.1:9222\", \"http://127.0.0.1:9223\"]\n",
        )
        .unwrap();
        assert_eq!(
            c.browser_pool,
            ["http://127.0.0.1:9222", "http://127.0.0.1:9223"]
        );
        // `browser_connect()` still answers for callers that only ever wanted one.
        assert_eq!(c.browser_connect(), Some("http://127.0.0.1:9222"));
        // Blank entries are dropped instead of becoming an endpoint nothing listens on.
        let c = Config::from_toml("[browser]\nconnect = [\"http://127.0.0.1:9222\", \"  \"]\n")
            .unwrap();
        assert_eq!(c.browser_pool, ["http://127.0.0.1:9222"]);
    }

    #[test]
    fn test_retired_idle_timeout_key_is_ignored_not_fatal() {
        // `browser.idle_timeout` drove the browser reaper, which is gone: pacewright no longer
        // launches browsers to reap, and `chrome-agent gc` never touches an attached session
        // anyway. An operator upgrading with the old key in config.toml must not get a hard
        // failure on daemon boot.
        let c = Config::from_toml("[browser]\nidle_timeout = \"10m\"\n").unwrap();
        assert!(c.browser_pool.is_empty());
    }

    #[test]
    fn test_unknown_key_is_permissive() {
        let c = Config::default();
        let l = c.limit_for("anything");
        assert_eq!(l, LimitConfig::permissive());
    }
    #[test]
    fn test_spread_parses_and_defaults_to_zero() {
        // Present → parsed as a duration.
        let c = Config::from_toml(
            "[limits.\"x.post\"]\ndaily_cap = 1\nactive = \"09:00-20:00\"\nspread = \"45m\"\n",
        )
        .unwrap();
        assert_eq!(c.limit_for("x.post").spread_ms, 45 * 60_000);
        // Absent → 0 (backward compatible: behaves exactly as before spread existed).
        let c = Config::from_toml("[limits.\"x.post\"]\ndaily_cap = 1\n").unwrap();
        assert_eq!(c.limit_for("x.post").spread_ms, 0);
        // Bare-seconds form is accepted too.
        let c = Config::from_toml("[limits.\"x.post\"]\nspread = \"90s\"\n").unwrap();
        assert_eq!(c.limit_for("x.post").spread_ms, 90_000);
    }

    #[test]
    fn test_duration_units() {
        assert_eq!(parse_duration_ms("20s").unwrap(), 20_000);
        assert_eq!(parse_duration_ms("2h").unwrap(), 7_200_000);
        assert_eq!(parse_duration_ms("500ms").unwrap(), 500);
    }
}

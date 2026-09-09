//! How much of the Claude subscription limits is already spent.
//!
//! Two sources, in order of freshness:
//!
//! 1. The harvest file. Claude Code hands its status-line command the session
//!    JSON on every render, `rate_limits` included, and the status line writes
//!    those windows out for us. This moves while you work and costs nothing.
//! 2. `~/.claude.json`, where the CLI keeps a `cachedUsageUtilization` block.
//!    A fallback only: it refreshes when `/usage` is actually run and can sit
//!    unchanged for days otherwise.
//!
//! Asking the API directly is not an option — `/api/oauth/usage` answers 429 to
//! anything that polls it (anthropics/claude-code#31637), which is why Claude
//! Code's own status line is the way in.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::Deserialize;

/// A window nobody has refreshed in this long is shown as such: the numbers
/// only move while a local Claude Code session renders, and usage spent in a
/// browser or on another machine never reaches us at all.
const STALE_AFTER: chrono::TimeDelta = chrono::TimeDelta::minutes(30);

/// Percent of each window already spent, `None` for a window we cannot read.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    pub five_hour: Option<u8>,
    pub seven_day: Option<u8>,
    /// Nothing has refreshed the reading recently, so it may lag the truth.
    pub stale: bool,
}

impl std::fmt::Display for Usage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        fn window(pct: Option<u8>) -> String {
            pct.map_or_else(|| "--".to_string(), |pct| format!("{pct}%"))
        }
        write!(f, "5h {}, 7d {}", window(self.five_hour), window(self.seven_day))?;
        if self.stale {
            write!(f, " (stale)")?;
        }
        Ok(())
    }
}

/// Where the two readings live.
pub struct Sources {
    /// Written by the status line, on every render.
    pub harvest: PathBuf,
    /// Claude Code's own config, refreshed only by `/usage`.
    pub config: PathBuf,
}

impl Default for Sources {
    fn default() -> Self {
        let runtime = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/tmp"));
        let config = match std::env::var_os("CLAUDE_CONFIG_DIR") {
            Some(dir) => PathBuf::from(dir).join(".claude.json"),
            None => std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_default()
                .join(".claude.json"),
        };
        Self {
            harvest: runtime.join("claude-usage.json"),
            config,
        }
    }
}

impl Sources {
    /// Reads both windows, preferring the harvest and falling back to the config.
    ///
    /// Anything unreadable — no file, no cache yet, a shape we do not know — is
    /// `None` rather than an error. A missing reading is a normal state of the
    /// world here, and the screen already says `--` for it.
    pub fn read(&self) -> Usage {
        read(&self.harvest, &self.config)
    }
}

fn read(harvest: &Path, config: &Path) -> Usage {
    let now = Utc::now();
    let harvested = std::fs::read_to_string(harvest)
        .ok()
        .map(|text| parse_harvest(&text, now))
        .unwrap_or_default();
    if harvested.five_hour.is_some() || harvested.seven_day.is_some() {
        return harvested;
    }
    // No status line has run yet on this login. The config cache is older by
    // construction, so whatever it yields is stale until proven otherwise.
    match std::fs::read_to_string(config) {
        Ok(text) => Usage {
            stale: true,
            ..parse_config(&text, now)
        },
        Err(_) => Usage::default(),
    }
}

/// The harvest, written by the status line: percentages plus the epoch second
/// it was written at, which is what tells us the reading has stopped moving.
fn parse_harvest(text: &str, now: DateTime<Utc>) -> Usage {
    let Ok(harvest) = serde_json::from_str::<Harvest>(text) else {
        return Usage::default();
    };
    let stale = harvest
        .written_at
        .and_then(|at| DateTime::from_timestamp(at, 0))
        .is_none_or(|at| now - at >= STALE_AFTER);
    Usage {
        five_hour: harvest.five_hour.and_then(|window| window.percent(now)),
        seven_day: harvest.seven_day.and_then(|window| window.percent(now)),
        stale,
    }
}

/// The CLI's own cache. Its `resets_at` is an RFC 3339 string rather than the
/// status line's epoch seconds, and there is no timestamp on the block itself.
fn parse_config(text: &str, now: DateTime<Utc>) -> Usage {
    let Ok(config) = serde_json::from_str::<Config>(text) else {
        return Usage::default();
    };
    let Some(windows) = config.cached_usage_utilization.and_then(|cached| cached.utilization) else {
        return Usage::default();
    };
    Usage {
        five_hour: windows.five_hour.and_then(|window| window.percent(now)),
        seven_day: windows.seven_day.and_then(|window| window.percent(now)),
        stale: false,
    }
}

#[derive(Deserialize)]
struct Harvest {
    five_hour: Option<HarvestWindow>,
    seven_day: Option<HarvestWindow>,
    written_at: Option<i64>,
}

#[derive(Deserialize)]
struct HarvestWindow {
    used_percentage: Option<f64>,
    /// Unix epoch seconds, unlike the config's RFC 3339 string.
    resets_at: Option<i64>,
}

impl HarvestWindow {
    fn percent(&self, now: DateTime<Utc>) -> Option<u8> {
        let reset = self
            .resets_at
            .and_then(|at| DateTime::from_timestamp(at, 0))
            .is_some_and(|at| at <= now);
        if reset {
            return Some(0);
        }
        Some(percent(self.used_percentage?))
    }
}

#[derive(Deserialize)]
struct Config {
    #[serde(rename = "cachedUsageUtilization")]
    cached_usage_utilization: Option<Cached>,
}

#[derive(Deserialize)]
struct Cached {
    utilization: Option<Windows>,
}

#[derive(Deserialize)]
struct Windows {
    five_hour: Option<ConfigWindow>,
    seven_day: Option<ConfigWindow>,
}

#[derive(Deserialize)]
struct ConfigWindow {
    utilization: Option<f64>,
    resets_at: Option<String>,
}

impl ConfigWindow {
    fn percent(&self, now: DateTime<Utc>) -> Option<u8> {
        let reset = self
            .resets_at
            .as_deref()
            .and_then(|at| DateTime::parse_from_rfc3339(at).ok())
            .is_some_and(|at| at.with_timezone(&Utc) <= now);
        if reset {
            return Some(0);
        }
        Some(percent(self.utilization?))
    }
}

/// A window whose own reset time has passed is empty by definition, which is
/// what keeps a source that stopped refreshing from advertising a limit that
/// has since rolled over. Whole percent on the wire; the API has only ever sent
/// integers, but a fractional one would still have to land somewhere sane.
fn percent(value: f64) -> u8 {
    value.round().clamp(0.0, 100.0) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-09-08T12:00:00+00:00")
            .expect("fixed timestamp")
            .with_timezone(&Utc)
    }

    fn epoch(rfc3339: &str) -> i64 {
        DateTime::parse_from_rfc3339(rfc3339).expect("fixed timestamp").timestamp()
    }

    fn harvest(five: u32, written: &str) -> String {
        format!(
            r#"{{"five_hour":{{"used_percentage":{five},"resets_at":{}}},
                "seven_day":{{"used_percentage":13,"resets_at":{}}},
                "written_at":{}}}"#,
            epoch("2026-09-08T15:00:00+00:00"),
            epoch("2026-09-15T11:00:00+00:00"),
            epoch(written),
        )
    }

    #[test]
    fn reads_a_fresh_harvest() {
        assert_eq!(
            parse_harvest(&harvest(78, "2026-09-08T11:55:00+00:00"), now()),
            Usage {
                five_hour: Some(78),
                seven_day: Some(13),
                stale: false,
            }
        );
    }

    #[test]
    fn flags_a_harvest_nothing_has_refreshed() {
        let usage = parse_harvest(&harvest(78, "2026-09-08T10:00:00+00:00"), now());
        assert!(usage.stale, "two hours without a render is stale");
        assert_eq!(usage.five_hour, Some(78), "the reading is kept, only marked");
    }

    #[test]
    fn a_window_past_its_reset_is_empty_rather_than_stale() {
        let config = format!(
            r#"{{"five_hour":{{"used_percentage":80,"resets_at":{}}},"written_at":{}}}"#,
            epoch("2026-09-08T09:00:00+00:00"),
            epoch("2026-09-08T11:55:00+00:00"),
        );
        assert_eq!(parse_harvest(&config, now()).five_hour, Some(0));
    }

    #[test]
    fn a_harvest_without_the_windows_reads_as_unknown() {
        // The staleness of a harvest carrying no reading at all says nothing:
        // `read` falls through to the config cache on exactly this shape.
        let empty = parse_harvest(r#"{"written_at":0}"#, now());
        assert_eq!((empty.five_hour, empty.seven_day), (None, None));
        assert_eq!(parse_harvest("half a written file", now()), Usage::default());
    }

    #[test]
    fn reads_the_config_cache() {
        // Shape copied from a real ~/.claude.json, trimmed to what we read.
        let config = r#"{"cachedUsageUtilization":{"fetchedAtMs":1788878612419,"utilization":{
            "five_hour":{"utilization":24,"resets_at":"2026-09-08T15:49:59.804477+00:00"},
            "seven_day":{"utilization":2,"resets_at":"2026-09-15T10:59:59.804499+00:00"},
            "seven_day_opus":null}}}"#;
        assert_eq!(
            parse_config(config, now()),
            Usage {
                five_hour: Some(24),
                seven_day: Some(2),
                stale: false,
            }
        );
    }

    #[test]
    fn a_config_window_past_its_reset_is_empty_too() {
        let config = r#"{"cachedUsageUtilization":{"utilization":{
            "five_hour":{"utilization":80,"resets_at":"2026-09-08T09:00:00+00:00"},
            "seven_day":{"utilization":40,"resets_at":"2026-09-15T10:59:59+00:00"}}}}"#;
        assert_eq!(
            parse_config(config, now()),
            Usage {
                five_hour: Some(0),
                seven_day: Some(40),
                stale: false,
            }
        );
    }

    #[test]
    fn a_config_without_the_cache_reads_as_unknown() {
        assert_eq!(parse_config(r#"{"hasCompletedOnboarding":true}"#, now()), Usage::default());
    }

    #[test]
    fn falls_back_to_the_config_and_says_the_reading_is_second_hand() {
        let usage = read(Path::new("/nonexistent/claude-usage.json"), Path::new("/nonexistent/.claude.json"));
        assert_eq!(usage, Usage::default());
        assert!(!usage.stale, "nothing read at all is unknown, not stale");
    }
}

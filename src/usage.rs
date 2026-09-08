//! How much of the Claude subscription limits is already spent.
//!
//! Claude Code has no command that reports this: the percentages come back on
//! `anthropic-ratelimit-unified-*` response headers and the CLI caches them in
//! `~/.claude.json` under `cachedUsageUtilization`. Reading that cache is the
//! only way in from the outside, and it carries one caveat — see [`parse`].

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::Deserialize;

/// Percent of each window already spent, `None` for a window we cannot read.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    pub five_hour: Option<u8>,
    pub seven_day: Option<u8>,
}

impl std::fmt::Display for Usage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        fn window(pct: Option<u8>) -> String {
            pct.map_or_else(|| "--".to_string(), |pct| format!("{pct}%"))
        }
        write!(f, "5h {}, 7d {}", window(self.five_hour), window(self.seven_day))
    }
}

pub fn default_config_path() -> PathBuf {
    if let Some(dir) = std::env::var_os("CLAUDE_CONFIG_DIR") {
        return PathBuf::from(dir).join(".claude.json");
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    home.join(".claude.json")
}

/// Reads both windows out of the Claude Code config.
///
/// Anything unreadable — no file, no cache yet, a shape we do not know — is
/// `None` rather than an error. A missing reading is a normal state of the
/// world here, and the screen already says `--` for it.
pub fn read(path: &Path) -> Usage {
    match std::fs::read_to_string(path) {
        Ok(text) => parse(&text, Utc::now()),
        Err(_) => Usage::default(),
    }
}

/// The cache only refreshes while a session is running, so an afternoon without
/// Claude Code would otherwise leave the screen advertising a limit that has
/// since rolled over. Each window carries its own `resets_at`: once that moment
/// has passed the window is empty by definition, and reporting it as `0` is a
/// fact rather than a guess.
fn parse(text: &str, now: DateTime<Utc>) -> Usage {
    let Ok(config) = serde_json::from_str::<Config>(text) else {
        return Usage::default();
    };
    let Some(windows) = config.cached_usage_utilization.and_then(|cached| cached.utilization) else {
        return Usage::default();
    };
    Usage {
        five_hour: windows.five_hour.and_then(|window| window.percent(now)),
        seven_day: windows.seven_day.and_then(|window| window.percent(now)),
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
    five_hour: Option<Window>,
    seven_day: Option<Window>,
}

#[derive(Deserialize)]
struct Window {
    utilization: Option<f64>,
    resets_at: Option<String>,
}

impl Window {
    fn percent(&self, now: DateTime<Utc>) -> Option<u8> {
        let reset = self
            .resets_at
            .as_deref()
            .and_then(|at| DateTime::parse_from_rfc3339(at).ok())
            .is_some_and(|at| at.with_timezone(&Utc) <= now);
        if reset {
            return Some(0);
        }
        // Whole percent on the wire; the API has only ever sent integers, but a
        // fractional one would still have to land somewhere sane.
        Some(self.utilization?.round().clamp(0.0, 100.0) as u8)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-09-08T12:00:00+00:00")
            .expect("fixed timestamp")
            .with_timezone(&Utc)
    }

    #[test]
    fn reads_both_windows() {
        // Shape copied from a real ~/.claude.json, trimmed to what we read.
        let config = r#"{"cachedUsageUtilization":{"fetchedAtMs":1788878612419,"utilization":{
            "five_hour":{"utilization":24,"resets_at":"2026-09-08T15:49:59.804477+00:00"},
            "seven_day":{"utilization":2,"resets_at":"2026-09-15T10:59:59.804499+00:00"},
            "seven_day_opus":null}}}"#;
        assert_eq!(
            parse(config, now()),
            Usage {
                five_hour: Some(24),
                seven_day: Some(2),
            }
        );
    }

    #[test]
    fn a_window_past_its_reset_is_empty_rather_than_stale() {
        let config = r#"{"cachedUsageUtilization":{"utilization":{
            "five_hour":{"utilization":80,"resets_at":"2026-09-08T09:00:00+00:00"},
            "seven_day":{"utilization":40,"resets_at":"2026-09-15T10:59:59+00:00"}}}}"#;
        assert_eq!(
            parse(config, now()),
            Usage {
                five_hour: Some(0),
                seven_day: Some(40),
            }
        );
    }

    #[test]
    fn a_window_the_cache_does_not_carry_stays_unknown() {
        let config = r#"{"cachedUsageUtilization":{"utilization":{
            "five_hour":{"utilization":24,"resets_at":"2026-09-08T15:49:59+00:00"},
            "seven_day":null}}}"#;
        assert_eq!(
            parse(config, now()),
            Usage {
                five_hour: Some(24),
                seven_day: None,
            }
        );
    }

    #[test]
    fn a_config_without_the_cache_reads_as_unknown() {
        assert_eq!(parse(r#"{"hasCompletedOnboarding":true}"#, now()), Usage::default());
    }

    #[test]
    fn a_config_that_is_not_json_reads_as_unknown() {
        assert_eq!(parse("half a written file", now()), Usage::default());
    }

    #[test]
    fn an_absent_config_reads_as_unknown() {
        assert_eq!(read(Path::new("/nonexistent/.claude.json")), Usage::default());
    }
}

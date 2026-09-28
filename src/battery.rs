//! A CSV log of the halves' battery levels, for watching how fast they drain.
//!
//! One row per poll — time, left %, right %, then each half's sleep statistics
//! (wake-ups, awake minutes, uptime minutes) — with empty cells for a half the
//! dongle had no reading from, so a disconnect shows up as a gap rather than as
//! a repeated last value.

use std::fs::OpenOptions;
use std::io::{BufRead as _, BufReader, Write as _};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::qube::{Halves, SleepStats};

const HEADER: &str =
    "time,left,right,left_wakes,left_awake_min,left_up_min,right_wakes,right_awake_min,right_up_min";

pub struct Log {
    path: PathBuf,
    last: Option<(Option<u8>, Option<u8>)>,
}

impl Log {
    pub fn new(path: PathBuf) -> Self {
        Self { path, last: None }
    }

    /// `$XDG_STATE_HOME/qubeherd/battery.csv`, falling back to `~/.local/state`.
    pub fn default_path() -> PathBuf {
        std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")))
            .unwrap_or_else(|| PathBuf::from("."))
            .join("qubeherd")
            .join("battery.csv")
    }

    pub fn append(&mut self, halves: Halves) -> Result<()> {
        // Uptime moves every poll; only a level change is worth a journal line.
        let levels = (halves.left, halves.right);
        if self.last != Some(levels) {
            log::info!("battery: {halves}");
        }
        self.last = Some(levels);

        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        set_aside_other_layout(&self.path)?;
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .with_context(|| format!("opening {}", self.path.display()))?;
        if file.metadata()?.len() == 0 {
            writeln!(file, "{HEADER}")?;
        }
        writeln!(
            file,
            "{},{},{},{},{}",
            chrono::Local::now().format("%Y-%m-%dT%H:%M:%S%:z"),
            level_cell(halves.left),
            level_cell(halves.right),
            sleep_cells(halves.left_sleep),
            sleep_cells(halves.right_sleep),
        )
        .with_context(|| format!("writing {}", self.path.display()))
    }
}

fn level_cell(level: Option<u8>) -> String {
    level.map(|level| level.to_string()).unwrap_or_default()
}

fn sleep_cells(stats: Option<SleepStats>) -> String {
    stats.map_or_else(
        || ",,".to_string(),
        |stats| format!("{},{},{}", stats.wakes, stats.awake_min, stats.uptime_min),
    )
}

/// Renames a log written with other columns to `battery-until-<date>.csv`, so
/// new rows never land under a header that does not describe them.
fn set_aside_other_layout(path: &Path) -> Result<()> {
    let Ok(file) = std::fs::File::open(path) else {
        return Ok(());
    };
    let mut header = String::new();
    BufReader::new(file).read_line(&mut header)?;
    if header.is_empty() || header.trim_end() == HEADER {
        return Ok(());
    }
    let aside = path.with_file_name(format!("battery-until-{}.csv", chrono::Local::now().format("%Y-%m-%d")));
    log::info!("battery: the log has other columns, moving it to {}", aside.display());
    std::fs::rename(path, &aside).with_context(|| format!("moving {} aside", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sleep_cells_leave_a_gap_for_a_half_without_stats() {
        assert_eq!(sleep_cells(None), ",,");
        let stats = SleepStats {
            wakes: 3,
            awake_min: 41,
            uptime_min: 600,
        };
        assert_eq!(sleep_cells(Some(stats)), "3,41,600");
    }

    #[test]
    fn a_log_with_the_old_header_is_set_aside() {
        let dir = std::env::temp_dir().join(format!("qubeherd-battery-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("battery.csv");
        std::fs::write(&path, "time,left,right\n2026-09-27T16:54:08+03:00,95,67\n").unwrap();

        set_aside_other_layout(&path).unwrap();

        assert!(!path.exists());
        let moved: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(moved.len(), 1);
        assert!(moved[0].to_string_lossy().starts_with("battery-until-"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

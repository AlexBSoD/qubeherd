//! A CSV log of the halves' battery levels, for watching how fast they drain.
//!
//! One row per poll — time, left %, right % — with an empty cell for a half the
//! dongle had no reading from, so a disconnect shows up as a gap rather than as
//! a repeated last value.

use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::qube::Halves;

pub struct Log {
    path: PathBuf,
    last: Option<Halves>,
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
        if self.last != Some(halves) {
            log::info!("battery: {halves}");
        }
        self.last = Some(halves);

        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .with_context(|| format!("opening {}", self.path.display()))?;
        if file.metadata()?.len() == 0 {
            writeln!(file, "time,left,right")?;
        }
        let cell = |level: Option<u8>| level.map(|level| level.to_string()).unwrap_or_default();
        writeln!(
            file,
            "{},{},{}",
            chrono::Local::now().format("%Y-%m-%dT%H:%M:%S%:z"),
            cell(halves.left),
            cell(halves.right),
        )
        .with_context(|| format!("writing {}", self.path.display()))
    }
}

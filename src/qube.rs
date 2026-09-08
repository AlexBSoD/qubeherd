//! Raw-HID transport to the Qube dongle.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Packet types, mirrored in `rmk/src/host/via/mod.rs`.
const PACKET_AGENTS: u8 = 0xB0;
const PACKET_AGENTS_VERSION: u8 = 0x01;
const PACKET_USAGE: u8 = 0xB1;
const PACKET_USAGE_VERSION: u8 = 0x01;
/// A window the daemon could not read at all, as opposed to one sitting at 0%.
const PACKET_USAGE_UNKNOWN: u8 = 0xFF;
const PACKET_CLOCK: u8 = 0xAA;
const PACKET_LAYOUT: u8 = 0xAC;
const PACKET_LEN: usize = 32;

/// Ergohaven vendor id; the dongle and the wired halves share it.
const HID_VENDOR_ID: u16 = 0xE126;
/// Usage page 0xFF60 / usage 0x61 — the QMK-compatible raw HID interface.
const RAW_HID_DESCRIPTOR_PREFIX: [u8; 5] = [0x06, 0x60, 0xFF, 0x09, 0x61];

pub fn agents_packet(counts: &crate::herdr::AgentCounts) -> [u8; PACKET_LEN] {
    let mut payload = [0u8; PACKET_LEN];
    payload[0] = PACKET_AGENTS;
    payload[1] = PACKET_AGENTS_VERSION;
    payload[2] = counts.working;
    payload[3] = counts.idle;
    payload[4] = counts.blocked;
    payload[5] = counts.done;
    payload[6] = counts.unknown;
    payload
}

pub fn usage_packet(usage: &crate::usage::Usage) -> [u8; PACKET_LEN] {
    let mut payload = [0u8; PACKET_LEN];
    payload[0] = PACKET_USAGE;
    payload[1] = PACKET_USAGE_VERSION;
    payload[2] = usage.five_hour.unwrap_or(PACKET_USAGE_UNKNOWN);
    payload[3] = usage.seven_day.unwrap_or(PACKET_USAGE_UNKNOWN);
    payload
}

pub fn clock_packet(hour: u8, minute: u8) -> [u8; PACKET_LEN] {
    let mut payload = [0u8; PACKET_LEN];
    payload[0] = PACKET_CLOCK;
    payload[1] = hour;
    payload[2] = minute;
    payload
}

pub fn layout_packet(code: u8) -> [u8; PACKET_LEN] {
    let mut payload = [0u8; PACKET_LEN];
    payload[0] = PACKET_LAYOUT;
    payload[1] = code;
    payload
}

/// The dongle's raw-HID node, reopened across unplug/replug.
pub struct Qube {
    device: Option<File>,
    path: Option<PathBuf>,
    /// Set from `--device` to skip the search entirely.
    pinned: Option<PathBuf>,
    writes: u64,
    opens: u64,
}

impl Qube {
    pub fn new(pinned: Option<PathBuf>) -> Self {
        Self {
            device: None,
            path: None,
            pinned,
            writes: 0,
            opens: 0,
        }
    }

    /// Packets that reached the device, and how often it had to be opened —
    /// both only meaningful as a rate, which is what the summary reports.
    pub fn counters(&self) -> (u64, u64) {
        (self.writes, self.opens)
    }

    /// Opens the device if needed; returns true when this call opened it.
    ///
    /// A freshly opened device usually means the keyboard just rebooted, and a
    /// rebooted keyboard has forgotten both the clock and the host layout —
    /// callers use this to know they must push them again.
    pub fn ensure_open(&mut self) -> Result<bool> {
        if self.device.is_some() {
            return Ok(false);
        }
        let (path, name) = match self.pinned.clone() {
            Some(path) => (path, None),
            None => {
                let found = find_raw_hid_device()?.context("no Ergohaven raw HID interface found")?;
                (found.path, Some(found.name))
            }
        };
        let device = OpenOptions::new()
            .write(true)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        match name {
            Some(name) => log::info!("writing to {} ({name})", path.display()),
            None => log::info!("writing to {}", path.display()),
        }
        self.device = Some(device);
        self.path = Some(path);
        self.opens += 1;
        Ok(true)
    }

    pub fn write(&mut self, payload: &[u8; PACKET_LEN]) -> Result<()> {
        let Some(device) = self.device.as_mut() else {
            anyhow::bail!("device is not open");
        };
        // The interface has no report id, so hidraw wants a leading 0x00.
        let mut report = [0u8; PACKET_LEN + 1];
        report[1..].copy_from_slice(payload);
        if let Err(err) = device.write_all(&report) {
            self.close();
            return Err(err).context("writing to the dongle");
        }
        self.writes += 1;
        log::debug!("sent {:02x?}", &payload[..7]);
        Ok(())
    }

    pub fn close(&mut self) {
        self.device = None;
        self.path = None;
    }
}

/// One hidraw node that speaks the QMK-compatible raw HID protocol.
struct Candidate {
    path: PathBuf,
    /// The node number, because the directory listing is lexicographic and
    /// there `hidraw10` sorts before `hidraw2`: without this the pick silently
    /// flips between replugs once numbering crosses nine.
    number: u32,
    name: String,
}

/// Locates the keyboard's raw-HID node by walking sysfs.
///
/// Matching on the report descriptor rather than a fixed product id keeps this
/// working across the Qube/mini/micro variants, which differ in pid. The
/// tradeoff is that a wired half attached alongside the dongle matches just as
/// well — writes to the wrong node succeed silently, so say so out loud and let
/// `--device` settle it.
fn find_raw_hid_device() -> Result<Option<Candidate>> {
    let mut candidates: Vec<_> = std::fs::read_dir("/sys/class/hidraw")
        .context("listing /sys/class/hidraw")?
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| candidate(&entry.path()))
        .collect();
    candidates.sort_by_key(|candidate| candidate.number);

    if candidates.len() > 1 {
        let others: Vec<_> = candidates[1..]
            .iter()
            .map(|candidate| format!("{} ({})", candidate.path.display(), candidate.name))
            .collect();
        log::warn!(
            "several Ergohaven raw HID interfaces match; using {} and ignoring {} — pass --device to pick another",
            candidates[0].path.display(),
            others.join(", ")
        );
    }
    Ok(candidates.into_iter().next())
}

fn candidate(entry: &Path) -> Option<Candidate> {
    let uevent = std::fs::read_to_string(entry.join("device/uevent")).ok()?;
    let vendor_matches = uevent
        .lines()
        .find_map(|line| line.strip_prefix("HID_ID="))
        .and_then(|id| id.split(':').nth(1))
        .and_then(|vendor| u32::from_str_radix(vendor, 16).ok())
        .is_some_and(|vendor| vendor == u32::from(HID_VENDOR_ID));
    if !vendor_matches {
        return None;
    }
    let raw_hid = std::fs::read(entry.join("device/report_descriptor"))
        .is_ok_and(|descriptor| descriptor.starts_with(&RAW_HID_DESCRIPTOR_PREFIX));
    if !raw_hid {
        return None;
    }

    let node = entry.file_name()?.to_str()?;
    Some(Candidate {
        path: Path::new("/dev").join(node),
        number: node.trim_start_matches("hidraw").parse().unwrap_or(u32::MAX),
        name: uevent
            .lines()
            .find_map(|line| line.strip_prefix("HID_NAME="))
            .unwrap_or("unnamed")
            .to_string(),
    })
}

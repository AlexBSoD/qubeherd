//! Raw-HID transport to the Qube dongle.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Packet types, mirrored in `rmk/src/host/via/mod.rs`.
const PACKET_AGENTS: u8 = 0xB0;
const PACKET_AGENTS_VERSION: u8 = 0x01;
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
}

impl Qube {
    pub fn new() -> Self {
        Self {
            device: None,
            path: None,
        }
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
        let path = find_raw_hid_device()?.context("no Ergohaven raw HID interface found")?;
        let device = OpenOptions::new()
            .write(true)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        log::info!("writing to {}", path.display());
        self.device = Some(device);
        self.path = Some(path);
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
        log::debug!("sent {:02x?}", &payload[..7]);
        Ok(())
    }

    pub fn close(&mut self) {
        self.device = None;
        self.path = None;
    }
}

/// Locates the keyboard's raw-HID node by walking sysfs.
///
/// Matching on the report descriptor rather than a fixed product id keeps this
/// working across the Qube/mini/micro variants, which differ in pid.
fn find_raw_hid_device() -> Result<Option<PathBuf>> {
    let mut entries: Vec<_> = std::fs::read_dir("/sys/class/hidraw")
        .context("listing /sys/class/hidraw")?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .collect();
    entries.sort();

    for entry in entries {
        if !matches_qube(&entry) {
            continue;
        }
        let Some(name) = entry.file_name() else {
            continue;
        };
        return Ok(Some(Path::new("/dev").join(name)));
    }
    Ok(None)
}

fn matches_qube(entry: &Path) -> bool {
    let uevent = match std::fs::read_to_string(entry.join("device/uevent")) {
        Ok(text) => text,
        Err(_) => return false,
    };
    let vendor_matches = uevent
        .lines()
        .find_map(|line| line.strip_prefix("HID_ID="))
        .and_then(|id| id.split(':').nth(1))
        .and_then(|vendor| u32::from_str_radix(vendor, 16).ok())
        .is_some_and(|vendor| vendor == u32::from(HID_VENDOR_ID));
    if !vendor_matches {
        return false;
    }
    std::fs::read(entry.join("device/report_descriptor"))
        .is_ok_and(|descriptor| descriptor.starts_with(&RAW_HID_DESCRIPTOR_PREFIX))
}

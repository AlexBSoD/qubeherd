//! Raw-HID transport to the Qube dongle.

use std::fs::{File, OpenOptions};
use std::io::{Read as _, Write};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::unix::AsyncFd;

/// Packet types, mirrored in `rmk/src/host/via/mod.rs`.
const PACKET_AGENTS: u8 = 0xB0;
const PACKET_AGENTS_VERSION: u8 = 0x01;
const PACKET_USAGE: u8 = 0xB1;
const PACKET_USAGE_VERSION: u8 = 0x01;
/// A window the daemon could not read at all, as opposed to one sitting at 0%.
const PACKET_USAGE_UNKNOWN: u8 = 0xFF;
/// Nothing has refreshed the reading recently; the screen dims the bars for it.
const PACKET_USAGE_FLAG_STALE: u8 = 0x01;
const PACKET_CLOCK: u8 = 0xAA;
const PACKET_LAYOUT: u8 = 0xAC;
const PACKET_LEN: usize = 32;

/// Via `CustomGetValue` in the Ergohaven namespace, asking for the halves'
/// battery levels; answered by `ERGOHAVEN_CUSTOM_BATTERY_HALVES` in the firmware.
const BATTERY_REQUEST: [u8; 3] = [0x08, 0xE8, 0x01];
const BATTERY_REPLY_VERSION: u8 = 0x01;
/// The dongle answers from RAM within milliseconds; this only bounds a wedged one.
const BATTERY_REPLY_TIMEOUT: Duration = Duration::from_secs(1);
/// `O_NONBLOCK` on Linux, spelled out rather than pulling in `libc` for one flag.
const O_NONBLOCK: i32 = 0o4000;

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
    if usage.stale {
        payload[4] = PACKET_USAGE_FLAG_STALE;
    }
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

/// Battery levels of the two halves, in percent; `None` for a half the dongle
/// has no reading from — disconnected, or not reported since it booted.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub struct Halves {
    pub left: Option<u8>,
    pub right: Option<u8>,
    pub left_sleep: Option<SleepStats>,
    pub right_sleep: Option<SleepStats>,
}

/// How a half has slept since it booted, as last reported to the dongle; only
/// firmware with the sleep-stats diagnostic sends it. Lags one poll behind:
/// the half answers the refresh this request triggers after the reply is out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SleepStats {
    pub wakes: u16,
    pub awake_min: u16,
    pub uptime_min: u16,
}

impl SleepStats {
    fn parse(fields: &[u8]) -> Self {
        let word = |i: usize| u16::from_le_bytes([fields[i], fields[i + 1]]);
        Self {
            wakes: word(0),
            awake_min: word(2),
            uptime_min: word(4),
        }
    }
}

impl std::fmt::Display for Halves {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let show = |level: Option<u8>| level.map_or("--".to_string(), |level| format!("{level}%"));
        write!(f, "left {}, right {}", show(self.left), show(self.right))
    }
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

    /// Asks the dongle for the halves' battery levels.
    ///
    /// The only packet here that expects an answer. The reply comes back on a
    /// reader opened just for it: hidraw hands every input report to every open
    /// reader and drops new ones once a reader's queue is full, so one kept open
    /// would fill with the dongle's answers to the packets above and then lose
    /// exactly the reply it was waiting for. A fresh reader starts empty, and
    /// the header match skips an answer meant for Entropy on the same node.
    pub async fn battery_halves(&mut self) -> Result<Halves> {
        let path = self.path.clone().context("device is not open")?;
        let reader = OpenOptions::new()
            .read(true)
            .custom_flags(O_NONBLOCK)
            .open(&path)
            .with_context(|| format!("opening {} for reading", path.display()))?;
        let reader = AsyncFd::new(reader).context("registering the reader")?;

        let mut request = [0u8; PACKET_LEN];
        request[..BATTERY_REQUEST.len()].copy_from_slice(&BATTERY_REQUEST);
        self.write(&request)?;

        let reply = async {
            loop {
                let mut ready = reader.readable().await?;
                let mut report = [0u8; PACKET_LEN];
                let Ok(read) = ready.try_io(|fd| fd.get_ref().read(&mut report)) else {
                    continue;
                };
                let read = read?;
                if read >= 7 && report[..3] == BATTERY_REQUEST && report[3] == BATTERY_REPLY_VERSION {
                    let flags = report[4];
                    let has_sleep = |flag: u8| read >= 19 && flags & flag != 0;
                    return std::io::Result::Ok(Halves {
                        left: (flags & 0x01 != 0).then_some(report[5]),
                        right: (flags & 0x02 != 0).then_some(report[6]),
                        left_sleep: has_sleep(0x04).then(|| SleepStats::parse(&report[7..13])),
                        right_sleep: has_sleep(0x08).then(|| SleepStats::parse(&report[13..19])),
                    });
                }
            }
        };
        tokio::time::timeout(BATTERY_REPLY_TIMEOUT, reply)
            .await
            .context("the dongle did not answer the battery request")?
            .context("reading the battery reply")
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

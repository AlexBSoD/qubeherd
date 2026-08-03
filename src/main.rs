//! Push herdr agent state, the host clock and the keyboard layout to the
//! Ergohaven Qube dongle screen.
//!
//! herdr knows which coding agents are working, idle or blocked. The dongle
//! has a screen right under your hands. This bridges the two over raw HID,
//! and carries the two host-data packets Entropy would otherwise own: the
//! header clock and — more importantly — the active keyboard layout, which
//! Universal Symbols need to pick the right keycodes.

mod herdr;
mod layout;
mod qube;

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use futures_util::StreamExt as _;
use tokio::time::{sleep, Instant};

/// Agent states flap between tool calls; coalesce a burst into one packet.
const DEBOUNCE: Duration = Duration::from_millis(300);
/// Well under the firmware's 30s expiry for the agent counts.
const HEARTBEAT: Duration = Duration::from_secs(10);
/// Nothing expires the layout on the firmware side, so a lost packet would be
/// silent and lasting: refresh it on the same beat as the agent counts.
const LAYOUT_REFRESH: Duration = Duration::from_secs(10);
const RECONNECT_MIN: Duration = Duration::from_secs(1);
const RECONNECT_MAX: Duration = Duration::from_secs(30);

#[derive(Parser)]
#[command(about, version)]
struct Args {
    /// herdr API socket (default: $HERDR_SOCKET_PATH or ~/.config/herdr/herdr.sock)
    #[arg(long)]
    socket: Option<PathBuf>,

    /// Send one update from the current state and exit
    #[arg(long)]
    once: bool,

    /// Leave the header clock to Entropy instead of sending it
    #[arg(long)]
    no_clock: bool,

    /// Do not sync the host keyboard layout (Universal Symbols need it)
    #[arg(long)]
    no_layout: bool,

    /// Log every packet
    #[arg(long)]
    verbose: bool,
}

fn default_socket_path() -> PathBuf {
    if let Some(from_env) = std::env::var_os("HERDR_SOCKET_PATH") {
        return PathBuf::from(from_env);
    }
    let config_home = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."));
    config_home.join("herdr").join("herdr.sock")
}

/// Everything the dongle currently believes, so we only spend USB packets on
/// real changes — and know what to replay when it reboots.
#[derive(Default)]
struct Pushed {
    counts: Option<herdr::AgentCounts>,
    counts_at: Option<Instant>,
    clock: Option<(u8, u8)>,
    layout: Option<u8>,
    layout_at: Option<Instant>,
}

struct Bridge {
    client: herdr::Client,
    qube: qube::Qube,
    layouts: Option<layout::KdeLayouts>,
    clock: bool,
    pushed: Pushed,
}

impl Bridge {
    /// Sends the clock and the layout, whatever their last known values.
    ///
    /// Called after every (re)open of the device: a keyboard that just rebooted
    /// starts from `HostLayout::English` and an empty clock, so replaying them
    /// is what keeps Universal Symbols correct across a reflash or a replug.
    async fn resync_after_open(&mut self) -> Result<()> {
        if self.clock {
            let (hour, minute) = now_hm();
            self.qube.write(&qube::clock_packet(hour, minute))?;
            self.pushed.clock = Some((hour, minute));
        }
        if let Some(layouts) = self.layouts.as_mut() {
            if let Some(code) = layouts.current().await {
                self.qube.write(&qube::layout_packet(code))?;
                log::info!("layout: {}", layout::code_name(code));
                self.pushed.layout = Some(code);
                self.pushed.layout_at = Some(Instant::now());
            }
        }
        Ok(())
    }

    /// Opens the device if needed, replaying state when it was just opened.
    async fn ensure_ready(&mut self) -> Result<()> {
        if self.qube.ensure_open()? {
            self.resync_after_open().await?;
        }
        Ok(())
    }

    async fn push_layout(&mut self, code: u8) -> Result<()> {
        self.ensure_ready().await?;
        self.qube.write(&qube::layout_packet(code))?;
        if self.pushed.layout != Some(code) {
            log::info!("layout: {}", layout::code_name(code));
        }
        self.pushed.layout = Some(code);
        self.pushed.layout_at = Some(Instant::now());
        Ok(())
    }

    /// The periodic beat: agent counts, clock and a layout refresh.
    async fn push_periodic(&mut self) -> Result<()> {
        self.ensure_ready().await?;

        if self.clock {
            let hm = now_hm();
            if self.pushed.clock != Some(hm) {
                self.qube.write(&qube::clock_packet(hm.0, hm.1))?;
                self.pushed.clock = Some(hm);
            }
        }

        let stale = self
            .pushed
            .layout_at
            .is_none_or(|at| at.elapsed() >= LAYOUT_REFRESH);
        if stale {
            if let Some(layouts) = self.layouts.as_mut() {
                if let Some(code) = layouts.current().await {
                    self.qube.write(&qube::layout_packet(code))?;
                    if self.pushed.layout != Some(code) {
                        log::info!("layout: {}", layout::code_name(code));
                    }
                    self.pushed.layout = Some(code);
                    self.pushed.layout_at = Some(Instant::now());
                }
            }
        }

        // herdr emits pane.updated for scrolling and resizes too, so most
        // wake-ups carry no news: only spend a USB packet on a real change or
        // on keeping the firmware's 30s expiry alive.
        let counts = self.client.agent_counts().await?;
        let changed = self.pushed.counts != Some(counts);
        let stale = self
            .pushed
            .counts_at
            .is_none_or(|at| at.elapsed() >= HEARTBEAT);
        if changed || stale {
            if changed {
                log::info!("agents: {counts}");
            }
            self.qube.write(&qube::agents_packet(&counts))?;
            self.pushed.counts = Some(counts);
            self.pushed.counts_at = Some(Instant::now());
        }
        Ok(())
    }
}

fn now_hm() -> (u8, u8) {
    use chrono::Timelike as _;
    let now = chrono::Local::now();
    (now.hour() as u8, now.minute() as u8)
}

/// One connected lifetime: subscribed to herdr, following layout changes.
async fn session(bridge: &mut Bridge) -> Result<()> {
    let mut events = bridge.client.subscribe().await?;
    let mut changes = match bridge.layouts.as_ref() {
        Some(layouts) => Some(layouts.changes().await?),
        None => None,
    };

    let mut deadline = Instant::now();
    loop {
        let layout_change = async {
            match changes.as_mut() {
                Some(stream) => stream.next().await,
                // Keep this branch pending forever rather than resolving to
                // None in a busy loop when there is no layout source.
                None => std::future::pending().await,
            }
        };

        tokio::select! {
            result = events.next() => {
                result?;
                deadline = deadline.min(Instant::now() + DEBOUNCE);
            }
            signal = layout_change => {
                // A layout change must reach the keyboard immediately: every
                // keystroke until it does produces the wrong symbol.
                let index = signal.and_then(|signal| signal.args().ok().map(|args| args.index));
                if let Some(index) = index {
                    if let Some(layouts) = bridge.layouts.as_mut() {
                        if let Some(code) = layouts.code_for_index(index).await {
                            bridge.push_layout(code).await?;
                        }
                    }
                }
            }
            _ = sleep_until(deadline) => {
                bridge.push_periodic().await?;
                deadline = Instant::now() + HEARTBEAT;
            }
        }
    }
}

async fn sleep_until(deadline: Instant) -> Instant {
    tokio::time::sleep_until(deadline).await;
    deadline
}

async fn run(args: Args) -> Result<()> {
    let socket_path = args.socket.clone().unwrap_or_else(default_socket_path);
    let layouts = if args.no_layout {
        None
    } else {
        match layout::KdeLayouts::connect().await {
            Ok(layouts) => Some(layouts),
            Err(err) => {
                log::warn!("no KDE layout service ({err:#}); Universal Symbols will not be synced");
                None
            }
        }
    };

    let mut bridge = Bridge {
        client: herdr::Client::new(&socket_path),
        qube: qube::Qube::new(),
        layouts,
        clock: !args.no_clock,
        pushed: Pushed::default(),
    };

    if args.once {
        bridge.ensure_ready().await?;
        return bridge.push_periodic().await;
    }

    let mut backoff = RECONNECT_MIN;
    loop {
        match session(&mut bridge).await {
            Ok(()) => unreachable!("the session loop only exits with an error"),
            Err(err) => {
                bridge.qube.close();
                bridge.pushed = Pushed::default();
                log::warn!("{err:#}; retrying in {}s", backoff.as_secs());
                sleep(backoff).await;
                backoff = (backoff * 2).min(RECONNECT_MAX);
            }
        }
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let args = Args::parse();
    env_logger::Builder::new()
        .filter_level(if args.verbose {
            log::LevelFilter::Debug
        } else {
            log::LevelFilter::Info
        })
        .format_target(false)
        .format_timestamp(None)
        .init();

    match run(args).await.context("qubeherd") {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            log::error!("{err:#}");
            ExitCode::FAILURE
        }
    }
}

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
/// A session that lasted this long is evidence the world is healthy again, and
/// the next hiccup deserves a fast retry rather than the backoff we ended on.
const HEALTHY_SESSION: Duration = Duration::from_secs(60);
/// Self-requested restarts are normally instant; more than a few in a row
/// without a healthy session in between means something is flapping.
const RAPID_RESTART_LIMIT: u32 = 3;
/// How often to look for a KDE layout service that was not there at startup.
const LAYOUT_PROBE: Duration = Duration::from_secs(30);

#[derive(Parser)]
#[command(about, version)]
struct Args {
    /// herdr API socket (default: $HERDR_SOCKET_PATH or ~/.config/herdr/herdr.sock)
    #[arg(long)]
    socket: Option<PathBuf>,

    /// hidraw node to write to, skipping the search (e.g. /dev/hidraw5)
    #[arg(long)]
    device: Option<PathBuf>,

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
    /// Whether to keep looking for a layout source we do not have yet.
    wants_layout: bool,
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
        // The counts are gone from the keyboard too, and `pushed` still claims
        // they are on screen. Forget them so the next beat resends them instead
        // of waiting out a heartbeat the firmware may not survive.
        self.pushed.counts = None;
        self.pushed.counts_at = None;
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
                let code = layouts.current().await;
                // Stamp the attempt, not the success. A layout the firmware has
                // no code for (a third one such as `de`) or a service that
                // stopped answering yields None every time, and rate-limiting
                // only on success would turn every wake-up into a bus round trip
                // and a log line.
                self.pushed.layout_at = Some(Instant::now());
                if let Some(code) = code {
                    self.qube.write(&qube::layout_packet(code))?;
                    if self.pushed.layout != Some(code) {
                        log::info!("layout: {}", layout::code_name(code));
                    }
                    self.pushed.layout = Some(code);
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
///
/// Returns `Ok(())` when the caller should start a fresh session: the layout
/// source appeared and needs its signal streams, or the one we had went away
/// and must be dropped before it can be looked for again.
async fn session(bridge: &mut Bridge) -> Result<()> {
    let mut events = bridge.client.subscribe().await?;
    // The layout source is optional. Failing to follow it costs the layout, so
    // let go of the source and keep the clock and the agent counts running,
    // rather than propagating and taking the whole daemon down with it.
    let mut signals = match bridge.layouts.as_ref() {
        Some(layouts) => match layouts.signals().await {
            Ok(signals) => Some(signals),
            Err(err) => {
                log::warn!("cannot follow layout changes ({err:#}); will look again");
                bridge.layouts = None;
                None
            }
        },
        None => None,
    };
    let mut next_layout_probe = Instant::now() + LAYOUT_PROBE;

    let mut deadline = Instant::now();
    loop {
        // The session bus may well outlive us, but KDE's layout service can
        // also appear late (this daemon can start before Plasma) or come back
        // after a Plasma restart. Keep looking rather than staying blind.
        let probing = bridge.wants_layout && bridge.layouts.is_none();

        tokio::select! {
            // Branch order is a priority, not a formality:
            //   1. a layout change must reach the keyboard immediately — every
            //      keystroke until it does produces the wrong symbol;
            //   2. the beat comes next so a chatty herdr can never starve it;
            //   3. herdr events are only hints, and `agent.list` is the truth,
            //      so they are the one thing that can safely wait.
            biased;

            change = next_change(&mut signals) => match change {
                layout::Change::Active(index) => {
                    if let Some(layouts) = bridge.layouts.as_mut() {
                        if let Some(code) = layouts.code_for_index(index).await {
                            bridge.push_layout(code).await?;
                        }
                    }
                }
                layout::Change::ListEdited => {
                    // An index is a position in that list: a reorder keeping the
                    // same length would otherwise map ru onto the en code and
                    // stay wrong until this daemon restarts.
                    if let Some(layouts) = bridge.layouts.as_mut() {
                        layouts.refresh_names().await;
                        if let Some(code) = layouts.current().await {
                            bridge.push_layout(code).await?;
                        }
                    }
                }
                layout::Change::Lost => {
                    // An ended stream is Ready(None) on every later poll, so
                    // leaving it in the select! would spin this loop at 100% CPU
                    // and — on a current_thread runtime, where nothing else can
                    // make progress while we never yield — freeze the timer and
                    // the herdr socket with it.
                    log::warn!("the layout service went away; will look for it again");
                    bridge.layouts = None;
                    return Ok(());
                }
                layout::Change::Unreadable => {}
            },
            // The probe deadline is armed only in the same breath as it is
            // consulted: a disarmed one cannot wake us, because the guard stops
            // the branch from being polled at all.
            _ = sleep_until(next_layout_probe), if probing => {
                next_layout_probe = Instant::now() + LAYOUT_PROBE;
                if let Ok(layouts) = layout::KdeLayouts::connect().await {
                    log::info!("KDE layout service appeared; syncing layout again");
                    bridge.layouts = Some(layouts);
                    return Ok(());
                }
            }
            _ = sleep_until(deadline) => {
                bridge.push_periodic().await?;
                deadline = Instant::now() + HEARTBEAT;
            }
            result = events.next() => {
                result?;
                // Pull the beat in, never push it out: a burst coalesces into
                // one packet instead of deferring it for the length of the burst.
                deadline = deadline.min(Instant::now() + DEBOUNCE);
            }
        }
    }
}

/// The next layout signal, or forever-pending when there is no layout source.
async fn next_change(signals: &mut Option<layout::Signals>) -> layout::Change {
    match signals.as_mut() {
        Some(signals) => signals.next().await,
        None => std::future::pending().await,
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
                log::warn!("no KDE layout service yet ({err:#}); will keep looking");
                None
            }
        }
    };

    let mut bridge = Bridge {
        client: herdr::Client::new(&socket_path),
        qube: qube::Qube::new(args.device.clone()),
        layouts,
        wants_layout: !args.no_layout,
        clock: !args.no_clock,
        pushed: Pushed::default(),
    };

    if args.once {
        bridge.ensure_ready().await?;
        return bridge.push_periodic().await;
    }

    let mut backoff = RECONNECT_MIN;
    let mut rapid_restarts = 0;
    loop {
        let started = Instant::now();
        let outcome = session(&mut bridge).await;
        // Without this the backoff decays at most once per process: a boot race
        // that saturates it at RECONNECT_MAX would still be charging 30s for a
        // one-second herdr restart hours later — long enough for the firmware to
        // expire the counts and blank the row over nothing.
        if started.elapsed() >= HEALTHY_SESSION {
            backoff = RECONNECT_MIN;
            rapid_restarts = 0;
        }

        match outcome {
            // A restart we asked for ourselves: the layout source appeared or
            // went away, and either way the next session is built differently.
            Ok(()) => {
                rapid_restarts += 1;
                if rapid_restarts > RAPID_RESTART_LIMIT {
                    log::warn!(
                        "the layout source keeps flapping; pausing for {}s",
                        RECONNECT_MIN.as_secs()
                    );
                    sleep(RECONNECT_MIN).await;
                }
            }
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

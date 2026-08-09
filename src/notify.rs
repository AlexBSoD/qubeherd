//! systemd readiness and watchdog notifications.
//!
//! Hand-rolled rather than pulling in a crate: this is one datagram sent to a
//! socket systemd names in the environment, and the flake would have to vendor
//! a dependency for it. Every method is a no-op outside systemd, so running the
//! binary by hand behaves exactly as before.

use std::os::linux::net::SocketAddrExt as _;
use std::os::unix::net::{SocketAddr, UnixDatagram};
use std::time::Duration;

use tokio::time::Instant;

/// Ping at half the interval systemd asks for — the customary safety margin,
/// so one missed round trip does not trip the watchdog.
const PING_DIVISOR: u32 = 2;

pub struct Notifier {
    socket: Option<(UnixDatagram, SocketAddr)>,
    /// `None` when systemd did not ask to be kept informed.
    interval: Option<Duration>,
    next_ping: Option<Instant>,
}

impl Notifier {
    /// Reads `NOTIFY_SOCKET` and `WATCHDOG_USEC`, both set by systemd.
    pub fn from_env() -> Self {
        let socket = std::env::var_os("NOTIFY_SOCKET").and_then(|value| {
            let value = value.to_str()?;
            // A leading '@' means the abstract namespace, where the name is
            // not a filesystem path.
            let address = match value.strip_prefix('@') {
                Some(name) => SocketAddr::from_abstract_name(name.as_bytes()),
                None => SocketAddr::from_pathname(value),
            };
            Some((UnixDatagram::unbound().ok()?, address.ok()?))
        });

        // WATCHDOG_PID guards against inheriting the variable into a child that
        // is not the process systemd is watching.
        let ours = std::env::var("WATCHDOG_PID")
            .ok()
            .and_then(|pid| pid.parse::<u32>().ok())
            .is_none_or(|pid| pid == std::process::id());
        let interval = std::env::var("WATCHDOG_USEC")
            .ok()
            .and_then(|usec| usec.parse::<u64>().ok())
            .filter(|_| ours)
            .map(|usec| Duration::from_micros(usec) / PING_DIVISOR);

        if let Some(interval) = interval {
            log::debug!("watchdog: pinging every {}s", interval.as_secs());
        }
        Self {
            socket,
            interval,
            next_ping: None,
        }
    }

    /// Announces startup. Sent before the dongle is even looked for: the daemon
    /// is ready to do its job whether or not the keyboard is plugged in, and
    /// withholding it would leave the unit stuck in `activating` until the
    /// start timeout fired.
    pub fn ready(&self) {
        self.send(b"READY=1");
    }

    /// Says we are still going round the loop.
    ///
    /// Rate-limited to the interval systemd asked for, so hot paths can call it
    /// without thinking. Note what this can and cannot catch: a wedged await is
    /// exactly what it is for, while a spinning loop pings very cheerfully —
    /// that one shows up in the periodic summary instead.
    pub fn ping(&mut self) {
        let Some(interval) = self.interval else {
            return;
        };
        let now = Instant::now();
        if self.next_ping.is_some_and(|next| now < next) {
            return;
        }
        self.next_ping = Some(now + interval);
        self.send(b"WATCHDOG=1");
    }

    fn send(&self, message: &[u8]) {
        let Some((socket, address)) = self.socket.as_ref() else {
            return;
        };
        // Failing to notify is not worth taking the daemon down for, and a
        // warning per ping would be its own kind of outage.
        if let Err(err) = socket.send_to_addr(message, address) {
            log::debug!("cannot notify systemd: {err}");
        }
    }
}

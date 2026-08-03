//! herdr socket API: an event subscription plus `agent.list` snapshots.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

/// `pane.agent_status_changed` needs a pane_id, which would mean tracking
/// subscriptions per pane as they come and go. `pane.updated` is global and
/// already carries agent_status, which is all this daemon needs.
const SUBSCRIPTIONS: [&str; 5] = [
    "pane.updated",
    "pane.created",
    "pane.closed",
    "pane.exited",
    "pane.agent_detected",
];

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AgentCounts {
    pub working: u8,
    pub idle: u8,
    pub blocked: u8,
    pub done: u8,
    pub unknown: u8,
}

impl std::fmt::Display for AgentCounts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} working, {} idle, {} blocked, {} done, {} unknown",
            self.working, self.idle, self.blocked, self.done, self.unknown
        )
    }
}

#[derive(Deserialize)]
struct Response {
    result: Option<AgentList>,
    error: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct AgentList {
    #[serde(default)]
    agents: Vec<Agent>,
}

#[derive(Deserialize)]
struct Agent {
    agent_status: Option<String>,
}

pub struct Client {
    socket_path: PathBuf,
}

impl Client {
    pub fn new(socket_path: &Path) -> Self {
        Self {
            socket_path: socket_path.to_path_buf(),
        }
    }

    /// One short-lived request/response call.
    pub async fn agent_counts(&self) -> Result<AgentCounts> {
        let stream = UnixStream::connect(&self.socket_path)
            .await
            .with_context(|| format!("connecting to {}", self.socket_path.display()))?;
        let mut reader = BufReader::new(stream);
        let request = serde_json::json!({
            "id": "qubeherd:agent.list",
            "method": "agent.list",
            "params": {},
        });
        reader
            .get_mut()
            .write_all(format!("{request}\n").as_bytes())
            .await
            .context("sending agent.list")?;

        let mut line = String::new();
        if reader
            .read_line(&mut line)
            .await
            .context("reading agent.list")?
            == 0
        {
            bail!("herdr closed the socket during agent.list");
        }
        let response: Response = serde_json::from_str(&line).context("parsing agent.list")?;
        if let Some(error) = response.error {
            bail!("agent.list failed: {error}");
        }
        let agents = response.result.map(|list| list.agents).unwrap_or_default();

        let mut counts = AgentCounts::default();
        for agent in agents {
            let slot = match agent.agent_status.as_deref() {
                Some("working") => &mut counts.working,
                Some("idle") => &mut counts.idle,
                Some("blocked") => &mut counts.blocked,
                Some("done") => &mut counts.done,
                _ => &mut counts.unknown,
            };
            *slot = slot.saturating_add(1);
        }
        Ok(counts)
    }

    /// Subscribes to the pane events that hint an agent may have moved.
    pub async fn subscribe(&self) -> Result<Events> {
        let stream = UnixStream::connect(&self.socket_path)
            .await
            .with_context(|| format!("connecting to {}", self.socket_path.display()))?;
        let mut reader = BufReader::new(stream);
        let subscriptions: Vec<_> = SUBSCRIPTIONS
            .iter()
            .map(|name| serde_json::json!({ "type": name }))
            .collect();
        let request = serde_json::json!({
            "id": "qubeherd:subscribe",
            "method": "events.subscribe",
            "params": { "subscriptions": subscriptions },
        });
        reader
            .get_mut()
            .write_all(format!("{request}\n").as_bytes())
            .await
            .context("sending events.subscribe")?;

        let mut line = String::new();
        if reader
            .read_line(&mut line)
            .await
            .context("reading subscribe reply")?
            == 0
        {
            bail!("herdr closed the socket during subscribe");
        }
        let reply: serde_json::Value =
            serde_json::from_str(&line).context("parsing subscribe reply")?;
        if let Some(error) = reply.get("error") {
            bail!("subscribe rejected: {error}");
        }
        log::info!("subscribed to herdr at {}", self.socket_path.display());
        Ok(Events { reader })
    }
}

pub struct Events {
    reader: BufReader<UnixStream>,
}

impl Events {
    /// Waits for the next event. The payload is deliberately ignored: it is
    /// only a hint that something moved, and `agent.list` is the truth.
    pub async fn next(&mut self) -> Result<()> {
        let mut line = String::new();
        if self
            .reader
            .read_line(&mut line)
            .await
            .context("reading event stream")?
            == 0
        {
            bail!("herdr closed the event stream");
        }
        Ok(())
    }
}

//! herdr socket API: an event subscription plus `agent.list` snapshots.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
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

/// herdr answers these from memory, so anything slower is a hang, not load.
///
/// A unix socket has no read timeout of its own, and a peer that accepts the
/// connection without ever replying would park us inside `read_line` forever:
/// the call happens in the body of a `select!` arm, so the whole event loop —
/// heartbeat, clock and layout — would freeze with no error to recover from.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

const AGENT_LIST_ID: &str = "qubeherd:agent.list";
const SUBSCRIBE_ID: &str = "qubeherd:subscribe";

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
    #[serde(default)]
    id: Option<String>,
    result: Option<AgentList>,
    error: Option<serde_json::Value>,
}

/// A reply we only check for failure: `events.subscribe` answers with a result
/// of its own shape, which must not be held to `agent.list`'s.
#[derive(Deserialize)]
struct Ack {
    #[serde(default)]
    id: Option<String>,
    error: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct AgentList {
    /// Deliberately not `#[serde(default)]`: a reply whose shape we no longer
    /// understand must be an error, not a confident report of zero agents.
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

    /// Connects and sends one request line, returning the reader for the reply.
    async fn send(&self, request: &serde_json::Value, what: &'static str) -> Result<Connection> {
        let stream = deadline(
            UnixStream::connect(&self.socket_path),
            &format!("connecting to {}", self.socket_path.display()),
        )
        .await?;
        let mut reader = BufReader::new(stream);
        deadline(
            reader.get_mut().write_all(format!("{request}\n").as_bytes()),
            what,
        )
        .await?;
        Ok(reader)
    }

    /// One short-lived request/response call.
    pub async fn agent_counts(&self) -> Result<AgentCounts> {
        let request = serde_json::json!({
            "id": AGENT_LIST_ID,
            "method": "agent.list",
            "params": {},
        });
        let mut reader = self.send(&request, "sending agent.list").await?;

        let line = read_reply(&mut reader, "reading agent.list").await?;
        let response: Response = serde_json::from_str(&line).context("parsing agent.list")?;
        if let Some(error) = error_of(response.error) {
            bail!("agent.list failed: {error}");
        }
        // The socket is ours alone, but an unsolicited line arriving first
        // would otherwise be read as an authoritative agent list.
        if let Some(id) = response.id.as_deref() {
            if id != AGENT_LIST_ID {
                bail!("agent.list answered with a foreign id: {id}");
            }
        }
        let agents = response
            .result
            .context("agent.list returned neither a result nor an error")?
            .agents;
        Ok(tally(agents))
    }

    /// Subscribes to the pane events that hint an agent may have moved.
    pub async fn subscribe(&self) -> Result<Events> {
        let subscriptions: Vec<_> = SUBSCRIPTIONS
            .iter()
            .map(|name| serde_json::json!({ "type": name }))
            .collect();
        let request = serde_json::json!({
            "id": SUBSCRIBE_ID,
            "method": "events.subscribe",
            "params": { "subscriptions": subscriptions },
        });
        let mut reader = self.send(&request, "sending events.subscribe").await?;

        let line = read_reply(&mut reader, "reading subscribe reply").await?;
        let reply: Ack = serde_json::from_str(&line).context("parsing subscribe reply")?;
        if let Some(error) = error_of(reply.error) {
            bail!("subscribe rejected: {error}");
        }
        if let Some(id) = reply.id.as_deref() {
            if id != SUBSCRIBE_ID {
                bail!("subscribe answered with a foreign id: {id}");
            }
        }
        log::info!("subscribed to herdr at {}", self.socket_path.display());
        Ok(Events {
            lines: reader.lines(),
        })
    }
}

type Connection = BufReader<UnixStream>;

/// A JSON-RPC reply carries `"error": null` on success, which is a present key
/// holding null — not an absent one. Treating it as a failure would reject
/// every reply from a server that spells success that way.
fn error_of(error: Option<serde_json::Value>) -> Option<serde_json::Value> {
    error.filter(|error| !error.is_null())
}

fn tally(agents: Vec<Agent>) -> AgentCounts {
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
    counts
}

async fn read_reply(reader: &mut Connection, what: &'static str) -> Result<String> {
    let mut line = String::new();
    if deadline(reader.read_line(&mut line), what).await? == 0 {
        bail!("herdr closed the socket during {what}");
    }
    Ok(line)
}

/// Bounds one socket operation, turning a hung peer into an ordinary error.
async fn deadline<T>(
    operation: impl std::future::Future<Output = std::io::Result<T>>,
    what: &str,
) -> Result<T> {
    match tokio::time::timeout(REQUEST_TIMEOUT, operation).await {
        Ok(result) => result.with_context(|| what.to_string()),
        Err(_) => bail!("{what} timed out after {}s", REQUEST_TIMEOUT.as_secs()),
    }
}

pub struct Events {
    /// `Lines` rather than a bare `BufReader`: this is polled as a `select!`
    /// arm, and `read_line` is not cancellation-safe — it drains bytes into the
    /// future, so every wake-up on another arm would discard a partial line and
    /// eventually split a UTF-8 sequence. `Lines::next_line` keeps that partial
    /// state in the struct, which survives cancellation.
    lines: Lines<Connection>,
}

impl Events {
    /// Waits for the next event. The payload is deliberately ignored: it is
    /// only a hint that something moved, and `agent.list` is the truth.
    ///
    /// No timeout here — a quiet herdr is normal, unlike a quiet request.
    pub async fn next(&mut self) -> Result<()> {
        match self
            .lines
            .next_line()
            .await
            .context("reading event stream")?
        {
            Some(_) => Ok(()),
            None => bail!("herdr closed the event stream"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Captured from a live herdr: the subscribe result has a shape of its own,
    /// so holding it to `agent.list`'s would reject every subscription.
    const REAL_SUBSCRIBE_ACK: &str =
        r#"{"id":"qubeherd:subscribe","result":{"type":"subscription_started"}}"#;
    /// Also captured live, trimmed to the fields this daemon reads.
    const REAL_AGENT_LIST: &str = r#"{"id":"qubeherd:agent.list","result":{"type":"agent_list","agents":[{"terminal_id":"t1","agent":"pi","agent_status":"idle"},{"terminal_id":"t2","agent_status":"working"},{"terminal_id":"t3"}]}}"#;

    #[test]
    fn accepts_the_subscribe_ack_herdr_actually_sends() {
        let reply: Ack = serde_json::from_str(REAL_SUBSCRIBE_ACK).expect("parses");
        assert!(error_of(reply.error).is_none());
        assert_eq!(reply.id.as_deref(), Some(SUBSCRIBE_ID));
    }

    #[test]
    fn reads_the_agent_list_herdr_actually_sends() {
        let response: Response = serde_json::from_str(REAL_AGENT_LIST).expect("parses");
        let counts = tally(response.result.expect("has a result").agents);
        assert_eq!(
            counts,
            AgentCounts {
                working: 1,
                idle: 1,
                unknown: 1,
                ..AgentCounts::default()
            }
        );
    }

    #[test]
    fn treats_an_explicit_null_error_as_success() {
        let reply: Ack =
            serde_json::from_str(r#"{"id":"qubeherd:subscribe","error":null}"#).expect("parses");
        assert!(error_of(reply.error).is_none());
    }

    #[test]
    fn keeps_a_real_error_object() {
        let reply: Ack =
            serde_json::from_str(r#"{"id":"x","error":{"code":-32601}}"#).expect("parses");
        assert!(error_of(reply.error).is_some());
    }

    #[test]
    fn refuses_an_agent_list_without_an_agents_field() {
        // Silently counting this as zero agents would blank the dongle while
        // agents are working, and keep heartbeating so the firmware's own
        // expiry never notices.
        let response: Result<Response, _> =
            serde_json::from_str(r#"{"id":"x","result":{"type":"agent_list"}}"#);
        assert!(response.is_err());
    }
}

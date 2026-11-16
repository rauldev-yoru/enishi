//! Local IPC between the CLI and its daemon (unix socket / named pipe).
//! One request frame, one response frame per connection.

use serde::{Deserialize, Serialize};

use crate::session::ExecRequest;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum IpcRequest {
    /// Presence of all peers, or only `device` when given.
    Status { device: Option<String> },
    /// Run a command on `device` (default peer when `None`). On
    /// [`IpcResponse::ExecStarted`] the connection switches to
    /// [`crate::session::ExecFrame`]s in both directions.
    Exec {
        device: Option<String>,
        request: ExecRequest,
    },
    /// Create a one-time invite. `name` is the name this device will give the joiner.
    PairInvite { name: Option<String>, ttl_secs: u64 },
    /// Pair with the device that printed `code`; `name` is the local name for it.
    PairJoin { code: String, name: Option<String> },
    /// Re-read `enishi.toml` and apply peers/grants without a restart.
    Reload,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum IpcResponse {
    Status(StatusReport),
    /// The peer acknowledged the exec session; stdio frames follow.
    ExecStarted,
    /// `code` is one of [`crate::exit`].
    Error {
        code: i32,
        message: String,
    },
    Invite {
        code: String,
        ttl_secs: u64,
        /// Address hints embedded in the code.
        addrs: Vec<String>,
    },
    Paired(Paired),
    Reloaded(ReloadReport),
}

/// A pairing that completed on this device.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Paired {
    /// Local name of the new peer (`[peers.<name>]`).
    pub name: String,
    /// The peer's own `[device] name`.
    pub device: String,
    pub id: String,
    /// `default_peer` was unset and now points to this peer.
    pub default_peer: bool,
    /// Config file that was updated.
    pub config: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ReloadReport {
    pub added: Vec<String>,
    pub removed: Vec<String>,
    /// Peers whose id or addresses changed (reconnected).
    pub changed: Vec<String>,
    /// Settings that only take effect after a restart.
    pub needs_restart: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StatusReport {
    pub device: String,
    pub id: String,
    pub version: String,
    pub peers: Vec<PeerStatus>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum Presence {
    Online,
    Offline,
}

/// Durations are relative to the moment the daemon built the report.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PeerStatus {
    pub name: String,
    pub id: String,
    pub presence: Presence,
    /// Time in the current presence state.
    pub since_ms: u64,
    /// Time since the last message received from the peer; `None` if never.
    pub last_seen_ms: Option<u64>,
    /// Last measured heartbeat round trip.
    pub rtt_us: Option<u64>,
    /// Why the last connection attempt or connection failed.
    pub last_error: Option<String>,
}

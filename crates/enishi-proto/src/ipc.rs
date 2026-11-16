//! Local IPC between the CLI and its daemon (unix socket / named pipe).
//! One request frame, one response frame per connection.

use serde::{Deserialize, Serialize};

use crate::session::{ExecRequest, ServiceDetails, ServiceInfo, SessionKind};

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
    /// Open the service `service` published by `device` (default peer when `None`). Same
    /// streaming as [`IpcRequest::Exec`]: [`IpcResponse::ExecStarted`], then frames.
    Run {
        device: Option<String>,
        service: String,
        args: Vec<String>,
    },
    /// Services `device` publishes to this device.
    Services { device: Option<String> },
    /// Create a one-time invite. `name` is the name this device will give the joiner.
    PairInvite { name: Option<String>, ttl_secs: u64 },
    /// Pair with the device that printed `code`; `name` is the local name for it.
    PairJoin { code: String, name: Option<String> },
    /// Re-read `enishi.toml` and apply peers/grants without a restart.
    Reload,
    /// Open any session (`header`) on `device` (default peer when `None`); streams like
    /// [`IpcRequest::Exec`]. Newer session kinds use this instead of a request each.
    Open {
        device: Option<String>,
        header: SessionKind,
    },
    /// `enishi forward`: listen on `bind:port` (default 127.0.0.1 and the service's target
    /// port) and tunnel each connection to the tcp service; lasts while this IPC
    /// connection stays open. Replies `Forwarding`, then `Note`s.
    Forward {
        device: Option<String>,
        service: String,
        bind: Option<String>,
        port: Option<u16>,
    },
    /// Details of the service `service` published by `device` (added in 0.1.0: an older
    /// daemon closes the connection instead of answering).
    ServiceDetails {
        device: Option<String>,
        service: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum IpcResponse {
    Status(StatusReport),
    /// The peer acknowledged the exec or service session; stdio frames follow.
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
    Services {
        /// Local name of the peer that was asked.
        device: String,
        services: Vec<ServiceInfo>,
    },
    /// The forward is listening on `local`.
    Forwarding {
        device: String,
        service: String,
        local: String,
    },
    /// A log line for the CLI (e.g. a forwarded connection refused while the peer is down).
    Note(String),
    /// Answer to [`IpcRequest::ServiceDetails`]; `None` when the device does not publish
    /// that service to us.
    ServiceDetails(Option<ServiceDetails>),
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

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

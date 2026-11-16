//! `enishi/ctl/1` messages. One request frame and one response frame per QUIC bi-stream.

use serde::{Deserialize, Serialize};

/// Identifies a daemon to its peer. Sent first on every new ctl connection, and again after a
/// resume from sleep, so the receiver can reconnect immediately instead of waiting a backoff.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Hello {
    /// The sender's own device name (`[device] name`); informational, peers use their alias.
    pub device: String,
    /// enishi version of the sender.
    pub version: String,
    /// Random per daemon start: a change means the peer restarted.
    pub instance: u64,
    /// The sender just detected a resume from sleep (wall-clock jump).
    pub resume: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum CtlRequest {
    Hello(Hello),
    Ping { seq: u64 },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum CtlResponse {
    Hello(Hello),
    Pong { seq: u64 },
    Error { message: String },
}

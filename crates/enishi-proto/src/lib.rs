//! Wire types shared by every enishi layer. No I/O here.

use serde::{Deserialize, Serialize};

pub mod ctl;
pub mod frame;
pub mod ipc;

pub const ALPN_CTL: &[u8] = b"enishi/ctl/1";
pub const ALPN_STREAM: &[u8] = b"enishi/stream/1";

/// Reserved process exit codes for enishi's own failures (remote exit codes pass through).
pub mod exit {
    pub const PEER_OFFLINE: i32 = 255;
    pub const NOT_ALLOWED: i32 = 254;
    pub const NO_SUCH_SERVICE: i32 = 253;
    pub const DAEMON_DOWN: i32 = 252;
}

/// First frame of every session stream.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum SessionKind {
    Exec {
        command: Vec<String>,
        shell: Option<String>,
        raw: bool,
        cwd: Option<String>,
        env: Vec<(String, String)>,
    },
    Service {
        name: String,
        args: Vec<String>,
    },
}

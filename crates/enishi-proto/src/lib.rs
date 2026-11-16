//! Wire types shared by every enishi layer. No I/O here.

pub mod ctl;
pub mod frame;
pub mod ipc;
pub mod session;

pub use session::SessionKind;

pub const ALPN_CTL: &[u8] = b"enishi/ctl/1";
pub const ALPN_STREAM: &[u8] = b"enishi/stream/1";

/// Reserved process exit codes for enishi's own failures (remote exit codes pass through).
pub mod exit {
    pub const PEER_OFFLINE: i32 = 255;
    pub const NOT_ALLOWED: i32 = 254;
    pub const NO_SUCH_SERVICE: i32 = 253;
    pub const DAEMON_DOWN: i32 = 252;
    /// Bad invocation (unknown device, missing config…), same as clap's usage errors.
    pub const USAGE: i32 = 2;
    /// The command could not be found (raw spawn), like shells do.
    pub const COMMAND_NOT_FOUND: i32 = 127;
    /// The command was found but could not be started (raw spawn), like shells do.
    pub const CANNOT_EXECUTE: i32 = 126;
    /// `--timeout` expired and the process tree was killed, like `timeout(1)`.
    pub const TIMED_OUT: i32 = 124;
}

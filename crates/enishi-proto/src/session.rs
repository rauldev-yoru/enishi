//! `enishi/stream/1` sessions: one QUIC bi-stream per session.
//!
//! The opener sends a [`SessionKind`] header frame; the peer answers a [`SessionAck`]
//! within a bounded timeout (so a half-dead path errors instead of hanging); then
//! [`ExecFrame`]s flow both ways until an `Exit` or `Error` frame ends the session. The local
//! CLI ↔ daemon IPC uses the same [`ExecFrame`]s after its own handshake.

use serde::{Deserialize, Serialize};

/// What the peer should run: `enishi exec` options (design §3.3).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ExecRequest {
    /// Shell mode: words joined with spaces into one command string. Raw mode: argv.
    pub command: Vec<String>,
    /// `pwsh`, `powershell`, `cmd`, `bash` or `sh`; `None` = the peer's platform default.
    pub shell: Option<String>,
    /// Spawn `command` directly, without a shell.
    pub raw: bool,
    pub cwd: Option<String>,
    pub env: Vec<(String, String)>,
    /// The peer kills the process tree after this many seconds.
    pub timeout_secs: Option<u64>,
}

/// First frame of every session stream.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum SessionKind {
    Exec(ExecRequest),
    Service { name: String, args: Vec<String> },
}

/// The peer's answer to a [`SessionKind`] header.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum SessionAck {
    Ok,
    /// `code` is one of [`crate::exit`].
    Err {
        code: i32,
        message: String,
    },
}

/// Multiplexed stdio of an exec session. `Stdin*` flow towards the process; the rest flow
/// back to the caller. Exactly one `Exit` or `Error` frame ends a session.
#[derive(Clone, Serialize, Deserialize, PartialEq)]
pub enum ExecFrame {
    Stdin(#[serde(with = "serde_bytes")] Vec<u8>),
    StdinEof,
    Stdout(#[serde(with = "serde_bytes")] Vec<u8>),
    Stderr(#[serde(with = "serde_bytes")] Vec<u8>),
    /// The process exited (signals map to 128 + signal number on unix).
    Exit {
        code: i32,
    },
    /// The session ended without an exit code of its own (timeout, link lost…);
    /// `code` is one of [`crate::exit`].
    Error {
        code: i32,
        message: String,
    },
}

impl std::fmt::Debug for ExecFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExecFrame::Stdin(b) => write!(f, "Stdin({} bytes)", b.len()),
            ExecFrame::StdinEof => write!(f, "StdinEof"),
            ExecFrame::Stdout(b) => write!(f, "Stdout({} bytes)", b.len()),
            ExecFrame::Stderr(b) => write!(f, "Stderr({} bytes)", b.len()),
            ExecFrame::Exit { code } => write!(f, "Exit({code})"),
            ExecFrame::Error { code, message } => write!(f, "Error({code}, {message:?})"),
        }
    }
}

/// Largest stdio payload per frame; well under [`crate::frame::MAX_FRAME`].
pub const MAX_CHUNK: usize = 64 * 1024;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::{HEADER_LEN, decode, encode};

    fn rt<T: Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug>(v: T) {
        let bytes = encode(&v).unwrap();
        let (back, used): (T, usize) = decode(&bytes).unwrap();
        assert_eq!(back, v);
        assert_eq!(used, bytes.len());
    }

    #[test]
    fn session_frames_roundtrip() {
        rt(SessionKind::Exec(ExecRequest {
            command: vec!["echo".into(), "hi there".into()],
            shell: Some("bash".into()),
            raw: false,
            cwd: Some("/tmp".into()),
            env: vec![("A".into(), "1".into())],
            timeout_secs: Some(5),
        }));
        rt(SessionKind::Service {
            name: "fs".into(),
            args: vec![],
        });
        rt(SessionAck::Ok);
        rt(SessionAck::Err {
            code: crate::exit::NOT_ALLOWED,
            message: "no".into(),
        });
        for f in [
            ExecFrame::Stdin(b"abc".to_vec()),
            ExecFrame::StdinEof,
            ExecFrame::Stdout(vec![0, 255, 10]),
            ExecFrame::Stderr(vec![]),
            ExecFrame::Exit { code: -1 },
            ExecFrame::Exit { code: 7 },
            ExecFrame::Error {
                code: crate::exit::TIMED_OUT,
                message: "t".into(),
            },
        ] {
            rt(f);
        }
    }

    #[test]
    fn byte_payloads_are_compact() {
        // serde_bytes: a full chunk costs a few bytes of overhead, not one per element.
        let bytes = encode(&ExecFrame::Stdout(vec![0xAB; MAX_CHUNK])).unwrap();
        assert!(bytes.len() <= HEADER_LEN + MAX_CHUNK + 8, "{}", bytes.len());
    }
}

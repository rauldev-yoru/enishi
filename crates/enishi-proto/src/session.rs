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

/// How a published service speaks on its stdio (`[services.<name>] kind`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ServiceKind {
    /// An MCP server (newline-delimited JSON-RPC); consume it with `enishi mcp`.
    Mcp,
    /// Any other stdio program; consume it with `enishi run` (the default).
    #[default]
    Stdio,
    /// A TCP port on (or reachable from) the device (`target = "host:port"`); consume it
    /// with `enishi forward`.
    Tcp,
}

impl ServiceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ServiceKind::Mcp => "mcp",
            ServiceKind::Stdio => "stdio",
            ServiceKind::Tcp => "tcp",
        }
    }
}

/// A service as listed to a peer (`enishi services`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ServiceInfo {
    pub name: String,
    pub kind: ServiceKind,
    /// Extra args after `--` are forwarded to the command.
    pub allow_args: bool,
    /// Tcp services: the target's port (`enishi forward`'s default local port).
    pub port: Option<u16>,
}

/// One service with the settings a consumer may need beyond [`ServiceInfo`]
/// (`CtlRequest::ServiceDetails`). A separate message so that [`ServiceInfo`] — and the
/// `Services` exchange with older peers — keeps its wire shape.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ServiceDetails {
    pub info: ServiceInfo,
    /// `[services.<name>] startup_timeout`: how long the service may take to answer its
    /// first request after a start (e.g. an MCP server's `initialize`), in ms.
    pub startup_timeout_ms: Option<u64>,
}

/// First frame of every session stream.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum SessionKind {
    Exec(ExecRequest),
    /// Open the published service `name`; `args` are appended to its command only if the
    /// service has `allow_args`.
    Service {
        name: String,
        args: Vec<String>,
    },
    /// Run `command` (argv, spawned directly: `shell`/`timeout_secs` are ignored) as an
    /// MCP server for this session — `enishi mcp -d <dev> -- <command…>` ("transparent
    /// mode"). The peer authorizes it with its `[grants.<peer>] mcp` policy; it never parses
    /// the MCP traffic, which is plain stdio like any service.
    Mcp(ExecRequest),
    /// Tunnel one TCP connection to the published `kind = "tcp"` service: after the ack
    /// the stream carries raw bytes both ways; finishing a direction is a TCP half-close.
    Tcp {
        service: String,
    },
    /// A resumable session (added in 0.1.2, design D18): `Service` or `Mcp` whose process
    /// outlives a link loss for the service's `detach_ttl`. The peer answers
    /// [`SessionAck::Resumable`] (or plain `Ok` when it does not keep this one).
    /// Exec sessions are never resumable. Only sent to peers ≥ 0.1.2.
    Resumable(Box<SessionKind>),
    /// Reattach the resumable session `id` after a link loss: the peer answers
    /// [`SessionAck::Ok`] and replays its output from frame `seq` (the number of output
    /// frames the caller delivered), or `Err` when the session is gone. With `cancel` the
    /// peer kills the session instead (the caller went away while detached).
    Resume {
        id: u64,
        token: u128,
        seq: u64,
        cancel: bool,
    },
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
    /// The session started and survives link losses for `ttl_ms`: reattach with
    /// [`SessionKind::Resume`] and this `id`/`token`.
    Resumable {
        id: u64,
        token: u128,
        ttl_ms: u64,
    },
    /// Answer to a successful [`SessionKind::Resume`]: the session is reattached on this
    /// stream; `input` is how many input frames (`Stdin` + `StdinEof`) the peer received
    /// so far, so the caller re-sends exactly the ones after it (never one twice).
    Reattached {
        input: u64,
    },
}

/// Multiplexed stdio of an exec or service session. `Stdin*` flow towards the process; the rest flow
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
    /// Resumable sessions only. Caller → peer: output frames up to `seq` (count since the
    /// start) were delivered and may be dropped from the replay buffer. Peer → caller: input
    /// frames up to `seq` were received, the caller may drop its copies.
    Ack {
        seq: u64,
    },
    /// Daemon → local client of a resumable session: the link was lost, the remote
    /// process is kept alive; stdin is paused until [`ExecFrame::Resumed`] (or the
    /// session ends with `Error`).
    Detached {
        message: String,
    },
    /// Daemon → local client: the session was reattached, the same process continues.
    Resumed,
}

impl ExecFrame {
    /// Frames that count in a resumable session's output sequence (peer → caller).
    pub fn is_output(&self) -> bool {
        matches!(
            self,
            ExecFrame::Stdout(_)
                | ExecFrame::Stderr(_)
                | ExecFrame::Exit { .. }
                | ExecFrame::Error { .. }
        )
    }
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
            ExecFrame::Ack { seq } => write!(f, "Ack({seq})"),
            ExecFrame::Detached { message } => write!(f, "Detached({message:?})"),
            ExecFrame::Resumed => write!(f, "Resumed"),
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
        rt(SessionKind::Tcp {
            service: "web".into(),
        });
        rt(SessionKind::Mcp(ExecRequest {
            command: vec!["npx".into(), "-y".into(), "pkg".into()],
            raw: true,
            ..Default::default()
        }));
        rt(ServiceInfo {
            name: "fs".into(),
            kind: ServiceKind::Mcp,
            allow_args: true,
            port: None,
        });
        rt(crate::ctl::CtlResponse::Services(vec![ServiceInfo {
            name: "wc".into(),
            kind: ServiceKind::Stdio,
            allow_args: false,
            port: Some(3000),
        }]));
        rt(crate::ctl::CtlRequest::Services);
        rt(crate::ctl::CtlRequest::ServiceDetails { name: "fs".into() });
        rt(crate::ctl::CtlResponse::ServiceDetails(Some(
            ServiceDetails {
                info: ServiceInfo {
                    name: "fs".into(),
                    kind: ServiceKind::Mcp,
                    allow_args: false,
                    port: None,
                },
                startup_timeout_ms: Some(90_000),
            },
        )));
        rt(crate::ctl::CtlResponse::ServiceDetails(None));
        // Appending variants keeps the existing ones' encoding (older peers still decode
        // `Services`): the discriminant of `Services` is unchanged.
        assert_eq!(
            encode(&crate::ctl::CtlRequest::Services).unwrap()[HEADER_LEN..],
            [2u8]
        );
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
            ExecFrame::Ack { seq: u64::MAX },
            ExecFrame::Detached {
                message: "link lost".into(),
            },
            ExecFrame::Resumed,
        ] {
            rt(f);
        }
    }

    #[test]
    fn resumable_variants_roundtrip_and_keep_old_encodings() {
        let svc = SessionKind::Service {
            name: "pw".into(),
            args: vec![],
        };
        rt(SessionKind::Resumable(Box::new(svc.clone())));
        rt(SessionKind::Resume {
            id: 7,
            token: u128::MAX - 3,
            seq: 42,
            cancel: false,
        });
        rt(SessionAck::Resumable {
            id: 1,
            token: 99,
            ttl_ms: 600_000,
        });
        rt(SessionAck::Reattached { input: 3 });
        // Appended variants: what 0.1.1 peers send and decode is unchanged.
        assert_eq!(encode(&SessionAck::Ok).unwrap()[HEADER_LEN..], [0u8]);
        assert_eq!(encode(&svc).unwrap()[HEADER_LEN], 1);
        assert_eq!(encode(&ExecFrame::StdinEof).unwrap()[HEADER_LEN..], [1u8]);
        assert!(ExecFrame::Exit { code: 0 }.is_output());
        assert!(!ExecFrame::Ack { seq: 1 }.is_output());
        assert!(!ExecFrame::Resumed.is_output());
    }

    #[test]
    fn byte_payloads_are_compact() {
        // serde_bytes: a full chunk costs a few bytes of overhead, not one per element.
        let bytes = encode(&ExecFrame::Stdout(vec![0xAB; MAX_CHUNK])).unwrap();
        assert!(bytes.len() <= HEADER_LEN + MAX_CHUNK + 8, "{}", bytes.len());
    }
}

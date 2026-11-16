//! The real [`Connector`]: each open is an `enishi run`-style session through the local
//! daemon (`IpcRequest::Run` for a published service, `IpcRequest::Open` with
//! `SessionKind::Mcp` for a transparent-mode command), with stdout split into JSON-RPC
//! lines and the service's stderr copied to ours (agents log the shim's stderr).

use enishi_core::{ipc, paths::Paths, wire};
use enishi_proto::{
    exit,
    ipc::IpcRequest,
    session::{ExecFrame, ExecRequest, MAX_CHUNK, SessionKind},
};
use serde_json::Value;
use tokio::{io::AsyncWriteExt, sync::mpsc};

use crate::shim::{Connector, OpenError, Remote};

/// Lines longer than this (no newline yet) are dropped rather than buffered forever.
const MAX_LINE: usize = 64 << 20;

pub struct DaemonConnector {
    pub paths: Paths,
    pub device: String,
    pub target: Target,
}

/// What to open on the device.
#[derive(Debug, Clone, PartialEq)]
pub enum Target {
    /// A published `kind = "mcp"` service; `args` only if it has `allow_args`.
    Service { name: String, args: Vec<String> },
    /// Transparent mode: any MCP server command (needs the peer's `mcp` grant).
    Command(ExecRequest),
}

impl Target {
    /// `fs` or `npx -y pkg…` for log lines.
    pub fn label(&self) -> String {
        match self {
            Target::Service { name, .. } => name.clone(),
            Target::Command(r) => r.command.join(" "),
        }
    }
}

impl Connector for DaemonConnector {
    async fn open(&self) -> Result<Remote, OpenError> {
        let device = Some(self.device.clone());
        let req = match &self.target {
            Target::Service { name, args } => IpcRequest::Run {
                device,
                service: name.clone(),
                args: args.clone(),
            },
            Target::Command(r) => IpcRequest::Open {
                device,
                header: SessionKind::Mcp(r.clone()),
            },
        };
        let stream = ipc::open_session(&self.paths, &req)
            .await
            .map_err(|(code, message)| OpenError {
                code,
                message: if code == exit::DAEMON_DOWN {
                    format!("{message}; start it with `enishi up`")
                } else {
                    message
                },
            })?;
        let (mut r, mut w) = tokio::io::split(stream);
        let (to_remote, mut to_remote_rx) = mpsc::channel::<Value>(64);
        let (from_remote_tx, from_remote) = mpsc::channel::<Value>(64);
        let (done_tx, done_rx) = tokio::sync::oneshot::channel::<()>();
        let label = format!("{}/{}", self.device, self.target.label());

        // Shim → service stdin. After the shim drops its sender: stdin EOF, but the IPC
        // write half stays open until the session ends (closing it = caller gone = kill).
        tokio::spawn(async move {
            while let Some(v) = to_remote_rx.recv().await {
                let mut line = serde_json::to_vec(&v).unwrap_or_default();
                line.push(b'\n');
                for chunk in line.chunks(MAX_CHUNK) {
                    if wire::write_frame(&mut w, &ExecFrame::Stdin(chunk.to_vec()))
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
            }
            let _ = wire::write_frame(&mut w, &ExecFrame::StdinEof).await;
            let _ = done_rx.await;
        });

        // Service stdout → JSON-RPC messages; stderr → our stderr.
        tokio::spawn(async move {
            let mut buf: Vec<u8> = Vec::new();
            let mut stderr = tokio::io::stderr();
            loop {
                match wire::read_frame::<_, ExecFrame>(&mut r).await {
                    Ok(ExecFrame::Stdout(b)) => {
                        buf.extend_from_slice(&b);
                        while let Some(nl) = buf.iter().position(|&c| c == b'\n') {
                            let line: Vec<u8> = buf.drain(..=nl).collect();
                            let text = String::from_utf8_lossy(&line);
                            if text.trim().is_empty() {
                                continue;
                            }
                            match serde_json::from_str::<Value>(&text) {
                                Ok(v) => {
                                    if from_remote_tx.send(v).await.is_err() {
                                        return; // shim dropped this remote
                                    }
                                }
                                Err(_) => eprintln!(
                                    "enishi mcp {label}: ignoring non-JSON stdout line: {}",
                                    text.trim_end()
                                ),
                            }
                        }
                        if buf.len() > MAX_LINE {
                            eprintln!("enishi mcp {label}: dropping an over-long stdout line");
                            buf.clear();
                        }
                    }
                    Ok(ExecFrame::Stderr(b)) => {
                        let _ = stderr.write_all(&b).await;
                        let _ = stderr.flush().await;
                    }
                    Ok(ExecFrame::Exit { code }) => {
                        eprintln!("enishi mcp {label}: remote service exited with code {code}");
                        break;
                    }
                    Ok(ExecFrame::Error { code, message }) => {
                        eprintln!("enishi mcp {label}: session ended: {message} (code {code})");
                        break;
                    }
                    Ok(_) => {}
                    Err(_) => {
                        eprintln!("enishi mcp {label}: lost the local daemon");
                        break;
                    }
                }
            }
            drop(done_tx);
        });
        Ok(Remote {
            tx: to_remote,
            rx: from_remote,
        })
    }
}

//! `enishi/stream/1` sessions (design §5) — `exec` and published services — both ends:
//!
//! - caller: [`client`] relays a local CLI (IPC) to a bi-stream on the warm stream
//!   connection to the peer, with a bounded ack;
//! - executor: [`serve_streams`] accepts sessions from a peer, authorizes them with
//!   [`authorize_exec`] / [`authorize_service`] and runs them through `enishi_svc::exec`
//!   (services: their argv spawned directly, no shell).
//!
//! Both ends log per-session timings and append one audit line.

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::Result;
use enishi_proto::{
    SessionKind, exit,
    ipc::{IpcResponse, Presence},
    session::{ExecFrame, SessionAck},
};
use enishi_svc::exec::{self as svc, End};
use iroh::endpoint::{Connection, RecvStream, SendStream};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    sync::mpsc,
};
use tracing::{debug, info, warn};

use crate::{
    audit::{Direction, Entry},
    config::ServiceKind,
    daemon::{Peer, Shared},
    policy::{
        Refusal, authorize_exec, authorize_mcp, authorize_service, mcp_command, service_command,
    },
    wire,
};

/// The peer must acknowledge (or refuse) a session within this bound.
pub const ACK_TIMEOUT: Duration = Duration::from_millis(3000);
/// Stream error code: the local caller went away.
const STREAM_CLIENT_GONE: u32 = 1;

fn ms(d: Duration) -> String {
    format!("{:.0}", d.as_secs_f64() * 1000.0)
}

fn show(cmd: &[String]) -> String {
    let s = cmd.join(" ");
    if s.chars().count() > 120 {
        format!("{}…", s.chars().take(120).collect::<String>())
    } else {
        s
    }
}

/// `("exec", command)` or `("service", name)` for log lines.
fn describe(h: &SessionKind) -> (&'static str, String) {
    match h {
        SessionKind::Exec(r) => ("exec", show(&r.command)),
        SessionKind::Service { name, args } if args.is_empty() => ("service", name.clone()),
        SessionKind::Service { name, args } => ("service", format!("{name} {}", show(args))),
        SessionKind::Mcp(r) => ("mcp", show(&r.command)),
        SessionKind::Tcp { service } => ("tcp", service.clone()),
    }
}

fn entry<'a>(dir: Direction, peer: &'a str, h: &'a SessionKind) -> Entry<'a> {
    match h {
        SessionKind::Exec(r) => Entry::exec(dir, peer, r),
        SessionKind::Service { name, args } => Entry::service(dir, peer, name, args),
        SessionKind::Mcp(r) => Entry::mcp(dir, peer, r),
        SessionKind::Tcp { service } => Entry::service(dir, peer, service, &[]),
    }
}

async fn reply<S: AsyncWrite + Unpin>(ipc: &mut S, code: i32, message: String) -> Result<()> {
    wire::write_frame(ipc, &IpcResponse::Error { code, message }).await
}

/// Caller side: open the session `header` (exec or service) on `device` for the CLI
/// connected on `ipc`.
pub(crate) async fn client<S>(
    sh: Arc<Shared>,
    mut ipc: S,
    device: Option<String>,
    header: SessionKind,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let peer = match sh.target(device.as_deref()) {
        Ok(p) => p,
        Err(message) => return reply(&mut ipc, exit::USAGE, message).await,
    };
    let t0 = Instant::now();
    let (word, what) = describe(&header);
    let audit = |exit: Option<i32>, error: Option<&str>| {
        sh.audit
            .record(&entry(Direction::Out, &peer.name, &header).finish(exit, error, t0.elapsed()));
    };
    // Presence decides: never dial on the call path.
    let online = {
        let s = peer.state();
        match (&s.stream, s.presence) {
            (Some(c), Presence::Online) => Ok(c.clone()),
            _ => Err(s
                .last_seen
                .map(|t| format!("last seen {:.1}s ago", t.elapsed().as_secs_f64()))
                .unwrap_or_else(|| "never seen".into())),
        }
    };
    let conn = match online {
        Ok(c) => c,
        Err(seen) => {
            audit(None, Some("peer offline"));
            let msg = format!("peer {} is offline ({seen})", peer.name);
            return reply(&mut ipc, exit::PEER_OFFLINE, msg).await;
        }
    };

    let open = async {
        let (mut send, mut recv) = conn.open_bi().await?;
        wire::write_frame(&mut send, &header).await?;
        let ack: SessionAck = wire::read_frame(&mut recv).await?;
        anyhow::Ok((send, recv, ack))
    };
    let (mut send, mut recv) = match tokio::time::timeout(ACK_TIMEOUT, open).await {
        Ok(Ok((s, r, SessionAck::Ok))) => (s, r),
        Ok(Ok((_, _, SessionAck::Err { code, message }))) => {
            info!(peer = %peer.name, code, "{word} `{what}` refused: {message}");
            audit(None, Some(&message));
            return reply(&mut ipc, code, format!("{}: {message}", peer.name)).await;
        }
        Ok(Err(e)) => {
            audit(None, Some("session failed"));
            return reply(
                &mut ipc,
                exit::PEER_OFFLINE,
                format!("session to {} failed: {e:#}", peer.name),
            )
            .await;
        }
        Err(_) => {
            audit(None, Some("ack timeout"));
            return reply(
                &mut ipc,
                exit::PEER_OFFLINE,
                format!(
                    "{} did not acknowledge within {} ms",
                    peer.name,
                    ACK_TIMEOUT.as_millis()
                ),
            )
            .await;
        }
    };
    let t_ack = t0.elapsed();
    wire::write_frame(&mut ipc, &IpcResponse::ExecStarted).await?;
    let (mut ir, mut iw) = tokio::io::split(ipc);

    enum Down {
        Done(ExecFrame),
        LinkLost(String),
        ClientGone,
    }
    let mut first_byte = None;
    let mut bytes = 0u64;
    let outcome = {
        // CLI → peer: stdin. Ends only when the CLI goes away (or misbehaves).
        let up = async {
            let mut forward = true;
            loop {
                match wire::read_frame::<_, ExecFrame>(&mut ir).await {
                    Ok(f @ ExecFrame::Stdin(_)) => {
                        if forward && wire::write_frame(&mut send, &f).await.is_err() {
                            forward = false;
                        }
                    }
                    Ok(ExecFrame::StdinEof) => {
                        if forward {
                            let _ = wire::write_frame(&mut send, &ExecFrame::StdinEof).await;
                            let _ = send.finish();
                        }
                        forward = false;
                    }
                    Ok(other) => {
                        debug!("unexpected frame from CLI: {other:?}");
                        return;
                    }
                    Err(_) => return,
                }
            }
        };
        // peer → CLI: stdout/stderr/exit.
        let down = async {
            loop {
                let f: ExecFrame = match wire::read_frame(&mut recv).await {
                    Ok(f) => f,
                    Err(e) => return Down::LinkLost(format!("{e:#}")),
                };
                let done = match &f {
                    ExecFrame::Stdout(b) | ExecFrame::Stderr(b) => {
                        first_byte.get_or_insert_with(|| t0.elapsed());
                        bytes += b.len() as u64;
                        false
                    }
                    ExecFrame::Exit { .. } | ExecFrame::Error { .. } => true,
                    ExecFrame::Stdin(_) | ExecFrame::StdinEof => {
                        return Down::LinkLost(format!("protocol error: peer sent {f:?}"));
                    }
                };
                if wire::write_frame(&mut iw, &f).await.is_err() {
                    return Down::ClientGone;
                }
                if done {
                    return Down::Done(f);
                }
            }
        };
        tokio::select! {
            biased;
            d = down => d,
            _ = up => Down::ClientGone,
        }
    };

    let (exit_code, error) = match &outcome {
        Down::Done(ExecFrame::Exit { code }) => (Some(*code), None),
        Down::Done(ExecFrame::Error { message, .. }) => (None, Some(message.clone())),
        Down::Done(_) => (None, None),
        Down::LinkLost(e) => {
            let msg = format!("link to {} lost during the session: {e}", peer.name);
            let _ = wire::write_frame(
                &mut iw,
                &ExecFrame::Error {
                    code: exit::PEER_OFFLINE,
                    message: msg,
                },
            )
            .await;
            (None, Some("link lost".to_string()))
        }
        Down::ClientGone => {
            // Make the peer kill the process: abort both directions of the stream.
            let _ = send.reset(STREAM_CLIENT_GONE.into());
            let _ = recv.stop(STREAM_CLIENT_GONE.into());
            (None, Some("caller gone".to_string()))
        }
    };
    info!(
        peer = %peer.name,
        "{word} out `{what}`: ack={}ms first_byte={}ms end={}ms bytes={bytes} exit={}",
        ms(t_ack),
        first_byte.map(ms).unwrap_or_else(|| "-".into()),
        ms(t0.elapsed()),
        exit_code.map(exit::describe).unwrap_or_else(|| error.clone().unwrap_or_default()),
    );
    audit(exit_code, error.as_deref());
    Ok(())
}

/// Executor side: serve every session the peer opens on its stream connection.
pub(crate) async fn serve_streams(conn: Connection, sh: Arc<Shared>, peer: Arc<Peer>) {
    while let Ok((send, recv)) = conn.accept_bi().await {
        tokio::spawn(serve_session(sh.clone(), peer.clone(), send, recv));
    }
}

async fn serve_session(sh: Arc<Shared>, peer: Arc<Peer>, mut send: SendStream, recv: RecvStream) {
    let mut recv = recv;
    let header = match tokio::time::timeout(
        ACK_TIMEOUT,
        wire::read_frame::<_, SessionKind>(&mut recv),
    )
    .await
    {
        Ok(Ok(h)) => h,
        Ok(Err(e)) => return debug!(peer = %peer.name, "session header: {e:#}"),
        Err(_) => return debug!(peer = %peer.name, "session header timed out"),
    };
    let t0 = Instant::now();
    let (word, what) = describe(&header);
    // Services: authorize, then the service's argv (no shell) is the process to spawn.
    let (req, service, mcp) = match header {
        SessionKind::Tcp { service } => {
            return crate::forward::serve(sh, peer, send, recv, service).await;
        }
        SessionKind::Exec(r) => (r, None, false),
        SessionKind::Mcp(r) => (mcp_command(r), None, true),
        SessionKind::Service { name, args } => {
            let planned = {
                let cfg = sh.cfg();
                authorize_service(&cfg, &peer.name, &name, &args).and_then(|s| {
                    if s.kind == ServiceKind::Tcp {
                        return Err(Refusal {
                            code: exit::USAGE,
                            message: format!(
                                "service {name:?} is a tcp service (use `enishi forward {name}`)"
                            ),
                        });
                    }
                    Ok(service_command(s, &args))
                })
            };
            match planned {
                Ok(req) => (req, Some(name), false),
                Err(r) => {
                    warn!(peer = %peer.name, code = r.code, "service `{what}` refused: {}", r.message);
                    sh.audit.record(
                        &Entry::service(Direction::In, &peer.name, &name, &args).finish(
                            None,
                            Some(&r.message),
                            t0.elapsed(),
                        ),
                    );
                    let _ = refuse(&mut send, r.code, r.message).await;
                    return;
                }
            }
        }
    };
    let audit = |exit: Option<i32>, error: Option<&str>| {
        let e = match &service {
            Some(name) => Entry::service(Direction::In, &peer.name, name, &req.command),
            None if mcp => Entry::mcp(Direction::In, &peer.name, &req),
            None => Entry::exec(Direction::In, &peer.name, &req),
        };
        sh.audit.record(&e.finish(exit, error, t0.elapsed()));
    };
    if service.is_none() {
        let grant = sh.grant(&peer.name);
        let verdict = if mcp {
            authorize_mcp(&grant.mcp, &req.command)
        } else {
            authorize_exec(&grant.exec, &req.command)
        };
        if let Err(why) = verdict {
            warn!(peer = %peer.name, "{word} denied `{what}`: {why}");
            audit(None, Some(why));
            let _ = refuse(&mut send, exit::NOT_ALLOWED, why.into()).await;
            return;
        }
    }
    let running = match svc::spawn(&req) {
        Ok(r) => r,
        Err(e) => {
            info!(peer = %peer.name, "{word} `{what}` failed to start: {e}");
            audit(None, Some(&e.message));
            let _ = refuse(&mut send, e.code, e.message).await;
            return;
        }
    };
    if wire::write_frame(&mut send, &SessionAck::Ok).await.is_err() {
        audit(None, Some("caller gone before ack"));
        return; // `running` is dropped: kill_on_drop.
    }
    debug!(peer = %peer.name, pid = ?running.id(), program = %running.plan.program, "{word} started");

    // caller → process stdin; closing `in_tx` is EOF.
    let (in_tx, in_rx) = mpsc::channel::<Vec<u8>>(8);
    let stdin_task = tokio::spawn(async move {
        while let Ok(ExecFrame::Stdin(b)) = wire::read_frame::<_, ExecFrame>(&mut recv).await {
            if in_tx.send(b).await.is_err() {
                break;
            }
        }
        recv
    });
    // process → caller; dropping `out_rx` (caller stopped the stream / link lost) makes
    // the session kill the process tree.
    let (out_tx, mut out_rx) = mpsc::channel::<ExecFrame>(8);
    let writer = tokio::spawn(async move {
        let stopped = send.stopped();
        tokio::pin!(stopped);
        loop {
            tokio::select! {
                biased;
                f = out_rx.recv() => match f {
                    Some(f) => if wire::write_frame(&mut send, &f).await.is_err() { return None },
                    None => return Some(send),
                },
                _ = &mut stopped => return None,
            }
        }
    });

    let timeout = req.timeout_secs.map(Duration::from_secs);
    let out = running.run(in_rx, out_tx, timeout).await;
    stdin_task.abort();
    let last = match out.end {
        End::Exited(code) => Some(ExecFrame::Exit { code }),
        End::TimedOut => Some(ExecFrame::Error {
            code: exit::TIMED_OUT,
            message: format!(
                "timed out after {}s, process killed",
                req.timeout_secs.unwrap_or_default()
            ),
        }),
        End::Disconnected => None,
    };
    if let (Ok(Some(mut send)), Some(last)) = (writer.await, last) {
        let _ = wire::write_frame(&mut send, &last).await;
        let _ = send.finish();
    }
    let (exit_code, error) = match out.end {
        End::Exited(c) => (Some(c), None),
        End::TimedOut => (None, Some("timeout")),
        End::Disconnected => (None, Some("caller gone, killed")),
    };
    info!(
        peer = %peer.name,
        "{word} in `{what}`: spawn={}ms first_out={}ms eof={}ms reaped={}ms exit={}ms out={}B err={}B result={}",
        ms(out.spawn),
        out.first_out.map(ms).unwrap_or_else(|| "-".into()),
        out.eof.map(ms).unwrap_or_else(|| "-".into()),
        out.exited.map(ms).unwrap_or_else(|| "-".into()),
        ms(out.total),
        out.out_bytes,
        out.err_bytes,
        exit_code.map(exit::describe).unwrap_or_else(|| error.unwrap_or_default().into()),
    );
    audit(exit_code, error);
}

pub(crate) async fn refuse(send: &mut SendStream, code: i32, message: String) -> Result<()> {
    wire::write_frame(send, &SessionAck::Err { code, message }).await?;
    send.finish()?;
    Ok(())
}

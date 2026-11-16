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
    collections::VecDeque,
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
    detach,
    policy::{
        Refusal, authorize_exec, authorize_mcp, authorize_service, mcp_command, service_command,
    },
    wire,
};

/// The peer must acknowledge (or refuse) a session within this bound.
pub const ACK_TIMEOUT: Duration = Duration::from_millis(3000);
/// Stream error code: the local caller went away.
pub(crate) const STREAM_CLIENT_GONE: u32 = 1;
/// Resumable sessions: the caller acknowledges delivered output at least every this many
/// frames, and at least this often while output flows.
const ACK_EVERY: u64 = 16;
const ACK_INTERVAL: Duration = Duration::from_millis(250);
/// While detached, how often the caller checks whether the peer is back.
const RESUME_POLL: Duration = Duration::from_millis(250);
/// Stdin frames queued while detached before the local client is back-pressured.
const MAX_QUEUED: usize = 64;

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
        SessionKind::Resumable(inner) => describe(inner),
        SessionKind::Resume { id, .. } => ("resume", format!("{id:016x}")),
    }
}

fn entry<'a>(dir: Direction, peer: &'a str, h: &'a SessionKind) -> Entry<'a> {
    match h {
        SessionKind::Exec(r) => Entry::exec(dir, peer, r),
        SessionKind::Service { name, args } => Entry::service(dir, peer, name, args),
        SessionKind::Mcp(r) => Entry::mcp(dir, peer, r),
        SessionKind::Tcp { service } => Entry::service(dir, peer, service, &[]),
        SessionKind::Resumable(inner) => entry(dir, peer, inner),
        SessionKind::Resume { .. } => Entry::service(dir, peer, "", &[]),
    }
}

async fn reply<S: AsyncWrite + Unpin>(ipc: &mut S, code: i32, message: String) -> Result<()> {
    wire::write_frame(ipc, &IpcResponse::Error { code, message }).await
}

/// Whether a peer that said `version` in its hello serves resumable sessions (≥ 0.1.2).
pub(crate) fn supports_resume(version: Option<&str>) -> bool {
    let Some(v) = version else { return false };
    let core = v
        .trim_start_matches('v')
        .split(['-', '+'])
        .next()
        .unwrap_or("");
    let mut n = core.split('.').map(|p| p.parse::<u64>().unwrap_or(0));
    let t = (
        n.next().unwrap_or(0),
        n.next().unwrap_or(0),
        n.next().unwrap_or(0),
    );
    t >= (0, 1, 2)
}

/// Bound of the input a resumable session keeps for re-sending (the client is
/// back-pressured beyond it until the peer acknowledges).
const MAX_UNACKED_INPUT: usize = 8 << 20;

fn input_cost(f: &ExecFrame) -> usize {
    16 + match f {
        ExecFrame::Stdin(b) => b.len(),
        _ => 0,
    }
}

/// A resumable session as the caller holds it.
#[derive(Debug, Clone, Copy)]
struct ClientTicket {
    id: u64,
    token: u128,
    ttl: Duration,
}

/// Open a `Resume` stream for `t` on `conn`: the reattached stream with the number of
/// input frames the peer received, or the peer's refusal.
async fn try_resume(
    conn: &Connection,
    t: &ClientTicket,
    seq: u64,
    cancel: bool,
) -> Result<std::result::Result<(SendStream, RecvStream, u64), String>> {
    let attempt = async {
        let (mut send, mut recv) = conn.open_bi().await?;
        let header = SessionKind::Resume {
            id: t.id,
            token: t.token,
            seq,
            cancel,
        };
        wire::write_frame(&mut send, &header).await?;
        let ack: SessionAck = wire::read_frame(&mut recv).await?;
        anyhow::Ok(match ack {
            SessionAck::Reattached { input } => Ok((send, recv, input)),
            SessionAck::Ok if cancel => Ok((send, recv, 0)),
            SessionAck::Err { message, .. } => Err(message),
            other => Err(format!("unexpected answer {other:?}")),
        })
    };
    match tokio::time::timeout(ACK_TIMEOUT, attempt).await {
        Ok(r) => r,
        Err(_) => anyhow::bail!("no answer within {} ms", ACK_TIMEOUT.as_millis()),
    }
}

/// The peer's warm stream connection, while online and not closed.
fn live_conn(peer: &Peer) -> Option<Connection> {
    let s = peer.state();
    match (&s.stream, s.presence) {
        (Some(c), Presence::Online) if c.close_reason().is_none() => Some(c.clone()),
        _ => None,
    }
}

/// The local client left while its session was detached: tell the peer to kill it once
/// the link is back (else the process lives out its TTL).
fn cancel_later(peer: Arc<Peer>, t: ClientTicket, seq: u64) {
    tokio::spawn(async move {
        let deadline = tokio::time::Instant::now() + t.ttl;
        while tokio::time::Instant::now() < deadline {
            if let Some(conn) = live_conn(&peer)
                && try_resume(&conn, &t, seq, true).await.is_ok()
            {
                debug!(peer = %peer.name, session = t.id, "detached session cancelled");
                return;
            }
            tokio::time::sleep(RESUME_POLL).await;
        }
    });
}

enum Parked {
    /// Reattached; the peer received this many input frames.
    Resumed(SendStream, RecvStream, u64),
    ClientGone,
    /// The session cannot be resumed (expired, peer restarted, output lost…).
    Lost(String),
}

/// Peer → caller frames of one attachment of a session (`epoch` tells attachments apart).
fn spawn_down(
    epoch: u64,
    mut recv: RecvStream,
    tx: mpsc::Sender<(u64, std::result::Result<ExecFrame, String>)>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let r = wire::read_frame::<_, ExecFrame>(&mut recv)
                .await
                .map_err(|e| format!("{e:#}"));
            let end = r.is_err();
            if tx.send((epoch, r)).await.is_err() || end {
                return;
            }
        }
    })
}

/// Caller side: open the session `header` (exec or service) on `device` for the CLI
/// connected on `ipc`. `SessionKind::Resumable(inner)` asks for a resumable session
/// (design D18): when the peer grants it, a lost link parks the session — the client gets
/// `ExecFrame::Detached`, then `Resumed` once the session is reattached, or `Error` when
/// it cannot be.
pub(crate) async fn client<S>(
    sh: Arc<Shared>,
    mut ipc: S,
    device: Option<String>,
    header: SessionKind,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (header, want_resume) = match header {
        SessionKind::Resumable(inner) => (*inner, true),
        h => (h, false),
    };
    if matches!(
        header,
        SessionKind::Resumable(_) | SessionKind::Resume { .. }
    ) {
        return reply(&mut ipc, exit::USAGE, "invalid session header".into()).await;
    }
    let peer = match sh.target(device.as_deref()) {
        Ok(p) => p,
        Err(message) => return reply(&mut ipc, exit::USAGE, message).await,
    };
    let t0 = Instant::now();
    let (word, what) = describe(&header);
    let audit = |sid: Option<u64>, event: Option<&str>, exit: Option<i32>, error: Option<&str>| {
        let e = entry(Direction::Out, &peer.name, &header).session(sid);
        let e = match event {
            // Event lines carry their reason as `detail`, not as an error.
            Some(ev) => e.event(ev, error).finish(exit, None, t0.elapsed()),
            None => e.finish(exit, error, t0.elapsed()),
        };
        sh.audit.record(&e);
    };
    // Presence decides: never dial on the call path.
    let online = {
        let s = peer.state();
        match (&s.stream, s.presence) {
            (Some(c), Presence::Online) => Ok((c.clone(), s.remote_version.clone())),
            _ => Err(s
                .last_seen
                .map(|t| format!("last seen {:.1}s ago", t.elapsed().as_secs_f64()))
                .unwrap_or_else(|| "never seen".into())),
        }
    };
    let (conn, remote_version) = match online {
        Ok(c) => c,
        Err(seen) => {
            audit(None, None, None, Some("peer offline"));
            let msg = format!("peer {} is offline ({seen})", peer.name);
            return reply(&mut ipc, exit::PEER_OFFLINE, msg).await;
        }
    };
    let ask_resume = want_resume
        && matches!(header, SessionKind::Service { .. } | SessionKind::Mcp(_))
        && supports_resume(remote_version.as_deref());
    let wire_header = if ask_resume {
        SessionKind::Resumable(Box::new(header.clone()))
    } else {
        header.clone()
    };

    let open = async {
        let (mut send, mut recv) = conn.open_bi().await?;
        wire::write_frame(&mut send, &wire_header).await?;
        let ack: SessionAck = wire::read_frame(&mut recv).await?;
        anyhow::Ok((send, recv, ack))
    };
    let (send, recv, ticket) = match tokio::time::timeout(ACK_TIMEOUT, open).await {
        Ok(Ok((s, r, SessionAck::Ok))) => (s, r, None),
        Ok(Ok((s, r, SessionAck::Resumable { id, token, ttl_ms }))) => {
            let t = ClientTicket {
                id,
                token,
                ttl: Duration::from_millis(ttl_ms),
            };
            (s, r, Some(t))
        }
        Ok(Ok((_, _, SessionAck::Reattached { .. }))) => {
            audit(None, None, None, Some("unexpected answer"));
            return reply(
                &mut ipc,
                exit::PEER_OFFLINE,
                format!("{}: unexpected answer to the session", peer.name),
            )
            .await;
        }
        Ok(Ok((_, _, SessionAck::Err { code, message }))) => {
            info!(peer = %peer.name, code, "{word} `{what}` refused: {message}");
            audit(None, None, None, Some(&message));
            return reply(&mut ipc, code, format!("{}: {message}", peer.name)).await;
        }
        Ok(Err(e)) => {
            audit(None, None, None, Some("session failed"));
            return reply(
                &mut ipc,
                exit::PEER_OFFLINE,
                format!("session to {} failed: {e:#}", peer.name),
            )
            .await;
        }
        Err(_) => {
            audit(None, None, None, Some("ack timeout"));
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
    let sid = ticket.map(|t| t.id);
    let t_ack = t0.elapsed();
    wire::write_frame(&mut ipc, &IpcResponse::ExecStarted).await?;
    let (mut ir, mut iw) = tokio::io::split(ipc);

    // CLI → here: stdin frames. The channel closing means the CLI went away (or
    // misbehaved). A task, so that a frame is never half-read when we stop waiting.
    let (up_tx, mut up_rx) = mpsc::channel::<ExecFrame>(8);
    let up_task = tokio::spawn(async move {
        loop {
            match wire::read_frame::<_, ExecFrame>(&mut ir).await {
                Ok(f @ (ExecFrame::Stdin(_) | ExecFrame::StdinEof)) => {
                    if up_tx.send(f).await.is_err() {
                        return;
                    }
                }
                Ok(other) => return debug!("unexpected frame from CLI: {other:?}"),
                Err(_) => return,
            }
        }
    });
    let (down_tx, mut down_rx) = mpsc::channel(8);
    let mut epoch = 0u64;
    let mut send = send;
    let mut down_task = spawn_down(epoch, recv, down_tx.clone());

    enum Down {
        Done(ExecFrame),
        LinkLost(String),
        /// Resumable session that could not be resumed.
        Lost(String),
        ClientGone,
        /// The client left while the session was detached (cancelled on the peer).
        ClientGoneDetached,
    }
    let mut first_byte = None;
    let mut bytes = 0u64;
    let mut delivered = 0u64;
    let mut acked = 0u64;
    let mut stdin_done = false;
    let mut resumes = 0u32;
    // Resumable sessions: input frames sent but not yet acknowledged by the peer (it
    // received `input_base` before them), re-sent after a resume from where it stopped.
    let mut unacked: VecDeque<ExecFrame> = VecDeque::new();
    let mut unacked_bytes = 0usize;
    let mut input_base = 0u64;
    let mut ack_tick = tokio::time::interval(ACK_INTERVAL);
    ack_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let outcome = loop {
        tokio::select! {
            biased;
            Some((g, r)) = down_rx.recv() => {
                if g != epoch {
                    continue;
                }
                let f = match r {
                    Ok(f) if f.is_output() => f,
                    Ok(ExecFrame::Ack { seq }) if ticket.is_some() => {
                        while input_base < seq {
                            match unacked.pop_front() {
                                Some(f) => unacked_bytes -= input_cost(&f),
                                None => break,
                            }
                            input_base += 1;
                        }
                        continue;
                    }
                    Ok(f) => break Down::LinkLost(format!("protocol error: peer sent {f:?}")),
                    Err(e) => {
                        let Some(t) = ticket else { break Down::LinkLost(e) };
                        audit(sid, Some("detach"), None, Some(&e));
                        match park(&peer, &t, delivered, &mut up_rx, &mut iw, &e).await {
                            (Parked::Resumed(s, r, received), queued) => {
                                let sent = input_base + unacked.len() as u64;
                                if received < input_base || received > sent {
                                    break Down::Lost(format!(
                                        "peer received {received} input frames, {input_base}..{sent} expected"
                                    ));
                                }
                                for _ in input_base..received {
                                    if let Some(f) = unacked.pop_front() {
                                        unacked_bytes -= input_cost(&f);
                                    }
                                }
                                input_base = received;
                                epoch += 1;
                                send = s;
                                down_task = spawn_down(epoch, r, down_tx.clone());
                                acked = delivered;
                                resumes += 1;
                                // Input the peer missed (lost with the old stream), then
                                // what the client sent while detached: each exactly once.
                                for f in &unacked {
                                    let _ = wire::write_frame(&mut send, f).await;
                                }
                                for f in queued {
                                    if stdin_done {
                                        break;
                                    }
                                    stdin_done = matches!(f, ExecFrame::StdinEof);
                                    let _ = wire::write_frame(&mut send, &f).await;
                                    unacked_bytes += input_cost(&f);
                                    unacked.push_back(f);
                                }
                                let detail = format!(
                                    "re-sent {} input frames the peer did not get",
                                    unacked.len()
                                );
                                debug!(peer = %peer.name, "{detail}");
                                audit(sid, Some("resume"), None, Some(&detail));
                                continue;
                            }
                            (Parked::ClientGone, _) => break Down::ClientGoneDetached,
                            (Parked::Lost(m), _) => break Down::Lost(m),
                        }
                    }
                };
                match &f {
                    ExecFrame::Stdout(b) | ExecFrame::Stderr(b) => {
                        first_byte.get_or_insert_with(|| t0.elapsed());
                        bytes += b.len() as u64;
                    }
                    _ => {}
                }
                delivered += 1;
                if wire::write_frame(&mut iw, &f).await.is_err() {
                    break Down::ClientGone;
                }
                if matches!(f, ExecFrame::Exit { .. } | ExecFrame::Error { .. }) {
                    if ticket.is_some() {
                        // Lets the peer drop the session at once.
                        let _ = wire::write_frame(&mut send, &ExecFrame::Ack { seq: delivered }).await;
                        let _ = send.finish();
                    }
                    break Down::Done(f);
                }
                if ticket.is_some()
                    && delivered - acked >= ACK_EVERY
                    && wire::write_frame(&mut send, &ExecFrame::Ack { seq: delivered }).await.is_ok()
                {
                    acked = delivered;
                }
            }
            f = up_rx.recv(), if unacked_bytes < MAX_UNACKED_INPUT => match f {
                // A write error means the link is going; the reader reports the loss. A
                // plain session drops the frame (input is delivered at most once); a
                // resumable one keeps it until acknowledged and re-sends it after a resume
                // if the peer did not get it (exactly once).
                Some(f @ ExecFrame::Stdin(_)) => {
                    if !stdin_done {
                        let _ = wire::write_frame(&mut send, &f).await;
                        if ticket.is_some() {
                            unacked_bytes += input_cost(&f);
                            unacked.push_back(f);
                        }
                    }
                }
                Some(ExecFrame::StdinEof) => {
                    if !stdin_done {
                        stdin_done = true;
                        let _ = wire::write_frame(&mut send, &ExecFrame::StdinEof).await;
                        if ticket.is_none() {
                            let _ = send.finish();
                        } else {
                            unacked_bytes += input_cost(&ExecFrame::StdinEof);
                            unacked.push_back(ExecFrame::StdinEof);
                        }
                    }
                }
                Some(_) => {}
                None => break Down::ClientGone,
            },
            _ = ack_tick.tick(), if ticket.is_some() && delivered > acked => {
                if wire::write_frame(&mut send, &ExecFrame::Ack { seq: delivered }).await.is_ok() {
                    acked = delivered;
                }
            }
        }
    };
    up_task.abort();

    let (exit_code, error) = match &outcome {
        Down::Done(ExecFrame::Exit { code }) => (Some(*code), None),
        Down::Done(ExecFrame::Error { message, .. }) => (None, Some(message.clone())),
        Down::Done(_) => (None, None),
        Down::LinkLost(e) | Down::Lost(e) => {
            let msg = match &outcome {
                Down::Lost(_) => format!("session on {} lost: {e}", peer.name),
                _ => format!("link to {} lost during the session: {e}", peer.name),
            };
            let _ = wire::write_frame(
                &mut iw,
                &ExecFrame::Error {
                    code: exit::PEER_OFFLINE,
                    message: msg,
                },
            )
            .await;
            match &outcome {
                Down::Lost(_) => (None, Some(format!("session lost: {e}"))),
                _ => (None, Some("link lost".to_string())),
            }
        }
        Down::ClientGone => {
            // Make the peer kill the process: abort our side of the stream.
            let _ = send.reset(STREAM_CLIENT_GONE.into());
            down_task.abort();
            (None, Some("caller gone".to_string()))
        }
        Down::ClientGoneDetached => (None, Some("caller gone while detached".to_string())),
    };
    info!(
        peer = %peer.name,
        "{word} out `{what}`: ack={}ms first_byte={}ms end={}ms bytes={bytes} resumes={resumes} exit={}",
        ms(t_ack),
        first_byte.map(ms).unwrap_or_else(|| "-".into()),
        ms(t0.elapsed()),
        exit_code.map(exit::describe).unwrap_or_else(|| error.clone().unwrap_or_default()),
    );
    audit(sid, None, exit_code, error.as_deref());
    Ok(())
}

/// The link of resumable session `t` was lost (`why`): tell the local client, then try to
/// reattach whenever the peer is online, until the TTL is over. Stdin from the client is
/// queued meanwhile (bounded: then it is back-pressured) and returned for sending after a
/// resume.
async fn park<W: AsyncWrite + Unpin>(
    peer: &Arc<Peer>,
    t: &ClientTicket,
    delivered: u64,
    up_rx: &mut mpsc::Receiver<ExecFrame>,
    iw: &mut W,
    why: &str,
) -> (Parked, Vec<ExecFrame>) {
    let lost_at = tokio::time::Instant::now();
    let give_up = lost_at + t.ttl + ACK_TIMEOUT;
    info!(
        peer = %peer.name, session = t.id,
        "link lost ({why}): session detached, the device keeps it for {:?}", t.ttl
    );
    let message = format!(
        "link to {} lost; the remote process is kept for {:?} and the session resumes when the \
         device is back",
        peer.name, t.ttl
    );
    let mut queued = Vec::new();
    if wire::write_frame(iw, &ExecFrame::Detached { message })
        .await
        .is_err()
    {
        cancel_later(peer.clone(), *t, delivered);
        return (Parked::ClientGone, queued);
    }
    loop {
        if let Some(conn) = live_conn(peer) {
            match try_resume(&conn, t, delivered, false).await {
                Ok(Ok((send, recv, input))) => {
                    info!(
                        peer = %peer.name, session = t.id,
                        "session resumed after {:.1}s", lost_at.elapsed().as_secs_f64()
                    );
                    if wire::write_frame(iw, &ExecFrame::Resumed).await.is_err() {
                        debug!("client gone right after the resume");
                    }
                    return (Parked::Resumed(send, recv, input), queued);
                }
                Ok(Err(m)) => {
                    info!(peer = %peer.name, session = t.id, "session cannot be resumed: {m}");
                    return (Parked::Lost(m), queued);
                }
                Err(e) => debug!(peer = %peer.name, session = t.id, "resume attempt: {e:#}"),
            }
        }
        if tokio::time::Instant::now() >= give_up {
            // The device may still hold it (its TTL runs from when *it* noticed the loss,
            // e.g. after waking up): have it killed when it is back.
            cancel_later(peer.clone(), *t, delivered);
            let m = format!("{} did not come back within {:?}", peer.name, t.ttl);
            return (Parked::Lost(m), queued);
        }
        tokio::select! {
            f = up_rx.recv(), if queued.len() < MAX_QUEUED => match f {
                Some(f) => queued.push(f),
                None => {
                    cancel_later(peer.clone(), *t, delivered);
                    return (Parked::ClientGone, queued);
                }
            },
            _ = tokio::time::sleep(RESUME_POLL) => {}
        }
    }
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
    let (header, want_resume) = match header {
        SessionKind::Resume {
            id,
            token,
            seq,
            cancel,
        } => {
            let a = detach::Attach {
                send,
                recv,
                seq,
                cancel,
            };
            return serve_resume(&sh, &peer, id, token, a).await;
        }
        SessionKind::Resumable(inner) => (*inner, true),
        h => (h, false),
    };
    let (word, what) = describe(&header);
    // Services: authorize, then the service's argv (no shell) is the process to spawn.
    let (req, service, mcp) = match header {
        SessionKind::Resumable(_) | SessionKind::Resume { .. } => {
            let _ = refuse(&mut send, exit::USAGE, "invalid session header".into()).await;
            return;
        }
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
    let mut au = InAudit {
        sh: &sh,
        peer: &peer.name,
        service: service.as_deref(),
        mcp,
        req: &req,
        t0,
        session: None,
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
            au.record(None, Some(why));
            let _ = refuse(&mut send, exit::NOT_ALLOWED, why.into()).await;
            return;
        }
    }
    let running = match svc::spawn(&req) {
        Ok(r) => r,
        Err(e) => {
            info!(peer = %peer.name, "{word} `{what}` failed to start: {e}");
            au.record(None, Some(&e.message));
            let _ = refuse(&mut send, e.code, e.message).await;
            return;
        }
    };
    // Resumable (design D18): services and transparent MCP, when the caller asks and the
    // TTL is not zero.
    let ttl = match (&service, mcp, want_resume) {
        (Some(name), _, true) => sh.cfg().detach_ttl(Some(name)),
        (None, true, true) => sh.cfg().detach_ttl(None),
        _ => Duration::ZERO,
    };
    let ticket = (!ttl.is_zero()).then(|| detach::Ticket::new(ttl));
    let ack = match &ticket {
        Some(t) => SessionAck::Resumable {
            id: t.id,
            token: t.token,
            ttl_ms: t.ttl.as_millis() as u64,
        },
        None => SessionAck::Ok,
    };
    if wire::write_frame(&mut send, &ack).await.is_err() {
        au.record(None, Some("caller gone before ack"));
        return; // `running` is dropped: kill_on_drop.
    }
    debug!(peer = %peer.name, pid = ?running.id(), program = %running.plan.program, session = ?ticket.as_ref().map(|t| t.id), "{word} started");
    let timeout = req.timeout_secs.map(Duration::from_secs);

    if let Some(t) = ticket {
        au.session = Some(t.id);
        let (out, end) = detach::run_hub(
            &sh.detached,
            &peer.name,
            &t,
            running,
            timeout,
            send,
            recv,
            &au,
        )
        .await;
        let (exit_code, error) = match (&out.end, end.describe()) {
            (_, Some(why)) => (None, Some(why)),
            (End::Exited(c), None) => (Some(*c), None),
            (End::TimedOut, None) => (None, Some("timeout")),
            (End::Disconnected, None) => (None, Some("caller gone, killed")),
        };
        log_in(&peer.name, word, &what, &out, exit_code, error);
        au.record(exit_code, error);
        return;
    }

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
    log_in(&peer.name, word, &what, &out, exit_code, error);
    au.record(exit_code, error);
}

fn log_in(
    peer: &str,
    word: &str,
    what: &str,
    out: &svc::Outcome,
    exit_code: Option<i32>,
    error: Option<&str>,
) {
    info!(
        peer = %peer,
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
}

/// Audit lines of a session served here.
struct InAudit<'a> {
    sh: &'a Shared,
    peer: &'a str,
    service: Option<&'a str>,
    mcp: bool,
    req: &'a enishi_proto::session::ExecRequest,
    t0: Instant,
    session: Option<u64>,
}

impl InAudit<'_> {
    fn entry(&self) -> Entry<'_> {
        let e = match self.service {
            Some(name) => Entry::service(Direction::In, self.peer, name, &self.req.command),
            None if self.mcp => Entry::mcp(Direction::In, self.peer, self.req),
            None => Entry::exec(Direction::In, self.peer, self.req),
        };
        e.session(self.session)
    }

    fn record(&self, exit: Option<i32>, error: Option<&str>) {
        self.sh
            .audit
            .record(&self.entry().finish(exit, error, self.t0.elapsed()));
    }
}

impl detach::Events for InAudit<'_> {
    fn event(&self, event: &str, detail: Option<&str>) {
        self.sh
            .audit
            .record(
                &self
                    .entry()
                    .event(event, detail)
                    .finish(None, None, self.t0.elapsed()),
            );
    }
}

/// A caller reattaches (or cancels) a detached session.
async fn serve_resume(sh: &Shared, peer: &Peer, id: u64, token: u128, a: detach::Attach) {
    let Some(tx) = sh.detached.lookup(id, &peer.name, token) else {
        info!(peer = %peer.name, session = id, "resume refused: unknown session");
        let mut send = a.send;
        let _ = refuse(
            &mut send,
            exit::PEER_OFFLINE,
            format!(
                "session {id:016x} is gone (ended, expired, or {} restarted)",
                sh.device
            ),
        )
        .await;
        return;
    };
    if let Err(mpsc::error::SendError(mut a)) = tx.send(a) {
        let _ = refuse(
            &mut a.send,
            exit::PEER_OFFLINE,
            "the session already ended".into(),
        )
        .await;
    }
}

pub(crate) async fn refuse(send: &mut SendStream, code: i32, message: String) -> Result<()> {
    wire::write_frame(send, &SessionAck::Err { code, message }).await?;
    send.finish()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::supports_resume;

    #[test]
    fn resume_needs_0_1_2() {
        for (v, ok) in [
            (None, false),
            (Some("0.1.1"), false),
            (Some("0.1.2"), true),
            (Some("0.1.2-rc.1"), true),
            (Some("v0.1.3"), true),
            (Some("0.2.0"), true),
            (Some("1.0.0+build"), true),
            (Some("0.0.9"), false),
            (Some("garbage"), false),
        ] {
            assert_eq!(supports_resume(v), ok, "{v:?}");
        }
    }
}

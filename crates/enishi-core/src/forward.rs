//! TCP forwarding (`kind = "tcp"` services, `enishi forward`, `[forwards.<name>]`).
//!
//! - consumer: [`listen`] accepts local TCP connections and tunnels each one as its own
//!   bi-stream (`SessionKind::Tcp`) on the warm stream connection to the peer. While the
//!   peer is offline a connection is accepted and closed at once (fast fail) and the
//!   listener keeps going, so it works again as soon as the peer is back.
//! - executor: [`serve`] authorizes the session like any service (allow ∧ grant),
//!   connects to the service's `target` before acking, then pipes bytes both ways.
//!
//! After the ack a tunnel stream carries raw bytes; finishing one direction of the QUIC
//! stream is a TCP half-close (`shutdown(Write)`) on the other side and vice versa.

use std::{
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant},
};

use enishi_proto::{
    SessionKind, exit,
    ipc::{IpcResponse, Presence},
    session::SessionAck,
};
use iroh::endpoint::{RecvStream, SendStream};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::mpsc,
};
use tracing::{debug, info, warn};

use crate::{
    audit::{Direction, Entry},
    config::{ForwardConfig, ServiceKind},
    daemon::{Peer, Shared},
    policy::authorize_service,
    session::ACK_TIMEOUT,
    wire,
};

/// Connecting to the target must leave time to ack within [`ACK_TIMEOUT`].
const CONNECT_TIMEOUT: Duration = Duration::from_millis(2500);
/// Stream error code: one side of the tunnel failed (reset/abort).
const STREAM_ABORTED: u32 = 2;

/// Where a forward's notes go: the `enishi forward` CLI (IPC) and/or the daemon log.
pub(crate) type Notes = Option<mpsc::UnboundedSender<String>>;

fn note(notes: &Notes, msg: String) {
    if let Some(tx) = notes {
        let _ = tx.send(msg);
    }
}

/// `enishi forward` (IPC): bind, report the address, then forward until the CLI goes away,
/// streaming notes (refused/failed connections) to it.
pub(crate) async fn serve_cli<S>(
    sh: Arc<Shared>,
    mut ipc: S,
    device: Option<String>,
    service: String,
    bind: Option<String>,
    port: Option<u16>,
) -> anyhow::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let err = |code: i32, message: String| IpcResponse::Error { code, message };
    let peer = match sh.target(device.as_deref()) {
        Ok(p) => p,
        Err(m) => return wire::write_frame(&mut ipc, &err(exit::USAGE, m)).await,
    };
    let port = match port {
        Some(p) => p,
        None => match remote_port(&sh, &peer.name, &service).await {
            Ok(p) => p,
            Err((code, m)) => return wire::write_frame(&mut ipc, &err(code, m)).await,
        },
    };
    let listener = match bind_listener(bind.as_deref(), port).await {
        Ok(l) => l,
        Err(m) => return wire::write_frame(&mut ipc, &err(exit::USAGE, m)).await,
    };
    let local = listener.local_addr()?.to_string();
    info!(peer = %peer.name, "forward {service}: listening on {local} (enishi forward)");
    let ok = IpcResponse::Forwarding {
        device: peer.name.clone(),
        service: service.clone(),
        local: local.clone(),
    };
    wire::write_frame(&mut ipc, &ok).await?;
    let (tx, mut rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(listen(
        sh,
        peer.name.clone(),
        service.clone(),
        listener,
        Some(tx),
    ));
    let (mut ir, mut iw) = tokio::io::split(ipc);
    let cli_gone = async {
        let mut b = [0u8; 64];
        while matches!(ir.read(&mut b).await, Ok(n) if n > 0) {}
    };
    let relay = async {
        while let Some(m) = rx.recv().await {
            if wire::write_frame(&mut iw, &IpcResponse::Note(m))
                .await
                .is_err()
            {
                break;
            }
        }
    };
    tokio::select! {
        _ = cli_gone => {}
        _ = relay => {}
    }
    task.abort(); // drops the listener; tunnels already open finish on their own
    info!("forward {service}: stopped listening on {local}");
    Ok(())
}

/// A `[forwards.<name>]` listener, for the daemon's lifetime (or until the config
/// changes). Binding is retried briefly: a just-aborted predecessor may still hold the port.
pub(crate) async fn run_configured(
    sh: Arc<Shared>,
    name: String,
    device: String,
    f: ForwardConfig,
) {
    let mut tries = 0;
    let listener = loop {
        match bind_listener(f.bind.as_deref(), f.local_port).await {
            Ok(l) => break l,
            Err(e) if tries < 20 => {
                tries += 1;
                if tries == 1 {
                    debug!("forward {name}: {e}; retrying");
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Err(e) => return warn!("forward {name}: {e}; not forwarding"),
        }
    };
    let local = listener
        .local_addr()
        .map(|a| a.to_string())
        .unwrap_or_default();
    info!("forward {name}: {local} -> {device}/{}", f.service);
    listen(sh, device, f.service, listener, None).await;
}

/// The target port of `device`'s tcp `service` (the default local port).
async fn remote_port(sh: &Shared, device: &str, service: &str) -> Result<u16, (i32, String)> {
    match sh.list_services(Some(device.to_string())).await {
        IpcResponse::Services { services, .. } => {
            match services.iter().find(|s| s.name == service) {
                Some(s) if s.kind == ServiceKind::Tcp => s.port.ok_or_else(|| {
                    (
                        exit::USAGE,
                        format!("{device} did not report a port for {service:?}; give one"),
                    )
                }),
                Some(s) => Err((
                    exit::USAGE,
                    format!(
                        "{device}/{service} is a {} service, not tcp",
                        s.kind.as_str()
                    ),
                )),
                None => Err((
                    exit::NO_SUCH_SERVICE,
                    format!("{device} publishes no service {service:?} to this device"),
                )),
            }
        }
        IpcResponse::Error { code, message } => Err((
            code,
            format!("{message} (give the local port explicitly to forward anyway)"),
        )),
        other => Err((exit::USAGE, format!("unexpected reply {other:?}"))),
    }
}

/// Bind `bind` (default loopback) : `port`.
pub(crate) async fn bind_listener(bind: Option<&str>, port: u16) -> Result<TcpListener, String> {
    let ip: std::net::IpAddr = bind
        .unwrap_or("127.0.0.1")
        .parse()
        .map_err(|_| format!("--bind wants an IP address, got {:?}", bind.unwrap_or("")))?;
    let addr = SocketAddr::new(ip, port);
    TcpListener::bind(addr)
        .await
        .map_err(|e| format!("cannot listen on {addr}: {e}"))
}

/// Accept connections on `listener` forever, tunnelling each to `device`'s `service`.
/// The peer is looked up by name per connection, so reloads and reconnects apply.
pub(crate) async fn listen(
    sh: Arc<Shared>,
    device: String,
    service: String,
    listener: TcpListener,
    notes: Notes,
) {
    loop {
        match listener.accept().await {
            Ok((tcp, from)) => {
                tokio::spawn(tunnel(
                    sh.clone(),
                    device.clone(),
                    service.clone(),
                    tcp,
                    from,
                    notes.clone(),
                ));
            }
            Err(e) => {
                // EMFILE and friends: back off instead of spinning.
                warn!("forward {device}/{service}: accept: {e}");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

/// Consumer side of one forwarded connection.
async fn tunnel(
    sh: Arc<Shared>,
    device: String,
    service: String,
    tcp: TcpStream,
    from: SocketAddr,
    notes: Notes,
) {
    let t0 = Instant::now();
    let header = SessionKind::Tcp {
        service: service.clone(),
    };
    let fail = |peer: &str, why: String| {
        sh.audit
            .record(&Entry::service(Direction::Out, peer, &service, &[]).finish(
                None,
                Some(&why),
                t0.elapsed(),
            ));
        let msg = format!("forward {device}/{service}: connection from {from} closed: {why}");
        info!("{msg}");
        note(&notes, msg);
    };
    let peer = match sh.target(Some(&device)) {
        Ok(p) => p,
        Err(why) => return fail(&device, why),
    };
    let conn = {
        let s = peer.state();
        match (&s.stream, s.presence) {
            (Some(c), Presence::Online) => Some(c.clone()),
            _ => None,
        }
    };
    let Some(conn) = conn else {
        // Dropping `tcp` closes it: the local client fails at once instead of hanging.
        return fail(&peer.name, format!("peer {} is offline", peer.name));
    };
    let open = async {
        let (mut send, mut recv) = conn.open_bi().await?;
        wire::write_frame(&mut send, &header).await?;
        let ack: SessionAck = wire::read_frame(&mut recv).await?;
        anyhow::Ok((send, recv, ack))
    };
    let (send, recv) = match tokio::time::timeout(ACK_TIMEOUT, open).await {
        Ok(Ok((s, r, SessionAck::Ok))) => (s, r),
        Ok(Ok((_, _, SessionAck::Err { message, .. }))) => {
            return fail(&peer.name, format!("{} refused: {message}", peer.name));
        }
        Ok(Ok((_, _, other))) => {
            return fail(&peer.name, format!("unexpected answer {other:?}"));
        }
        Ok(Err(e)) => return fail(&peer.name, format!("session failed: {e:#}")),
        Err(_) => return fail(&peer.name, "peer did not acknowledge in time".into()),
    };
    let t_ack = t0.elapsed();
    let _ = tcp.set_nodelay(true);
    let (up, down, error) = pipe(tcp, send, recv).await;
    info!(
        peer = %peer.name,
        "forward out {service} from {from}: ack={:.0}ms end={:.0}ms up={up}B down={down}B{}",
        t_ack.as_secs_f64() * 1000.0,
        t0.elapsed().as_secs_f64() * 1000.0,
        error.as_deref().map(|e| format!(" error={e}")).unwrap_or_default(),
    );
    sh.audit.record(
        &Entry::service(Direction::Out, &peer.name, &service, &[]).finish(
            error.is_none().then_some(0),
            error.as_deref(),
            t0.elapsed(),
        ),
    );
}

/// Executor side of a `SessionKind::Tcp` session.
pub(crate) async fn serve(
    sh: Arc<Shared>,
    peer: Arc<Peer>,
    mut send: SendStream,
    recv: RecvStream,
    service: String,
) {
    let t0 = Instant::now();
    let audit = |exit: Option<i32>, error: Option<&str>| {
        sh.audit.record(
            &Entry::service(Direction::In, &peer.name, &service, &[]).finish(
                exit,
                error,
                t0.elapsed(),
            ),
        );
    };
    let target = {
        let cfg = sh.cfg();
        match authorize_service(&cfg, &peer.name, &service, &[]) {
            Ok(svc) if svc.kind == ServiceKind::Tcp => Ok(svc.target.clone().unwrap_or_default()),
            Ok(svc) => Err((
                exit::USAGE,
                format!(
                    "service {service:?} is a {} service, not tcp (use `enishi {}`)",
                    svc.kind.as_str(),
                    if svc.kind == ServiceKind::Mcp {
                        "mcp"
                    } else {
                        "run"
                    }
                ),
            )),
            Err(r) => Err((r.code, r.message)),
        }
    };
    let target = match target {
        Ok(t) => t,
        Err((code, message)) => {
            warn!(peer = %peer.name, code, "tcp `{service}` refused: {message}");
            audit(None, Some(&message));
            let _ = crate::session::refuse(&mut send, code, message).await;
            return;
        }
    };
    let tcp = match tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(&target)).await {
        Ok(Ok(t)) => t,
        Ok(Err(e)) => {
            let message = format!("cannot connect to {target}: {e}");
            info!(peer = %peer.name, "tcp `{service}`: {message}");
            audit(None, Some(&message));
            let _ = crate::session::refuse(&mut send, exit::CANNOT_EXECUTE, message).await;
            return;
        }
        Err(_) => {
            let message = format!("connecting to {target} timed out");
            audit(None, Some(&message));
            let _ = crate::session::refuse(&mut send, exit::CANNOT_EXECUTE, message).await;
            return;
        }
    };
    if wire::write_frame(&mut send, &SessionAck::Ok).await.is_err() {
        return audit(None, Some("caller gone before ack"));
    }
    let t_conn = t0.elapsed();
    let _ = tcp.set_nodelay(true);
    let (up, down, error) = pipe(tcp, send, recv).await;
    info!(
        peer = %peer.name,
        "forward in {service} -> {target}: connect={:.0}ms end={:.0}ms to_target={down}B from_target={up}B{}",
        t_conn.as_secs_f64() * 1000.0,
        t0.elapsed().as_secs_f64() * 1000.0,
        error.as_deref().map(|e| format!(" error={e}")).unwrap_or_default(),
    );
    audit(error.is_none().then_some(0), error.as_deref());
}

/// Pipe `tcp` ⇄ (`send`, `recv`) with half-close both ways. Returns bytes tcp→send,
/// bytes recv→tcp and the first error; an error in either direction aborts both.
pub(crate) async fn pipe(
    tcp: TcpStream,
    send: SendStream,
    recv: RecvStream,
) -> (u64, u64, Option<String>) {
    let (tr, tw) = tcp.into_split();
    pipe_halves(tr, tw, send, recv).await
}

/// Generic over the halves so it can be tested without QUIC.
async fn pipe_halves<R, W, S, Q>(
    mut tr: R,
    mut tw: W,
    mut send: S,
    mut recv: Q,
) -> (u64, u64, Option<String>)
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
    S: AsyncWrite + Abort + Unpin,
    Q: AsyncRead + Abort + Unpin,
{
    let (mut n_up, mut n_down, mut error) = (0, 0, None);
    {
        let up = async {
            let n = tokio::io::copy(&mut tr, &mut send).await?;
            send.shutdown().await?; // QUIC finish = TCP FIN on the far side
            std::io::Result::Ok(n)
        };
        let down = async {
            let n = tokio::io::copy(&mut recv, &mut tw).await?;
            tw.shutdown().await?; // far side finished: half-close our socket
            std::io::Result::Ok(n)
        };
        tokio::pin!(up, down);
        let (mut up_done, mut down_done) = (false, false);
        while !(up_done && down_done) {
            tokio::select! {
                r = &mut up, if !up_done => {
                    up_done = true;
                    match r {
                        Ok(n) => n_up = n,
                        Err(e) => { error = Some(format!("local side: {e}")); break }
                    }
                }
                r = &mut down, if !down_done => {
                    down_done = true;
                    match r {
                        Ok(n) => n_down = n,
                        Err(e) => { error = Some(format!("remote side: {e}")); break }
                    }
                }
            }
        }
    }
    if error.is_some() {
        send.abort();
        recv.abort();
    }
    (n_up, n_down, error)
}

/// Abort a tunnel half on error (QUIC reset/stop); no-op for plain I/O.
pub(crate) trait Abort {
    fn abort(&mut self) {}
}

impl Abort for SendStream {
    fn abort(&mut self) {
        let _ = self.reset(STREAM_ABORTED.into());
    }
}

impl Abort for RecvStream {
    fn abort(&mut self) {
        let _ = self.stop(STREAM_ABORTED.into());
    }
}

#[cfg(test)]
impl Abort for tokio::io::DuplexStream {}
#[cfg(test)]
impl Abort for tokio::io::WriteHalf<tokio::io::DuplexStream> {}
#[cfg(test)]
impl Abort for tokio::io::ReadHalf<tokio::io::DuplexStream> {}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, duplex, split};

    /// Local client ⇄ [pipe] ⇄ "QUIC" duplex ⇄ remote end: data both ways and half-close.
    #[tokio::test]
    async fn pipes_both_ways_with_half_close() {
        let (client, local) = duplex(1 << 16); // client ⇄ local socket
        let (quic_local, mut remote) = duplex(1 << 16); // tunnel ⇄ remote end
        let (lr, lw) = split(local);
        let (qr, qw) = split(quic_local);
        let task = tokio::spawn(pipe_halves(lr, lw, qw, qr));
        let (mut cr, mut cw) = split(client);

        // Client sends a request and half-closes; the remote end sees EOF after it.
        cw.write_all(b"GET / HTTP/1.0\r\n\r\n").await.unwrap();
        cw.shutdown().await.unwrap();
        let mut req = Vec::new();
        let mut buf = [0u8; 64];
        loop {
            let n = remote.read(&mut buf).await.unwrap();
            if n == 0 {
                break;
            }
            req.extend_from_slice(&buf[..n]);
        }
        assert_eq!(req, b"GET / HTTP/1.0\r\n\r\n");
        // The remote can still answer after the client's half-close.
        let body = vec![7u8; 200_000];
        let expect = body.clone();
        let writer = tokio::spawn(async move {
            remote.write_all(&body).await.unwrap();
            remote.shutdown().await.unwrap();
        });
        let mut got = Vec::new();
        cr.read_to_end(&mut got).await.unwrap();
        writer.await.unwrap();
        assert_eq!(got, expect);
        let (up, down, err) = task.await.unwrap();
        assert_eq!((up, down, err), (18, 200_000, None));
    }
}

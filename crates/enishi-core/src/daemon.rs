//! `enishi up`: the link daemon (layer L2).
//!
//! For every configured peer the daemon dials and keeps one warm `enishi/ctl/1`
//! connection: a hello on connect, a heartbeat every [`HEARTBEAT`], offline after one
//! missed heartbeat, then reconnect with a short capped backoff. Incoming connections are
//! accepted only from configured peers (rejected during the handshake). A hello received
//! from a peer skips our backoff, and a daemon that detects a wall-clock jump (resume
//! from sleep) reconnects at once with `resume = true` so the peer does the same.

use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant, SystemTime},
};

use anyhow::{Context, Result, bail};
use enishi_proto::{
    ALPN_CTL,
    ctl::{CtlRequest, CtlResponse, Hello},
    exit,
    ipc::{IpcRequest, IpcResponse, PeerStatus, Presence, StatusReport},
};
use iroh::{
    Endpoint, EndpointAddr, EndpointId, SecretKey,
    endpoint::{AfterHandshakeOutcome, Connection, EndpointHooks, presets},
};
use tokio::sync::Notify;
use tracing::{debug, info, warn};

use crate::{config::Config, ipc, paths::Paths, wire};

pub const HEARTBEAT: Duration = Duration::from_millis(1500);
pub const HEARTBEAT_TIMEOUT: Duration = Duration::from_millis(1500);
pub const CONNECT_TIMEOUT: Duration = Duration::from_millis(3000);
pub const BACKOFF_MIN: Duration = Duration::from_millis(250);
pub const BACKOFF_MAX: Duration = Duration::from_millis(1500);
/// Wall-clock gap on a 1 s tick that counts as a resume from sleep.
pub const RESUME_JUMP: Duration = Duration::from_secs(5);
/// Bound for a single ctl request/response on an incoming connection.
const SERVE_TIMEOUT: Duration = Duration::from_secs(5);
/// "rejected unknown peer" is logged at most once per peer per this interval.
const REJECT_LOG_EVERY: Duration = Duration::from_secs(60);

const CLOSE_REJECTED: u32 = 1;
const CLOSE_RESET: u32 = 2;
const VERSION: &str = env!("CARGO_PKG_VERSION");

struct PeerState {
    presence: Presence,
    since: Instant,
    last_seen: Option<Instant>,
    rtt: Option<Duration>,
    last_error: Option<String>,
    /// `instance` from the hello on our current outgoing connection.
    remote_instance: Option<u64>,
    /// Set when the current connection must be dropped (peer restarted / resumed).
    force_reset: bool,
}

struct Peer {
    name: String,
    id: EndpointId,
    addr: EndpointAddr,
    state: Mutex<PeerState>,
    /// Wakes the supervisor: skips the backoff when offline; when online, probes the link
    /// with an immediate heartbeat, or drops it if `force_reset` is set.
    wake: Notify,
    /// Send `resume = true` in the next hello.
    resume: AtomicBool,
}

impl Peer {
    fn state(&self) -> std::sync::MutexGuard<'_, PeerState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn seen(&self, rtt: Option<Duration>) {
        let mut s = self.state();
        s.last_seen = Some(Instant::now());
        if rtt.is_some() {
            s.rtt = rtt;
        }
    }

    fn request_reconnect(&self, force: bool) {
        if force {
            self.state().force_reset = true;
        }
        self.wake.notify_one();
    }

    /// Decide what a hello from the peer (on its connection to us) means for ours.
    fn on_hello(&self, h: &Hello) {
        let (online, known) = {
            let s = self.state();
            (s.presence == Presence::Online, s.remote_instance)
        };
        if !online {
            info!(peer = %self.name, resume = h.resume, "hello from peer, reconnecting now");
            self.request_reconnect(false);
        } else if known != Some(h.instance) {
            // Our connection belongs to a previous run of the peer: it is dead.
            info!(peer = %self.name, "hello from restarted peer, resetting link");
            self.request_reconnect(true);
        } else if h.resume {
            // Our connection may or may not have survived the peer's sleep: probe it now
            // instead of waiting for the next heartbeat (a fresh one simply stays up).
            info!(peer = %self.name, "hello from resumed peer, probing link");
            self.request_reconnect(false);
        }
    }

    fn status(&self, now: Instant) -> PeerStatus {
        let s = self.state();
        let ms = |i: Instant| now.saturating_duration_since(i).as_millis() as u64;
        PeerStatus {
            name: self.name.clone(),
            id: self.id.to_string(),
            presence: s.presence,
            since_ms: ms(s.since),
            last_seen_ms: s.last_seen.map(ms),
            rtt_us: s.rtt.map(|d| d.as_micros() as u64),
            last_error: s.last_error.clone(),
        }
    }
}

struct Shared {
    device: String,
    id: EndpointId,
    instance: u64,
    peers: Vec<Arc<Peer>>,
    by_id: HashMap<EndpointId, Arc<Peer>>,
}

impl Shared {
    fn hello(&self, resume: bool) -> Hello {
        Hello {
            device: self.device.clone(),
            version: VERSION.into(),
            instance: self.instance,
            resume,
        }
    }

    fn handle_ipc(&self, req: IpcRequest) -> IpcResponse {
        match req {
            IpcRequest::Status { device } => {
                if let Some(d) = &device
                    && !self.peers.iter().any(|p| &p.name == d)
                {
                    return IpcResponse::Error {
                        code: exit::USAGE,
                        message: format!("unknown device {d:?}"),
                    };
                }
                let now = Instant::now();
                IpcResponse::Status(StatusReport {
                    device: self.device.clone(),
                    id: self.id.to_string(),
                    version: VERSION.into(),
                    peers: self
                        .peers
                        .iter()
                        .filter(|p| device.as_ref().is_none_or(|d| &p.name == d))
                        .map(|p| p.status(now))
                        .collect(),
                })
            }
        }
    }
}

/// Logs a key at most once per interval.
#[derive(Debug)]
pub(crate) struct RateLimit<K> {
    every: Duration,
    last: Mutex<HashMap<K, Instant>>,
}

impl<K: std::hash::Hash + Eq> RateLimit<K> {
    pub(crate) fn new(every: Duration) -> Self {
        RateLimit {
            every,
            last: Mutex::new(HashMap::new()),
        }
    }

    pub(crate) fn allow(&self, key: K, now: Instant) -> bool {
        let mut last = self.last.lock().unwrap_or_else(|e| e.into_inner());
        if last.len() > 1024 {
            let every = self.every;
            last.retain(|_, t| now.saturating_duration_since(*t) < every);
        }
        match last.get(&key) {
            Some(t) if now.saturating_duration_since(*t) < self.every => false,
            _ => {
                last.insert(key, now);
                true
            }
        }
    }
}

/// Rejects every peer that is not in the config, right after the TLS handshake.
#[derive(Debug)]
struct AllowList {
    ids: Vec<EndpointId>,
    log: RateLimit<EndpointId>,
}

impl EndpointHooks for AllowList {
    async fn after_handshake<'a>(&'a self, conn: &'a Connection) -> AfterHandshakeOutcome {
        let id = conn.remote_id();
        if self.ids.contains(&id) {
            AfterHandshakeOutcome::Accept
        } else {
            if self.log.allow(id, Instant::now()) {
                warn!(peer = %id, "rejected connection from unknown peer (logged once a minute)");
            } else {
                debug!(peer = %id, "rejected connection from unknown peer");
            }
            AfterHandshakeOutcome::Reject {
                error_code: CLOSE_REJECTED.into(),
                reason: b"unknown peer".to_vec(),
            }
        }
    }
}

/// Run the daemon until SIGINT/SIGTERM.
pub async fn run(cfg: Config, paths: Paths, key: SecretKey) -> Result<()> {
    // IPC first: fail fast (before binding UDP) if this instance is already running.
    let mut listener = ipc::Listener::bind(&paths).await?;

    let id = key.public();
    let instance = u64::from_le_bytes(SecretKey::generate().to_bytes()[..8].try_into()?);
    let peers: Vec<Arc<Peer>> = cfg
        .peers
        .iter()
        .map(|(name, p)| {
            let mut addr = EndpointAddr::new(p.id);
            for a in &p.addrs {
                addr = addr.with_ip_addr(*a);
            }
            Arc::new(Peer {
                name: name.clone(),
                id: p.id,
                addr,
                state: Mutex::new(PeerState {
                    presence: Presence::Offline,
                    since: Instant::now(),
                    last_seen: None,
                    rtt: None,
                    last_error: None,
                    remote_instance: None,
                    force_reset: false,
                }),
                wake: Notify::new(),
                resume: AtomicBool::new(false),
            })
        })
        .collect();
    if peers.iter().any(|p| p.id == id) {
        bail!("a configured peer has this device's own id");
    }

    let mut builder = Endpoint::builder(presets::N0)
        .secret_key(key)
        .alpns(vec![ALPN_CTL.to_vec()])
        .hooks(AllowList {
            ids: peers.iter().map(|p| p.id).collect(),
            log: RateLimit::new(REJECT_LOG_EVERY),
        });
    if let Some(port) = cfg.device.listen_port {
        builder = builder.bind_addr(SocketAddr::from(([0, 0, 0, 0], port)))?;
    }
    let ep = builder.bind().await.context("binding iroh endpoint")?;

    let shared = Arc::new(Shared {
        device: cfg.device.name.clone(),
        id,
        instance,
        by_id: peers.iter().map(|p| (p.id, p.clone())).collect(),
        peers,
    });
    info!(
        device = %shared.device,
        id = %id,
        port = ?cfg.device.listen_port,
        ipc = %listener.display(),
        peers = shared.peers.len(),
        "daemon up"
    );

    for p in &shared.peers {
        tokio::spawn(supervise(ep.clone(), shared.clone(), p.clone()));
    }
    tokio::spawn(accept_loop(ep.clone(), shared.clone()));
    tokio::spawn(watch_clock(shared.clone()));

    let ipc_shared = shared.clone();
    let ipc_task = async move {
        loop {
            match listener.accept().await {
                Ok(stream) => {
                    let sh = ipc_shared.clone();
                    tokio::spawn(async move {
                        if let Err(e) = ipc::serve_one(stream, |r| sh.handle_ipc(r)).await {
                            debug!("ipc client: {e:#}");
                        }
                    });
                }
                Err(e) => {
                    warn!("ipc accept: {e}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
    };

    tokio::select! {
        _ = ipc_task => {}
        _ = shutdown_signal() => info!("shutting down"),
    }
    // Closing the endpoint sends CONNECTION_CLOSE, so peers see us offline immediately.
    if tokio::time::timeout(Duration::from_secs(2), ep.close())
        .await
        .is_err()
    {
        warn!("endpoint close timed out");
    }
    Ok(())
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = term.recv() => {}
                }
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

/// One request/response exchange on a fresh bi-stream.
async fn ctl_request(conn: &Connection, req: &CtlRequest) -> Result<CtlResponse> {
    let (mut send, mut recv) = conn.open_bi().await?;
    wire::write_frame(&mut send, req).await?;
    send.finish()?;
    wire::read_frame(&mut recv).await
}

async fn dial(
    ep: &Endpoint,
    sh: &Shared,
    peer: &Peer,
    resume: bool,
) -> Result<(Connection, Hello)> {
    let conn = ep.connect(peer.addr.clone(), ALPN_CTL).await?;
    match ctl_request(&conn, &CtlRequest::Hello(sh.hello(resume))).await? {
        CtlResponse::Hello(h) => Ok((conn, h)),
        CtlResponse::Error { message } => bail!("peer refused hello: {message}"),
        other => bail!("unexpected hello reply {other:?}"),
    }
}

async fn supervise(ep: Endpoint, sh: Arc<Shared>, peer: Arc<Peer>) {
    let mut backoff = BACKOFF_MIN;
    let mut last_err = String::new();
    loop {
        let resume = peer.resume.swap(false, Ordering::SeqCst);
        let t = Instant::now();
        let err = match tokio::time::timeout(CONNECT_TIMEOUT, dial(&ep, &sh, &peer, resume)).await {
            Ok(Ok((conn, hello))) => {
                backoff = BACKOFF_MIN;
                last_err.clear();
                {
                    let mut s = peer.state();
                    s.presence = Presence::Online;
                    s.since = Instant::now();
                    s.last_seen = Some(Instant::now());
                    s.remote_instance = Some(hello.instance);
                    s.force_reset = false;
                    s.last_error = None;
                }
                info!(
                    peer = %peer.name,
                    remote_device = %hello.device,
                    remote_version = %hello.version,
                    resume,
                    "ONLINE (connect {} ms)",
                    t.elapsed().as_millis()
                );
                let why = heartbeat(&conn, &peer).await;
                conn.close(CLOSE_RESET.into(), b"reset");
                {
                    let mut s = peer.state();
                    s.presence = Presence::Offline;
                    s.since = Instant::now();
                    s.last_error = Some(why.clone());
                }
                info!(peer = %peer.name, "OFFLINE ({why})");
                None
            }
            Ok(Err(e)) => Some(format!("{e:#}")),
            Err(_) => Some(format!("no answer in {} ms", CONNECT_TIMEOUT.as_millis())),
        };
        if let Some(e) = err {
            if resume {
                // Still owe the peer a resume hello.
                peer.resume.store(true, Ordering::SeqCst);
            }
            if e != last_err {
                info!(peer = %peer.name, "connect failed: {e}");
                last_err = e.clone();
            } else {
                debug!(peer = %peer.name, "connect failed: {e}");
            }
            peer.state().last_error = Some(e);
        }
        tokio::select! {
            _ = tokio::time::sleep(backoff) => {}
            _ = peer.wake.notified() => {
                backoff = BACKOFF_MIN;
                continue;
            }
        }
        backoff = (backoff * 2).min(BACKOFF_MAX);
    }
}

/// Returns why the connection is considered lost.
async fn heartbeat(conn: &Connection, peer: &Peer) -> String {
    let mut seq: u64 = 0;
    loop {
        tokio::select! {
            e = conn.closed() => return format!("connection closed: {e}"),
            _ = peer.wake.notified() => {
                if std::mem::take(&mut peer.state().force_reset) {
                    return "reset: restart or resume".into();
                }
                // Otherwise: heartbeat right now.
            }
            _ = tokio::time::sleep(HEARTBEAT) => {}
        }
        seq += 1;
        let t = Instant::now();
        match tokio::time::timeout(
            HEARTBEAT_TIMEOUT,
            ctl_request(conn, &CtlRequest::Ping { seq }),
        )
        .await
        {
            Ok(Ok(CtlResponse::Pong { seq: s })) if s == seq => peer.seen(Some(t.elapsed())),
            Ok(Ok(other)) => return format!("unexpected heartbeat reply {other:?}"),
            Ok(Err(e)) => return format!("heartbeat failed: {e:#}"),
            Err(_) => return format!("heartbeat timeout ({} ms)", HEARTBEAT_TIMEOUT.as_millis()),
        }
    }
}

async fn accept_loop(ep: Endpoint, sh: Arc<Shared>) {
    while let Some(incoming) = ep.accept().await {
        let sh = sh.clone();
        tokio::spawn(async move {
            let conn = match incoming.await {
                Ok(c) => c,
                Err(e) => {
                    debug!("incoming connection failed: {e}");
                    return;
                }
            };
            let id = conn.remote_id();
            // The handshake hook already rejects unknown ids; double-check here.
            let Some(peer) = sh.by_id.get(&id).cloned() else {
                conn.close(CLOSE_REJECTED.into(), b"unknown peer");
                return;
            };
            if conn.alpn() != ALPN_CTL {
                conn.close(CLOSE_REJECTED.into(), b"unsupported alpn");
                return;
            }
            debug!(peer = %peer.name, "incoming ctl connection");
            serve_ctl(conn, sh, peer).await;
        });
    }
}

async fn serve_ctl(conn: Connection, sh: Arc<Shared>, peer: Arc<Peer>) {
    while let Ok((mut send, mut recv)) = conn.accept_bi().await {
        let sh = sh.clone();
        let peer = peer.clone();
        tokio::spawn(async move {
            let exchange = async {
                let req: CtlRequest = wire::read_frame(&mut recv).await?;
                peer.seen(None);
                let resp = match req {
                    CtlRequest::Hello(h) => {
                        debug!(peer = %peer.name, remote_device = %h.device, "hello received");
                        peer.on_hello(&h);
                        CtlResponse::Hello(sh.hello(false))
                    }
                    CtlRequest::Ping { seq } => CtlResponse::Pong { seq },
                };
                wire::write_frame(&mut send, &resp).await?;
                send.finish()?;
                anyhow::Ok(())
            };
            match tokio::time::timeout(SERVE_TIMEOUT, exchange).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => debug!(peer = %peer.name, "ctl stream: {e:#}"),
                Err(_) => debug!(peer = %peer.name, "ctl stream timed out"),
            }
        });
    }
}

/// Detect resume from sleep portably: the wall clock jumps while the 1 s tick was frozen.
async fn watch_clock(sh: Arc<Shared>) {
    let mut last = SystemTime::now();
    loop {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let now = SystemTime::now();
        let gap = now.duration_since(last).unwrap_or_default();
        last = now;
        if gap > RESUME_JUMP {
            info!(
                "resume detected (wall clock jumped {:.1}s), sending hello to peers",
                gap.as_secs_f64()
            );
            for p in &sh.peers {
                p.resume.store(true, Ordering::SeqCst);
                p.request_reconnect(true);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_limit_once_per_interval_per_key() {
        let rl = RateLimit::new(Duration::from_secs(60));
        let t = Instant::now();
        assert!(rl.allow(1, t));
        assert!(!rl.allow(1, t + Duration::from_secs(1)));
        assert!(
            rl.allow(2, t + Duration::from_secs(1)),
            "other key unaffected"
        );
        assert!(!rl.allow(1, t + Duration::from_secs(59)));
        assert!(rl.allow(1, t + Duration::from_secs(61)));
    }
}

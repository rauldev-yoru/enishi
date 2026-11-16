//! `enishi up`: the link daemon (layer L2).
//!
//! For every configured peer the daemon dials and keeps one warm `enishi/ctl/1`
//! connection: a hello on connect, a heartbeat every [`HEARTBEAT`], offline after one
//! missed heartbeat, then reconnect with a short capped backoff. Incoming connections are
//! accepted only from configured peers (rejected during the handshake). A hello received
//! from a peer skips our backoff, and a daemon that detects a wall-clock jump (resume
//! from sleep) reconnects at once with `resume = true` so the peer does the same.
//!
//! Next to each ctl connection the daemon keeps a warm `enishi/stream/1` connection to the
//! same peer, on which exec and service sessions are opened (see [`crate::session`]).
//!
//! The peer set is live: pairing (`enishi/pair/1`, see [`crate::pairing`]) and
//! `enishi reload` add, replace and retire peers without a restart.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    net::SocketAddr,
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant, SystemTime},
};

use anyhow::{Context, Result, bail};
use enishi_proto::{
    ALPN_CTL, ALPN_PAIR, ALPN_STREAM, SessionKind,
    ctl::{CtlRequest, CtlResponse, Hello},
    exit,
    ipc::{IpcRequest, IpcResponse, PeerStatus, Presence, ReloadReport, StatusReport},
};
use iroh::{
    Endpoint, EndpointAddr, EndpointId, SecretKey,
    endpoint::{AfterHandshakeOutcome, Connection, EndpointHooks, Side, presets},
};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    sync::{Notify, watch},
};
use tracing::{debug, info, warn};

use crate::{
    audit::{AUDIT_FILE, Audit},
    config::ForwardConfig,
    config::{Config, Grant, PeerConfig},
    ipc,
    pair::{InviteBook, own_hint_addrs},
    pairing,
    paths::Paths,
    policy::{service_details, visible_services},
    session, wire,
};

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

pub(crate) const CLOSE_REJECTED: u32 = 1;
const CLOSE_RESET: u32 = 2;
/// The peer was removed from the config (reload) or replaced.
const CLOSE_REMOVED: u32 = 3;
/// This daemon is stopping.
const CLOSE_SHUTDOWN: u32 = 4;
/// On shutdown, how long to let the endpoint drain after every connection was closed.
/// The CONNECTION_CLOSE frames go out right away, so peers see us offline at once;
/// waiting the full QUIC draining period (3 PTO of the slowest path, seconds over a
/// relay) only delays the exit.
const SHUTDOWN_DRAIN: Duration = Duration::from_millis(500);
const VERSION: &str = env!("CARGO_PKG_VERSION");

pub(crate) struct PeerState {
    pub(crate) presence: Presence,
    since: Instant,
    pub(crate) last_seen: Option<Instant>,
    rtt: Option<Duration>,
    last_error: Option<String>,
    /// `instance` from the hello on our current outgoing connection.
    remote_instance: Option<u64>,
    /// Set when the current connection must be dropped (peer restarted / resumed).
    force_reset: bool,
    /// Warm `enishi/stream/1` connection (ours, outgoing) while online.
    pub(crate) stream: Option<Connection>,
    /// Our `enishi/ctl/1` connection while online (for requests such as `Services`).
    pub(crate) ctl: Option<Connection>,
}

pub(crate) struct Peer {
    pub(crate) name: String,
    pub(crate) id: EndpointId,
    addrs: Vec<SocketAddr>,
    addr: EndpointAddr,
    state: Mutex<PeerState>,
    /// Wakes the supervisor: skips the backoff when offline; when online, probes the link
    /// with an immediate heartbeat, or drops it if `force_reset` is set.
    wake: Notify,
    /// Send `resume = true` in the next hello.
    resume: AtomicBool,
    /// `true` once the peer left the config: its supervisor exits and every connection
    /// with it (ours and the ones it opened to us) is closed.
    retired: watch::Sender<bool>,
}

impl Peer {
    fn new(name: &str, cfg: &PeerConfig) -> Peer {
        let mut addr = EndpointAddr::new(cfg.id);
        for a in &cfg.addrs {
            addr = addr.with_ip_addr(*a);
        }
        Peer {
            name: name.to_string(),
            id: cfg.id,
            addrs: cfg.addrs.clone(),
            addr,
            state: Mutex::new(PeerState {
                presence: Presence::Offline,
                since: Instant::now(),
                last_seen: None,
                rtt: None,
                last_error: None,
                remote_instance: None,
                force_reset: false,
                stream: None,
                ctl: None,
            }),
            wake: Notify::new(),
            resume: AtomicBool::new(false),
            retired: watch::Sender::new(false),
        }
    }

    fn retire(&self) {
        self.retired.send_replace(true);
    }

    fn is_retired(&self) -> bool {
        *self.retired.borrow()
    }

    /// Resolves once the peer is retired.
    async fn until_retired(&self) {
        let mut rx = self.retired.subscribe();
        let _ = rx.wait_for(|r| *r).await;
    }

    pub(crate) fn state(&self) -> std::sync::MutexGuard<'_, PeerState> {
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

pub(crate) struct Shared {
    pub(crate) device: String,
    pub(crate) id: EndpointId,
    instance: u64,
    listen_port: Option<u16>,
    pub(crate) ep: Endpoint,
    pub(crate) paths: Paths,
    /// Sorted by name. Replaced as a whole by [`Shared::apply_config`].
    peers: RwLock<Vec<Arc<Peer>>>,
    pub(crate) cfg: RwLock<Config>,
    pub(crate) gate: Arc<Gate>,
    /// Serializes edits of the config file and of the peer set (pairing, reload).
    pub(crate) cfg_edit: tokio::sync::Mutex<()>,
    pub(crate) audit: Audit,
    /// Running `[forwards.*]` listeners, by name, with the config they were started from.
    forwards: Mutex<HashMap<String, (ForwardConfig, tokio::task::AbortHandle)>>,
    /// Incoming peer connections being served, by `stable_id` (closed on shutdown).
    incoming: Mutex<HashMap<usize, Connection>>,
}

impl Shared {
    /// Grant for `peer`; absent = deny everything.
    pub(crate) fn grant(&self, peer: &str) -> Grant {
        self.cfg().grant(peer)
    }

    pub(crate) fn cfg(&self) -> std::sync::RwLockReadGuard<'_, Config> {
        self.cfg.read().unwrap_or_else(|e| e.into_inner())
    }

    /// Close every peer connection (ours and theirs) with `code`/`reason`, so peers learn
    /// why at once.
    fn close_all(&self, code: u32, reason: &[u8]) {
        for peer in self.peers() {
            let s = peer.state();
            for c in [&s.ctl, &s.stream].into_iter().flatten() {
                c.close(code.into(), reason);
            }
        }
        for c in self.lock_incoming().values() {
            c.close(code.into(), reason);
        }
    }

    fn lock_incoming(&self) -> std::sync::MutexGuard<'_, HashMap<usize, Connection>> {
        self.incoming.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Snapshot of the current peers.
    pub(crate) fn peers(&self) -> Vec<Arc<Peer>> {
        self.peers.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub(crate) fn peer_by_id(&self, id: &EndpointId) -> Option<Arc<Peer>> {
        self.peers().into_iter().find(|p| &p.id == id)
    }

    /// Resolve `-d/--device` (or `default_peer`) to a configured peer.
    pub(crate) fn target(&self, device: Option<&str>) -> Result<Arc<Peer>, String> {
        let name = self
            .cfg()
            .target_peer(device)
            .map_err(|e| format!("{e:#}"))?
            .to_string();
        self.peers()
            .into_iter()
            .find(|p| p.name == name)
            .ok_or_else(|| format!("unknown device {name:?}"))
    }

    /// Address hints worth storing on the other side: only with a fixed `listen_port`
    /// (an ephemeral port changes on every restart), and only our own addresses on that
    /// port ([`own_hint_addrs`]: no NAT mappings observed by relays).
    pub(crate) fn stable_addrs(&self) -> Vec<SocketAddr> {
        match self.listen_port {
            Some(port) => own_hint_addrs(self.ep.addr().ip_addrs().copied(), &[port]),
            None => Vec::new(),
        }
    }

    /// Hints for a pairing invite: our own addresses on the fixed `listen_port`, else on
    /// the sockets bound right now (an invite lives in this daemon's memory, so those
    /// ports stay valid for as long as the invite does).
    pub(crate) fn invite_addrs(&self) -> Vec<SocketAddr> {
        let ports: Vec<u16> = match self.listen_port {
            Some(port) => vec![port],
            None => self.ep.bound_sockets().iter().map(|a| a.port()).collect(),
        };
        own_hint_addrs(self.ep.addr().ip_addrs().copied(), &ports)
    }

    /// Make `new` the running config: retire peers that left or changed, start the new
    /// ones, update the handshake allow-list. Grants and services apply to the next
    /// session. The caller holds `cfg_edit` and has checked `new` (see
    /// [`Shared::check_config`]).
    pub(crate) fn apply_config(self: &Arc<Self>, new: Config) -> ReloadReport {
        let mut report = ReloadReport::default();
        let mut started = Vec::new();
        {
            let mut peers = self.peers.write().unwrap_or_else(|e| e.into_inner());
            peers.retain(|p| match new.peers.get(&p.name) {
                Some(pc) if pc.id == p.id && pc.addrs == p.addrs => true,
                other => {
                    info!(peer = %p.name, "peer {} config, closing its links",
                        if other.is_some() { "changed in" } else { "removed from" });
                    p.retire();
                    if other.is_some() {
                        report.changed.push(p.name.clone());
                    } else {
                        report.removed.push(p.name.clone());
                    }
                    false
                }
            });
            for (name, pc) in &new.peers {
                if !peers.iter().any(|p| &p.name == name) {
                    let p = Arc::new(Peer::new(name, pc));
                    peers.push(p.clone());
                    started.push(p);
                    if !report.changed.contains(name) {
                        report.added.push(name.clone());
                    }
                }
            }
            peers.sort_by(|a, b| a.name.cmp(&b.name));
            *self.gate.allowed.write().unwrap_or_else(|e| e.into_inner()) =
                peers.iter().map(|p| p.id).collect();
        }
        if new.device.name != self.device {
            report.needs_restart.push("[device] name".into());
        }
        if new.device.listen_port != self.listen_port {
            report.needs_restart.push("[device] listen_port".into());
        }
        let forwards = new.forwards.clone();
        let default_peer = new.device.default_peer.clone();
        *self.cfg.write().unwrap_or_else(|e| e.into_inner()) = new;
        self.sync_forwards(forwards, default_peer);
        for p in started {
            info!(peer = %p.name, "peer added");
            tokio::spawn(supervise(self.ep.clone(), self.clone(), p));
        }
        report
    }

    /// Start/stop `[forwards.*]` listeners to match the config (unchanged ones keep running).
    fn sync_forwards(
        self: &Arc<Self>,
        want: BTreeMap<String, ForwardConfig>,
        default_peer: Option<String>,
    ) {
        let mut running = self.forwards.lock().unwrap_or_else(|e| e.into_inner());
        running.retain(|name, (cfg, task)| {
            let keep = want.get(name) == Some(cfg);
            if !keep {
                info!("forward {name}: stopped (config changed)");
                task.abort();
            }
            keep
        });
        for (name, f) in want {
            if running.contains_key(&name) {
                continue;
            }
            let Some(device) = f.peer.clone().or_else(|| default_peer.clone()) else {
                continue; // rejected by Config::validate
            };
            let task = tokio::spawn(crate::forward::run_configured(
                self.clone(),
                name.clone(),
                device,
                f.clone(),
            ));
            running.insert(name, (f, task.abort_handle()));
        }
    }

    /// Checks that only make sense for the running daemon.
    pub(crate) fn check_config(&self, cfg: &Config) -> Result<()> {
        if cfg.peers.values().any(|p| p.id == self.id) {
            bail!("a configured peer has this device's own id");
        }
        Ok(())
    }

    /// `enishi reload`: re-read the config file and apply it.
    async fn reload(self: &Arc<Self>) -> IpcResponse {
        let _edit = self.cfg_edit.lock().await;
        let cfg = match Config::load(&self.paths.config_file)
            .and_then(|c| self.check_config(&c).map(|_| c))
        {
            Ok(c) => c,
            Err(e) => {
                warn!("reload refused, keeping the running config: {e:#}");
                return IpcResponse::Error {
                    code: exit::USAGE,
                    message: format!("config not reloaded (running config unchanged): {e:#}"),
                };
            }
        };
        let report = self.apply_config(cfg);
        info!(?report, "config reloaded");
        IpcResponse::Reloaded(report)
    }

    /// `enishi services`: ask the peer which services it publishes to us.
    pub(crate) async fn list_services(&self, device: Option<String>) -> IpcResponse {
        match self.ask_peer(device, CtlRequest::Services).await {
            Ok((peer, CtlResponse::Services(services))) => IpcResponse::Services {
                device: peer,
                services,
            },
            Ok((peer, other)) => IpcResponse::Error {
                code: exit::PEER_OFFLINE,
                message: format!("{peer}: unexpected reply {other:?}"),
            },
            Err(e) => e,
        }
    }

    /// Details of one service the peer publishes to us (`None` from peers older than
    /// 0.1.0, which do not know the request).
    pub(crate) async fn service_details(
        &self,
        device: Option<String>,
        service: String,
    ) -> IpcResponse {
        match self
            .ask_peer(device, CtlRequest::ServiceDetails { name: service })
            .await
        {
            Ok((_, CtlResponse::ServiceDetails(d))) => IpcResponse::ServiceDetails(d),
            Ok((peer, other)) => IpcResponse::Error {
                code: exit::PEER_OFFLINE,
                message: format!("{peer}: unexpected reply {other:?}"),
            },
            Err(e) => e,
        }
    }

    /// One ctl request to the target peer while it is online, bounded by the ack timeout.
    /// Returns the peer's local name with the answer, or the IPC error to report.
    async fn ask_peer(
        &self,
        device: Option<String>,
        req: CtlRequest,
    ) -> Result<(String, CtlResponse), IpcResponse> {
        let peer = match self.target(device.as_deref()) {
            Ok(p) => p,
            Err(message) => {
                return Err(IpcResponse::Error {
                    code: exit::USAGE,
                    message,
                });
            }
        };
        let conn = {
            let s = peer.state();
            match (&s.ctl, s.presence) {
                (Some(c), Presence::Online) => Some(c.clone()),
                _ => None,
            }
        };
        let error = |message: String| IpcResponse::Error {
            code: exit::PEER_OFFLINE,
            message,
        };
        let Some(conn) = conn else {
            return Err(error(format!("peer {} is offline", peer.name)));
        };
        match tokio::time::timeout(session::ACK_TIMEOUT, ctl_request(&conn, &req)).await {
            Ok(Ok(CtlResponse::Error { message })) => {
                Err(error(format!("{}: {message}", peer.name)))
            }
            Ok(Ok(resp)) => Ok((peer.name.clone(), resp)),
            Ok(Err(e)) => Err(error(format!(
                "{}: request failed (peer too old?): {e:#}",
                peer.name
            ))),
            Err(_) => Err(error(format!(
                "{}: no answer within {:?}",
                peer.name,
                session::ACK_TIMEOUT
            ))),
        }
    }

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
                let peers = self.peers();
                if let Some(d) = &device
                    && !peers.iter().any(|p| &p.name == d)
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
                    peers: peers
                        .iter()
                        .filter(|p| device.as_ref().is_none_or(|d| &p.name == d))
                        .map(|p| p.status(now))
                        .collect(),
                })
            }
            IpcRequest::PairInvite { name, ttl_secs } => self.pair_invite(name, ttl_secs),
            IpcRequest::Exec { .. }
            | IpcRequest::Run { .. }
            | IpcRequest::Open { .. }
            | IpcRequest::Forward { .. }
            | IpcRequest::Services { .. }
            | IpcRequest::ServiceDetails { .. }
            | IpcRequest::PairJoin { .. }
            | IpcRequest::Reload => IpcResponse::Error {
                code: exit::USAGE,
                message: "request needs the async IPC path".into(),
            },
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

/// Admission state read by the handshake hook.
#[derive(Debug)]
pub(crate) struct Gate {
    /// Ids of the configured peers.
    allowed: RwLock<HashSet<EndpointId>>,
    pub(crate) invites: Mutex<InviteBook>,
    log: RateLimit<EndpointId>,
}

impl Gate {
    pub(crate) fn invites(&self) -> std::sync::MutexGuard<'_, InviteBook> {
        self.invites.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Right after the TLS handshake: rejects every peer that is not in the config, except
/// on `enishi/pair/1`, which unknown devices may use while this daemon has an invite
/// out (the secret is checked on the stream). Our own outgoing pairing dials pass.
#[derive(Debug)]
struct AllowList(Arc<Gate>);

impl EndpointHooks for AllowList {
    async fn after_handshake<'a>(&'a self, conn: &'a Connection) -> AfterHandshakeOutcome {
        let id = conn.remote_id();
        if conn.alpn() == ALPN_PAIR {
            if conn.side() == Side::Client
                || self.0.invites().any(Instant::now(), SystemTime::now())
            {
                return AfterHandshakeOutcome::Accept;
            }
            debug!(peer = %id, "rejected pairing connection: no invite out");
            return AfterHandshakeOutcome::Reject {
                error_code: CLOSE_REJECTED.into(),
                reason: b"no pending invite".to_vec(),
            };
        }
        let known = self
            .0
            .allowed
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .contains(&id);
        if known {
            AfterHandshakeOutcome::Accept
        } else {
            if self.0.log.allow(id, Instant::now()) {
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
    if cfg.peers.values().any(|p| p.id == id) {
        bail!("a configured peer has this device's own id");
    }

    let gate = Arc::new(Gate {
        allowed: RwLock::new(HashSet::new()),
        invites: Mutex::new(InviteBook::default()),
        log: RateLimit::new(REJECT_LOG_EVERY),
    });
    let mut builder = Endpoint::builder(presets::N0)
        .secret_key(key)
        .alpns(vec![
            ALPN_CTL.to_vec(),
            ALPN_STREAM.to_vec(),
            ALPN_PAIR.to_vec(),
        ])
        .hooks(AllowList(gate.clone()));
    if let Some(port) = cfg.device.listen_port {
        builder = builder.bind_addr(SocketAddr::from(([0, 0, 0, 0], port)))?;
    }
    let ep = builder.bind().await.context("binding iroh endpoint")?;

    let shared = Arc::new(Shared {
        device: cfg.device.name.clone(),
        id,
        instance,
        listen_port: cfg.device.listen_port,
        ep: ep.clone(),
        audit: Audit::new(paths.dir.join(AUDIT_FILE)),
        paths,
        peers: RwLock::new(Vec::new()),
        cfg: RwLock::new(Config {
            peers: Default::default(),
            ..cfg.clone()
        }),
        gate,
        cfg_edit: tokio::sync::Mutex::new(()),
        forwards: Mutex::new(HashMap::new()),
        incoming: Mutex::new(HashMap::new()),
    });
    let n_peers = cfg.peers.len();
    // Starts a supervisor per peer and fills the allow-list.
    shared.apply_config(cfg);
    info!(
        device = %shared.device,
        id = %id,
        port = ?shared.listen_port,
        ipc = %listener.display(),
        peers = n_peers,
        "daemon up"
    );

    tokio::spawn(accept_loop(ep.clone(), shared.clone()));
    tokio::spawn(watch_clock(shared.clone()));

    let ipc_shared = shared.clone();
    let ipc_task = async move {
        loop {
            match listener.accept().await {
                Ok(stream) => {
                    let sh = ipc_shared.clone();
                    tokio::spawn(async move {
                        if let Err(e) = serve_ipc(sh, stream).await {
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
    let t = Instant::now();
    shared.close_all(CLOSE_SHUTDOWN, b"shutdown");
    // Closing sends CONNECTION_CLOSE at once, so peers see us offline immediately; the
    // endpoint's close then waits for every connection to drain, which is bounded here.
    match tokio::time::timeout(SHUTDOWN_DRAIN, ep.close()).await {
        Ok(()) => info!("stopped ({} ms)", t.elapsed().as_millis()),
        Err(_) => {
            info!(
                "stopped ({} ms; connections closed, not waiting for them to drain)",
                t.elapsed().as_millis()
            );
            // The process is exiting: dropping the half-closed endpoint would only log
            // "Endpoint dropped without calling `Endpoint::close`" while aborting it.
            std::mem::forget(ep);
        }
    }
    Ok(())
}

/// One CLI connection: a single request, or an exec session that streams.
async fn serve_ipc<S>(sh: Arc<Shared>, mut stream: S) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let req: IpcRequest =
        tokio::time::timeout(ipc::CLIENT_TIMEOUT, wire::read_frame(&mut stream)).await??;
    match req {
        IpcRequest::Exec { device, request } => {
            session::client(sh, stream, device, SessionKind::Exec(request)).await
        }
        IpcRequest::Run {
            device,
            service,
            args,
        } => {
            let header = SessionKind::Service {
                name: service,
                args,
            };
            session::client(sh, stream, device, header).await
        }
        IpcRequest::Open { device, header } => session::client(sh, stream, device, header).await,
        IpcRequest::Forward {
            device,
            service,
            bind,
            port,
        } => crate::forward::serve_cli(sh, stream, device, service, bind, port).await,
        IpcRequest::Services { device } => {
            let resp = sh.list_services(device).await;
            wire::write_frame(&mut stream, &resp).await
        }
        IpcRequest::ServiceDetails { device, service } => {
            let resp = sh.service_details(device, service).await;
            wire::write_frame(&mut stream, &resp).await
        }
        IpcRequest::PairJoin { code, name } => {
            let resp = sh.pair_join(code, name).await;
            wire::write_frame(&mut stream, &resp).await
        }
        IpcRequest::Reload => {
            let resp = sh.reload().await;
            wire::write_frame(&mut stream, &resp).await
        }
        other => {
            let resp = sh.handle_ipc(other);
            wire::write_frame(&mut stream, &resp).await
        }
    }
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
        // A windowless daemon (the logon task's GUI-subsystem copy) has no console: if the
        // handler cannot be installed, run until killed instead of shutting down at once.
        if tokio::signal::ctrl_c().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

/// One request/response exchange on a fresh bi-stream.
async fn ctl_request(conn: &Connection, req: &CtlRequest) -> Result<CtlResponse> {
    let (mut send, mut recv) = conn.open_bi().await?;
    wire::write_frame(&mut send, req).await?;
    send.finish()?;
    wire::read_frame(&mut recv).await
}

/// Connect ctl (+ hello) and the warm stream connection.
async fn dial(
    ep: &Endpoint,
    sh: &Shared,
    peer: &Peer,
    resume: bool,
) -> Result<(Connection, Connection, Hello)> {
    let conn = ep.connect(peer.addr.clone(), ALPN_CTL).await?;
    let hello = match ctl_request(&conn, &CtlRequest::Hello(sh.hello(resume))).await? {
        CtlResponse::Hello(h) => h,
        CtlResponse::Error { message } => bail!("peer refused hello: {message}"),
        other => bail!("unexpected hello reply {other:?}"),
    };
    let stream = ep
        .connect(peer.addr.clone(), ALPN_STREAM)
        .await
        .context("stream connection")?;
    Ok((conn, stream, hello))
}

async fn supervise(ep: Endpoint, sh: Arc<Shared>, peer: Arc<Peer>) {
    let mut backoff = BACKOFF_MIN;
    let mut last_err = String::new();
    loop {
        if peer.is_retired() {
            debug!(peer = %peer.name, "supervisor exits (peer retired)");
            return;
        }
        let resume = peer.resume.swap(false, Ordering::SeqCst);
        let t = Instant::now();
        let dialed = tokio::select! {
            r = tokio::time::timeout(CONNECT_TIMEOUT, dial(&ep, &sh, &peer, resume)) => r,
            _ = peer.until_retired() => continue,
        };
        let err = match dialed {
            Ok(Ok((conn, stream, hello))) => {
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
                    s.stream = Some(stream.clone());
                    s.ctl = Some(conn.clone());
                }
                info!(
                    peer = %peer.name,
                    remote_device = %hello.device,
                    remote_version = %hello.version,
                    resume,
                    "ONLINE (connect {} ms)",
                    t.elapsed().as_millis()
                );
                let (why, code) = tokio::select! {
                    why = heartbeat(&conn, &stream, &peer) => (why, CLOSE_RESET),
                    _ = peer.until_retired() => ("removed from config".to_string(), CLOSE_REMOVED),
                };
                conn.close(code.into(), b"reset");
                // Sessions in flight on a dead link fail now instead of hanging.
                stream.close(code.into(), b"reset");
                {
                    let mut s = peer.state();
                    s.stream = None;
                    s.ctl = None;
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
            _ = peer.until_retired() => continue,
        }
        backoff = (backoff * 2).min(BACKOFF_MAX);
    }
}

/// Returns why the connection is considered lost.
async fn heartbeat(conn: &Connection, stream: &Connection, peer: &Peer) -> String {
    let mut seq: u64 = 0;
    loop {
        tokio::select! {
            e = conn.closed() => return format!("connection closed: {e}"),
            e = stream.closed() => return format!("stream connection closed: {e}"),
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
            let alpn = conn.alpn().to_vec();
            if alpn == ALPN_PAIR {
                pairing::serve_pair(conn, sh).await;
                return;
            }
            // The handshake hook already rejects unknown ids; double-check here.
            let Some(peer) = sh.peer_by_id(&id) else {
                conn.close(CLOSE_REJECTED.into(), b"unknown peer");
                return;
            };
            let key = conn.stable_id();
            sh.lock_incoming().insert(key, conn.clone());
            let serving = sh.clone();
            let serve = async {
                let sh = serving;
                if alpn == ALPN_CTL {
                    debug!(peer = %peer.name, "incoming ctl connection");
                    serve_ctl(conn.clone(), sh, peer.clone()).await;
                } else if alpn == ALPN_STREAM {
                    debug!(peer = %peer.name, "incoming stream connection");
                    session::serve_streams(conn.clone(), sh, peer.clone()).await;
                } else {
                    conn.close(CLOSE_REJECTED.into(), b"unsupported alpn");
                }
            };
            tokio::select! {
                _ = serve => {}
                // Unpaired/replaced while connected: drop its links (and sessions).
                _ = peer.until_retired() => conn.close(CLOSE_REMOVED.into(), b"removed"),
            }
            sh.lock_incoming().remove(&key);
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
                    CtlRequest::Services => {
                        CtlResponse::Services(visible_services(&sh.cfg(), &peer.name))
                    }
                    CtlRequest::ServiceDetails { name } => {
                        CtlResponse::ServiceDetails(service_details(&sh.cfg(), &peer.name, &name))
                    }
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
            for p in &sh.peers() {
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

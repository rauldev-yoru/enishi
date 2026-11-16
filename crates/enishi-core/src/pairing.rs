//! `enishi pair`: the daemon side of design §6.
//!
//! Inviter (A): `pair invite` registers a one-time secret in the daemon's [`InviteBook`]
//! and returns an [`InviteCode`]. While an invite is out, A's handshake hook lets unknown
//! devices open `enishi/pair/1`; [`serve_pair`] reads one [`PairRequest`], checks the
//! secret (constant time, unexpired, unused), stores the joiner as a peer and answers
//! with A's device name and address hints.
//!
//! Joiner (B): `pair join <code>` makes B's daemon dial A on `enishi/pair/1` with the
//! code's address hints, send the secret, and on success store A as a peer.
//!
//! Both sides append `[peers.<name>]` to their own `enishi.toml` (comments kept), set
//! `default_peer` if it was unset, and hot-apply the new peer set. Nothing is granted.

use std::{
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant, SystemTime},
};

use anyhow::{Result, anyhow};
use enishi_proto::{
    ALPN_PAIR, exit,
    ipc::{IpcResponse, Paired},
    pair::{MAX_ADDRS, PairRequest, PairResponse},
};
use iroh::{EndpointAddr, EndpointId, SecretKey, endpoint::Connection};
use tracing::{info, warn};

use crate::{
    config::{self, Config, check_name},
    daemon::{CLOSE_REJECTED, Shared},
    pair::{InviteCode, clean_device, sanitize_name},
    wire,
};

/// Bound for the whole pairing exchange (dial + request + response).
pub const PAIR_TIMEOUT: Duration = Duration::from_secs(10);
const CLOSE_DONE: u32 = 0;

fn usage(message: impl Into<String>) -> IpcResponse {
    IpcResponse::Error {
        code: exit::USAGE,
        message: message.into(),
    }
}

impl Shared {
    /// `enishi pair invite`.
    pub(crate) fn pair_invite(&self, name: Option<String>, ttl_secs: u64) -> IpcResponse {
        let ttl = Duration::from_secs(ttl_secs);
        if ttl.is_zero() || ttl > crate::pair::MAX_TTL {
            return usage("--ttl must be between 1s and 24h");
        }
        if let Some(n) = &name {
            if let Err(e) = check_name("peer", n) {
                return usage(format!("{e:#}"));
            }
            if self.cfg().peers.contains_key(n) {
                return usage(format!(
                    "a peer named {n:?} already exists; pick another --name"
                ));
            }
        }
        let secret: [u8; 16] = SecretKey::generate().to_bytes()[..16]
            .try_into()
            .expect("16 bytes");
        self.gate
            .invites()
            .create(secret, name.clone(), ttl, Instant::now(), SystemTime::now());
        let addrs = self.current_addrs();
        if addrs.is_empty() {
            warn!("pairing invite has no direct address hints; the joiner relies on discovery");
        }
        let code = InviteCode {
            id: *self.id.as_bytes(),
            secret,
            device: self.device.clone(),
            addrs: addrs.clone(),
        }
        .encode();
        // Never log the code: it carries the secret.
        info!(
            name = ?name,
            ttl_s = ttl_secs,
            hints = addrs.len(),
            "pairing invite created"
        );
        IpcResponse::Invite {
            code,
            ttl_secs,
            addrs: addrs.iter().map(|a| a.to_string()).collect(),
        }
    }

    /// `enishi pair join`: dial the inviter and, if it accepts, store it as a peer.
    pub(crate) async fn pair_join(
        self: &Arc<Self>,
        code: String,
        name: Option<String>,
    ) -> IpcResponse {
        let inv = match InviteCode::decode(&code) {
            Ok(i) => i,
            Err(e) => return usage(format!("{e:#}")),
        };
        let their_id = inv.endpoint_id().expect("checked by decode");
        let their_device = clean_device(&inv.device);
        let _edit = self.cfg_edit.lock().await;
        if their_id == self.id {
            return usage(
                "this invite was created by this device; run `pair join` on the other one",
            );
        }
        if let Some(p) = self.peer_by_id(&their_id) {
            return usage(format!(
                "already paired with {their_device:?} (it is peer {:?} here)",
                p.name
            ));
        }
        let name = name.unwrap_or_else(|| sanitize_name(&their_device));
        if let Err(e) = check_name("peer", &name) {
            return usage(format!("{e:#}"));
        }
        if self.cfg().peers.contains_key(&name) {
            return usage(format!(
                "a peer named {name:?} already exists here; pass --name <free-name>"
            ));
        }

        let mut addr = EndpointAddr::new(their_id);
        for a in inv.addrs.iter().take(MAX_ADDRS) {
            addr = addr.with_ip_addr(*a);
        }
        let req = PairRequest {
            secret: inv.secret,
            device: self.device.clone(),
            addrs: self.stable_addrs(),
        };
        let t = Instant::now();
        let exchange = async {
            let conn = self.ep.connect(addr, ALPN_PAIR).await?;
            let (mut send, mut recv) = conn.open_bi().await?;
            wire::write_frame(&mut send, &req).await?;
            send.finish()?;
            let resp: PairResponse = wire::read_frame(&mut recv).await?;
            conn.close(CLOSE_DONE.into(), b"done");
            anyhow::Ok(resp)
        };
        let resp = match tokio::time::timeout(PAIR_TIMEOUT, exchange).await {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => {
                let e = format!("{e:#}");
                let refused = e.contains("no pending invite");
                let message = if refused {
                    format!(
                        "{their_device} has no open invite (expired long ago, or its daemon \
                         restarted) — create a new one with `enishi pair invite`"
                    )
                } else {
                    format!("could not pair with {their_device}: {e}")
                };
                warn!("pair join failed: {message}");
                return IpcResponse::Error {
                    code: if refused {
                        exit::NOT_ALLOWED
                    } else {
                        exit::PEER_OFFLINE
                    },
                    message,
                };
            }
            Err(_) => {
                let hints: Vec<String> = inv.addrs.iter().map(|a| a.to_string()).collect();
                return IpcResponse::Error {
                    code: exit::PEER_OFFLINE,
                    message: format!(
                        "no answer from {their_device} within {}s (is its daemon running? \
                         hints: {})",
                        PAIR_TIMEOUT.as_secs(),
                        if hints.is_empty() {
                            "none".into()
                        } else {
                            hints.join(", ")
                        }
                    ),
                };
            }
        };
        let their_addrs = match resp {
            PairResponse::Ok { device: _, addrs } => {
                addrs.into_iter().take(MAX_ADDRS).collect::<Vec<_>>()
            }
            PairResponse::Err { message } => {
                warn!("pairing refused by {their_device}: {message}");
                return IpcResponse::Error {
                    code: exit::NOT_ALLOWED,
                    message: format!("{their_device} refused: {message}"),
                };
            }
        };
        match self.store_peer(&name, their_id, &their_addrs) {
            Ok(default_peer) => {
                info!(
                    peer = %name,
                    device = %their_device,
                    id = %their_id,
                    "PAIRED (joined, {} ms)",
                    t.elapsed().as_millis()
                );
                IpcResponse::Paired(Paired {
                    name,
                    device: their_device,
                    id: their_id.to_string(),
                    default_peer,
                    config: self.paths.config_file.display().to_string(),
                })
            }
            Err(e) => IpcResponse::Error {
                code: 1,
                message: format!(
                    "{their_device} accepted the pairing but saving it here failed: {e:#}. \
                     Add it by hand to {}:\n[peers.{name}]\nid = \"{their_id}\"",
                    self.paths.config_file.display()
                ),
            },
        }
    }

    /// Inviter side: validate the request and store the joiner.
    async fn accept_pair(self: &Arc<Self>, id: EndpointId, req: PairRequest) -> PairResponse {
        let refuse = |message: String| PairResponse::Err { message };
        let _edit = self.cfg_edit.lock().await;
        let checked = self
            .gate
            .invites()
            .check(&req.secret, Instant::now(), SystemTime::now());
        let (seq, reserved) = match checked {
            Ok(x) => x,
            Err(e) => {
                warn!(peer = %id, "pairing attempt rejected: {e}");
                return refuse(e.to_string());
            }
        };
        let device = clean_device(&req.device);
        if id == self.id {
            return refuse("cannot pair a device with itself".into());
        }
        if let Some(p) = self.peer_by_id(&id) {
            return refuse(format!(
                "{} already has this device as peer {:?}; remove it there first",
                self.device, p.name
            ));
        }
        let name = reserved.unwrap_or_else(|| sanitize_name(&device));
        if self.cfg().peers.contains_key(&name) {
            return refuse(format!(
                "{} already has a peer named {name:?}; create the invite with \
                 `enishi pair invite --name <free-name>`",
                self.device
            ));
        }
        let addrs: Vec<SocketAddr> = req.addrs.into_iter().take(MAX_ADDRS).collect();
        if let Err(e) = self.store_peer(&name, id, &addrs) {
            warn!(peer = %id, "pairing failed saving the config: {e:#}");
            return refuse(format!("{} could not save its config", self.device));
        }
        self.gate.invites().consume(seq);
        info!(peer = %name, device = %device, id = %id, "PAIRED (invite used)");
        PairResponse::Ok {
            device: self.device.clone(),
            addrs: self.stable_addrs(),
        }
    }

    /// Append the peer to our config file and apply it. Caller holds `cfg_edit`.
    /// Returns whether `default_peer` was set to it.
    fn store_peer(
        self: &Arc<Self>,
        name: &str,
        id: EndpointId,
        addrs: &[SocketAddr],
    ) -> Result<bool> {
        let path = &self.paths.config_file;
        let text = std::fs::read_to_string(path)
            .map_err(|e| anyhow!("reading {}: {e}", path.display()))?;
        let (new_text, default_set) = config::add_peer_to_toml(&text, name, &id, addrs)?;
        let cfg: Config = new_text.parse()?;
        self.check_config(&cfg)?;
        config::write_atomic(path, &new_text)?;
        let report = self.apply_config(cfg);
        if report.removed.len() + report.changed.len() > 0 || report.added.len() > 1 {
            // The file had other edits not yet reloaded; they are live now too.
            info!(?report, "pairing also applied pending config edits");
        }
        Ok(default_set)
    }
}

/// One incoming `enishi/pair/1` connection (inviter side).
pub(crate) async fn serve_pair(conn: Connection, sh: Arc<Shared>) {
    let id = conn.remote_id();
    let exchange = async {
        let (mut send, mut recv) = conn.accept_bi().await?;
        let req: PairRequest = wire::read_frame(&mut recv).await?;
        let resp = sh.accept_pair(id, req).await;
        wire::write_frame(&mut send, &resp).await?;
        send.finish()?;
        anyhow::Ok(resp)
    };
    match tokio::time::timeout(PAIR_TIMEOUT, exchange).await {
        Ok(Ok(_)) => {
            // The joiner closes once it has read the answer.
            let _ = tokio::time::timeout(Duration::from_secs(3), conn.closed()).await;
        }
        Ok(Err(e)) => {
            info!(peer = %id, "pairing connection failed: {e:#}");
            conn.close(CLOSE_REJECTED.into(), b"pairing failed");
        }
        Err(_) => {
            info!(peer = %id, "pairing connection timed out");
            conn.close(CLOSE_REJECTED.into(), b"timeout");
        }
    }
}

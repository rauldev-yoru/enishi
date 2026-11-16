//! Pairing building blocks (design §6): the invite code, the daemon's book of one-time
//! invites, and helpers for names and address hints. The network exchange itself
//! (`enishi/pair/1`) lives in [`crate::daemon`].
//!
//! Invite code: `enishi1-` + lowercase base32 (no padding) of
//! `postcard(InviteCode) ‖ fnv1a32(postcard bytes)`. The checksum only catches copy/paste
//! damage; authenticity comes from the inviter's `EndpointId` (TLS) and the secret.

use std::{
    net::{IpAddr, SocketAddr},
    time::{Duration, Instant, SystemTime},
};

use anyhow::{Context, Result, bail};
use enishi_proto::pair::{MAX_ADDRS, MAX_DEVICE_NAME, SECRET_LEN};
use iroh::EndpointId;
use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq;

/// Prefix of every invite code; the digit is the code format version.
pub const CODE_PREFIX: &str = "enishi1-";
pub const DEFAULT_TTL: Duration = Duration::from_secs(10 * 60);
pub const MAX_TTL: Duration = Duration::from_secs(24 * 3600);
/// Used and expired invites are remembered this long, to answer "already used" /
/// "expired" instead of "unknown".
const KEEP_AFTER_EXPIRY: Duration = Duration::from_secs(3600);

/// What `enishi pair invite` prints, encoded.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InviteCode {
    /// The inviter's `EndpointId`.
    pub id: [u8; 32],
    pub secret: [u8; SECRET_LEN],
    /// The inviter's `[device] name` (the joiner's default name for it).
    pub device: String,
    /// Where to dial the inviter (its current direct addresses).
    pub addrs: Vec<SocketAddr>,
}

impl InviteCode {
    pub fn encode(&self) -> String {
        let mut bytes = postcard::to_stdvec(self).expect("invite serializes");
        let sum = fnv1a32(&bytes);
        bytes.extend_from_slice(&sum.to_be_bytes());
        format!(
            "{CODE_PREFIX}{}",
            data_encoding::BASE32_NOPAD
                .encode(&bytes)
                .to_ascii_lowercase()
        )
    }

    /// Accepts surrounding/embedded whitespace (wrapped terminals) and any letter case.
    pub fn decode(code: &str) -> Result<InviteCode> {
        let compact: String = code.chars().filter(|c| !c.is_whitespace()).collect();
        let Some(body) = compact
            .get(..CODE_PREFIX.len())
            .filter(|p| p.eq_ignore_ascii_case(CODE_PREFIX))
            .map(|_| &compact[CODE_PREFIX.len()..])
        else {
            bail!("not an enishi invite code (expected it to start with {CODE_PREFIX:?})");
        };
        let bytes = data_encoding::BASE32_NOPAD
            .decode(body.to_ascii_uppercase().as_bytes())
            .map_err(|_| anyhow::anyhow!("invite code is damaged (not valid base32)"))?;
        if bytes.len() < 4 {
            bail!("invite code is too short");
        }
        let (payload, sum) = bytes.split_at(bytes.len() - 4);
        if fnv1a32(payload).to_be_bytes() != sum {
            bail!("invite code is damaged (checksum mismatch) — copy it again");
        }
        let inv: InviteCode =
            postcard::from_bytes(payload).context("invite code has an unknown layout")?;
        inv.endpoint_id()?;
        Ok(inv)
    }

    pub fn endpoint_id(&self) -> Result<EndpointId> {
        EndpointId::from_bytes(&self.id).context("invite code holds an invalid device id")
    }
}

fn fnv1a32(bytes: &[u8]) -> u32 {
    let mut h: u32 = 0x811c9dc5;
    for b in bytes {
        h ^= u32::from(*b);
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

/// Why an invite secret was not accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InviteError {
    Unknown,
    Expired,
    Used,
}

impl std::fmt::Display for InviteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            InviteError::Unknown => {
                "unknown invite (wrong code, or the inviting daemon restarted since)"
            }
            InviteError::Expired => "invite expired — create a new one with `enishi pair invite`",
            InviteError::Used => "invite already used — create a new one with `enishi pair invite`",
        })
    }
}

struct Invite {
    seq: u64,
    secret: [u8; SECRET_LEN],
    name: Option<String>,
    created: Instant,
    created_wall: SystemTime,
    ttl: Duration,
    used: bool,
}

impl Invite {
    /// Age by the monotonic clock or the wall clock, whichever is larger: the monotonic
    /// clock may stop while the machine sleeps, the wall clock keeps going.
    fn age(&self, now: Instant, wall: SystemTime) -> Duration {
        let mono = now.saturating_duration_since(self.created);
        let wall = wall.duration_since(self.created_wall).unwrap_or_default();
        mono.max(wall)
    }
}

/// The daemon's one-time invites (memory only: a restart forgets them).
#[derive(Default)]
pub struct InviteBook {
    invites: Vec<Invite>,
    next: u64,
}

impl std::fmt::Debug for InviteBook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print secrets.
        write!(f, "InviteBook({} invites)", self.invites.len())
    }
}

impl InviteBook {
    /// Register a new invite; `name` is what this device will call the joiner.
    pub fn create(
        &mut self,
        secret: [u8; SECRET_LEN],
        name: Option<String>,
        ttl: Duration,
        now: Instant,
        wall: SystemTime,
    ) -> u64 {
        self.prune(now, wall);
        self.next += 1;
        self.invites.push(Invite {
            seq: self.next,
            secret,
            name,
            created: now,
            created_wall: wall,
            ttl,
            used: false,
        });
        self.next
    }

    /// Find the invite for `secret` (constant-time comparison against every invite).
    /// Returns its sequence number and the name reserved for the joiner.
    pub fn check(
        &self,
        secret: &[u8; SECRET_LEN],
        now: Instant,
        wall: SystemTime,
    ) -> Result<(u64, Option<String>), InviteError> {
        let mut found = None;
        for inv in &self.invites {
            if bool::from(inv.secret.ct_eq(secret)) {
                found = Some(inv);
            }
        }
        let inv = found.ok_or(InviteError::Unknown)?;
        if inv.used {
            Err(InviteError::Used)
        } else if inv.age(now, wall) >= inv.ttl {
            Err(InviteError::Expired)
        } else {
            Ok((inv.seq, inv.name.clone()))
        }
    }

    pub fn consume(&mut self, seq: u64) {
        if let Some(inv) = self.invites.iter_mut().find(|i| i.seq == seq) {
            inv.used = true;
        }
    }

    /// Whether pairing connections should be accepted at all: an invite is open, or one
    /// ended recently enough to explain why it can't be used.
    pub fn any(&self, now: Instant, wall: SystemTime) -> bool {
        self.invites.iter().any(|i| !Self::stale(i, now, wall))
    }

    fn stale(i: &Invite, now: Instant, wall: SystemTime) -> bool {
        i.age(now, wall) >= i.ttl + KEEP_AFTER_EXPIRY
    }

    fn prune(&mut self, now: Instant, wall: SystemTime) {
        self.invites.retain(|i| !Self::stale(i, now, wall));
    }
}

/// `10m`, `2s`, `1h`, or plain seconds.
pub fn parse_ttl(s: &str) -> Result<Duration, String> {
    let s = s.trim();
    let (num, mult) = match s.char_indices().last() {
        Some((i, 's')) => (&s[..i], 1),
        Some((i, 'm')) => (&s[..i], 60),
        Some((i, 'h')) => (&s[..i], 3600),
        _ => (s, 1),
    };
    let n: u64 = num
        .trim()
        .parse()
        .map_err(|_| format!("invalid duration {s:?} (examples: 90s, 10m, 1h)"))?;
    let d = Duration::from_secs(n.saturating_mul(mult));
    if d.is_zero() || d > MAX_TTL {
        return Err(format!(
            "duration must be between 1s and {}h",
            MAX_TTL.as_secs() / 3600
        ));
    }
    Ok(d)
}

/// Default peer name derived from a device name: allowed characters only.
pub fn sanitize_name(device: &str) -> String {
    let s: String = device
        .chars()
        .take(MAX_DEVICE_NAME)
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let s = s.trim_matches('-');
    if s.is_empty() {
        "peer".into()
    } else {
        s.into()
    }
}

/// Untrusted text from the other side: bounded and single-line.
pub fn clean_device(device: &str) -> String {
    device
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_DEVICE_NAME)
        .collect()
}

/// Pick the address hints to publish, most useful first: Tailscale/CGNAT IPv4
/// (100.64/10), 192.168/16, 10/8, other IPv4, Tailscale IPv6, 172.16/12 (mostly
/// container bridges, useless off-host), other IPv6. Loopback, unspecified, link-local
/// and multicast addresses are dropped, 172.16/12 too when anything better exists; at
/// most [`MAX_ADDRS`] are kept.
pub fn hint_addrs(addrs: impl IntoIterator<Item = SocketAddr>) -> Vec<SocketAddr> {
    hint_addrs_by(addrs, |_| true)
}

/// The hints a device gives about itself (invites, pair requests/answers): only its own
/// interface addresses on one of `ports` (its fixed `listen_port`, or the sockets it is
/// bound to right now) in the ranges a peer can reach directly — Tailscale/CGNAT
/// 100.64/10, private IPv4 (10/8, 172.16/12, 192.168/16), IPv6 ULA (fc00::/7, Tailscale
/// included) and global unicast (2000::/3). Public IPv4 addresses are dropped: what iroh
/// reports there is a NAT mapping observed by relays (its own port, gone with the
/// mapping), useless as a static hint; other ports are other sockets or mappings too.
pub fn own_hint_addrs(
    addrs: impl IntoIterator<Item = SocketAddr>,
    ports: &[u16],
) -> Vec<SocketAddr> {
    hint_addrs_by(addrs, |a| ports.contains(&a.port()) && own_scope(a.ip()))
}

fn own_scope(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            v4.is_private() || (o[0] == 100 && (o[1] & 0xc0) == 64)
        }
        IpAddr::V6(v6) => {
            let s0 = v6.segments()[0];
            (s0 & 0xfe00) == 0xfc00 || (s0 & 0xe000) == 0x2000
        }
    }
}

fn hint_addrs_by(
    addrs: impl IntoIterator<Item = SocketAddr>,
    keep: impl Fn(&SocketAddr) -> bool,
) -> Vec<SocketAddr> {
    let mut v: Vec<(u8, SocketAddr)> = addrs
        .into_iter()
        .filter(|a| keep(a))
        .filter_map(|a| rank(a.ip()).map(|r| (r, a)))
        .collect();
    v.sort();
    v.dedup_by(|a, b| a.1 == b.1);
    if v.iter().any(|(r, _)| *r < BRIDGE_RANK) {
        v.retain(|(r, _)| *r != BRIDGE_RANK);
    }
    v.into_iter().map(|(_, a)| a).take(MAX_ADDRS).collect()
}

const BRIDGE_RANK: u8 = 5;

fn rank(ip: IpAddr) -> Option<u8> {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            if v4.is_loopback() || v4.is_unspecified() || v4.is_link_local() || v4.is_multicast() {
                None
            } else if o[0] == 100 && (o[1] & 0xc0) == 64 {
                Some(0)
            } else if o[0] == 192 && o[1] == 168 {
                Some(1)
            } else if o[0] == 10 {
                Some(2)
            } else if v4.is_private() {
                Some(BRIDGE_RANK)
            } else {
                Some(3)
            }
        }
        IpAddr::V6(v6) => {
            let s = v6.segments();
            let link_local = (s[0] & 0xffc0) == 0xfe80;
            if v6.is_loopback() || v6.is_unspecified() || v6.is_multicast() || link_local {
                None
            } else if s[0] == 0xfd7a && s[1] == 0x115c && s[2] == 0xa1e0 {
                Some(4)
            } else {
                Some(6)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use iroh::SecretKey;

    fn code() -> InviteCode {
        InviteCode {
            id: *SecretKey::from_bytes(&[3; 32]).public().as_bytes(),
            secret: [9; SECRET_LEN],
            device: "server".into(),
            addrs: vec![
                "100.71.1.2:7811".parse().unwrap(),
                "[fd7a:115c:a1e0::1]:7811".parse().unwrap(),
            ],
        }
    }

    #[test]
    fn code_roundtrip_and_tolerance() {
        let c = code();
        let s = c.encode();
        assert!(s.starts_with(CODE_PREFIX), "{s}");
        assert!(
            s[CODE_PREFIX.len()..]
                .chars()
                .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit())
        );
        assert_eq!(InviteCode::decode(&s).unwrap(), c);
        // Wrapped by a terminal, upper-cased, surrounded by spaces.
        let mangled = format!("  {}\n{}  ", s[..40].to_uppercase(), &s[40..]);
        assert_eq!(InviteCode::decode(&mangled).unwrap(), c);
        assert_eq!(
            InviteCode::decode(&s).unwrap().endpoint_id().unwrap(),
            SecretKey::from_bytes(&[3; 32]).public()
        );
    }

    #[test]
    fn damaged_codes_are_rejected() {
        let s = code().encode();
        let e = |x: &str| format!("{:#}", InviteCode::decode(x).unwrap_err());
        assert!(e("hello").contains("not an enishi invite"));
        assert!(e("enishi1-").contains("too short"));
        assert!(e("enishi1-!!!!").contains("base32"));
        // Flip one character in the middle.
        let mut b = s.clone().into_bytes();
        let i = b.len() / 2;
        b[i] = if b[i] == b'a' { b'b' } else { b'a' };
        assert!(e(std::str::from_utf8(&b).unwrap()).contains("checksum"));
        // Truncated.
        assert!(InviteCode::decode(&s[..s.len() - 3]).is_err());
    }

    #[test]
    fn invite_expiry_and_single_use() {
        let mut book = InviteBook::default();
        let t = Instant::now();
        let w = SystemTime::now();
        let ttl = Duration::from_secs(60);
        let a = book.create([1; SECRET_LEN], Some("pc".into()), ttl, t, w);
        let b = book.create([2; SECRET_LEN], None, ttl, t, w);
        assert_ne!(a, b);
        assert_eq!(
            book.check(&[1; SECRET_LEN], t, w).unwrap(),
            (a, Some("pc".into()))
        );
        assert_eq!(
            book.check(&[3; SECRET_LEN], t, w),
            Err(InviteError::Unknown)
        );
        book.consume(a);
        assert_eq!(book.check(&[1; SECRET_LEN], t, w), Err(InviteError::Used));
        // The other invite is unaffected.
        assert_eq!(book.check(&[2; SECRET_LEN], t, w).unwrap().0, b);
        // Expired by the monotonic clock…
        let later = t + Duration::from_secs(61);
        assert_eq!(
            book.check(&[2; SECRET_LEN], later, w),
            Err(InviteError::Expired)
        );
        // …or by the wall clock (the monotonic one may stop during sleep).
        assert_eq!(
            book.check(&[2; SECRET_LEN], t, w + Duration::from_secs(61)),
            Err(InviteError::Expired)
        );
        assert!(book.any(later, w), "recently expired still explained");
        let much_later = t + ttl + KEEP_AFTER_EXPIRY;
        assert!(!book.any(much_later, w));
        book.create([4; SECRET_LEN], None, ttl, much_later, w);
        assert_eq!(
            book.check(&[2; SECRET_LEN], much_later, w),
            Err(InviteError::Unknown),
            "stale invites are pruned"
        );
        assert!(
            !format!("{book:?}").contains('4'),
            "Debug never shows secrets"
        );
    }

    #[test]
    fn ttl_parsing() {
        assert_eq!(parse_ttl("10m").unwrap(), Duration::from_secs(600));
        assert_eq!(parse_ttl("2s").unwrap(), Duration::from_secs(2));
        assert_eq!(parse_ttl("90").unwrap(), Duration::from_secs(90));
        assert_eq!(parse_ttl("1h").unwrap(), Duration::from_secs(3600));
        assert!(parse_ttl("0s").is_err());
        assert!(parse_ttl("25h").is_err());
        assert!(parse_ttl("abc").is_err());
        assert!(parse_ttl("").is_err());
    }

    #[test]
    fn names_and_hints() {
        assert_eq!(sanitize_name("Raul PC"), "Raul-PC");
        assert_eq!(sanitize_name("  "), "peer");
        assert_eq!(sanitize_name("windows-home"), "windows-home");
        assert_eq!(clean_device("a\nb"), "ab");
        let hints = hint_addrs(
            [
                "127.0.0.1:1",
                "172.17.0.1:1",
                "[fe80::1]:1",
                "[2001:db8::1]:1",
                "192.168.1.5:1",
                "100.71.1.2:1",
                "0.0.0.0:1",
                "10.0.0.2:1",
                "[fd7a:115c:a1e0::5]:1",
                "100.71.1.2:1",
            ]
            .map(|s| s.parse().unwrap()),
        );
        let got: Vec<String> = hints.iter().map(|a| a.to_string()).collect();
        assert_eq!(
            got,
            [
                "100.71.1.2:1",
                "192.168.1.5:1",
                "10.0.0.2:1",
                "[fd7a:115c:a1e0::5]:1",
                "[2001:db8::1]:1"
            ]
        );
        // Container bridges only when there is nothing better.
        let only: Vec<SocketAddr> = vec!["172.17.0.1:1".parse().unwrap()];
        assert_eq!(hint_addrs(only.clone()), only);
    }

    #[test]
    fn own_hints_only_fixed_port_and_reachable_ranges() {
        // What iroh reported for a daemon with listen_port = 7811 behind a home NAT.
        let seen = [
            "100.83.136.2:7811",
            "192.168.1.42:7811",
            "190.237.34.66:53684", // public NAT mapping
            "190.237.34.66:7811",  // public, even on our port
            "[2001:1388:4a01:7997:3266:481e:8ed8:f6cf]:40660", // v6 socket, random port
            "[2001:1388:4a01:7997::5]:7811",
            "[fd7a:115c:a1e0::5]:7811",
            "[fd00:1::2]:7811",
            "10.0.0.2:7811",
            "172.17.0.1:7811", // bridge: dropped since better ones exist
            "10.0.0.9:9999",
            "127.0.0.1:7811",
            "[fe80::1]:7811",
            "8.8.8.8:7811",
        ]
        .map(|s| s.parse::<SocketAddr>().unwrap());
        let got: Vec<String> = own_hint_addrs(seen, &[7811])
            .iter()
            .map(|a| a.to_string())
            .collect();
        assert_eq!(
            got,
            [
                "100.83.136.2:7811",
                "192.168.1.42:7811",
                "10.0.0.2:7811",
                "[fd7a:115c:a1e0::5]:7811",
                "[2001:1388:4a01:7997::5]:7811",
                "[fd00:1::2]:7811",
            ]
        );
        // Several bound sockets (no fixed port): each one's own addresses.
        let got = own_hint_addrs(seen, &[9999, 40660]);
        assert_eq!(
            got,
            [
                "10.0.0.9:9999".parse::<SocketAddr>().unwrap(),
                "[2001:1388:4a01:7997:3266:481e:8ed8:f6cf]:40660"
                    .parse()
                    .unwrap(),
            ]
        );
        assert!(own_hint_addrs(seen, &[]).is_empty());
    }
}

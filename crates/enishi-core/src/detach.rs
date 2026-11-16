//! Resumable sessions (design D18), serving side: a `Service`/`Mcp` session whose link
//! is lost is *detached* instead of killed. Its process keeps running for the session's
//! `detach_ttl`; its output goes to a bounded replay buffer ([`Ring`]) numbered from 0;
//! its stdin is paused (nothing reads the dead stream). The caller reattaches with
//! `SessionKind::Resume { id, token, seq }` on a new stream: the [`Hub`] checks the token,
//! replays the output from `seq` and the same process continues.
//!
//! The session ends — process tree killed as before — when the caller goes away (stream
//! reset, connection closed for shutdown/removal), when the TTL expires while detached,
//! or when the buffer overflows while detached (the output is then incomplete: the
//! session is *lost*). A session whose process exited stays resumable until its last
//! frames were acknowledged or the TTL expires, so a caller that lost the link right at
//! the end still gets the output and the exit code.

use std::{
    collections::{HashMap, VecDeque},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use enishi_proto::{
    exit,
    session::{ExecFrame, SessionAck},
};
use enishi_svc::exec::{End, Outcome, Running};
use iroh::endpoint::{ConnectionError, ReadError, RecvStream, SendStream};
use subtle::ConstantTimeEq;
use tokio::{
    sync::mpsc,
    task::JoinHandle,
    time::{Instant, sleep_until},
};
use tracing::{debug, info};

use crate::wire;

/// Replay buffer bound per session (bytes of stdio payload).
pub const DEFAULT_BUFFER: usize = 8 << 20;
/// Testing knob: overrides [`DEFAULT_BUFFER`] (read when a session starts).
pub const BUFFER_ENV: &str = "ENISHI_DETACH_BUFFER_BYTES";
/// How often received input is acknowledged to the caller.
const INPUT_ACK_INTERVAL: Duration = Duration::from_millis(250);
/// Accounting overhead per buffered frame.
const FRAME_COST: usize = 16;

/// Connection close codes of the caller's daemon that mean "the caller is gone for good"
/// (shutdown, peer removed): the session is killed instead of detached.
const CALLER_GONE_CLOSE_CODES: [u32; 2] = [3, 4];
/// Stream error code of a deliberate caller exit (`session::STREAM_CLIENT_GONE`).
const RESET_CLIENT_GONE: u32 = 1;

pub(crate) fn buffer_cap() -> usize {
    std::env::var(BUFFER_ENV)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .filter(|&n: &usize| n > 0)
        .unwrap_or(DEFAULT_BUFFER)
}

fn cost(f: &ExecFrame) -> usize {
    FRAME_COST
        + match f {
            ExecFrame::Stdout(b) | ExecFrame::Stderr(b) => b.len(),
            ExecFrame::Error { message, .. } => message.len(),
            _ => 0,
        }
}

/// Output frames not yet acknowledged by the caller, numbered from 0 for the session.
#[derive(Debug)]
pub(crate) struct Ring {
    frames: VecDeque<ExecFrame>,
    /// Sequence number of `frames[0]`.
    first: u64,
    bytes: usize,
    cap: usize,
}

impl Ring {
    pub(crate) fn new(cap: usize) -> Ring {
        Ring {
            frames: VecDeque::new(),
            first: 0,
            bytes: 0,
            cap,
        }
    }

    /// Sequence number the next frame gets (= frames produced so far).
    pub(crate) fn next(&self) -> u64 {
        self.first + self.frames.len() as u64
    }

    pub(crate) fn push(&mut self, f: ExecFrame) {
        self.bytes += cost(&f);
        self.frames.push_back(f);
    }

    /// The caller has every frame before `seq`.
    pub(crate) fn ack(&mut self, seq: u64) {
        while self.first < seq {
            let Some(f) = self.frames.pop_front() else {
                break;
            };
            self.bytes -= cost(&f);
            self.first += 1;
        }
    }

    pub(crate) fn over(&self) -> bool {
        self.bytes > self.cap
    }

    /// While attached the frames were written already; when the caller is slow to
    /// acknowledge, forget the oldest instead of growing (a resume that needs them then
    /// fails and the caller falls back to a new session).
    pub(crate) fn trim(&mut self) {
        while self.over() && self.frames.len() > 1 {
            let f = self.frames.pop_front().expect("len > 1");
            self.bytes -= cost(&f);
            self.first += 1;
        }
    }

    /// Frames from `seq` on, if the buffer still holds all of them.
    pub(crate) fn from(&self, seq: u64) -> Option<impl Iterator<Item = &ExecFrame>> {
        (seq >= self.first && seq <= self.next())
            .then(|| self.frames.iter().skip((seq - self.first) as usize))
    }

    pub(crate) fn bytes(&self) -> usize {
        self.bytes
    }
}

/// A caller's request to reattach (or cancel) a detached session.
pub(crate) struct Attach {
    pub(crate) send: SendStream,
    pub(crate) recv: RecvStream,
    pub(crate) seq: u64,
    pub(crate) cancel: bool,
}

struct Slot {
    peer: String,
    token: u128,
    tx: mpsc::UnboundedSender<Attach>,
}

/// The resumable sessions this daemon serves, by id.
#[derive(Default)]
pub(crate) struct Registry {
    slots: Mutex<HashMap<u64, Slot>>,
}

impl Registry {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<u64, Slot>> {
        self.slots.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn insert(&self, id: u64, peer: &str, token: u128, tx: mpsc::UnboundedSender<Attach>) {
        self.lock().insert(
            id,
            Slot {
                peer: peer.to_string(),
                token,
                tx,
            },
        );
    }

    fn remove(&self, id: u64) {
        self.lock().remove(&id);
    }

    /// The hub of session `id`, if it exists, belongs to `peer` and `token` matches.
    pub(crate) fn lookup(
        &self,
        id: u64,
        peer: &str,
        token: u128,
    ) -> Option<mpsc::UnboundedSender<Attach>> {
        let slots = self.lock();
        let s = slots.get(&id)?;
        let ok = s.token.to_le_bytes().ct_eq(&token.to_le_bytes());
        (bool::from(ok) && s.peer == peer).then(|| s.tx.clone())
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.lock().len()
    }
}

/// What a stream reader saw.
#[derive(Debug)]
enum Ctl {
    Ack(u64),
    StdinEof,
    /// The caller ended the session on purpose (reset, finished, shut down).
    Gone(String),
    /// The link died: detach.
    Lost(String),
}

/// Whether a read error on a session stream means the caller left on purpose (`true`) or
/// the link was lost (`false`).
fn caller_gone(e: &anyhow::Error) -> bool {
    let Some(io) = e.chain().find_map(|c| c.downcast_ref::<std::io::Error>()) else {
        return false;
    };
    if io.kind() == std::io::ErrorKind::UnexpectedEof {
        return true; // the caller finished its side
    }
    match io.get_ref().and_then(|i| i.downcast_ref::<ReadError>()) {
        Some(ReadError::Reset(code)) => u64::from(*code) == u64::from(RESET_CLIENT_GONE),
        Some(ReadError::ConnectionLost(ConnectionError::LocallyClosed)) => true,
        Some(ReadError::ConnectionLost(ConnectionError::ApplicationClosed(c))) => {
            CALLER_GONE_CLOSE_CODES
                .iter()
                .any(|&k| u64::from(c.error_code) == u64::from(k))
        }
        _ => io.kind() == std::io::ErrorKind::ConnectionReset,
    }
}

fn spawn_reader(
    epoch: u64,
    mut recv: RecvStream,
    mut stdin: Option<mpsc::Sender<Vec<u8>>>,
    input: Arc<AtomicU64>,
    ctl: mpsc::UnboundedSender<(u64, Ctl)>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let c = match wire::read_frame::<_, ExecFrame>(&mut recv).await {
                Ok(ExecFrame::Stdin(b)) => {
                    if let Some(tx) = &stdin
                        && tx.send(b).await.is_err()
                    {
                        stdin = None;
                    }
                    // Counted once delivered, with no await in between: an abort (the
                    // stream is replaced) either lost the frame uncounted or counted it.
                    input.fetch_add(1, Ordering::SeqCst);
                    continue;
                }
                Ok(ExecFrame::StdinEof) => {
                    stdin = None;
                    input.fetch_add(1, Ordering::SeqCst);
                    Ctl::StdinEof
                }
                Ok(ExecFrame::Ack { seq }) => Ctl::Ack(seq),
                Ok(other) => {
                    debug!("resumable session: ignoring {other:?} from the caller");
                    continue;
                }
                Err(e) if caller_gone(&e) => {
                    let _ = ctl.send((epoch, Ctl::Gone(format!("{e:#}"))));
                    return;
                }
                Err(e) => {
                    let _ = ctl.send((epoch, Ctl::Lost(format!("{e:#}"))));
                    return;
                }
            };
            if ctl.send((epoch, c)).is_err() {
                return;
            }
        }
    })
}

/// Identity and policy of one resumable session.
pub(crate) struct Ticket {
    pub(crate) id: u64,
    pub(crate) token: u128,
    pub(crate) ttl: Duration,
    pub(crate) cap: usize,
}

impl Ticket {
    pub(crate) fn new(ttl: Duration) -> Ticket {
        let r = iroh::SecretKey::generate().to_bytes();
        Ticket {
            id: u64::from_le_bytes(r[..8].try_into().expect("8 bytes")),
            token: u128::from_le_bytes(r[8..24].try_into().expect("16 bytes")),
            ttl,
            cap: buffer_cap(),
        }
    }
}

/// How a resumable session ended (besides the process [`Outcome`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HubEnd {
    /// The process ended and its output was delivered (or the caller left after it).
    Done,
    /// The caller went away: the process was killed.
    CallerGone,
    /// Detached longer than the TTL: killed.
    Expired,
    /// The replay buffer overflowed while detached: killed.
    Overflow,
    /// The caller asked to cancel the detached session.
    Cancelled,
    /// A resume asked for output the buffer no longer holds: killed.
    Unreplayable,
}

impl HubEnd {
    pub(crate) fn describe(&self) -> Option<&'static str> {
        match self {
            HubEnd::Done => None,
            HubEnd::CallerGone => Some("caller gone, killed"),
            HubEnd::Expired => Some("detach_ttl expired, killed"),
            HubEnd::Overflow => Some("lost: replay buffer overflowed while detached, killed"),
            HubEnd::Cancelled => Some("caller gone while detached, killed"),
            HubEnd::Unreplayable => Some("lost: output to replay was dropped, killed"),
        }
    }
}

/// Events the hub reports (audit lines, logs).
pub(crate) trait Events: Sync {
    /// `detach`, `resume`, `expire`, `lost`, with a detail.
    fn event(&self, event: &str, detail: Option<&str>);
}

struct Attached {
    send: SendStream,
    reader: Option<JoinHandle<()>>,
}

impl Attached {
    /// Stop reading the stream; the (aborted) reader is returned so a resume can wait
    /// until it is really gone.
    fn stop(mut self) -> Option<JoinHandle<()>> {
        let r = self.reader.take();
        if let Some(r) = &r {
            r.abort();
        }
        r
    }
}

impl Drop for Attached {
    fn drop(&mut self) {
        if let Some(r) = &self.reader {
            r.abort();
        }
    }
}

/// Run the started process for a resumable session until it ends (see the module doc).
/// `send`/`recv` is the first stream, already acknowledged with `SessionAck::Resumable`.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_hub(
    reg: &Registry,
    peer: &str,
    t: &Ticket,
    running: Running,
    timeout: Option<Duration>,
    send: SendStream,
    recv: RecvStream,
    ev: &dyn Events,
) -> (Outcome, HubEnd) {
    let (att_tx, mut att_rx) = mpsc::unbounded_channel::<Attach>();
    reg.insert(t.id, peer, t.token, att_tx);
    let r = hub(t, running, timeout, send, recv, &mut att_rx, ev).await;
    reg.remove(t.id);
    // Resumes that raced with the end are refused, not left hanging.
    att_rx.close();
    while let Ok(mut a) = att_rx.try_recv() {
        let _ = crate::session::refuse(
            &mut a.send,
            exit::PEER_OFFLINE,
            "the session already ended".into(),
        )
        .await;
    }
    r
}

async fn hub(
    t: &Ticket,
    running: Running,
    timeout: Option<Duration>,
    send: SendStream,
    recv: RecvStream,
    att_rx: &mut mpsc::UnboundedReceiver<Attach>,
    ev: &dyn Events,
) -> (Outcome, HubEnd) {
    let (in_tx, in_rx) = mpsc::channel::<Vec<u8>>(8);
    let (out_tx, mut out_rx) = mpsc::channel::<ExecFrame>(8);
    let mut run = tokio::spawn(running.run(in_rx, out_tx, timeout));
    let mut stdin = Some(in_tx);
    let (ctl_tx, mut ctl_rx) = mpsc::unbounded_channel::<(u64, Ctl)>();
    let mut epoch = 0u64;
    // Input frames received from the caller (acknowledged back so it can drop its copies,
    // and told on a resume so it re-sends exactly the missing ones).
    let input = Arc::new(AtomicU64::new(0));
    let mut input_acked = 0u64;
    let mut ack_tick = tokio::time::interval(INPUT_ACK_INTERVAL);
    ack_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut cur = Some(Attached {
        send,
        reader: Some(spawn_reader(
            epoch,
            recv,
            stdin.clone(),
            input.clone(),
            ctl_tx.clone(),
        )),
    });
    // Readers of replaced streams, aborted; awaited before `input` is reported.
    let mut stale: Vec<JoinHandle<()>> = Vec::new();
    let mut ring = Ring::new(t.cap);
    let mut detached_at: Option<Instant> = None;
    let mut outcome: Option<Outcome> = None;
    // The process' last frame is in the ring (or the process was killed).
    let mut end: Option<HubEnd> = None;
    // A resume that arrived while a write to the previous stream was blocked.
    let mut pending: Option<Attach> = None;

    // Kill the process (if still running) and collect its outcome.
    macro_rules! finish {
        ($why:expr) => {{
            out_rx.close();
            let o = match outcome.take() {
                Some(o) => o,
                None => (&mut run).await.expect("session task"),
            };
            return (o, $why);
        }};
    }
    macro_rules! detach {
        ($why:expr) => {{
            let why: String = $why;
            if let Some(a) = cur.take() {
                stale.extend(a.stop());
            }
            detached_at = Some(Instant::now());
            info!(
                session = t.id,
                "session detached ({why}); kept for {:?}", t.ttl
            );
            ev.event("detach", Some(&why));
            if ring.over() {
                ev.event("lost", Some("replay buffer overflowed"));
                finish!(HubEnd::Overflow);
            }
        }};
    }

    loop {
        let expiry = detached_at.map(|d| d + t.ttl);
        tokio::select! {
            f = out_rx.recv(), if end.is_none() => {
                let f = match f {
                    Some(f) => f,
                    None => {
                        // The process ended (its output is all in): its last frame.
                        let o = (&mut run).await.expect("session task");
                        let last = match o.end {
                            End::Exited(code) => ExecFrame::Exit { code },
                            End::TimedOut => ExecFrame::Error {
                                code: exit::TIMED_OUT,
                                message: format!(
                                    "timed out after {}s, process killed",
                                    timeout.unwrap_or_default().as_secs()
                                ),
                            },
                            End::Disconnected => unreachable!("only the hub closes out_rx"),
                        };
                        outcome = Some(o);
                        end = Some(HubEnd::Done);
                        last
                    }
                };
                ring.push(f.clone());
                match &mut cur {
                    Some(a) => {
                        // A stream whose link died silently can block this write (flow
                        // control) until the connection idles out: a resume arriving
                        // meanwhile wins (the frame is in the ring and gets replayed).
                        let written = tokio::select! {
                            r = wire::write_frame(&mut a.send, &f) => Some(r),
                            Some(a) = att_rx.recv(), if pending.is_none() => {
                                pending = Some(a);
                                None
                            }
                        };
                        match written {
                            Some(Err(e)) => detach!(format!("write failed: {e:#}")),
                            Some(Ok(())) => ring.trim(),
                            None => {}
                        }
                    }
                    None if ring.over() => {
                        info!(session = t.id, "replay buffer full ({} bytes) while detached: session lost", ring.bytes());
                        ev.event("lost", Some("replay buffer overflowed"));
                        finish!(HubEnd::Overflow);
                    }
                    None => {}
                }
            }
            Some((g, c)) = ctl_rx.recv() => {
                if g != epoch {
                    continue; // from a replaced stream
                }
                match c {
                    Ctl::Ack(seq) => {
                        ring.ack(seq);
                        if end.is_some() && ring.next() <= seq {
                            finish!(HubEnd::Done);
                        }
                    }
                    Ctl::StdinEof => stdin = None,
                    Ctl::Gone(why) => {
                        debug!(session = t.id, "caller gone: {why}");
                        if end.is_some() {
                            finish!(HubEnd::Done);
                        }
                        finish!(HubEnd::CallerGone);
                    }
                    Ctl::Lost(why) => {
                        if cur.is_some() {
                            detach!(why);
                        }
                    }
                }
            }
            Some(mut a) = next_attach(&mut pending, att_rx) => {
                if a.cancel {
                    let _ = crate::session::refuse(&mut a.send, exit::PEER_OFFLINE, "cancelled".into()).await;
                    ev.event("expire", Some("cancelled by the caller"));
                    finish!(HubEnd::Cancelled);
                }
                let Some(replay) = ring.from(a.seq) else {
                    let msg = format!(
                        "output from frame {} is no longer buffered (holds {}..{})",
                        a.seq, ring.first, ring.next()
                    );
                    let _ = crate::session::refuse(&mut a.send, exit::PEER_OFFLINE, msg.clone()).await;
                    ev.event("lost", Some(&msg));
                    finish!(HubEnd::Unreplayable);
                };
                let frames: Vec<ExecFrame> = replay.cloned().collect();
                ring.ack(a.seq);
                // Replace the previous stream (dead, or half-open on our side): its reader
                // must be gone before `input` is read, or a late frame would be counted
                // after the caller was told to re-send it.
                epoch += 1;
                if let Some(old) = cur.take() {
                    stale.extend(old.stop());
                }
                for r in stale.drain(..) {
                    let _ = r.await;
                }
                let received = input.load(Ordering::SeqCst);
                input_acked = received;
                let mut written =
                    wire::write_frame(&mut a.send, &SessionAck::Reattached { input: received }).await;
                for f in &frames {
                    if written.is_err() {
                        break;
                    }
                    written = wire::write_frame(&mut a.send, f).await;
                }
                let was = detached_at.take();
                let reader =
                    spawn_reader(epoch, a.recv, stdin.clone(), input.clone(), ctl_tx.clone());
                cur = Some(Attached { send: a.send, reader: Some(reader) });
                let detail = format!(
                    "replayed {} frames after {:.1}s detached",
                    frames.len(),
                    was.map(|d| d.elapsed().as_secs_f64()).unwrap_or(0.0)
                );
                info!(session = t.id, "session resumed ({detail})");
                ev.event("resume", Some(&detail));
                if let Err(e) = written {
                    detach!(format!("write failed during replay: {e:#}"));
                }
            }
            _ = ack_tick.tick(), if cur.is_some() && input.load(Ordering::SeqCst) > input_acked => {
                let n = input.load(Ordering::SeqCst);
                let ack = ExecFrame::Ack { seq: n };
                if let Some(a) = &mut cur {
                    let written = tokio::select! {
                        r = wire::write_frame(&mut a.send, &ack) => Some(r),
                        Some(a) = att_rx.recv(), if pending.is_none() => {
                            pending = Some(a);
                            None
                        }
                    };
                    match written {
                        Some(Ok(())) => input_acked = n,
                        Some(Err(e)) => detach!(format!("write failed: {e:#}")),
                        None => {}
                    }
                }
            }
            _ = sleep_until(expiry.unwrap_or_else(Instant::now)), if expiry.is_some() => {
                info!(session = t.id, "detached for {:?} (detach_ttl): ending the session", t.ttl);
                ev.event("expire", None);
                finish!(HubEnd::Expired);
            }
        }
    }
}

async fn next_attach(
    pending: &mut Option<Attach>,
    rx: &mut mpsc::UnboundedReceiver<Attach>,
) -> Option<Attach> {
    match pending.take() {
        Some(a) => Some(a),
        None => rx.recv().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn out(n: usize) -> ExecFrame {
        ExecFrame::Stdout(vec![b'x'; n])
    }

    #[test]
    fn ring_numbers_acks_and_replays() {
        let mut r = Ring::new(1 << 20);
        for i in 0..5 {
            r.push(out(i + 1));
        }
        assert_eq!(r.next(), 5);
        let lens = |r: &Ring, seq| {
            r.from(seq).map(|i| {
                i.map(|f| match f {
                    ExecFrame::Stdout(b) => b.len(),
                    _ => 0,
                })
                .collect::<Vec<_>>()
            })
        };
        assert_eq!(lens(&r, 0), Some(vec![1, 2, 3, 4, 5]));
        assert_eq!(lens(&r, 2), Some(vec![3, 4, 5]));
        assert_eq!(lens(&r, 5), Some(vec![]), "nothing missed");
        assert!(r.from(6).is_none(), "from the future");
        r.ack(3);
        assert_eq!(r.first, 3);
        assert!(r.from(2).is_none(), "acked frames are gone");
        assert_eq!(lens(&r, 3), Some(vec![4, 5]));
        assert_eq!(r.bytes(), 2 * FRAME_COST + 4 + 5);
        r.ack(100);
        assert_eq!((r.first, r.next(), r.bytes()), (5, 5, 0));
    }

    #[test]
    fn ring_overflow_and_trim() {
        let mut r = Ring::new(100);
        r.push(out(50));
        assert!(!r.over());
        r.push(out(50));
        assert!(r.over());
        r.trim();
        assert!(!r.over());
        assert_eq!((r.first, r.next()), (1, 2));
        // A single frame larger than the cap is kept (it was written already).
        r.push(out(500));
        r.trim();
        assert_eq!((r.first, r.next()), (2, 3));
    }

    #[test]
    fn registry_checks_token_and_peer() {
        let reg = Registry::default();
        let (tx, _rx) = mpsc::unbounded_channel();
        reg.insert(7, "pc", 42, tx);
        assert!(reg.lookup(7, "pc", 42).is_some());
        assert!(reg.lookup(7, "pc", 43).is_none(), "wrong token");
        assert!(reg.lookup(7, "phone", 42).is_none(), "other peer");
        assert!(reg.lookup(8, "pc", 42).is_none(), "unknown id");
        reg.remove(7);
        assert!(reg.lookup(7, "pc", 42).is_none());
        assert_eq!(reg.len(), 0);
    }

    #[test]
    fn buffer_cap_env() {
        // Not set in tests: the default.
        if std::env::var(BUFFER_ENV).is_err() {
            assert_eq!(buffer_cap(), DEFAULT_BUFFER);
        }
    }

    #[test]
    fn caller_gone_classification() {
        let io = |kind, inner: ReadError| anyhow::Error::from(std::io::Error::new(kind, inner));
        use std::io::ErrorKind::*;
        assert!(caller_gone(&io(
            ConnectionReset,
            ReadError::Reset(1u32.into())
        )));
        assert!(!caller_gone(&io(
            ConnectionReset,
            ReadError::Reset(9u32.into())
        )));
        assert!(caller_gone(&anyhow::Error::from(std::io::Error::from(
            UnexpectedEof
        ))));
        assert!(caller_gone(&io(
            NotConnected,
            ReadError::ConnectionLost(ConnectionError::LocallyClosed)
        )));
        assert!(!caller_gone(&io(
            NotConnected,
            ReadError::ConnectionLost(ConnectionError::TimedOut)
        )));
        let closed = |code: u32| {
            io(
                NotConnected,
                ReadError::ConnectionLost(ConnectionError::ApplicationClosed(
                    iroh::endpoint::ApplicationClose {
                        error_code: code.into(),
                        reason: Default::default(),
                    },
                )),
            )
        };
        assert!(caller_gone(&closed(4)), "caller daemon shut down");
        assert!(caller_gone(&closed(3)), "we were removed");
        assert!(!caller_gone(&closed(2)), "link reset: detach");
        assert!(!caller_gone(&anyhow::anyhow!("something else")));
    }
}

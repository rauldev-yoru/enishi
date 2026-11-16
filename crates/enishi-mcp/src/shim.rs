//! The shim state machine, independent of how the remote service is reached
//! ([`Connector`]): the real one goes through the local daemon
//! ([`crate::DaemonConnector`]), tests use a fake.
//!
//! Rules (port of the validated PoC):
//! - the remote is opened lazily, on the first client message, and re-opened on the next
//!   message after it was lost;
//! - online: everything passes through; results of `initialize` and of the list methods
//!   (without a `cursor`) are written to the [`Cache`];
//! - offline: those methods are answered from the cache, `ping` with `{}`, every other
//!   request gets an immediate JSON-RPC error; notifications and client responses are
//!   dropped;
//! - when a remote is opened and the client is already past `initialize` (it initialized
//!   against an earlier remote, or from the cache), the shim first replays the client's
//!   `initialize` with an internal id plus `notifications/initialized`. The replay waits
//!   up to the service's startup timeout (default [`DEFAULT_STARTUP_TIMEOUT`]: a cold
//!   `npx` after a reboot is slow) without blocking the client: meanwhile cached methods
//!   and `ping` are answered at once, other requests are held for up to [`START_HOLD`]
//!   (so a quick replay is invisible) and then failed;
//! - when the remote is lost, requests in flight are failed;
//! - on stdin EOF, replies still in flight are awaited (up to [`FLUSH_TIMEOUT`]).

use std::{
    collections::{HashMap, VecDeque},
    future::Future,
    time::Duration,
};

use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader},
    sync::mpsc,
    time::Instant,
};

use crate::cache::Cache;

/// Results the shim remembers and serves offline (only for requests without `cursor`).
pub const CACHED_METHODS: &[&str] = &[
    "initialize",
    "tools/list",
    "prompts/list",
    "resources/list",
    "resources/templates/list",
];
/// JSON-RPC error code of every error the shim generates (implementation-defined range).
pub const ERROR_CODE: i64 = -32000;
/// How long a replayed `initialize` may take (includes the remote server's start-up),
/// unless the service (`startup_timeout` in its enishi.toml entry) or `enishi mcp
/// --startup-timeout` says otherwise.
pub const DEFAULT_STARTUP_TIMEOUT: Duration = Duration::from_secs(60);
/// While a handshake replay is in progress, how long a request that cannot be answered
/// from the cache waits for it before failing.
pub const START_HOLD: Duration = Duration::from_secs(3);
/// After stdin EOF, how long to wait for replies still in flight.
pub const FLUSH_TIMEOUT: Duration = Duration::from_secs(10);

/// An open session with the remote service, as JSON-RPC messages. Dropping `tx` closes the
/// service's stdin; `rx` ending means the session is gone (process exited, link lost).
pub struct Remote {
    pub tx: mpsc::Sender<Value>,
    pub rx: mpsc::Receiver<Value>,
}

/// Why a remote could not be opened (shown to the client and on stderr).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenError {
    /// enishi exit code (`enishi_proto::exit`), e.g. `PEER_OFFLINE`.
    pub code: i32,
    pub message: String,
}

pub trait Connector {
    fn open(&self) -> impl Future<Output = Result<Remote, OpenError>>;
    /// How long the remote service may take to answer a replayed `initialize` (`None`:
    /// [`DEFAULT_STARTUP_TIMEOUT`]). Asked on each replay, so config changes apply.
    fn startup_timeout(&self) -> impl Future<Output = Option<Duration>> {
        async { None }
    }
}

struct Pending {
    id: Value,
    method: String,
    /// Cache key for the result, when cacheable.
    cache: Option<String>,
}

/// A handshake replay in progress on the current link.
struct Replay {
    rid: Value,
    deadline: Instant,
    timeout: Duration,
}

/// A client request waiting for the replay to finish.
struct Held {
    until: Instant,
    method: String,
    msg: Value,
}

struct Shim<C> {
    conn: C,
    cache: Cache,
    label: String,
    out: mpsc::UnboundedSender<Value>,
    link: Option<Remote>,
    /// Client requests forwarded on the current link, by serialized id.
    pending: HashMap<String, Pending>,
    /// The client's `initialize` request, replayed on new remotes.
    init_req: Option<Value>,
    replays: u64,
    /// Set while the current link replays the handshake (the link is not usable yet).
    starting: Option<Replay>,
    held: VecDeque<Held>,
    /// Why we are offline (for client errors); last state logged on stderr.
    reason: String,
    /// The device answered but refused the session (grant/allow/unknown service): not
    /// an outage, so the client is told to fix permissions rather than wait.
    refused: bool,
    logged: Option<String>,
}

fn id_key(id: &Value) -> String {
    id.to_string()
}

fn cache_key(method: &str, msg: &Value) -> Option<String> {
    let cursor = msg
        .get("params")
        .and_then(|p| p.get("cursor"))
        .is_some_and(|c| !c.is_null());
    (CACHED_METHODS.contains(&method) && !cursor).then(|| method.to_string())
}

fn result(id: Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn error(id: Value, message: String) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": ERROR_CODE, "message": message}})
}

/// A response (to a request from the other side): has an id, no method.
fn is_response(v: &Value) -> bool {
    v.get("method").is_none() && v.get("id").is_some_and(|i| !i.is_null())
}

enum Event {
    Client(Option<String>),
    Remote(Option<Value>),
    FlushTimeout,
    /// A replay or hold deadline passed.
    Timer,
}

async fn recv(link: &mut Option<Remote>) -> Option<Value> {
    match link {
        Some(r) => r.rx.recv().await,
        None => std::future::pending().await,
    }
}

/// Serve the MCP client on `input`/`output` until its stdin ends, forwarding to the
/// remote reached through `conn`. `label` (`device/service`) prefixes stderr messages.
pub async fn run<C, R, W>(
    conn: C,
    cache: Cache,
    label: String,
    input: R,
    output: W,
) -> std::io::Result<()>
where
    C: Connector,
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (out, mut out_rx) = mpsc::unbounded_channel::<Value>();
    let writer = tokio::spawn(async move {
        let mut output = output;
        while let Some(v) = out_rx.recv().await {
            let mut line = serde_json::to_vec(&v).unwrap_or_default();
            line.push(b'\n');
            output.write_all(&line).await?;
            output.flush().await?;
        }
        Ok::<_, std::io::Error>(())
    });
    let mut shim = Shim {
        conn,
        cache,
        label,
        out,
        link: None,
        pending: HashMap::new(),
        init_req: None,
        replays: 0,
        starting: None,
        held: VecDeque::new(),
        reason: "not connected yet".into(),
        refused: false,
        logged: None,
    };
    let mut lines = BufReader::new(input).lines();
    let mut eof_at: Option<Instant> = None;
    loop {
        if eof_at.is_some()
            && ((shim.pending.is_empty() && shim.held.is_empty()) || shim.link.is_none())
        {
            break;
        }
        let deadline = eof_at.map(|t| t + FLUSH_TIMEOUT);
        let timer = shim.next_timer();
        let ev = tokio::select! {
            l = lines.next_line(), if eof_at.is_none() => Event::Client(l.ok().flatten()),
            m = recv(&mut shim.link) => Event::Remote(m),
            _ = tokio::time::sleep_until(deadline.unwrap_or_else(Instant::now)), if deadline.is_some() => Event::FlushTimeout,
            _ = tokio::time::sleep_until(timer.unwrap_or_else(Instant::now)), if timer.is_some() => Event::Timer,
        };
        match ev {
            Event::Client(Some(line)) => shim.on_client_line(&line).await,
            Event::Client(None) => eof_at = Some(Instant::now()),
            Event::Remote(Some(v)) => shim.on_remote(v).await,
            Event::Remote(None) => shim.lost("the remote session ended"),
            Event::Timer => shim.on_timer(),
            Event::FlushTimeout => {
                shim.lost("the client closed stdin and no reply came in time");
                break;
            }
        }
    }
    // Close the remote's stdin, then flush everything queued to the client.
    shim.link = None;
    drop(shim);
    writer.await.map_err(std::io::Error::other)?
}

impl<C: Connector> Shim<C> {
    /// Log a state change on stderr once per distinct `state` (not per message: offline
    /// reasons embed changing details such as "last seen 2s ago").
    fn note(&mut self, state: &str, text: String) {
        if self.logged.as_deref() != Some(state) {
            eprintln!("enishi mcp {}: {text}", self.label);
            self.logged = Some(state.to_string());
        }
    }

    fn send(&self, v: Value) {
        let _ = self.out.send(v);
    }

    async fn on_client_line(&mut self, line: &str) {
        if line.trim().is_empty() {
            return;
        }
        match serde_json::from_str::<Value>(line) {
            // Batches (MCP 2025-03-26) are handled element by element.
            Ok(Value::Array(msgs)) => {
                for m in msgs {
                    self.on_client(m).await;
                }
            }
            Ok(m) => self.on_client(m).await,
            Err(e) => eprintln!("enishi mcp {}: ignoring non-JSON input: {e}", self.label),
        }
    }

    async fn on_client(&mut self, msg: Value) {
        let Some(method) = msg.get("method").and_then(Value::as_str).map(str::to_owned) else {
            // A response to a server→client request: only meaningful on the live link.
            if let Some(link) = &self.link {
                let _ = link.tx.send(msg).await;
            }
            return;
        };
        let id = msg.get("id").filter(|i| !i.is_null()).cloned();
        if method == "initialize" {
            self.init_req = Some(msg.clone());
        }
        let mut replayed = false;
        if self.link.is_none() {
            replayed = self.connect(method != "initialize").await;
        }
        if replayed && method == "notifications/initialized" {
            return; // the replay sends it
        }
        if self.starting.is_some() {
            // The service is still starting: never make the client wait on it.
            let Some(id) = id else { return }; // notifications are dropped
            let cached = cache_key(&method, &msg).is_some_and(|k| self.cache.get(&k).is_some());
            if cached || method == "ping" {
                self.offline_reply(&method, Some(id), &msg);
            } else {
                self.held.push_back(Held {
                    until: Instant::now() + START_HOLD,
                    method,
                    msg,
                });
            }
            return;
        }
        self.forward(method, id, msg).await;
    }

    /// Send a client request/notification on the live link (or answer it offline).
    async fn forward(&mut self, method: String, id: Option<Value>, msg: Value) {
        if let Some(link) = &self.link {
            if let Some(id) = &id {
                let cache = cache_key(&method, &msg);
                let p = Pending {
                    id: id.clone(),
                    method: method.clone(),
                    cache,
                };
                self.pending.insert(id_key(id), p);
            }
            if link.tx.send(msg.clone()).await.is_ok() {
                return;
            }
            if let Some(id) = &id {
                self.pending.remove(&id_key(id));
            }
            self.lost("the remote session ended");
        }
        self.offline_reply(&method, id, &msg);
    }

    fn offline_reply(&self, method: &str, id: Option<Value>, msg: &Value) {
        let Some(id) = id else { return }; // notifications are dropped
        let cached = cache_key(method, msg).and_then(|k| self.cache.get(&k).cloned());
        let reply = match cached {
            Some(r) => result(id, r),
            None if method == "ping" => result(id, json!({})),
            None if self.refused => error(
                id,
                format!(
                    "enishi: {} refused the session ({}); check the name and the device's grants",
                    self.label, self.reason
                ),
            ),
            None => error(
                id,
                format!(
                    "enishi: {} is unavailable ({}); `{method}` will work again once it is back",
                    self.label, self.reason
                ),
            ),
        };
        self.send(reply);
    }

    /// Open a remote; start replaying the handshake when the client is already
    /// initialized. Returns whether a replay was started (the link is then not usable
    /// until it completes, see [`Shim::finish_replay`]).
    async fn connect(&mut self, may_replay: bool) -> bool {
        let remote = match self.conn.open().await {
            Ok(r) => r,
            Err(e) => {
                self.reason = e.message.clone();
                self.refused = matches!(
                    e.code,
                    enishi_proto::exit::NOT_ALLOWED | enishi_proto::exit::NO_SUCH_SERVICE
                );
                let word = if self.refused { "refused" } else { "offline" };
                let hint = if self.cache.get("initialize").is_some() {
                    "answering from cache"
                } else {
                    "no cached handshake yet"
                };
                let state = format!("{word} {}", e.code);
                self.note(&state, format!("{word}: {} ({hint})", e.message));
                return false;
            }
        };
        self.refused = false;
        let Some(mut req) = self.init_req.clone().filter(|_| may_replay) else {
            self.link = Some(remote);
            self.note("online", "online".into());
            return false;
        };
        let timeout = self
            .conn
            .startup_timeout()
            .await
            .unwrap_or(DEFAULT_STARTUP_TIMEOUT);
        self.replays += 1;
        let rid = json!(format!("enishi-replay-{}", self.replays));
        req["id"] = rid.clone();
        if remote.tx.send(req).await.is_err() {
            self.reason = "handshake replay failed: remote closed".into();
            self.note("replay", format!("offline: {}", self.reason));
            return false;
        }
        self.link = Some(remote);
        self.starting = Some(Replay {
            rid,
            deadline: Instant::now() + timeout,
            timeout,
        });
        self.reason = format!("the service is starting (handshake replay, up to {timeout:?})");
        self.note(
            "starting",
            format!("connected, replaying the handshake (waiting up to {timeout:?})"),
        );
        true
    }

    /// The replayed `initialize` was answered: complete the handshake, then send the
    /// requests held meanwhile.
    async fn finish_replay(&mut self, resp: Value) {
        let Some(replay) = self.starting.take() else {
            return;
        };
        if let Some(e) = resp.get("error") {
            return self.fail_start(format!("handshake replay failed: initialize failed: {e}"));
        }
        if let Some(r) = resp.get("result") {
            self.cache.put("initialize", r.clone());
        }
        let done = json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
        let sent = match &self.link {
            Some(link) => link.tx.send(done).await.is_ok(),
            None => false,
        };
        if !sent {
            return self.fail_start("handshake replay failed: remote closed".into());
        }
        let took = replay
            .timeout
            .saturating_sub(replay.deadline.saturating_duration_since(Instant::now()));
        self.note(
            "online",
            format!("online (handshake replayed in {:.1}s)", took.as_secs_f64()),
        );
        for h in std::mem::take(&mut self.held) {
            let id = h.msg.get("id").cloned();
            self.forward(h.method, id, h.msg).await;
        }
    }

    /// The replay did not complete: drop the link and fail the held requests; the next
    /// client message tries again.
    fn fail_start(&mut self, why: String) {
        self.starting = None;
        self.link = None;
        self.reason = why.clone();
        self.note("replay", format!("offline: {why}"));
        for h in std::mem::take(&mut self.held) {
            let id = h.msg.get("id").cloned();
            self.offline_reply(&h.method, id, &h.msg);
        }
    }

    fn next_timer(&self) -> Option<Instant> {
        let hold = self.held.front().map(|h| h.until);
        let replay = self.starting.as_ref().map(|r| r.deadline);
        match (hold, replay) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    fn on_timer(&mut self) {
        let now = Instant::now();
        if let Some(r) = &self.starting
            && r.deadline <= now
        {
            let t = r.timeout;
            return self.fail_start(format!(
                "handshake replay failed: no initialize reply within {t:?} \
                 (raise the service's startup_timeout or --startup-timeout)"
            ));
        }
        while self.held.front().is_some_and(|h| h.until <= now) {
            let h = self.held.pop_front().expect("checked");
            let id = h.msg.get("id").cloned();
            self.offline_reply(&h.method, id, &h.msg);
        }
    }

    async fn on_remote(&mut self, v: Value) {
        if let Some(r) = &self.starting {
            if is_response(&v) && v["id"] == r.rid {
                return self.finish_replay(v).await;
            }
            // Notifications (logs) the server sends while starting reach the client.
        }
        if is_response(&v)
            && let Some(p) = self.pending.remove(&id_key(&v["id"]))
            && let (Some(k), Some(r)) = (p.cache, v.get("result"))
        {
            self.cache.put(&k, r.clone());
        }
        self.send(v);
    }

    /// The remote is gone: fail what is in flight; the next message re-opens.
    fn lost(&mut self, why: &str) {
        if self.starting.is_some() {
            return self.fail_start(format!("handshake replay failed: {why}"));
        }
        if self.link.take().is_none() && self.pending.is_empty() {
            return;
        }
        self.reason = why.to_string();
        for (_, p) in std::mem::take(&mut self.pending) {
            let msg = format!(
                "enishi: lost {} while `{}` was in flight ({why}); retry once it is back",
                self.label, p.method
            );
            self.send(error(p.id, msg));
        }
        self.note("lost", format!("offline: {why}"));
    }
}

#[cfg(test)]
mod tests;

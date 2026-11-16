//! Shim state machine against a fake remote MCP server.

use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, Lines},
    sync::{mpsc, oneshot},
    time::{Duration, Instant, timeout},
};

use super::*;

#[derive(Default)]
struct FakeState {
    online: bool,
    opens: usize,
    /// Every message the fake servers received, in order.
    seen: Vec<Value>,
    /// Kill switch of the current session (simulates losing the link).
    kill: Option<oneshot::Sender<()>>,
    /// Servers opened from now on answer `initialize` only after this (slow start).
    init_delay: Duration,
    /// What [`Connector::startup_timeout`] reports (the service's setting).
    startup_timeout: Option<Duration>,
}

#[derive(Clone, Default)]
struct Fake(Arc<Mutex<FakeState>>);

impl Fake {
    fn online(&self, on: bool) {
        let mut s = self.0.lock().unwrap();
        s.online = on;
        if !on {
            s.kill.take(); // drop the live session too
        }
    }
    fn slow_start(&self, delay: Duration, timeout: Option<Duration>) {
        let mut s = self.0.lock().unwrap();
        s.init_delay = delay;
        s.startup_timeout = timeout;
    }
    fn opens(&self) -> usize {
        self.0.lock().unwrap().opens
    }
    fn seen_methods(&self) -> Vec<String> {
        let s = self.0.lock().unwrap();
        s.seen
            .iter()
            .map(|v| v["method"].as_str().unwrap_or("<response>").to_string())
            .collect()
    }
}

/// A minimal MCP server: requires `initialize` first, answers `tools/list`, echoes
/// `tools/call`, never answers `slow`.
impl Connector for Fake {
    async fn startup_timeout(&self) -> Option<Duration> {
        self.0.lock().unwrap().startup_timeout
    }
    async fn open(&self) -> Result<Remote, OpenError> {
        let (tx, mut srv_rx) = mpsc::channel::<Value>(16);
        let (srv_tx, rx) = mpsc::channel::<Value>(16);
        let (kill_tx, mut kill_rx) = oneshot::channel::<()>();
        {
            let mut s = self.0.lock().unwrap();
            if !s.online {
                return Err(OpenError {
                    code: 255,
                    message: "peer pc is offline".into(),
                });
            }
            s.opens += 1;
            s.kill = Some(kill_tx);
        }
        let state = self.0.clone();
        let gen_ = self.opens();
        let init_delay = self.0.lock().unwrap().init_delay;
        tokio::spawn(async move {
            let mut initialized = false;
            loop {
                let msg = tokio::select! {
                    biased;
                    _ = &mut kill_rx => break,
                    m = srv_rx.recv() => match m { Some(m) => m, None => break },
                };
                state.lock().unwrap().seen.push(msg.clone());
                let (Some(method), Some(id)) = (msg["method"].as_str(), msg.get("id")) else {
                    continue;
                };
                let id = id.clone();
                let reply = match method {
                    "initialize" => {
                        tokio::select! {
                            _ = tokio::time::sleep(init_delay) => {}
                            _ = &mut kill_rx => break,
                        }
                        initialized = true;
                        json!({"jsonrpc":"2.0","id":id,"result":{
                            "protocolVersion":"2025-06-18",
                            "serverInfo":{"name":"fake","version":gen_.to_string()},
                            "capabilities":{"tools":{}}}})
                    }
                    _ if !initialized => {
                        json!({"jsonrpc":"2.0","id":id,"error":{"code":-32002,"message":"not initialized"}})
                    }
                    "tools/list" => {
                        json!({"jsonrpc":"2.0","id":id,"result":{"tools":[{"name":"echo"}]}})
                    }
                    "tools/call" => json!({"jsonrpc":"2.0","id":id,"result":{
                        "content":[{"type":"text","text":msg["params"]["arguments"]["text"]}]}}),
                    "slow" => continue,
                    _ => {
                        json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"no"}})
                    }
                };
                if srv_tx.send(reply).await.is_err() {
                    break;
                }
            }
        });
        Ok(Remote { tx, rx })
    }
}

struct Client {
    stdin: Option<DuplexStream>,
    stdout: Lines<BufReader<DuplexStream>>,
    task: tokio::task::JoinHandle<std::io::Result<()>>,
}

fn start(fake: &Fake, cache: Cache) -> Client {
    let (stdin, shim_in) = tokio::io::duplex(1 << 16);
    let (shim_out, stdout) = tokio::io::duplex(1 << 16);
    let fake = fake.clone();
    let task =
        tokio::spawn(async move { run(fake, cache, "pc/fs".into(), shim_in, shim_out).await });
    Client {
        stdin: Some(stdin),
        stdout: BufReader::new(stdout).lines(),
        task,
    }
}

impl Client {
    async fn send(&mut self, v: Value) {
        let mut line = serde_json::to_vec(&v).unwrap();
        line.push(b'\n');
        self.stdin.as_mut().unwrap().write_all(&line).await.unwrap();
    }
    async fn recv(&mut self) -> Value {
        let l = timeout(Duration::from_secs(5), self.stdout.next_line())
            .await
            .expect("reply in time")
            .unwrap()
            .expect("a line");
        serde_json::from_str(&l).unwrap()
    }
    async fn call(&mut self, id: i64, method: &str, params: Value) -> Value {
        self.send(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
            .await;
        let v = self.recv().await;
        assert_eq!(v["id"], id, "{v}");
        v
    }
    async fn notify(&mut self, method: &str) {
        self.send(json!({"jsonrpc":"2.0","method":method})).await;
    }
    async fn init(&mut self, id: i64) -> Value {
        let v = self
            .call(
                id,
                "initialize",
                json!({"protocolVersion":"2025-06-18","capabilities":{},
                       "clientInfo":{"name":"t","version":"1"}}),
            )
            .await;
        self.notify("notifications/initialized").await;
        v
    }
    async fn close(mut self) -> Vec<Value> {
        drop(self.stdin.take());
        let mut rest = Vec::new();
        while let Ok(Some(l)) = timeout(Duration::from_secs(15), self.stdout.next_line())
            .await
            .unwrap()
        {
            rest.push(serde_json::from_str(&l).unwrap());
        }
        self.task.await.unwrap().unwrap();
        rest
    }
}

fn echo(text: &str) -> Value {
    json!({"name":"echo","arguments":{"text":text}})
}

fn err_msg(v: &Value) -> &str {
    v["error"]["message"]
        .as_str()
        .unwrap_or_else(|| panic!("not an error: {v}"))
}

fn temp_cache(tag: &str) -> (std::path::PathBuf, Cache) {
    let dir = std::env::temp_dir().join(format!("enishi-shim-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let f = crate::cache::cache_file(&dir, "pc", "fs", &[]);
    (dir, Cache::open(Some(f)))
}

#[tokio::test]
async fn online_pass_through_writes_cache() {
    let fake = Fake::default();
    fake.online(true);
    let (dir, cache) = temp_cache("online");
    let path = cache.path().unwrap().to_path_buf();
    let mut c = start(&fake, cache);
    let init = c.init(1).await;
    assert_eq!(init["result"]["serverInfo"]["name"], "fake");
    let tools = c.call(2, "tools/list", json!({})).await;
    assert_eq!(tools["result"]["tools"][0]["name"], "echo");
    let r = c.call(3, "tools/call", echo("hi")).await;
    assert_eq!(r["result"]["content"][0]["text"], "hi");
    // A paginated list is not cached.
    c.call(4, "tools/list", json!({"cursor":"x"})).await;
    assert!(c.close().await.is_empty());
    assert_eq!(fake.opens(), 1);
    assert_eq!(
        fake.seen_methods(),
        [
            "initialize",
            "notifications/initialized",
            "tools/list",
            "tools/call",
            "tools/list"
        ]
    );
    let on_disk: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(on_disk["initialize"]["serverInfo"]["name"], "fake");
    assert_eq!(on_disk["tools/list"]["tools"][0]["name"], "echo");
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn offline_answers_from_cache_and_errors_fast() {
    let fake = Fake::default();
    let (dir, cache) = temp_cache("offline");
    // Warm the cache with an online session.
    fake.online(true);
    let mut c = start(&fake, cache);
    c.init(1).await;
    c.call(2, "tools/list", json!({})).await;
    c.close().await;
    // Fresh client, peer offline: handshake + tools/list from cache, the rest fails now.
    fake.online(false);
    let mut c = start(
        &fake,
        Cache::open(Some(crate::cache::cache_file(&dir, "pc", "fs", &[]))),
    );
    let t0 = Instant::now();
    let init = c.init(7).await;
    assert_eq!(init["result"]["serverInfo"]["name"], "fake");
    let tools = c.call(8, "tools/list", json!({})).await;
    assert_eq!(tools["result"]["tools"][0]["name"], "echo");
    let e = c.call(9, "tools/call", echo("x")).await;
    assert!(err_msg(&e).contains("peer pc is offline"), "{e}");
    assert_eq!(e["error"]["code"], ERROR_CODE);
    let p = c.call(10, "ping", json!({})).await;
    assert_eq!(p["result"], json!({}));
    assert!(
        t0.elapsed() < Duration::from_millis(500),
        "{:?}",
        t0.elapsed()
    );
    // Uncached list method → error, not a fabricated answer.
    let e = c.call(11, "prompts/list", json!({})).await;
    assert!(err_msg(&e).contains("unavailable"));
    c.close().await;
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn offline_without_cache_fails_initialize() {
    let fake = Fake::default();
    let mut c = start(&fake, Cache::memory());
    let e = c
        .call(1, "initialize", json!({"protocolVersion":"2025-06-18"}))
        .await;
    assert!(err_msg(&e).contains("offline"));
    c.close().await;
}

#[tokio::test]
async fn replays_handshake_after_reconnect_mid_session() {
    let fake = Fake::default();
    fake.online(true);
    let mut c = start(&fake, Cache::memory());
    c.init(1).await;
    c.call(2, "tools/call", echo("a")).await;
    // Link lost, then back: the next call goes to a fresh, initialized server.
    fake.online(false);
    let e = c.call(3, "tools/call", echo("b")).await;
    assert!(err_msg(&e).starts_with("enishi: "), "{e}");
    fake.online(true);
    let r = c.call(4, "tools/call", echo("c")).await;
    assert_eq!(r["result"]["content"][0]["text"], "c", "{r}");
    assert!(c.close().await.is_empty());
    assert_eq!(fake.opens(), 2);
    let seen = fake.0.lock().unwrap().seen.clone();
    let tail: Vec<_> = seen[seen.len() - 3..].to_vec();
    assert_eq!(tail[0]["method"], "initialize");
    assert!(
        tail[0]["id"]
            .as_str()
            .unwrap()
            .starts_with("enishi-replay-")
    );
    assert_eq!(tail[0]["params"]["clientInfo"]["name"], "t");
    assert_eq!(tail[1]["method"], "notifications/initialized");
    assert_eq!(tail[2]["params"]["arguments"]["text"], "c");
}

#[tokio::test]
async fn replays_on_first_connect_after_cached_init() {
    let fake = Fake::default();
    let mut cache = Cache::memory();
    cache.put("initialize", json!({"serverInfo":{"name":"cached"}}));
    let mut c = start(&fake, cache);
    // Initialized from cache while offline (its notifications/initialized is dropped).
    let init = c.init(1).await;
    assert_eq!(init["result"]["serverInfo"]["name"], "cached");
    fake.online(true);
    let r = c.call(2, "tools/call", echo("z")).await;
    assert_eq!(r["result"]["content"][0]["text"], "z", "{r}");
    c.close().await;
    // Exactly one initialize + initialized reached the server, before the call.
    assert_eq!(
        fake.seen_methods(),
        ["initialize", "notifications/initialized", "tools/call"]
    );
}

#[tokio::test]
async fn replay_does_not_duplicate_initialized() {
    // Peer comes up between the cached initialize and the client's initialized.
    let fake = Fake::default();
    let mut cache = Cache::memory();
    cache.put("initialize", json!({"serverInfo":{"name":"cached"}}));
    let mut c = start(&fake, cache);
    c.call(1, "initialize", json!({})).await;
    fake.online(true);
    c.notify("notifications/initialized").await;
    c.call(2, "tools/list", json!({})).await;
    c.close().await;
    assert_eq!(
        fake.seen_methods(),
        ["initialize", "notifications/initialized", "tools/list"]
    );
}

#[tokio::test]
async fn in_flight_requests_fail_on_link_loss() {
    let fake = Fake::default();
    fake.online(true);
    let mut c = start(&fake, Cache::memory());
    c.init(1).await;
    c.send(json!({"jsonrpc":"2.0","id":2,"method":"slow"}))
        .await;
    c.send(json!({"jsonrpc":"2.0","id":"s","method":"slow"}))
        .await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let t0 = Instant::now();
    fake.online(false);
    let mut ids = [c.recv().await, c.recv().await];
    assert!(t0.elapsed() < Duration::from_millis(500));
    ids.sort_by_key(|v| v["id"].is_string());
    assert_eq!(ids[0]["id"], 2);
    assert_eq!(ids[1]["id"], "s");
    assert!(err_msg(&ids[0]).contains("in flight"));
    c.close().await;
}

#[tokio::test]
async fn eof_flushes_replies_in_flight() {
    let fake = Fake::default();
    fake.online(true);
    let mut c = start(&fake, Cache::memory());
    // Pipe everything and close stdin right away, like `printf ... | enishi mcp`.
    for (i, m) in ["initialize", "tools/list"].iter().enumerate() {
        c.send(json!({"jsonrpc":"2.0","id":i,"method":m,"params":{}}))
            .await;
    }
    c.send(json!({"jsonrpc":"2.0","id":5,"method":"tools/call","params":echo("e")}))
        .await;
    let rest = c.close().await;
    let ids: Vec<_> = rest.iter().map(|v| v["id"].clone()).collect();
    assert_eq!(ids, [json!(0), json!(1), json!(5)]);
    assert_eq!(rest[2]["result"]["content"][0]["text"], "e");
}

#[tokio::test]
async fn server_requests_and_client_responses_pass_through() {
    let fake = Fake::default();
    fake.online(true);
    let mut c = start(&fake, Cache::memory());
    c.init(1).await;
    // A client response (to a server request) goes to the server, not to `pending`.
    c.send(json!({"jsonrpc":"2.0","id":99,"result":{"roots":[]}}))
        .await;
    c.call(2, "tools/list", json!({})).await;
    c.close().await;
    assert_eq!(fake.seen_methods()[2], "<response>");
}

/// A reconnect to a server that starts slowly (cold `npx`): the replay waits for it,
/// and meanwhile the client is never blocked.
#[tokio::test]
async fn slow_start_replay_answers_client_meanwhile() {
    let fake = Fake::default();
    fake.online(true);
    let mut c = start(&fake, Cache::memory());
    c.init(1).await;
    c.call(2, "tools/list", json!({})).await; // cached
    fake.online(false);
    c.call(3, "tools/call", echo("down")).await;
    // Back, but the new server needs 4 s to answer initialize (> START_HOLD).
    fake.slow_start(Duration::from_secs(4), None);
    fake.online(true);
    let t0 = Instant::now();
    c.send(json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":echo("held")}))
        .await;
    // Cached methods and ping are answered at once while the replay waits.
    let tools = c.call(5, "tools/list", json!({})).await;
    assert_eq!(tools["result"]["tools"][0]["name"], "echo");
    let p = c.call(6, "ping", json!({})).await;
    assert_eq!(p["result"], json!({}));
    assert!(
        t0.elapsed() < Duration::from_millis(300),
        "{:?}",
        t0.elapsed()
    );
    // The held call fails after START_HOLD, telling why, without waiting for the server.
    let e = c.recv().await;
    assert_eq!(e["id"], 4);
    assert!(err_msg(&e).contains("starting"), "{e}");
    let waited = t0.elapsed();
    assert!(waited >= START_HOLD && waited < START_HOLD + Duration::from_millis(500));
    // The replay itself still completes on the same server: later calls go through.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let r = c.call(7, "tools/call", echo("up")).await;
    assert_eq!(r["result"]["content"][0]["text"], "up", "{r}");
    c.close().await;
    assert_eq!(fake.opens(), 2, "no reopen: the first replay succeeded");
}

/// A call made while a quick replay runs is held and then sent: no error.
#[tokio::test]
async fn call_during_short_replay_is_held_then_sent() {
    let fake = Fake::default();
    fake.online(true);
    let mut c = start(&fake, Cache::memory());
    c.init(1).await;
    fake.online(false);
    c.call(2, "tools/call", echo("down")).await;
    fake.slow_start(Duration::from_millis(800), None);
    fake.online(true);
    let r = c.call(3, "tools/call", echo("a")).await;
    assert_eq!(r["result"]["content"][0]["text"], "a", "{r}");
    c.close().await;
}

/// The service's startup_timeout bounds the replay; the next message retries.
#[tokio::test]
async fn replay_gives_up_after_service_startup_timeout() {
    let fake = Fake::default();
    fake.online(true);
    let mut c = start(&fake, Cache::memory());
    c.init(1).await;
    fake.online(false);
    c.call(2, "tools/call", echo("down")).await;
    fake.slow_start(Duration::from_secs(3), Some(Duration::from_millis(500)));
    fake.online(true);
    let t0 = Instant::now();
    let e = c.call(3, "tools/call", echo("x")).await;
    assert!(err_msg(&e).contains("within 500ms"), "{e}");
    assert!(
        t0.elapsed() < Duration::from_millis(900),
        "{:?}",
        t0.elapsed()
    );
    // Server is fast again: the next call reopens and replays successfully.
    fake.slow_start(Duration::ZERO, None);
    let r = c.call(4, "tools/call", echo("y")).await;
    assert_eq!(r["result"]["content"][0]["text"], "y", "{r}");
    c.close().await;
    assert_eq!(fake.opens(), 3);
}

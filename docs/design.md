# enishi — Design (v0.1 draft)

Status: v0.1 design — decisions D1–D5 resolved (see §10). Implemented so far: config + identity
(`enishi id`) and the daemon base (`enishi up`, presence, `enishi status`). The rest is
design; the proof of concept (`link-poc`) measured results are in the README.

## 1. Goal

One small native binary, `enishi`, installed on each of your devices. Once two devices
are paired they share a private, encrypted, peer-to-peer **link**, and over that link:

- you (or an agent) can **run native commands** on the other device from a terminal;
- each device can **publish stdio services** (MCP servers, CLIs) that the other side
  consumes as if they were local;
- devices that **sleep or go offline** are handled natively: presence, instant
  `peer offline`, automatic reconnect.

v0.1 scope: **Linux ↔ Windows**, both directions. macOS / Android come later on the same
design (nothing below is Windows- or Linux-specific except the service-host details).

Non-goals for v0.1: GUI, file sync, generic L3 VPN, HTTP transports, browser/computer-use
features (those are services built *on top*, see layers).

## 2. Layers

Each layer only depends on the one below. Upper layers are optional.

```
L4  integrations   agent configs (Hermes, Claude Code, OpenClaw…), installers
L3  services       exec, MCP shim, published stdio services, (later: fs, clipboard, wake…)
L2  link           daemon: peers, presence, sessions, reconnect, wake hello, ACL
L1  transport      iroh (QUIC, NAT traversal, relay); direct addrs / Tailscale as hints
```

- **L1** is a dependency (iroh), not our code. Swappable in theory; we don't plan to.
- **L2** is the core of enishi: the daemon (`enishi up`) owns the connections. Every
  other command talks to the *local* daemon over local IPC, so calls are fast (warm
  connection) and never block on network setup.
- **L3** services are all the same primitive underneath: *a bidirectional byte stream
  to a process on the peer*. `exec` and `mcp` are just two front-ends over it.
- **L4** is documentation + generators (`enishi integrate hermes` could print the YAML).

## 3. CLI

Principle: the same binary is a **daemon**, a **terminal tool** and an **MCP server
launcher**. Everything a human can do from a terminal, an agent can do too.

### 3.1 Device selection

Every command that targets a peer accepts a device, falling back to the configured
default peer:

```
enishi exec ls                          # default device
enishi -d windows-home exec dir         # explicit device (-d / --device)
```

Device names are user data, not CLI grammar: `-d <name>` keeps the grammar fixed (no
clashes with real flags, working shell completion). Decided in D1.

### 3.2 Commands

| Command | What it does |
|---|---|
| `enishi up [--log-file [PATH]]` | Run the daemon (foreground). `--log-file` also writes a size-rotated log (default `enishi.log` in the config dir); the installed unit / task runs `up --log-file`. |
| `enishi install [--prefix DIR] [--name N] [--no-start] [--no-firewall]` / `enishi uninstall [--name N] [--purge --yes] [--no-firewall]` | Per-user install: binary, autostart (systemd user unit / logon task), start; reverse it (§7). |
| `enishi [-d dev] update [--version vX.Y.Z \| --from-file PATH] [--check] [--status] [--timeout S]` | Update the installed enishi through a detached, rollback-safe updater (§7, D17); with `-d`, ask that device to update itself (its `update` grant, D16) and follow it until it is back. |
| `enishi logs [-n N] [-f]` | Print / follow the daemon log file. |
| `enishi firewall [--name N] [--remove \| --check]` | Windows: Private-only inbound rule for enishi (self-elevates: one UAC prompt; `--check` is read-only). |
| `enishi id` | Print this device's public id. |
| `enishi pair invite` / `enishi pair join <code>` | One-time pairing (see §6). |
| `enishi reload` | Re-read `enishi.toml` into the running daemon (peers, grants, services). |
| `enishi status [-d dev]` | Peers, presence (`online`, `asleep/offline`, last seen), latency. |
| `enishi exec [-d dev] [opts] -- <cmd> [args…]` | Run a native command on the peer; stdin/stdout/stderr streamed, exit code propagated. |
| `enishi shell [-d dev]` | Interactive shell on the peer (PTY / ConPTY). v0.2. |
| `enishi run [-d dev] <service> [-- args]` | Pipe local stdio to a published service (raw). |
| `enishi mcp [dev/]<service> [-- extra args]` | Same as `run`, MCP-aware: cached `initialize`/`tools/list`, offline errors, handshake replay. This is what agents put in their config. |
| `enishi mcp [-d dev] [--cwd D] [--env K=V] -- <command> [args…]` | Transparent mode: run any MCP server command on the peer through the same shim (needs the `mcp` grant, §3.5). |
| `enishi mcp config <dev/service \| -d dev -- cmd…> --for hermes\|claude\|openclaw\|json` | Print a ready-to-paste config snippet for that agent. |
| `enishi services [-d dev]` | List services the peer publishes to us. |
| `enishi forward [-d dev] <service> [local_port] [--bind ip]` | Forward a local TCP port to the peer's `tcp` service until Ctrl-C (§3.6). |
| `enishi service add <name> [--mcp\|--stdio\|--tcp host:port] [--allow peer]… [--allow-args] [--cwd D] [--env K=V]… [--force] [-- cmd…]` | Publish a local service (edits `enishi.toml`, then reloads the daemon). |
| `enishi service list` / `enishi service remove <name>` | Local published services (name, kind, args, allow, command/target) / unpublish. |
| `enishi service import <format> <file> [--allow peer]… [--only a,b] [--dry-run] [--force]` | Publish services from another tool's config; formats are pluggable (`mcp` today, §3.7). |
| `enishi grant <peer> [--exec\|--no-exec] [--mcp\|--no-mcp] [--update\|--no-update] [--service S]… [--revoke-service S]…` | Show or change `[grants.<peer>]`; granting a service also adds the peer to its `allow` (§3.7). |
| `enishi wake [-d dev]` | Wake-on-LAN through another online peer on the same LAN. v0.2. |

### 3.3 `exec` details

```
enishi -d windows-home exec -- Get-ChildItem C:\Users           # default shell on Windows: PowerShell
enishi -d windows-home exec --shell cmd -- dir
enishi -d windows-home exec --raw -- C:\tools\foo.exe --flag      # no shell, direct process
enishi -d server exec --cwd ~/projects -- git status              # from Windows to Linux
echo data | enishi exec -- wc -c                                  # stdin is streamed
```

- Exit code of the remote process becomes `enishi`'s exit code; enishi's own errors use
  reserved codes (e.g. `255 peer offline`, `254 not allowed`, `253 no such service`,
  `252 daemon not running`; `2` usage errors such as an unknown device).
- A Windows NTSTATUS-style exit code (≥ `0x40000000`, e.g. a process killed at shutdown,
  interrupted or crashed) is still propagated as-is, but stderr, the shim and the session
  logs also name it: `enishi: remote process exited with 0x40010004
  (DBG_TERMINATE_PROCESS: terminated (shutdown/kill))`; unknown ones print in hex.
- `--timeout`, `--cwd`, `--env K=V`, `--shell <pwsh|powershell|cmd|bash|sh>`, `--raw`.
- stdout and stderr are kept separate (two streams), unlike the PoC.

As implemented (v0.1 step 3):

- Shell mode joins the command words with spaces into one command line: Windows →
  `pwsh` if on PATH else `powershell`, both `-NoProfile -NoLogo -NonInteractive -Command
  <line>`; `--shell cmd` → `cmd /d /s /c "<line>"` run by an outer `cmd /d /v:on /s /c
  "chcp.com 65001 >nul 2>&1 & cmd.exe /d /s /c !ENISHI_CMD_LINE!"` (both by
  `%SystemRoot%\System32` path; the line, quoted, travels in `ENISHI_CMD_LINE` and is
  passed verbatim). The outer cmd switches the session's hidden console to UTF-8 so
  built-ins and console programs stop emitting the OEM code page (cp850 garbled accents
  and lost `€`); a single `chcp 65001 & <line>` does not work because cmd caches the code
  page per parsed line. Cost ≈30 ms per call (two extra short processes). Legacy tools that ignore the console code page (e.g. `findstr`
  matching non-ASCII) are unaffected. unix → `$SHELL` (else
  `/bin/sh`) `-c <line>`. `--raw` spawns argv directly (`--raw` and `--shell` conflict).
- `--cwd` must exist on the device (a leading `~` is the device user's home); `--env`
  adds to the daemon's environment. Windows children get `CREATE_NO_WINDOW`.
- Process tree: unix → own process group, killed with `SIGKILL` to the group; Windows →
  job object, terminated as a whole. The tree is killed on `--timeout`, when the caller
  goes away (CLI killed, link lost) and when the session is dropped.
- After the process exits, output from background children still holding the pipes is
  forwarded until it is quiet for 500 ms, then the tree is killed. The session ends when
  the exit status is known **and** both pipes reached EOF (or went quiet): the exit code
  has to wait for the process to be reaped, and EOF normally arrives with the exit, so
  there is no teardown wait to shave (the `exec in` log line records `eof=` and
  `reaped=` separately to show it).
- More reserved codes, shell-style: `124` timeout (tree killed), `126` cannot execute
  (e.g. bad `--cwd`), `127` command not found (raw mode). A remote process may of course
  exit with any of these itself.
- Presence decides offline: if the daemon has no live link the CLI gets `255` at once
  (≈3 ms measured) — nothing is dialed on the call path.

- Windows text encoding: cmd and PowerShell output are forced to UTF-8 (cmd via an outer
  `chcp 65001` + fresh cmd; PowerShell via a UTF-8 prelude). Known v0.1 gap: non-ASCII
  **stdin** piped into PowerShell is decoded with the OEM code page.

### 3.4 Agent usage (MCP)

```yaml
mcp_servers:
  pc_files:
    command: enishi
    args: [mcp, windows-home/fs]
```

Extra args after `--` are forwarded to the remote MCP server **only if** the service
definition allows it (`allow_args`), so an agent can't turn a published MCP into
arbitrary execution.

enishi ships **no MCP servers and no service-specific logic**: a service is any stdio
program the publishing user declares, and the shim only speaks MCP framing
(newline-delimited JSON-RPC 2.0) and the handshake. As implemented (v0.1 step 5,
crate `enishi-mcp`, ported from the PoC):

- The remote is opened lazily (first client message) as an ordinary `run` session through
  the local daemon; the remote server's stderr is copied to the shim's stderr (agents log
  it). A remote that is lost (peer offline, process exited) is re-opened on the next
  message, which respawns the service.
- Online: pure pass-through. The results of `initialize`, `tools/list`, `prompts/list`,
  `resources/list` and `resources/templates/list` (requests without `cursor`) are cached
  on disk per device + service (+ hash of extra args): `$ENISHI_CACHE_DIR`, else
  `$XDG_CACHE_HOME/enishi` / `~/.cache/enishi` (Linux) or `%LOCALAPPDATA%\enishi\cache`
  (Windows), file `mcp/<device>/<service>.json`.
- Offline (peer offline, service refused, or **local daemon not running**): those methods
  are answered from the cache, `ping` with `{}`, every other request gets an immediate
  JSON-RPC error `-32000` naming the reason; notifications and client responses are
  dropped. A one-line state change goes to stderr (`offline: … (answering from cache)`,
  with a `start it with enishi up` hint when the daemon is down). `initialize` with no
  cache is an error, never a fabricated answer.
- When a remote is opened and the client is already past `initialize` (mid-session
  reconnect, or initialized from cache), the shim first replays the client's
  `initialize` with an internal id (`enishi-replay-N`) plus `notifications/initialized`;
  a client `notifications/initialized` that arrives right then is not duplicated. The
  replay waits up to the **startup timeout** — `enishi mcp --startup-timeout`, else the
  service's `startup_timeout` (fetched with `ServiceDetails`), else 60 s — since a cold
  `npx` right after a reboot can take tens of seconds. It never blocks the client: while
  it runs, cached methods and `ping` are answered at once, other requests are held up
  to 3 s (a quick replay stays invisible) and then fail with "the service is starting".
  A replay that times out or errors drops the session; the next request starts over.
- Requests in flight when the remote is lost get an error at once. On stdin EOF the shim
  waits up to 10 s for replies still in flight, then closes the service's stdin.
- Batches (MCP 2025-03-26) are handled element by element.

### 3.5 MCP transparent mode and config snippets

```
enishi mcp -d pc -- npx -y @modelcontextprotocol/server-filesystem C:\Users\me\Projects
enishi mcp config pc/fs --for claude
enishi mcp config -d pc --for hermes -- npx -y @modelcontextprotocol/server-filesystem C:\Users\me
```

- Transparent mode sends `SessionKind::Mcp(ExecRequest)` (argv, `--cwd`, `--env`) and the
  peer spawns it **raw** (no shell, no timeout) after `authorize_mcp`; then it is exactly
  a service session and the shim is the same: cache, offline answers, handshake replay.
  Cache file: `mcp/<device>/cmd~<fnv1a hex of argv + cwd + env>.json`.
- Peer side: `[grants.<peer>] mcp = true` (default false). Same shape as `exec`
  (`McpPolicy` = the `ExecPolicy` enum, `{ allow = [...] }` rules parse but are denied
  until enforced) with a single `authorize_mcp` check. `exec = true` does **not** imply
  `mcp` and vice versa (D13).
- A session the peer refuses (no grant, unknown service) is **not** treated as an outage:
  the shim answers with an error naming the refusal instead of serving the cache.
- Programs are resolved on the peer like any raw spawn; on Windows that includes
  `PATHEXT` so `npx`, `uvx`, `pnpm` (`.cmd` shims) work as written in MCP docs (D12).
- `enishi mcp config` prints the agent's own format: Hermes → `mcp_servers:` YAML;
  Claude (Desktop/Code, Cursor) → `{"mcpServers": {…}}`; OpenClaw → `{"mcp":
  {"servers": {…}}}`; `json` → just the `{command, args, env}` entry. The command is the
  absolute path of the running `enishi` (agents often start with a minimal `PATH`); a
  non-default `--config`/`ENISHI_CONFIG` is pinned in `env`. The entry name is
  `<device>-<service>`, or `<device>-<package>` in transparent mode
  (`pc-server-filesystem`); `--name` overrides it.

### 3.6 TCP forwarding

```
pc$     enishi service add web --tcp localhost:3000 --allow server   # publish
pc$     enishi grant server --service web
server$ enishi -d pc forward web            # listens on 127.0.0.1:3000 (the target's port)
server$ enishi -d pc forward web 8080 --bind 0.0.0.0
```

- A `kind = "tcp"` service has `target = "host:port"` instead of a command; it is
  double-gated exactly like other services (D9) and `enishi services` reports its port.
- `forward` asks the **local daemon** to listen (`IpcRequest::Forward`); the listener lives
  as long as the CLI's IPC connection (Ctrl-C stops it). Default local port = the
  service's target port (asks the peer, so it must be online; pass a port otherwise);
  default bind = `127.0.0.1`.
- Every accepted TCP connection becomes one bi-stream on the warm `enishi/stream/1`
  connection: header `SessionKind::Tcp {service}`; the peer authorizes, connects to
  `target` (2.5 s bound) **before** acking (so "connection refused" comes back as an
  ack error), then raw bytes both ways. Finishing one QUIC direction is a TCP
  half-close (`shutdown(Write)`) on the other side; an error resets both.
- Peer offline (or refusal): the connection is accepted and closed at once (fast fail,
  ≈ ms), with a note on the CLI's stderr / daemon log; the listener keeps going and works
  again as soon as the peer is back — no restart.
- Declared forwards, started by the daemon and re-synced on `enishi reload` (changed
  entries restart, unchanged ones keep running):

  ```toml
  [forwards.web]
  peer = "pc"              # default: default_peer
  service = "web"
  local_port = 3000        # required here (the peer may be offline at startup)
  bind = "127.0.0.1"       # default
  ```
- Both ends log one line per connection (`forward out` ack/end/bytes, `forward in`
  connect/end/bytes) and append an audit entry (as a service session).

### 3.7 Managing the config from the CLI

`service add|remove`, `service import` and `grant` edit the local `enishi.toml` with
`toml_edit` (comments, order and formatting kept), validate the result as a full config
before writing, and then ask the running daemon to reload (if it is not running the edit
is kept and the command says so). Duplicates are refused unless `--force`.

- `grant <peer> --service S` writes **both** D9 gates: `S` into `[grants.<peer>]
  services` and the peer into `[services.S] allow`; `--revoke-service` removes both. D9
  still holds in the file; the CLI just makes the common case one command (D10).
- `service import <format> <file>`: importers implement a small trait in `enishi-core`
  (`Importer { format, description, parse(text) -> ImportSet {services, skipped} }`) and are
  registered by the binary, so the generic layer knows no format. The only format today
  is `mcp`, implemented in `enishi-mcp`: the de-facto `mcpServers` JSON (Claude
  Desktop/Code, Cursor, …; VS Code's top-level `servers` and OpenClaw's `mcp.servers`
  too) → `{name: {command, args, env, cwd?}}`, imported as `kind = "mcp"`. Entries with
  a `url` or any `type` other than `stdio` (`http`, `sse`, …) are skipped with a note (remote/HTTP servers are not
  stdio). Names are sanitized to `[A-Za-z0-9._-]`. Unknown format → error listing the
  available ones (D11).

## 4. Configuration

One file per device: `~/.config/enishi/enishi.toml` (Linux) /
`%APPDATA%\enishi\enishi.toml` (Windows). Keys live next to it (`identity.key`,
permissions 0600 / user-only ACL).

```toml
[device]
name = "server"
default_peer = "windows-home"
listen_port = 7801            # optional; fixed port helps direct dialing / firewall rules

[peers.windows-home]
id = "0df46624…"              # filled by `enishi pair`
addrs = ["100.71.180.5:7802"] # optional hints (Tailscale/LAN); discovery otherwise

# What this device publishes, and to whom
[services.fs]
command = ["node", "C:/…/server-filesystem/dist/index.js", "C:/Users/raulr/Projects"]
kind = "mcp"                  # mcp | stdio (default) | tcp (with target = "host:port")
allow = ["server"]            # peer names
allow_args = false
startup_timeout = "90s"       # optional (mcp/stdio): 90, "90s", "2m"; shim replay wait
cwd = "~"                     # optional; default: the daemon's
env = { NODE_ENV = "production" }   # optional

[grants.server]               # what peer "server" may do on this device
exec = true                   # run arbitrary commands (off by default)
mcp = false                   # start arbitrary MCP commands (transparent mode, §3.5)
update = false                # update enishi here (`enishi -d <this> update`, D16; 0.1.1+)
services = ["fs", "ps"]
```

The config location can be overridden with the global `--config <path>` flag or the
`ENISHI_CONFIG` environment variable; the key and the daemon's IPC endpoint then live next
to that file, so several instances can run on one host (integration tests, multiple
identities). `identity.key` (32 raw bytes) is created on first use (`enishi id` or
`enishi up`); the daemon refuses a key readable by other users on unix.

Rule: **everything is denied unless granted**, per peer. `exec` is the most powerful grant
and is opt-in.

Services (D9): a peer may open service `S` only if **`S.allow` lists the peer AND
`[grants.<peer>] services` contains `S`** — the service says who may use it, the grant says
what the peer may use; both default to empty. Extra args are refused unless
`allow_args = true`. Refusals: `253` no such service, `254` not allowed (missing from
either list, or args given without `allow_args`). `exec = true` does not imply any
service. The command is spawned as argv (never through a shell) via the exec spawn path
(own process group / job object, `CREATE_NO_WINDOW`, tree killed when the caller goes
away); no timeout. `enishi services` lists exactly the services the asking peer may open
(name, kind, allow_args).

`exec` is modelled as a policy, not a plain bool, so command allowlists can be added
later without breaking configs (D5):

```toml
exec = true                                   # v0.1: allow all
exec = false                                  # v0.1: deny (default)
# later, same key:
exec = { allow = ["git *", "Get-ChildItem *"], deny = ["Remove-Item *"] }
```

In code: `enum ExecPolicy { Deny, AllowAll, Rules(..) }` with every exec request going
through one `authorize_exec(peer, request)` check, so adding `Rules` only touches that
function.

## 5. Protocol (L2/L3)

iroh connection per peer pair, kept warm by the daemon. ALPNs:

- `enishi/ctl/1` — control: hello (device name, version, capabilities, service catalog
  hash), heartbeat, presence, catalog fetch, wake hints.
- `enishi/pair/1` — one-time pairing (§6.1); the only ALPN unknown devices may use.
- `enishi/stream/1` — one QUIC bidirectional stream per session. A session starts with a
  small header frame `{kind: exec|service, target, args, cwd, env, pty}`, the peer answers
  `{ok}` or `{err, code, message}` (ack within a bounded timeout — PoC lesson), then raw
  stdio frames multiplexed as `{stdin|stdout|stderr|exit|signal}`.

`enishi/stream/1` as implemented (v0.1 step 3): every daemon keeps a second warm
connection per peer with this ALPN, dialed right after the ctl hello and dropped with it
(a close of either marks the peer offline). One bi-stream per session:

1. caller → `SessionKind::Exec(ExecRequest {command, shell, raw, cwd, env, timeout_secs})`
   or `SessionKind::Service {name, args}` (v0.1 step 5), `SessionKind::Mcp(ExecRequest)`
   (transparent MCP, §3.5) or `SessionKind::Tcp {service}` (one forwarded TCP
   connection, §3.6: raw bytes after the ack, no `ExecFrame`s);
2. peer → `SessionAck::Ok` or `SessionAck::Err {code, message}` within **3 s**, after
   authorizing (`authorize_exec` / `authorize_service`) and spawning (so spawn errors come
   back as acks). A service session runs the service's argv + allowed args, raw, with its
   `cwd`/`env`, and from then on is identical to exec;
3. caller → `ExecFrame::Stdin(bytes)…`, `StdinEof` (then finishes its send side);
   peer → `Stdout(bytes)` / `Stderr(bytes)` (≤ 64 KiB each) and exactly one terminal
   `Exit {code}` or `Error {code, message}` (e.g. `124` timeout).
4. The caller resets/stops the stream when its CLI goes away → the peer kills the tree.
   `signal` frames are not implemented yet.

Both ends log one timing line per session (`exec out` / `service out`: ack / first byte /
end / bytes; `exec in` / `service in`: spawn / first output / pipes EOF / reaped / exit /
out+err bytes) and append an audit line.

Framing: length-prefixed **postcard** frames (D3); the PoC used raw bytes + newline
header.

Presence (validated in PoC): heartbeat ~1.5 s, offline after a missed heartbeat, short
capped backoff, and a **hello on resume** sent by the device that slept (detected via wall
clock jump) so reconnect takes seconds instead of a backoff cycle.

`enishi/ctl/1` as implemented (v0.1 step 2): each daemon dials every configured peer and
keeps its own warm connection, so a pair of devices holds one ctl connection per
direction and each side measures presence independently. Every exchange is one
request frame + one response frame on a fresh bi-stream: `Hello {device, version,
instance, resume}` first, then `Ping {seq}` / `Pong {seq}` every 1.5 s (1.5 s timeout;
rtt = ping round trip). Unknown ids are rejected right after the TLS handshake. A
connection close from the peer marks it offline at once; a silent peer after one missed
heartbeat. A stopping daemon closes every peer connection (both directions) with code 4
`shutdown` first — the peer logs `closed by peer: shutdown (code 4)` within a ms — then
gives the endpoint at most 500 ms to drain instead of the full QUIC draining period
(3 PTO; with a peer offline the pending dials alone kept the old 2 s close timeout).
Retry backoff 250 ms doubling to 1.5 s. Receiving a hello: offline → skip the backoff;
online but a different `instance` (peer restarted) → drop and redial; online with
`resume = true` → probe with an immediate heartbeat. A daemon that sees its wall clock
jump > 5 s on a 1 s tick drops its links and redials at once with `resume = true`.
`Services` / `Services(Vec<ServiceInfo {name, kind, allow_args}>)` (v0.1 step 5) is the
catalog fetch: the receiver answers with the services the sender may open; the caller
sends it on its own ctl connection, so only an online peer can be asked (`255` otherwise).
`ServiceDetails {name}` / `ServiceDetails(Option<ServiceDetails {info, startup_timeout_ms}>)`
(0.1.0) fetches one service's settings the same way (`None`: not visible to the sender);
a peer older than 0.1.0 cannot decode it and drops the stream, which callers treat as
"no details" (defaults apply).

The CLI talks to the local daemon with the same framing: one `IpcRequest` / `IpcResponse`
per connection (`Status {device}`, `PairInvite`, `PairJoin`, `Reload`,
`Services {device}`, `ServiceDetails {device, service}`), except `Forward {device, service, bind, port}` (answers `Forwarding
{device, service, local}` then streams `Note`s until the CLI closes) and
`Exec {device, request}`, `Run {device, service, args}` and `Open {device, header}` (a
generic session carrier, used for transparent MCP):
the daemon answers `ExecStarted` (or `Error {code, message}`) and the connection then
carries the same `ExecFrame`s as the peer stream, relayed both ways. The CLI keeps the connection open
after stdin EOF; closing it means "caller gone".

## 6. Security

- Identity = Ed25519 key (iroh EndpointId). The only thing that authenticates a peer.
- Pairing: `enishi pair invite` prints a one-time code (id + secret, expires in minutes);
  the other side runs `enishi pair join <code>`; both store each other's id. No code reuse.
  Details in §6.1.
- Default deny; grants per peer (§4). Unknown ids are dropped during the handshake.
- No listening HTTP, no exposed debug ports. UDP only for QUIC.
- Audit log (JSONL) of every exec/service session: who, what, when, exit code.
  Implemented in v0.1 for exec: `<config dir>/audit.jsonl`, one line per session on both
  ends: `{ts, dir: in|out, peer, service?, cmd, shell?, raw, cwd?, env: [names only],
  exit, error?, duration_ms}`. Service sessions (step 5) carry `service` and log the
  spawned argv on the publishing side, the extra args on the calling side. Refused and
  offline attempts are logged too. No rotation yet.
- "Rejected unknown peer" warnings are rate-limited to one per peer id per minute.
- Relays: default public iroh relays only carry encrypted traffic; self-hosted relay
  supported for full independence (later).

### 6.1 Pairing as implemented (v0.1 step 4)

Pairing goes through the daemons on both sides (`252` if the local one is down).

```
A$ enishi pair invite [--name <A's name for B>] [--ttl 10m]
enishi1-…                                      # the code, on stdout alone
B$ enishi pair join enishi1-… [--name <B's name for A>]
paired with "server" as peer "server" (a1b2c3d4e5); exec and services stay denied …
```

- **Invite** (`IpcRequest::PairInvite`): the daemon draws a 16-byte secret (OS RNG) and
  keeps `{secret, name?, expires}` in memory only — a daemon restart drops open invites.
  TTL defaults to 10 min (`--ttl` accepts `90s`, `10m`, `1h` or plain seconds; max 24 h). More
  than one invite may be open; `--name` already taken (or invalid) is refused up front.
- **Code**: `enishi1-` + lowercase RFC 4648 base32 (no padding) of postcard
  `InviteCode {id: [u8; 32], secret: [u8; 16], device, addrs: Vec<SocketAddr>}` followed
  by a 4-byte FNV-1a checksum (catches truncated / mistyped pastes; integrity is not
  security — the secret and TLS are). `addrs` are dial hints with A's **own** addresses
  only: those on its fixed `listen_port` (without one: on the sockets the daemon is bound
  to now, valid as long as the in-memory invite) in directly reachable ranges —
  Tailscale/CGNAT 100.64/10, private IPv4, IPv6 ULA (fc00::/7) and global (2000::/3).
  Public IPv4 addresses and other ports are dropped: those are NAT mappings iroh observed
  through relays, useless as static hints (discovery/relay cover that path). Best first:
  Tailscale/CGNAT v4, 192.168/16, 10/8, Tailscale v6, other v6; container bridges
  (172.16/12) only when nothing better exists, max 8. Decoding is case-insensitive and
  ignores whitespace. Typically ~150–250 characters.
- **Handshake** (`enishi/pair/1`): B's daemon dials A with the code's id + addrs (iroh
  discovery/relay fill in otherwise) and opens one bi-stream: `PairRequest {secret,
  device, addrs}` → `PairResponse::Ok {device, addrs}` | `Err {message}`, within 10 s.
  B's id is taken from TLS, never from the message. A's handshake hook lets unknown ids
  in on this ALPN only while it has an unexpired invite out (`no pending invite`
  otherwise); every other ALPN still rejects unknown ids.
- **Checks on A**: secret compared in constant time against every open invite; expired
  → `invite expired`; already consumed → `invite already used`; unknown → `invalid
  invite`. The invite is consumed only when A has stored B (a refused join, e.g. name
  collision, leaves it usable). Used/expired invites are remembered for 1 h after expiry to give
  those specific errors. An id that is already a peer is refused (`already paired as
  <name>`). B refuses locally before dialing when its name for A is taken / A is
  already a peer, and when the code is its own.
- **Names**: each side names the other after the other's `[device] name`, sanitized to
  `[A-Za-z0-9._-]` (anything else → `-`), unless `--name` is given (invite → A's name for B,
  join → B's name for A). A collision is an error naming `--name` as the way out.
- **Stored**: each side appends `[peers.<name>] id = …` (+ `addrs` = the other side's
  stable hints, only when it has a fixed `listen_port`: the same own-address filter as
  invites, on that port)
  to **its own** `enishi.toml` with `toml_edit`, so comments and layout are kept; if
  `[device] default_peer` is unset it is set to the new peer. Grants are not written:
  the new peer can do nothing until `[grants.<name>]` is added (the CLI prints how).
  Then the daemon applies the file live (below) — the peer goes online in seconds.
  B writes its file only after A answered Ok; if B then fails to write, A has B and B
  has not (re-pair after removing B from A).

**Reload** (`enishi reload`, `IpcRequest::Reload`): re-reads `enishi.toml`; on a parse
error the running config is kept. Peers added are dialed, removed/changed ones have
their links closed (and changed ones re-dialed), the handshake allow-list is updated;
grants and services are read per session, so they apply to new sessions (running
sessions keep going). `[device] name` / `listen_port` changes are reported as needing a
restart. There is no file watcher: edit, then `enishi reload`. Pairing uses the same
path, so it also picks up pending manual edits.

## 7. Platform specifics

**Installing (v0.1 step 6).** No installer packages: the binary installs itself, per user,
with no admin rights; the firewall step asks for them once (Windows UAC / an offered
`sudo` on Linux) and is skippable (`--no-firewall`).
`enishi install` copies the *running* binary to the install dir, registers autostart,
starts it and waits until the daemon answers over IPC. Re-running it (from a newer
binary) is the upgrade path; it is idempotent. Each install writes a small record
(`<data dir>/enishi/installs/<name>.json`: binary paths, pinned config, PATH change) so
`enishi uninstall` reverses exactly what was done. `--name` (default `enishi`) names the
unit/task, so a test instance can live next to the real one. If no config exists, install
writes a minimal `[device] name = <hostname>`. When the config is non-default
(`--config` / `ENISHI_CONFIG` at install time) it is **pinned** into the unit/task, so
the service runs that instance. Install refuses to overwrite (and uninstall to remove) a
unit/task it did not write (marker in the unit file / task description).
`uninstall --purge --yes` also deletes the config dir (config, identity key, audit and
daemon logs) and, for the default instance, the MCP cache; without `--yes` it only lists
what it would delete.

**Logs.** The service/task runs `enishi up --log-file`: tracing goes to stderr (journald
on Linux) **and** to `<config dir>/enishi.log` (no ANSI), rotated at 5 MiB to
`enishi.log.1` (one generation). Panics are logged there too (a windowless daemon has no
stderr). `enishi logs [-n N] [-f]` prints/follows it across rotations.

**Windows**
- The daemon must run **in the user's interactive session**, not as a service (session 0
  can't touch the desktop). It is a **logon scheduled task** (`schtasks /Create /XML`,
  Task Scheduler 1.2 schema): `LogonTrigger` for the current user, `InteractiveToken`
  (runs only while the user is logged on, in session 1+, no stored password),
  `LeastPrivilege`, `ExecutionTimeLimit PT0S`, `RestartOnFailure` every 1 min (999×),
  `MultipleInstancesPolicy IgnoreNew`, battery restrictions off, priority 5 (normal —
  the schtasks default 7 would make every `exec` child below-normal). An optional tiny
  SYSTEM service for presence/wake before logon is v0.3+.
- **No console window**: install puts two copies in `%LOCALAPPDATA%\Programs\enishi`:
  `enishi.exe` (console subsystem, the CLI; that dir is added to the **user** `PATH` in
  `HKCU\Environment`, keeping `REG_EXPAND_SZ`, + `WM_SETTINGCHANGE` broadcast) and
  `enishiw.exe`, the same image with the PE optional-header *Subsystem* field flipped to
  GUI (what `editbin /SUBSYSTEM:WINDOWS` does; 2 bytes, checksum not enforced for
  user-mode images). The task runs `enishiw.exe up --log-file`: Windows never allocates a
  console for it, so nothing flashes or stays open, independent of how the task is
  started (a hidden-window task setting or a `--background` re-spawn can still flash or
  leave a conhost). Children it spawns already use `CREATE_NO_WINDOW`. Without a console,
  Ctrl-C handler registration fails; the daemon then just waits (stopped by the task /
  `TerminateProcess`).
- **Upgrade with the exe locked**: install ends the task, terminates any process whose
  image path is exactly the installed `enishiw.exe` (never by name — a dev/live
  `enishi.exe` elsewhere is untouched), then for each file deletes it or, if still locked
  (e.g. the CLI copy running the install), renames it to `*.old-<ms>` (allowed for a
  running image) and writes the new one; leftovers are cleaned on the next
  install/uninstall. Uninstall run from the installed `enishi.exe` deletes that file from
  a detached `cmd` after it exits.
- **Firewall** (automatic in `install`, also `enishi firewall`): one inbound **UDP** rule
  named after the install, **Private profile only**, scoped to `enishiw.exe` and
  `enishi.exe`; it first deletes every rule bound to those program paths (what the "allow
  access" prompt creates: allow *or* block, TCP+UDP, often Public too). Install reads the
  rules unelevated (`Get-NetFirewallRule` + application/port filters) and decides: exactly
  our two rules → nothing; anything else → fix. The fix runs **before the task starts** (so
  the daemon never listens without a rule and Windows never shows its own prompt, which
  leaves Block rules when ignored): unless already elevated, `ShellExecuteExW` with the
  `runas` verb launches the *installed* `enishi.exe firewall --elevated …` hidden (one UAC
  prompt) and waits for its exit code. Declined (`ERROR_CANCELLED`), failed or unanswered
  within 60 s → install continues and prints one line: enishi works without the rule
  (outbound-initiated link), add it later with `enishi firewall`. Upgrades only prompt when
  the rules are not already right. `enishi firewall` self-elevates the same way;
  `--check` only reports. Uninstall removes the rules with the same single prompt (skipped
  when there are none; declined or `--no-firewall` → prints the elevated `netsh` line).
  `install --firewall` is accepted as a hidden no-op for compatibility.
- Default `exec` shell: PowerShell (pwsh if present, else Windows PowerShell 5.1).
- Interactive `shell` uses ConPTY.

**Linux**
- `systemd --user` unit `~/.config/systemd/user/<name>.service`: `ExecStart=<bin> up
  --log-file`, `Restart=on-failure`, `WantedBy=default.target`, pinned config as
  `Environment=ENISHI_CONFIG=…`; `daemon-reload`, `enable`, `restart` (so an upgrade picks
  the new binary up; the binary itself is replaced by an atomic rename, safe while it
  runs). Binary in `~/.local/bin/enishi` (or `--prefix`). Firewall: no per-app prompts on
  Linux, but distros may block inbound UDP. Install detects an active firewalld
  (`firewall-cmd --state`, falling back to the unit state since `--state` may need polkit)
  or ufw (`ufw status verbose`; unreadable without root → "unknown") and checks the
  daemon's `listen_port`/udp in the zone of the default route's interface (firewalld
  `--query-port`, which matches ranges). Closed/unknown: with a terminal it asks
  `Open UDP <port> in <tool>? [y/N]` and runs the exact sudo commands (firewalld runtime +
  `--permanent`, no reload; `ufw allow <port>/udp`), sudo prompting for the password;
  without one it prints them. Never sudo non-interactively. No `listen_port` → says the
  link works outbound-only and suggests setting one. `install.sh` hands `/dev/tty` to
  `install` when piped. Install checks
  `loginctl show-user $USER -p Linger` and, if off, prints the
  `sudo loginctl enable-linger <user>` hint for headless servers (never runs sudo).
- Local IPC: Unix socket (0600) in `$XDG_RUNTIME_DIR/enishi.sock`, falling back to the
  config dir. Windows: named pipe `\\.\pipe\enishi-<user>` (remote clients rejected).
  Instances started with a non-default `--config` use `<config dir>/enishi.sock` /
  `\\.\pipe\enishi-<user>-<hash of config path>`.

**Releases.** Tag `vX.Y.Z` (must equal the crate version) → `.github/workflows/release.yml`
tests and builds `x86_64-unknown-linux-gnu` (on ubuntu-22.04 for an older glibc) and
`x86_64-pc-windows-msvc`, and publishes a GitHub Release with the two binaries and
`SHA256SUMS`. The README's one-liners download a binary, verify it against `SHA256SUMS`
and run `install` (integrity only: both files come from the same release).

**Updating (v0.1.1, bug #11).** Upgrading by re-running the installer *through* enishi
(`enishi -d pc exec -- irm …/install.ps1 | iex`) used to leave the device with no daemon: the
installer ran inside the daemon's lifetime — on Windows in the logon task's job object (ending
the task kills the whole job), on Linux in the unit's cgroup (`systemctl restart` kills every
process in it) — so stopping the old daemon killed the installer half-way. `enishi update`
therefore runs in two stages (D17):

1. **Stage 1** (the `enishi update` process, wherever it runs): resolve the install (the
   record whose binary is running, else `--name`, default `enishi`) and its config; pick the
   source — the newest release from `api.github.com/…/releases?per_page=1` (pre-releases
   included, like `install.sh`/`install.ps1`), `--version vX.Y.Z`, or `--from-file` — download
   this OS's asset and check it against the release's `SHA256SUMS`; stage it in
   `<data dir>/enishi/update/<name>/` and probe it (`--version` must print `enishi <ver>`,
   matching the tag); copy itself next to it as the updater (Windows: GUI-subsystem copy, so
   no console window even under Task Scheduler); then launch stage 2 **outside the daemon's
   lifetime**:
   - Linux: `systemd-run --user --collect --unit=<name>-update-<id>` (its own transient
     unit, so outside the daemon's cgroup; output appended to `update.log`); fallback
     `setsid -f` when systemd-run is unavailable.
   - Windows: `CreateProcess` with `CREATE_BREAKAWAY_FROM_JOB | DETACHED_PROCESS |
     CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW`, created suspended and resumed only if
     `IsProcessInJob` says it left every job (the `exec` job sets
     `JOB_OBJECT_LIMIT_BREAKAWAY_OK`; Task Scheduler's own job may not allow it). Otherwise a
     one-shot scheduled task `<name>-update-<id>` (no trigger, interactive token, 30 min limit)
     runs the updater, which deletes the task when done.
2. **Stage 2** (detached, `update --stage2`): back up the installed binaries, stop the daemon
   (`systemctl --user stop` / `schtasks /End` + kill by exact image path), install the staged
   binary with the normal install code (`--no-start`, firewall untouched), start the daemon and
   wait (30 s) until it answers over IPC **with the new version**. Any failure after the stop
   → **rollback**: restore the backed-up binaries and the old install record, start the
   daemon again and wait for it. Stage 2 waits 1–2 s before stopping so the caller (an `exec`
   session, a remote ack) gets its answer first.

Both stages write `update.json` next to the config (id, origin `local` / `exec` / `peer:<name>`,
from/to version, source, state `running|done|failed|rolled_back|rollback_failed`, and a phase
timeline: `requested`, `downloaded`, `verified`, `detached`, `stopped`, `swapped`, `started`,
`done` | `failed` | `rolling_back` → `rolled_back` / `rollback_failed`) and append to
`update.log`. A second update is refused while one is `running` (stale after 15 min).
`enishi install` run from inside the installed daemon (an `exec` child: same cgroup / an
ancestor is the installed `enishiw.exe`) hands over to the same detached path.

**Remote update.** `enishi -d dev update [--version v | --from-file PATH-on-dev]` sends
`CtlRequest::Update`; the device checks `[grants.<caller>] update = true` (D16), audits it,
starts stage 1 as a child of its daemon (which detaches stage 2 as above) and acks at once with
the update id. The caller then polls `CtlRequest::UpdateStatus` (version + `update.json`) every
second, printing new phases; the link drops while the daemon restarts (expected, reported) and
the command returns when the update with that id reaches a final state (exit 0 only for
`done` with the device reporting the new version), or after `--timeout` (default 180 s).
`--push` (sending the caller's binary over a stream instead of downloading on the device) is
not implemented yet; `--from-file` with a path on the device covers offline devices.
`ENISHI_RELEASES_API` / `ENISHI_RELEASES_URL` override the GitHub endpoints (mirrors, tests);
`ENISHI_UPDATE_FAIL_AT=stop|swap|start|health` makes stage 2 fail on purpose (tests only).

## 8. Repository layout

```
crates/
  enishi-proto     frames, headers, error codes (no I/O)
  enishi-core      daemon: endpoint, peers, presence, sessions, ACL, config, IPC
  enishi-svc       service host: process spawn, PTY/ConPTY, exec, stdio services
  enishi-mcp       MCP shim (cache, offline replies, handshake replay)
  enishi           the CLI binary (clap) — thin, wires everything
docs/              design, examples
```

Packaging needs no `packaging/` dir: the unit, the task XML and the firewall rule are
generated by `enishi install` (`crates/enishi/src/install/`, pure renderers unit-tested on
every OS). CI (GitHub Actions): fmt, clippy, tests on Linux + Windows; tag `v*` → release
builds for `x86_64-unknown-linux-gnu` and `x86_64-pc-windows-msvc` + `SHA256SUMS` (§7).

## 9. Milestones

| | Scope | Done when |
|---|---|---|
| **v0.1** | daemon, pairing, presence, `status`, `exec` (non-interactive), `run`, `mcp`, config + grants, Win logon task, Linux systemd unit | Server ↔ Windows both ways; sleep test passes; an MCP works through `enishi mcp` in a sandboxed agent |
| v0.2 | `shell` (PTY/ConPTY), `wake`, service catalog sync (hello hash; v0.1 has on-demand `enishi services`), installers | daily driver replacing SSH→WSL |
| v0.3 | macOS, self-hosted relay, pre-logon presence service | third device |
| later | Android, file/clipboard services, Windows UI/browser services on top | — |

## 10. Decisions

| | Decision |
|---|---|
| **D1** | Device selection with `-d <name>` / `--device <name>`; default peer from config. No `@name`, no dynamic flags. |
| **D2** | `exec` runs through the platform shell by default (PowerShell on Windows, `$SHELL`/`sh` on Linux); `--raw` spawns the process directly. |
| **D3** | Wire frames: length-prefixed postcard. |
| **D4** | Config: TOML, `enishi.toml` per device. |
| **D5** | `exec` grant is allow/deny per peer in v0.1, modelled as `ExecPolicy` so allowlists drop in later. `Rules {allow, deny}` parses but is denied until enforced (fail closed). |
| **D6** | Sessions use a dedicated warm `enishi/stream/1` connection next to the ctl one (not extra streams on ctl), so bulk stdio (own congestion window) cannot delay heartbeats into false offlines, while presence still owns the link lifecycle. |
| **D7** | Exec authorization, spawn and ack happen before any stdio flows; a spawn failure is an ack error (`126`/`127`), never a half-open session. |
| **D8** | Pairing is daemon-to-daemon on its own ALPN `enishi/pair/1` with a one-time, TTL-bound, in-memory invite; it adds the peer to each side's config with no grants and hot-applies it. Config edits are applied with an explicit `enishi reload` (no file watcher). |
| **D9** | Services are double-gated: `[services.S] allow` lists the peer **and** `[grants.<peer>] services` lists `S`; extra args only with `allow_args`. Unknown → `253`, refused → `254`. Services spawn argv directly (no shell) through the exec spawn path. enishi ships no services; the MCP shim is protocol-generic (JSON-RPC framing + handshake only). |
| **D10** | Config management commands (`service add/remove/import`, `grant`) edit `enishi.toml` with `toml_edit` and then reload the daemon; the file stays the only source of truth. `grant --service` writes both D9 gates in one command (and `--revoke-service` removes both). |
| **D11** | `service import <format>` uses a pluggable `Importer` registry in the generic layer; each format lives with its domain (`mcp` in `enishi-mcp`). Remote/HTTP MCP entries are skipped with a note, never turned into services. |
| **D12** | Raw spawns (services, `exec --raw`, transparent MCP) resolve the program on Windows through `PATH` × `PATHEXT` (never the cwd) to a full path; `.cmd`/`.bat` are then launched by Rust std, which since 1.77 runs them via `cmd.exe /e:ON /v:OFF /d /c` with BatBadBut-safe quoting (and refuses args it cannot quote safely). |
| **D13** | Transparent MCP (`enishi mcp -d dev -- cmd…`) needs its own grant `mcp = true` (`McpPolicy`, same enum as `ExecPolicy`, one `authorize_mcp`). `exec = true` does not imply it: `exec` is for humans/scripts with a shell, `mcp` is what an agent config may trigger unattended; keeping them apart lets a device allow one without the other (and later allowlist MCP packages only). Published services keep working with services grants alone. |
| **D14** | TCP forwarding: `kind = "tcp"` services (`target`), one QUIC bi-stream per TCP connection on the warm stream connection, connect-before-ack, half-close preserved, fast fail while the peer is offline with the listener kept. Listeners live in the daemon (`enishi forward` holds one for its lifetime; `[forwards.*]` for the daemon's). |
| **D15** | Packaging is the binary itself: `enishi install/uninstall` per user (systemd user unit / logon scheduled task + user PATH), no admin except the Private-only firewall rule, which install sets automatically with one UAC prompt before the daemon first listens (decline → still works outbound; Linux: offered sudo for firewalld/ufw); idempotent re-install = upgrade; an install record makes uninstall exact. Windows runs a GUI-subsystem copy (`enishiw.exe`) so the daemon never has a console window. The service logs to a 5 MiB-rotated file in the config dir (`enishi logs`). |
| **D16** | Remote update needs its own grant `[grants.<peer>] update = true` (default deny, `enishi grant <peer> --update`, one `authorize_update`). `exec = true` does not imply it: exec can already run an installer, but `update` is a narrow, audited capability (fixed binary source, checksum, rollback) that a device may want to give a peer that has no shell at all. `update --status` needs the same grant. |
| **D17** | Updates never run inside the daemon's lifetime: stage 1 verifies and stages, stage 2 is detached (Linux: transient `systemd-run --user` unit, fallback `setsid -f`; Windows: job breakaway checked with `IsProcessInJob`, fallback one-shot scheduled task) and owns stop → swap → start → health check (new version over IPC), rolling back to the backed-up binary and record on any failure after the stop. Progress is a phase timeline in `update.json` (+ `update.log`), which is also what a remote caller polls. |

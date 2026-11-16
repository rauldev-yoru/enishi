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
| `enishi up` | Run the daemon (foreground; installers wrap it as a service / logon task). |
| `enishi id` | Print this device's public id. |
| `enishi pair invite` / `enishi pair join <code>` | One-time pairing (see §6). |
| `enishi reload` | Re-read `enishi.toml` into the running daemon (peers, grants, services). |
| `enishi status [-d dev]` | Peers, presence (`online`, `asleep/offline`, last seen), latency. |
| `enishi exec [-d dev] [opts] -- <cmd> [args…]` | Run a native command on the peer; stdin/stdout/stderr streamed, exit code propagated. |
| `enishi shell [-d dev]` | Interactive shell on the peer (PTY / ConPTY). v0.2. |
| `enishi run [-d dev] <service>` | Pipe local stdio to a published service (raw). |
| `enishi mcp [dev/]<service> [-- extra args]` | Same as `run`, MCP-aware: cached `initialize`/`tools/list`, offline errors, handshake replay. This is what agents put in their config. |
| `enishi services [-d dev]` | List services the peer publishes to us. |
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
kind = "mcp"                  # mcp | stdio
allow = ["server"]            # peer names
allow_args = false

[grants.server]               # what peer "server" may do on this device
exec = true                   # run arbitrary commands (off by default)
services = ["fs", "ps"]
```

The config location can be overridden with the global `--config <path>` flag or the
`ENISHI_CONFIG` environment variable; the key and the daemon's IPC endpoint then live next
to that file, so several instances can run on one host (integration tests, multiple
identities). `identity.key` (32 raw bytes) is created on first use (`enishi id` or
`enishi up`); the daemon refuses a key readable by other users on unix.

Rule: **everything is denied unless granted**, per peer. `exec` is the most powerful grant
and is opt-in.

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
   (`Service {name, args}` is reserved and answered with `253` for now);
2. peer → `SessionAck::Ok` or `SessionAck::Err {code, message}` within **3 s**, after
   authorizing with `authorize_exec` and spawning (so spawn errors come back as acks);
3. caller → `ExecFrame::Stdin(bytes)…`, `StdinEof` (then finishes its send side);
   peer → `Stdout(bytes)` / `Stderr(bytes)` (≤ 64 KiB each) and exactly one terminal
   `Exit {code}` or `Error {code, message}` (e.g. `124` timeout).
4. The caller resets/stops the stream when its CLI goes away → the peer kills the tree.
   `signal` frames are not implemented yet.

Both ends log one timing line per session (`exec out`: ack / first byte / end / bytes;
`exec in`: spawn / first output / pipes EOF / reaped / exit / out+err bytes) and append
an audit line.

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
heartbeat. Retry backoff 250 ms doubling to 1.5 s. Receiving a hello: offline → skip the
backoff; online but a different `instance` (peer restarted) → drop and redial; online with
`resume = true` → probe with an immediate heartbeat. A daemon that sees its wall clock
jump > 5 s on a 1 s tick drops its links and redials at once with `resume = true`.

The CLI talks to the local daemon with the same framing: one `IpcRequest` / `IpcResponse`
per connection (`Status {device}`, `PairInvite`, `PairJoin`, `Reload`), except `Exec {device, request}`: the daemon answers
`ExecStarted` (or `Error {code, message}`) and the connection then carries the same
`ExecFrame`s as the peer stream, relayed both ways. The CLI keeps the connection open
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
  ends: `{ts, dir: in|out, peer, cmd, shell?, raw, cwd?, env: [names only], exit,
  error?, duration_ms}`. Refused and offline attempts are logged too. No rotation yet.
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
  security — the secret and TLS are). `addrs` are A's current direct addresses as dial
  hints, best first: Tailscale/CGNAT v4, 192.168/16, 10/8, other v4, Tailscale v6,
  other v6; loopback/link-local/unspecified/multicast dropped, container bridges
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
  stable hints, only when it has a fixed `listen_port`, and only addresses on that port)
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

**Windows**
- The daemon must run **in the user's interactive session**, not as a service (session 0
  can't touch the desktop). Installer registers a **logon scheduled task**; an optional
  tiny SYSTEM service for presence/wake before logon is v0.3+.
- Installer adds a firewall rule scoped to `enishi.exe`, private profile only (PoC showed
  the default prompt allows Public too).
- Default `exec` shell: PowerShell (pwsh if present, else Windows PowerShell 5.1).
- Interactive `shell` uses ConPTY.

**Linux**
- `systemd --user` unit (+ `loginctl enable-linger` for headless servers).
- Local IPC: Unix socket (0600) in `$XDG_RUNTIME_DIR/enishi.sock`, falling back to the
  config dir. Windows: named pipe `\\.\pipe\enishi-<user>` (remote clients rejected).
  Instances started with a non-default `--config` use `<config dir>/enishi.sock` /
  `\\.\pipe\enishi-<user>-<hash of config path>`.

## 8. Repository layout

```
crates/
  enishi-proto     frames, headers, error codes (no I/O)
  enishi-core      daemon: endpoint, peers, presence, sessions, ACL, config, IPC
  enishi-svc       service host: process spawn, PTY/ConPTY, exec, stdio services
  enishi-mcp       MCP shim (cache, offline replies, handshake replay)
  enishi           the CLI binary (clap) — thin, wires everything
docs/              design, protocol, security, install
packaging/         windows (task + firewall + installer), linux (systemd unit)
```

CI (GitHub Actions): fmt, clippy, tests on Linux + Windows, release builds for
`x86_64-unknown-linux-gnu` and `x86_64-pc-windows-msvc`.

## 9. Milestones

| | Scope | Done when |
|---|---|---|
| **v0.1** | daemon, pairing, presence, `status`, `exec` (non-interactive), `run`, `mcp`, config + grants, Win logon task, Linux systemd unit | Server ↔ Windows both ways; sleep test passes; an MCP works through `enishi mcp` in a sandboxed agent |
| v0.2 | `shell` (PTY/ConPTY), `wake`, service catalog sync, audit log for services (exec audit is in v0.1), installers | daily driver replacing SSH→WSL |
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

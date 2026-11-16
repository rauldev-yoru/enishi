# enishi — Design (v0.1 draft)

Status: draft for review. Nothing here is implemented yet beyond the proof of concept
(`link-poc`), whose measured results are in the README.

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
enishi exec ls                      # default device
enishi -d windows-home exec dir     # explicit device
enishi @windows-home exec dir       # short form (same thing)
```

> Why not `--windows-home` as a flag? Device names are user data, not part of the CLI
> grammar: dynamic flags would clash with real flags (`--version`, `--help`, a device
> named `status`…), break shell completion and make parsing ambiguous. `-d <name>` /
> `@name` keeps the grammar fixed and still reads naturally. **Open decision (D1).**

### 3.2 Commands

| Command | What it does |
|---|---|
| `enishi up` | Run the daemon (foreground; installers wrap it as a service / logon task). |
| `enishi id` | Print this device's public id. |
| `enishi pair invite` / `enishi pair join <code>` | One-time pairing (see §6). |
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
  reserved codes (e.g. `255 peer offline`, `254 not allowed`).
- `--timeout`, `--cwd`, `--env K=V`, `--shell <pwsh|powershell|cmd|bash|sh>`, `--raw`.
- stdout and stderr are kept separate (two streams), unlike the PoC.

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
exec = true                   # may *I* be asked to exec by this peer? see [grants]

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

Rule: **everything is denied unless granted**, per peer. `exec` is the most powerful grant
and is opt-in.

## 5. Protocol (L2/L3)

iroh connection per peer pair, kept warm by the daemon. ALPNs:

- `enishi/ctl/1` — control: hello (device name, version, capabilities, service catalog
  hash), heartbeat, presence, catalog fetch, wake hints.
- `enishi/stream/1` — one QUIC bidirectional stream per session. A session starts with a
  small header frame `{kind: exec|service, target, args, cwd, env, pty}`, the peer answers
  `{ok}` or `{err, code, message}` (ack within a bounded timeout — PoC lesson), then raw
  stdio frames multiplexed as `{stdin|stdout|stderr|exit|signal}`.

Framing: length-prefixed frames (CBOR or postcard) — **open decision (D3)**; the PoC used
raw bytes + newline header.

Presence (validated in PoC): heartbeat ~1.5 s, offline after a missed heartbeat, short
capped backoff, and a **hello on resume** sent by the device that slept (detected via wall
clock jump) so reconnect takes seconds instead of a backoff cycle.

## 6. Security

- Identity = Ed25519 key (iroh EndpointId). The only thing that authenticates a peer.
- Pairing: `enishi pair invite` prints a one-time code (id + secret, expires in minutes);
  the other side runs `enishi pair join <code>`; both store each other's id. No code reuse.
- Default deny; grants per peer (§4). Unknown ids are dropped during the handshake.
- No listening HTTP, no exposed debug ports. UDP only for QUIC.
- Audit log (JSONL) of every exec/service session: who, what, when, exit code.
- Relays: default public iroh relays only carry encrypted traffic; self-hosted relay
  supported for full independence (later).

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
- Local IPC: Unix socket in `$XDG_RUNTIME_DIR/enishi.sock`. Windows: named pipe.

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
| v0.2 | `shell` (PTY/ConPTY), `wake`, service catalog sync, audit log, installers | daily driver replacing SSH→WSL |
| v0.3 | macOS, self-hosted relay, pre-logon presence service | third device |
| later | Android, file/clipboard services, Windows UI/browser services on top | — |

## 10. Open decisions

- **D1** device selection syntax: `-d name` + `@name` (proposed) vs. dynamic `--name` flags.
- **D2** exec default: through the platform shell (proposed) vs. raw process by default.
- **D3** frame encoding: postcard vs. CBOR vs. JSON-lines.
- **D4** project config format/location (TOML proposed).
- **D5** whether `exec` grant is per-peer boolean (proposed) or command allowlists in v0.1.

# enishi 縁

> *enishi* (縁) — the bond that ties people together. Here: the bond between your devices.

**enishi** is one small native binary that links your machines peer-to-peer (Linux ↔ Windows
today; macOS and phone later). Once two devices are paired you can, from either side:

- **run native commands** on the other one (`enishi exec`), like SSH but without SSH;
- **use its services**: any stdio program, any MCP server, or any local TCP port (`run`, `mcp`, `forward`);
- **not care whether it is asleep**: presence is tracked, calls to a sleeping device fail in
  milliseconds instead of hanging, and everything reconnects on its own when it wakes up.

It was built for AI agents that live on a server but need to act on your PC (your real browser,
your apps, your corporate VPN), and it is just as useful from a plain terminal.

Status: **v0.1 in progress** — link, presence, pairing, `exec`, services, MCP and TCP forwarding
work and are tested Linux ↔ Windows, and `enishi install` sets it up as a per-user service.

## What enishi is (and is not)

| enishi is | enishi is not |
|---|---|
| A P2P link between *your* devices: identity by key, encrypted QUIC ([iroh](https://github.com/n0-computer/iroh)), NAT traversal | A VPN for arbitrary traffic |
| A way to publish and consume services across devices, with per-device permissions | A bundle of MCP servers or tools — you bring your own |
| Sleep-aware: presence, instant `peer offline`, auto-reconnect, resume hints | An HTTP gateway (no listening HTTP, no exposed debug ports) |

MCP support is a layer on top of the generic service machinery, not the core. On Windows the
daemon runs in the user's session (not session 0), so UI-capable tools work.

## Install

Per user, no admin rights. One command downloads the latest release, verifies its checksum and
runs `enishi install` (binary on your PATH + autostart + daemon started):

```bash
# Linux (x86_64)
curl -fsSL https://raw.githubusercontent.com/rauldev-yoru/enishi/main/install.sh | sh
```

```powershell
# Windows (PowerShell, no admin)
irm https://raw.githubusercontent.com/rauldev-yoru/enishi/main/install.ps1 | iex
# from cmd:
powershell -NoProfile -c "irm https://raw.githubusercontent.com/rauldev-yoru/enishi/main/install.ps1 | iex"
```

Pin a version with `ENISHI_VERSION=v0.1.0`. Prefer to do it by hand? Download the binary for your
OS from [Releases](https://github.com/rauldev-yoru/enishi/releases), check it against
`SHA256SUMS`, and run it with `install`. Or build it: `cargo build --release -p enishi`.

```bash
enishi status
enishi logs -f          # the daemon's log
```

(The checksum guards against a corrupted or swapped download; it comes from the same release,
so it is not a signature.)

What `install` does:

- **Linux**: binary in `~/.local/bin/enishi`, a `systemd --user` unit (`enishi.service`,
  `Restart=on-failure`) enabled and started. For a headless server that should run enishi
  without anyone logged in, it prints the `sudo loginctl enable-linger $USER` command if
  lingering is off.
- **Windows**: `enishi.exe` (CLI, added to your user `PATH`) and `enishiw.exe` (the same
  program built as a windowless app) in `%LOCALAPPDATA%\Programs\enishi`, and a logon
  scheduled task running `enishiw.exe up` in your session: starts when you log on, restarts on
  failure, no time limit, no console window. Before the daemon first listens, install asks for
  admin rights **once** (one UAC prompt) to add a single inbound UDP rule for enishi on
  **Private** networks only and remove the broader allow/block rules Windows creates on first
  use — so Windows never shows its own "allow access" dialog. Upgrades only ask if the rules
  changed. Say no and enishi still works (links are started outbound); add the rule later with
  `enishi firewall` (`--check` reports, `--remove` undoes). `--no-firewall` skips the step.
- **Linux firewall**: if firewalld or ufw is active and blocks the daemon's `listen_port`/udp,
  install offers to run the exact `sudo firewall-cmd …` / `sudo ufw allow …` command (asks
  first; without a terminal it only prints it). No fixed `listen_port` → outbound-only, which
  still works.

Re-running `enishi install` from a newer binary upgrades in place (stops the daemon, swaps the
files, starts it again). `enishi uninstall` removes the service/task, the `PATH` entry and the
binaries; `--purge --yes` also deletes the config, the device identity and the logs.
Options: `--prefix DIR` (install dir), `--name NAME` (unit/task name, to run a second instance,
e.g. with `--config`), `--no-start`, `--no-firewall`. A non-default config (`--config` / `ENISHI_CONFIG`) given at
install time is pinned into the service.

## Quick start

```bash
# on both devices
enishi install                             # or run `enishi up` in a terminal

# device A
enishi pair invite                         # prints a one-time code (valid 10 min)
# device B
enishi pair join enishi1-…                 # both sides now know each other
enishi status
```

A new peer can do **nothing** until you grant it something:

```bash
enishi grant server --exec                 # let "server" run commands here
enishi grant server --service filesystem   # let it use one published service
enishi grant server --mcp                  # let it start MCP servers here (transparent mode)
```

## Run commands

```bash
enishi exec -- whoami                                   # default device
enishi -d windows-home exec -- Get-ChildItem C:\Users   # PowerShell by default on Windows
enishi -d windows-home exec --shell cmd -- ver
enishi -d server exec --cwd ~/projects -- git status    # from Windows to Linux
echo data | enishi exec -- wc -c                        # stdin is streamed
```

stdout/stderr stay separate and the remote exit code becomes enishi's exit code. Flags:
`--shell`, `--raw` (no shell), `--cwd`, `--env K=V`, `--timeout`. Reserved codes: `255` peer
offline, `254` not allowed, `253` no such service, `252` local daemon not running.

## Publish services

No need to edit config files by hand — these commands write `enishi.toml` and reload the daemon:

```bash
enishi service add filesystem --mcp -- npx -y @modelcontextprotocol/server-filesystem C:\Users\me\Projects
enishi service add sort -- sort                         # any stdio program
enishi service add web --tcp localhost:3000             # a local TCP port
enishi service import mcp claude_desktop_config.json    # reuse existing mcpServers configs
enishi service list
enishi -d windows-home services                         # what a peer publishes to you
```

`npx`, `uvx`, `pnpm`… work on Windows exactly as in their docs (`.cmd` shims are resolved safely).

## Use MCP servers from an agent

Two ways, both keep `command: enishi`:

**Transparent** — reads like the original MCP config, just prefixed (needs `enishi grant <peer> --mcp`):

```yaml
command: enishi
args: [mcp, -d, windows-home, --, npx, -y, "@modelcontextprotocol/server-filesystem", "C:/Users/me/Projects"]
```

**Published** — the device owner decides exactly what is exposed; extra flags after `--` are
appended only if the service allows it:

```yaml
command: enishi
args: [mcp, windows-home/filesystem]
```

Get a ready-to-paste snippet for your agent:

```bash
enishi mcp config windows-home/filesystem --for hermes     # or claude | openclaw | json
```

While the device sleeps the agent still starts fine: `initialize` and `tools/list` come from a
local cache, tool calls fail instantly with a clear error, and the session resumes transparently
when the device is back. Walkthrough: [docs/examples/filesystem-mcp.md](docs/examples/filesystem-mcp.md).

## Forward a TCP port

```bash
# on the server: publish its local dev server
enishi service add web --tcp localhost:3000
# on Windows: open it locally (same port by default)
enishi -d server forward web           # → http://localhost:3000
enishi -d server forward web 8080      # → http://localhost:8080
```

No extra ports are opened on the publishing side and nothing listens beyond loopback by default:
every TCP connection is a stream inside the one encrypted link. Forwards can also be declared in
`enishi.toml` (`[forwards.<name>]`) so the daemon keeps them up.

## Security

- Identity is an Ed25519 key per device; only paired keys are accepted.
- Pairing uses one-time, expiring invites.
- Everything is denied unless granted per peer: `exec`, transparent `mcp` and each service are
  separate grants.
- Every session is written to a local audit log (`audit.jsonl`).

## Measured (Linux server ↔ Windows 11, LAN)

| | |
|---|---|
| `exec whoami` | p50 32 ms (≈4 ms enishi + network, the rest is Windows starting the process) vs 259 ms SSH→WSL |
| MCP `tools/call` (warm) | ~2 ms |
| TCP forward, 50 MB | ~110 MB/s Windows ← Linux, sha256 verified |
| Device goes to sleep | detected in ~3 s; calls meanwhile answer `peer offline` in ~3 ms |
| Device wakes up | back online ~4 s after wake, no action needed |

## Docs

- [Design](docs/design.md) — layers, protocol, decisions
- [Example: filesystem MCP on Windows](docs/examples/filesystem-mcp.md)

## License

Dual-licensed under MIT or Apache-2.0, at your option.

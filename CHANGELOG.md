# Changelog

All notable changes to enishi are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project uses
[Semantic Versioning](https://semver.org/).

## [0.1.1] — 2026-11-16

### Fixed

- **#11** Updating a device through enishi (`enishi -d pc exec -- irm …/install.ps1 | iex`,
  or `enishi install` from an `exec`) could leave it with **no daemon**: the installer ran
  inside the daemon's job object (Windows) / the unit's cgroup (Linux), so stopping the old
  daemon killed it half-way. Updates now run in a detached updater outside the daemon's
  lifetime (systemd-run transient unit; Windows job breakaway, else a one-shot scheduled
  task) that rolls back to the old binary if anything fails after the stop. `enishi install`
  run from inside the installed daemon hands over to it automatically, and `exec` children
  may now break away from their job object on purpose.

### Added

- `enishi update [--version vX.Y.Z | --from-file PATH] [--check] [--status]`: downloads the
  release asset for this OS (newest, pre-releases included, like the install scripts),
  verifies it against `SHA256SUMS` and `--version`, then stop → swap → start → health check
  (the daemon must answer with the new version), with rollback. Phases are recorded in
  `update.json` / `update.log` next to the config.
- Remote update: `enishi -d <device> update [...]` asks the device's daemon to update
  itself, follows it through the restart (link down → back online) and prints the phase
  timeline and result (`--timeout`, default 180 s; `--no-wait`; `--status`).
- `update` grant (`[grants.<peer>] update = true`, `enishi grant <peer> --update`), default
  deny and **not** implied by `exec`; remote update requests are written to the audit log.

### Notes

- A config that sets `update = …` in a grant is rejected by 0.1.0 (unknown field); update the
  device before granting it.
- Talking to a 0.1.0 peer, `enishi -d <peer> update` fails (unknown request): update that
  device once by hand (`enishi update` there, or the install one-liner outside enishi).
- `--push` (sending the caller's binary to the device) is not implemented yet.

## [0.1.0] — 2026-11-16

First stable release: a P2P link between your own devices (Linux ↔ Windows, x86_64), with
stdio services, MCP servers and TCP ports published across it, built for devices that sleep.

### Link and presence

- Identity by key (`enishi id`), encrypted QUIC over [iroh](https://github.com/n0-computer/iroh)
  with NAT traversal and relay fallback; direct/Tailscale addresses are used as hints.
- `enishi up` daemon: one warm connection per peer and direction, heartbeat presence
  (`online` / `offline`, last seen, RTT in `enishi status`), short capped reconnect backoff,
  a **hello on resume** after sleep (wall-clock jump detection) so links come back in seconds.
- Calls to an offline peer fail in milliseconds (exit `255`) instead of hanging.
- Clean shutdown closes every link with reason `shutdown` so peers mark it offline at once,
  and the daemon exits within ~0.5 s.

### Pairing

- `enishi pair invite` / `enishi pair join <code>`: single-use, expiring invite codes that
  write both configs (peer id, address hints) and reload both daemons. Invite hints carry only
  the device's own addresses on its fixed `listen_port` (Tailscale, LAN, IPv6 ULA/global).
- `enishi reload` applies config edits (peers, grants, services, forwards) without restart.

### Exec

- `enishi -d <device> exec -- <cmd>` runs native commands both directions through the
  platform shell (PowerShell / `cmd` on Windows with UTF-8 output, `sh`/`bash` on Unix) or `--raw`;
  streams stdout/stderr/stdin, `--cwd`, `--env`, `--timeout` (exit `124`, process tree
  killed), propagates the exit code. Windows NTSTATUS exit codes (e.g. `0x40010004`,
  `0xC000013A`) are also explained in plain words on stderr and in the logs.
- Default deny: `exec` must be granted per peer (`[grants.<peer>] exec = true`); every
  session is written to an audit log on the serving side.

### Services, `run` and MCP

- Published stdio services (`[services.<name>] command = [...]`), double-gated by the
  service's `allow` list and the peer's grant; extra arguments only with `allow_args`.
- `enishi service add|list|remove|import`, `enishi grant`: comment-preserving config edits
  and reload — no hand-editing TOML. `enishi service import mcp <file>` imports an agent's MCP
  config (pluggable importer registry).
- `enishi -d <device> run <service>` pipes stdio to a remote service (`253` no such service,
  `254` not allowed); `enishi services` lists what a peer offers.
- `enishi mcp <device>/<service>`: an always-alive local MCP shim. It answers instantly while
  the peer is away (cached `initialize` / `tools/list`, clean `peer offline` errors) and
  replays the handshake when the peer returns, without blocking the client meanwhile. The
  replay waits up to 60 s for slow cold starts (`startup_timeout` per service or
  `--startup-timeout`).
- Transparent mode `enishi mcp -d <device> -- npx -y <pkg> <args>` keeps agent configs
  recognizable as the original MCP (own `mcp` grant); `enishi mcp config ... --for <agent>`
  prints ready-to-paste snippets.

### TCP forwarding

- `enishi service add <name> --tcp host:port` + `enishi -d <device> forward <name> [port]`
  (or `[forwards.*]`) tunnel
  each TCP connection as a stream inside the existing link; the consumer picks any local port.

### Install, logs and release

- `enishi install` / `uninstall`: per-user service (systemd `--user` unit with linger on
  Linux; logon task running a windowless `enishiw.exe` in the user's session on Windows),
  lock-safe upgrades, PATH setup.
- Firewall step built into install: one UAC prompt on Windows for a scoped, Private-only UDP
  rule (`--no-firewall` skips); on Linux firewalld/ufw are detected and the exact `sudo`
  command is offered interactively. `enishi firewall --check` is read-only.
- Daemon log file and `enishi logs [-f]`.
- One-command installers (`install.sh`, `install.ps1`) that verify `SHA256SUMS`; release
  workflow publishes Linux gnu and Windows msvc binaries.

### Known limitations

- Non-ASCII text piped to **stdin** of PowerShell on Windows is decoded with the OEM code
  page before enishi's UTF-8 prelude runs (output is fine).
- Legacy `cmd` tools (`chcp` messages, `findstr` with non-ASCII) still do not speak UTF-8.
- Pairing can end half-done if one side saved the peer but the other failed to write its
  config; remove the leftover peer and pair again.
- Pairing over relay only (no direct address reachable) and several simultaneous invites
  are untested.
- x86_64 Linux and Windows only; macOS, ARM and phones are not supported yet.

[0.1.1]: https://github.com/rauldev-yoru/enishi/releases/tag/v0.1.1
[0.1.0]: https://github.com/rauldev-yoru/enishi/releases/tag/v0.1.0

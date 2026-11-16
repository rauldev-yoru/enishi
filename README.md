# enishi 縁

> *enishi* (縁) — the bond that ties people together. Here: the bond between your devices.

**enishi** is a small, native, cross-platform link between your machines (Linux ↔ Windows today, phone later), built so AI agents living on a server can use tools on your other devices as if they were local.

Status: **v0.1 in progress** — daemon, pairing, presence, `exec`, published services and the MCP shim work Linux ↔ Windows; installers are not done yet. Design: [docs/design.md](docs/design.md).

## Why

Agents usually live on a server, but much of the useful stuff lives on your PC: native apps, your real browser with your logins, your corporate VPN. Today people bridge that with SSH hops, WSL tricks, or HTTP MCP servers that fall into retry storms the moment the PC sleeps.

enishi aims to be the base layer that fixes that:

- **Direct, encrypted, peer-to-peer link** between devices (QUIC via [iroh](https://github.com/n0-computer/iroh)), identity = key, no exposed ports. Works over Tailscale today, standalone later.
- **Services both ways**: each device publishes stdio services (MCP servers, CLIs); the other side consumes them as local stdio.
- **Built for devices that sleep**: presence tracking, instant `peer offline` instead of hanging, automatic reconnect, wake hints.
- **Agent-friendly MCP shim**: cached `initialize` / `tools/list` so agents start fine with the device off, and the handshake is replayed transparently when it comes back.
- **Runs in the user's session on Windows** (not session 0), so UI-capable tools work.

## Usage

```sh
enishi up                         # the daemon (systemd --user unit / Windows logon task)
enishi pair invite                # on one device: prints a one-time code
enishi pair join <code>           # on the other
enishi status                     # peers, presence, rtt
enishi -d pc exec git status      # run a command on the peer (needs `exec = true` there)
enishi -d pc services             # services the peer publishes to you
enishi -d pc run sort < list.txt  # raw stdio pipe to a published service
enishi mcp pc/fs                  # the same, MCP-aware (what agents use)
```

enishi ships no services of its own: a device publishes any stdio program it chooses in
its `enishi.toml`, and only to the peers it names **and** grants:

```toml
[services.fs]
command = ["node", "C:/…/server-filesystem/dist/index.js", "C:/Users/me/Projects"]
kind = "mcp"           # mcp | stdio (default) — informational
allow = ["server"]     # peers that may open it
allow_args = false     # extra args after `--` are refused unless true

[grants.server]
services = ["fs"]      # …and the peer must be granted it too
```

Unknown service → exit `253`, not allowed → `254`, peer offline → `255`.

Agents point their MCP config at the shim:

```yaml
# any MCP client: Hermes, Claude Code, OpenClaw, ...
mcp_servers:
  pc_files:
    command: enishi
    args: [mcp, pc/fs]
```

`enishi mcp` passes JSON-RPC through while the peer is up and caches `initialize` and the
`*/list` results on disk. When the peer (or even the local daemon) is down it answers
those from the cache and fails everything else immediately, so agents start fine with
the PC asleep; when it comes back, the MCP handshake is replayed transparently in the
same session. Full walkthrough: [docs/examples/filesystem-mcp.md](docs/examples/filesystem-mcp.md).

## PoC results (Linux server ↔ Windows 11 over LAN)

| Metric | Result |
|---|---|
| Warm call (`whoami`) | 10–20 ms (vs ~230 ms SSH→WSL→PowerShell) |
| MCP `tools/call` | 2–5 ms |
| Detect sleep | ~2 s |
| Calls while asleep | ~3 ms `peer offline`, no hangs |
| Reconnect after wake | ~4.5 s, automatic |

## License

Dual-licensed under MIT or Apache-2.0, at your option.

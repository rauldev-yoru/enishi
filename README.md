# enishi 縁

> *enishi* (縁) — the bond that ties people together. Here: the bond between your devices.

**enishi** is a small, native, cross-platform link between your machines (Linux ↔ Windows today, phone later), built so AI agents living on a server can use tools on your other devices as if they were local.

Status: **early design** — a working proof of concept exists; this repo is where the real thing gets built.

## Why

Agents usually live on a server, but much of the useful stuff lives on your PC: native apps, your real browser with your logins, your corporate VPN. Today people bridge that with SSH hops, WSL tricks, or HTTP MCP servers that fall into retry storms the moment the PC sleeps.

enishi aims to be the base layer that fixes that:

- **Direct, encrypted, peer-to-peer link** between devices (QUIC via [iroh](https://github.com/n0-computer/iroh)), identity = key, no exposed ports. Works over Tailscale today, standalone later.
- **Services both ways**: each device publishes stdio services (MCP servers, CLIs); the other side consumes them as local stdio.
- **Built for devices that sleep**: presence tracking, instant `peer offline` instead of hanging, automatic reconnect, wake hints.
- **Agent-friendly MCP shim**: cached `initialize` / `tools/list` so agents start fine with the device off, and the handshake is replayed transparently when it comes back.
- **Runs in the user's session on Windows** (not session 0), so UI-capable tools work.

## Usage (target)

```yaml
# any MCP client: Hermes, Claude Code, OpenClaw, ...
mcp_servers:
  pc_files:
    command: enishi
    args: [mcp, pc/fs]
```

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

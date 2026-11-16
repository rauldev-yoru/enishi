# Example: a Windows folder as an MCP server for agents on a Linux server

enishi ships no MCP servers. This walkthrough publishes the official
[`@modelcontextprotocol/server-filesystem`](https://github.com/modelcontextprotocol/servers/tree/main/src/filesystem)
from a Windows PC and uses it from agents running on a Linux server. Any other stdio MCP
server is published the same way.

Names used below: the Windows device is `pc`, the Linux device is `server`; both run
`enishi up` and are paired (`enishi pair invite` on one, `enishi pair join <code>` on the
other).

## 1. Windows: install and publish

```powershell
npm install -g @modelcontextprotocol/server-filesystem
```

Add to `%APPDATA%\enishi\enishi.toml`:

```toml
[services.fs]
# argv, spawned directly (no shell). Use the full path to the script so the service does
# not depend on npx/PATH lookups; the trailing path(s) are the folders it may touch.
command = [
  "node",
  "C:/Users/me/AppData/Roaming/npm/node_modules/@modelcontextprotocol/server-filesystem/dist/index.js",
  "C:/Users/me/Projects",
]
kind = "mcp"
allow = ["server"]     # which peers may open it…
allow_args = false     # …and they cannot add args (e.g. extra folders)

[grants.server]
services = ["fs"]      # …and peer "server" is granted it
```

Then `enishi reload`. Both lists are required: a service missing from either is refused
with exit code `254`; `exec` is not needed (and not implied).

## 2. Linux: check it

```console
$ enishi -d pc services
SERVICE  KIND   ARGS  USE
fs       mcp    no    enishi mcp pc/fs
$ enishi -d pc run nope
enishi: pc: no service "nope" on this device          # exit 253
```

## 3. Point the agents at the shim

`enishi mcp pc/fs` is a stdio MCP server on the Linux side. (If `pc` is the
`default_peer`, `enishi mcp fs` works too.)

Hermes (`~/.hermes/config.yaml`):

```yaml
mcp_servers:
  pc_files:
    command: enishi
    args: [mcp, pc/fs]
```

Claude Code:

```sh
claude mcp add pc-files -- enishi mcp pc/fs
```

or in `.mcp.json`:

```json
{ "mcpServers": { "pc-files": { "command": "enishi", "args": ["mcp", "pc/fs"] } } }
```

OpenClaw:

```sh
openclaw mcp add pc-files --command enishi --arg mcp --arg pc/fs
```

## 4. What happens when the PC sleeps

The shim caches `initialize` and `tools/list` (and the other `*/list` results) in
`~/.cache/enishi/mcp/pc/fs.json` the first time they succeed. With the PC off:

- an agent that starts gets its tools from the cache in a few ms;
- `tools/call` fails at once with a JSON-RPC error saying the peer is offline — no
  hangs, no retry storms;
- when the PC is back, the next call reopens the server and replays the MCP handshake
  transparently, in the same agent session.

This also holds if the local `enishi up` is not running (a hint goes to stderr). The
remote server's stderr is copied to the shim's stderr, where agents usually log it.

Measured Linux server → Windows 11 over LAN (enishi v0.1, server-filesystem 2.3.0):

| Step | Latency |
|---|---|
| first `initialize` (spawns node on Windows) | 326 ms |
| `tools/list` / `tools/call list_directory` online | 9 ms / 2–6 ms |
| PC stops → shim notices | ~2.3 s (heartbeat) |
| `tools/list` / `tools/call` while offline | 0.5 ms (cache) / 0.2 ms (error) |
| first `tools/call` after it is back (respawn + replayed handshake) | 323 ms |
| new agent, PC off: `initialize` + `tools/list` | 1.5 ms + 0.5 ms |

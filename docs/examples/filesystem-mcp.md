# Example: a Windows folder as an MCP server for agents on a Linux server

enishi ships no MCP servers. This walkthrough publishes the official
[`@modelcontextprotocol/server-filesystem`](https://github.com/modelcontextprotocol/servers/tree/main/src/filesystem)
from a Windows PC and uses it from agents running on a Linux server. Any other stdio MCP
server works the same way.

Names used below: the Windows device is `pc`, the Linux device is `server`; both run
`enishi up` and are paired (`enishi pair invite` on one, `enishi pair join <code>` on the
other). Node.js (for `npx`) is installed on the PC.

## 1. Windows: publish it

Write the command exactly as the server's README does — `npx` (a `.cmd` shim) is found
through `PATH`/`PATHEXT` like in a terminal:

```powershell
enishi service add fs --mcp -- npx -y @modelcontextprotocol/server-filesystem C:\Users\me\Projects
enishi grant server --service fs
```

`service add` writes `[services.fs]` to `%APPDATA%\enishi\enishi.toml` (comments in the
file are kept) and reloads the running daemon; `grant` adds `server` to the service's
`allow` list **and** `fs` to `[grants.server] services` — both are required (D9), the
command just does both. The result:

```toml
[services.fs]
kind = "mcp"
command = ["npx", "-y", "@modelcontextprotocol/server-filesystem", 'C:\Users\me\Projects']
allow = ["server"]

[grants.server]
services = ["fs"]
```

Peers cannot add args (e.g. extra folders) unless the service has `allow_args = true`
(`--allow-args`). `exec` is not needed and not implied. `enishi service list` shows what
is published, `enishi grant server` what the peer may do, `enishi service remove fs`
unpublishes.

Already have these servers in Claude Desktop, Cursor or VS Code? Import them instead:

```powershell
enishi service import mcp $env:APPDATA\Claude\claude_desktop_config.json --allow server --dry-run
enishi service import mcp $env:APPDATA\Claude\claude_desktop_config.json --allow server --only filesystem
enishi grant server --service filesystem
```

Remote (`url` / `type: http|sse`) entries are skipped with a note — only stdio servers
can be published.

## 2. Linux: check it

```console
$ enishi -d pc services
SERVICE  KIND   ARGS  USE
fs       mcp    no    enishi mcp pc/fs
$ enishi -d pc run nope
enishi: pc: no service "nope" on this device          # exit 253
```

## 3. Point the agents at the shim

`enishi mcp pc/fs` is a stdio MCP server on the Linux side (if `pc` is the
`default_peer`, `enishi mcp fs` works too). Let enishi print the entry for your agent:

```console
$ enishi mcp config pc/fs --for hermes        # ~/.hermes/config.yaml
mcp_servers:
  "pc-fs":
    command: "/usr/local/bin/enishi"
    args:
      - "mcp"
      - "pc/fs"
$ enishi mcp config pc/fs --for claude        # .mcp.json / claude_desktop_config.json / Cursor
{
  "mcpServers": {
    "pc-fs": {
      "args": [
        "mcp",
        "pc/fs"
      ],
      "command": "/usr/local/bin/enishi"
    }
  }
}
```

`--for openclaw` prints `{"mcp": {"servers": {…}}}`, `--for json` just the
`{command, args}` entry; `--name` changes the entry name. The command is the absolute
path of `enishi`, because agents often start MCP servers with a minimal `PATH`.

## 4. Transparent mode: no publishing step

If the PC grants the server `mcp` (any MCP command), the Linux side can launch an MCP
server on the PC directly, written exactly as in its docs:

```powershell
# on the PC, once:
enishi grant server --mcp
```

```sh
enishi mcp -d pc -- npx -y @modelcontextprotocol/server-filesystem C:\Users\me\Projects
enishi mcp config -d pc --for claude -- npx -y @modelcontextprotocol/server-filesystem C:\Users\me\Projects
```

`--cwd` and `--env K=V` (before `--`) set the server's working directory and environment.
The `mcp` grant is separate from `exec` (D13): `exec = true` does not allow it, and a
published service needs only its service grant. Use transparent mode for your own
devices; publish services when you want the PC to decide exactly what runs.

## 5. What happens when the PC sleeps

The shim caches `initialize` and `tools/list` (and the other `*/list` results) per device
and service (`~/.cache/enishi/mcp/pc/fs.json`; transparent mode keys the file on a hash of
the command) the first time they succeed. With the PC off:

- an agent that starts gets its tools from the cache in a few ms;
- `tools/call` fails at once with a JSON-RPC error saying the peer is offline — no
  hangs, no retry storms;
- when the PC is back, the next call reopens the server and replays the MCP handshake
  transparently, in the same agent session.

This also holds if the local `enishi up` is not running (a hint goes to stderr). A
session the PC *refuses* (not granted, unknown service) is reported as such instead of
being served from cache. The remote server's stderr is copied to the shim's stderr, where
agents usually log it.

Measured Linux server → Windows 11 over LAN (server-filesystem 2.3.0 via `npx -y`):

| Step | Published `pc/fs` | Transparent `-d pc -- npx …` |
|---|---|---|
| first `initialize` (npx resolves the package, spawns node) | 4.75 s cold npx cache / ~1.9 s warm | 2.1 s |
| `tools/list` / `tools/call list_directory` online | 10.8 ms / 2–5 ms | 8.4 ms / 1.4–4.6 ms |
| PC stops → shim notices | ~2.9 s | ~2.1 s (heartbeat) |
| `tools/list` / `tools/call` while offline | 0.6 ms (cache) / 0.2 ms (error) | 0.6 ms / 0.2 ms |
| first `tools/call` after it is back (respawn + replayed handshake) | 1.9 s | 1.8 s |
| new agent, PC off: `initialize` + `tools/list` | 2.2 ms + 0.6 ms | 1.6 ms + 0.5 ms |

Most of the first-call time is `npx` itself (package resolution + node start on
Windows); publishing `node <path>\dist\index.js` instead of `npx` brings it to ~0.3 s.

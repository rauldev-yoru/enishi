//! `enishi mcp config …`: ready-to-paste MCP server entries for agent config files.
//!
//! Every format describes the same stdio server: `command` + `args` (+ `env`), where the
//! command is the enishi binary and the args are `mcp <target>` or `mcp -d dev -- cmd…`.

use serde_json::{Map, Value, json};

/// Agent config formats `enishi mcp config --for` knows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Agent {
    /// Hermes Agent `~/.hermes/config.yaml` → `mcp_servers:`.
    Hermes,
    /// Claude Desktop `claude_desktop_config.json`, Claude Code `.mcp.json`, Cursor
    /// `mcp.json` → `{"mcpServers": {...}}`.
    Claude,
    /// OpenClaw `openclaw.json` → `{"mcp": {"servers": {...}}}`.
    OpenClaw,
    /// Just the server entry `{"command", "args", "env"}`, for anything else.
    Json,
}

impl Agent {
    pub const ALL: [(&'static str, Agent); 4] = [
        ("hermes", Agent::Hermes),
        ("claude", Agent::Claude),
        ("openclaw", Agent::OpenClaw),
        ("json", Agent::Json),
    ];

    pub fn parse(s: &str) -> Result<Agent, String> {
        Agent::ALL
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(s))
            .map(|(_, a)| *a)
            .ok_or_else(|| {
                let names: Vec<_> = Agent::ALL.iter().map(|(n, _)| *n).collect();
                format!("unknown agent {s:?} (one of: {})", names.join(", "))
            })
    }
}

/// One stdio MCP server entry.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

impl Entry {
    fn json(&self) -> Value {
        let mut o = Map::new();
        o.insert("command".into(), json!(self.command));
        o.insert("args".into(), json!(self.args));
        if !self.env.is_empty() {
            let env: Map<String, Value> = self
                .env
                .iter()
                .map(|(k, v)| (k.clone(), json!(v)))
                .collect();
            o.insert("env".into(), Value::Object(env));
        }
        Value::Object(o)
    }
}

/// The snippet for `agent`, ending in a newline.
pub fn render(agent: Agent, e: &Entry) -> String {
    let wrap = |outer: &[&str]| {
        let mut v = e.json();
        let mut servers = Map::new();
        servers.insert(e.name.clone(), v.take());
        let mut v = Value::Object(servers);
        for key in outer.iter().rev() {
            let mut m = Map::new();
            m.insert((*key).to_string(), v);
            v = Value::Object(m);
        }
        v
    };
    let pretty = |v: Value| serde_json::to_string_pretty(&v).expect("json") + "\n";
    match agent {
        Agent::Json => pretty(e.json()),
        Agent::Claude => pretty(wrap(&["mcpServers"])),
        Agent::OpenClaw => pretty(wrap(&["mcp", "servers"])),
        Agent::Hermes => {
            // YAML; JSON string literals are valid YAML scalars and quote everything safely.
            let q = |s: &str| serde_json::to_string(s).expect("json");
            let mut out = format!(
                "mcp_servers:\n  {}:\n    command: {}\n    args:\n",
                q(&e.name),
                q(&e.command)
            );
            for a in &e.args {
                out.push_str(&format!("      - {}\n", q(a)));
            }
            if !e.env.is_empty() {
                out.push_str("    env:\n");
                for (k, v) in &e.env {
                    out.push_str(&format!("      {}: {}\n", q(k), q(v)));
                }
            }
            out
        }
    }
}

/// Default entry name for a transparent-mode command: the package/program it runs, e.g.
/// `npx -y @modelcontextprotocol/server-filesystem /x` → `server-filesystem`.
pub fn name_for_command(argv: &[String]) -> String {
    let pick = argv
        .iter()
        .skip(1)
        .find(|a| !a.starts_with('-'))
        .or(argv.first())
        .map(String::as_str)
        .unwrap_or("mcp");
    // Last path segment (handles scopes and paths), minus a version suffix and extension.
    let last = pick.rsplit(['/', '\\']).next().unwrap_or(pick);
    let last = match last.rfind('@') {
        Some(i) if i > 0 => &last[..i],
        _ => last,
    };
    let last = last.strip_suffix(".js").unwrap_or(last);
    let name = enishi_core::import::sanitize_name(last);
    if name.is_empty() { "mcp".into() } else { name }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry() -> Entry {
        Entry {
            name: "pc-fs".into(),
            command: "/usr/bin/enishi".into(),
            args: vec!["mcp".into(), "pc/fs".into()],
            env: vec![],
        }
    }

    #[test]
    fn formats() {
        let e = entry();
        let v: Value = serde_json::from_str(&render(Agent::Claude, &e)).unwrap();
        assert_eq!(v["mcpServers"]["pc-fs"]["command"], "/usr/bin/enishi");
        assert_eq!(v["mcpServers"]["pc-fs"]["args"], json!(["mcp", "pc/fs"]));
        assert!(v["mcpServers"]["pc-fs"].get("env").is_none());
        let v: Value = serde_json::from_str(&render(Agent::OpenClaw, &e)).unwrap();
        assert_eq!(v["mcp"]["servers"]["pc-fs"]["args"][1], "pc/fs");
        let v: Value = serde_json::from_str(&render(Agent::Json, &e)).unwrap();
        assert_eq!(v["command"], "/usr/bin/enishi");
        let mut e = entry();
        e.command = r"C:\Program Files\enishi\enishi.exe".into();
        e.args.push("a \"quoted\" arg".into());
        e.env = vec![("ENISHI_CONFIG".into(), "/x/enishi.toml".into())];
        assert_eq!(
            render(Agent::Hermes, &e),
            "mcp_servers:\n  \"pc-fs\":\n    command: \"C:\\\\Program Files\\\\enishi\\\\enishi.exe\"\n    \
             args:\n      - \"mcp\"\n      - \"pc/fs\"\n      - \"a \\\"quoted\\\" arg\"\n    \
             env:\n      \"ENISHI_CONFIG\": \"/x/enishi.toml\"\n"
        );
        let v: Value = serde_json::from_str(&render(Agent::Claude, &e)).unwrap();
        assert_eq!(
            v["mcpServers"]["pc-fs"]["env"]["ENISHI_CONFIG"],
            "/x/enishi.toml"
        );
    }

    #[test]
    fn agents_and_names() {
        assert_eq!(Agent::parse("Claude"), Ok(Agent::Claude));
        assert!(
            Agent::parse("vim")
                .unwrap_err()
                .contains("hermes, claude, openclaw, json")
        );
        let v = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let n = |a: &[&str]| name_for_command(&v(a));
        assert_eq!(
            n(&["npx", "-y", "@modelcontextprotocol/server-filesystem", "/x"]),
            "server-filesystem"
        );
        assert_eq!(n(&["uvx", "mcp-server-fetch@1.2"]), "mcp-server-fetch");
        assert_eq!(n(&["node", r"C:\srv\index.js"]), "index");
        assert_eq!(n(&["my-server"]), "my-server");
        assert_eq!(n(&[]), "mcp");
    }
}

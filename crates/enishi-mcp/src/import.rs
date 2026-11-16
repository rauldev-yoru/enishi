//! The `mcp` import format: the de-facto MCP client config shape —
//! `{"mcpServers": {"<name>": {"command", "args", "env", "cwd"}}}` written by Claude
//! Desktop, Cursor, Windsurf, Cline…; VS Code's `{"servers": {…}}` (`.vscode/mcp.json`) and
//! OpenClaw's `{"mcp": {"servers": {…}}}` are accepted too. Only stdio servers can become
//! enishi services; remote (`url` / `type: http|sse`) entries are reported as skipped.

use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};
use enishi_core::{
    config::ServiceKind,
    import::{ImportSet, ImportedService, Importer},
};
use serde_json::{Map, Value};

pub struct McpServersImporter;

impl Importer for McpServersImporter {
    fn format(&self) -> &'static str {
        "mcp"
    }

    fn description(&self) -> &'static str {
        "MCP client JSON: {\"mcpServers\": …} (Claude Desktop, Cursor…) or {\"servers\": …} (VS Code)"
    }

    fn parse(&self, text: &str) -> Result<ImportSet> {
        parse(text)
    }
}

/// Servers object of a config file, whichever of the known keys holds it.
fn servers(root: &Value) -> Result<&Map<String, Value>> {
    let root = root.as_object().context("expected a JSON object")?;
    let found = root
        .get("mcpServers")
        .or_else(|| root.get("servers"))
        .or_else(|| root.get("mcp").and_then(|m| m.get("servers")));
    match found {
        Some(Value::Object(m)) => Ok(m),
        Some(_) => bail!("the servers entry must be an object of name → server"),
        None => bail!("no \"mcpServers\" (or VS Code \"servers\") object in this file"),
    }
}

fn string_of(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

pub fn parse(text: &str) -> Result<ImportSet> {
    // Claude Desktop and VS Code files are plain JSON; tolerate a UTF-8 BOM (Notepad).
    let root: Value = serde_json::from_str(text.trim_start_matches('\u{feff}'))
        .context("not valid JSON (comments/trailing commas are not supported)")?;
    let mut set = ImportSet::default();
    for (name, entry) in servers(&root)? {
        let Some(e) = entry.as_object() else {
            set.skipped.push((name.clone(), "not an object".into()));
            continue;
        };
        let kind = e.get("type").and_then(Value::as_str).unwrap_or("stdio");
        if e.contains_key("url") || kind != "stdio" {
            set.skipped.push((
                name.clone(),
                format!(
                    "remote/HTTP servers are not stdio, skipped (type {:?}{})",
                    kind,
                    if e.contains_key("url") {
                        ", has url"
                    } else {
                        ""
                    }
                ),
            ));
            continue;
        }
        if e.get("disabled").and_then(Value::as_bool) == Some(true) {
            set.skipped
                .push((name.clone(), "disabled in the source file".into()));
            continue;
        }
        let Some(command) = e
            .get("command")
            .and_then(Value::as_str)
            .filter(|c| !c.is_empty())
        else {
            set.skipped.push((name.clone(), "no \"command\"".into()));
            continue;
        };
        let mut argv = vec![command.to_string()];
        match e.get("args") {
            None | Some(Value::Null) => {}
            Some(Value::Array(a)) => argv.extend(a.iter().map(string_of)),
            Some(_) => {
                set.skipped
                    .push((name.clone(), "\"args\" is not an array".into()));
                continue;
            }
        }
        let env: BTreeMap<String, String> = match e.get("env") {
            Some(Value::Object(m)) => m.iter().map(|(k, v)| (k.clone(), string_of(v))).collect(),
            _ => BTreeMap::new(),
        };
        set.services.push(ImportedService {
            name: name.clone(),
            kind: ServiceKind::Mcp,
            command: argv,
            cwd: e.get("cwd").and_then(Value::as_str).map(str::to_string),
            env,
        });
    }
    Ok(set)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_desktop_shape() {
        let text = r#"{
          "mcpServers": {
            "filesystem": {
              "command": "npx",
              "args": ["-y", "@modelcontextprotocol/server-filesystem", "C:\\Users\\me\\Desktop"]
            },
            "github": {
              "command": "docker",
              "args": ["run", "-i", "--rm", "ghcr.io/github/github-mcp-server"],
              "env": {"GITHUB_PERSONAL_ACCESS_TOKEN": "x", "N": 3},
              "cwd": "/srv"
            },
            "remote": {"url": "https://example.com/mcp"},
            "sse": {"type": "sse", "command": "ignored"},
            "off": {"command": "x", "disabled": true},
            "broken": {"args": []}
          },
          "globalShortcut": ""
        }"#;
        let set = parse(text).unwrap();
        let names: Vec<_> = set.services.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["filesystem", "github"]);
        let fs = &set.services[0];
        assert_eq!(fs.kind, ServiceKind::Mcp);
        assert_eq!(
            fs.command,
            [
                "npx",
                "-y",
                "@modelcontextprotocol/server-filesystem",
                "C:\\Users\\me\\Desktop"
            ]
        );
        let gh = &set.services[1];
        assert_eq!(gh.env["N"], "3");
        assert_eq!(gh.cwd.as_deref(), Some("/srv"));
        let skipped: BTreeMap<_, _> = set.skipped.iter().cloned().collect();
        assert!(skipped["remote"].starts_with("remote/HTTP servers are not stdio, skipped"));
        assert!(skipped["sse"].contains("\"sse\""), "{}", skipped["sse"]);
        assert!(skipped["off"].contains("disabled"));
        assert!(skipped["broken"].contains("command"));
        assert_eq!(set.skipped.len(), 4);
    }

    #[test]
    fn vscode_and_openclaw_shapes() {
        let vs = "\u{feff}{\"servers\": {\"fs\": {\"type\": \"stdio\", \"command\": \"uvx\", \
                  \"args\": [\"mcp-server-fetch\"]}, \"h\": {\"type\": \"http\", \"url\": \"http://x\"}}, \
                  \"inputs\": []}";
        let set = parse(vs).unwrap();
        assert_eq!(set.services[0].command, ["uvx", "mcp-server-fetch"]);
        assert_eq!(set.skipped.len(), 1);
        let oc = r#"{"mcp": {"servers": {"t": {"command": "node", "args": ["s.js"]}}}}"#;
        assert_eq!(parse(oc).unwrap().services[0].command, ["node", "s.js"]);
    }

    #[test]
    fn errors() {
        assert!(parse("not json").is_err());
        assert!(parse("[]").is_err());
        let e = parse(r#"{"other": 1}"#).unwrap_err().to_string();
        assert!(e.contains("mcpServers"), "{e}");
        assert!(parse(r#"{"mcpServers": []}"#).is_err());
        assert_eq!(
            parse(r#"{"mcpServers": {}}"#).unwrap(),
            ImportSet::default()
        );
    }
}

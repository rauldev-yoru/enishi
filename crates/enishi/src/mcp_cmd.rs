//! `enishi mcp …` — the MCP layer's CLI: the stdio shim (published service or transparent
//! command) and `enishi mcp config` snippets. Everything MCP-specific in the CLI is here.

use std::{path::PathBuf, process::ExitCode};

use anyhow::Result;
use clap::{Args, Subcommand};
use enishi_core::{config::Config, paths::Paths};
use enishi_mcp::{
    Target,
    snippet::{self, Agent},
};
use enishi_proto::{exit, session::ExecRequest};

use crate::{code, parse_env};

/// Expose a remote MCP server as a local stdio MCP server.
///
/// Published service: `enishi mcp windows/fs [-- extra args]` (extra args only if the
/// service has allow_args). Transparent mode: `enishi mcp -d windows -- npx -y pkg …`
/// runs any MCP command on the device (needs `mcp = true` in its [grants.<you>]).
///
/// Offline-aware: initialize and the list methods are answered from a local cache
/// while the device is unreachable (or the local daemon is down), other requests fail
/// at once, and the session resumes by itself when the device is back.
#[derive(Args, Debug)]
#[command(args_conflicts_with_subcommands = true)]
pub struct McpArgs {
    #[command(subcommand)]
    pub action: Option<McpCmd>,
    #[command(flatten)]
    pub spec: McpSpec,
}

#[derive(Subcommand, Debug)]
pub enum McpCmd {
    /// Print a ready-to-paste MCP server entry for an agent's config
    /// (`enishi mcp config windows/fs --for claude`)
    Config {
        #[command(flatten)]
        spec: McpSpec,
        /// hermes (config.yaml), claude (mcpServers JSON: Claude Desktop/Code, Cursor),
        /// openclaw (mcp.servers JSON) or json (just the server entry)
        #[arg(long = "for", value_parser = Agent::parse, default_value = "json")]
        agent: Agent,
        /// Entry name (default: <device>-<service>, or <device>-<package> for a command)
        #[arg(long)]
        name: Option<String>,
    },
}

#[derive(Args, Debug, Default)]
pub struct McpSpec {
    /// <service> or <device>/<service>; omit it and give `-- <command…>` for transparent mode
    pub target: Option<String>,
    /// Transparent mode: environment for the server, KEY=VALUE (repeatable)
    #[arg(long, value_parser = parse_env)]
    pub env: Vec<(String, String)>,
    /// Transparent mode: working directory on the device
    #[arg(long)]
    pub cwd: Option<String>,
    /// Extra args for a published service, or the whole command in transparent mode
    #[arg(last = true)]
    pub args: Vec<String>,
}

/// A resolved `enishi mcp` invocation.
#[derive(Debug, PartialEq)]
pub struct Resolved {
    pub device: String,
    pub target: Target,
}

impl McpSpec {
    pub fn resolve(
        &self,
        device_flag: Option<&str>,
        default_peer: Option<&str>,
    ) -> Result<Resolved, String> {
        match &self.target {
            Some(t) => {
                if !self.env.is_empty() || self.cwd.is_some() {
                    return Err("--env/--cwd are for transparent mode (`-- <command…>`); \
                                a published service sets them in its [services.*] entry"
                        .into());
                }
                let (device, name) = enishi_mcp::parse_target(t, device_flag, default_peer)?;
                Ok(Resolved {
                    device,
                    target: Target::Service {
                        name,
                        args: self.args.clone(),
                    },
                })
            }
            None => {
                if self.args.is_empty() {
                    return Err("give <device>/<service>, or `-d <device> -- <command…>` \
                                to run an MCP command on the device"
                        .into());
                }
                let device = device_flag.or(default_peer).ok_or(
                    "no device given (-d) and no default_peer in config for transparent mode",
                )?;
                Ok(Resolved {
                    device: device.to_string(),
                    target: Target::Command(ExecRequest {
                        command: self.args.clone(),
                        raw: true,
                        cwd: self.cwd.clone(),
                        env: self.env.clone(),
                        ..Default::default()
                    }),
                })
            }
        }
    }
}

impl Resolved {
    /// Args for a fully explicit `enishi …` invocation of this (device pinned).
    pub fn cli_args(&self) -> Vec<String> {
        let mut a = vec!["mcp".to_string()];
        match &self.target {
            Target::Service { name, args } => {
                a.push(format!("{}/{name}", self.device));
                if !args.is_empty() {
                    a.push("--".into());
                    a.extend(args.iter().cloned());
                }
            }
            Target::Command(r) => {
                a.extend(["-d".to_string(), self.device.clone()]);
                if let Some(c) = &r.cwd {
                    a.extend(["--cwd".to_string(), c.clone()]);
                }
                for (k, v) in &r.env {
                    a.extend(["--env".to_string(), format!("{k}={v}")]);
                }
                a.push("--".into());
                a.extend(r.command.iter().cloned());
            }
        }
        a
    }

    pub fn default_name(&self) -> String {
        match &self.target {
            Target::Service { name, .. } => name.clone(),
            Target::Command(r) => snippet::name_for_command(&r.command),
        }
    }
}

pub async fn run(
    paths: Paths,
    config_override: Option<PathBuf>,
    device: Option<String>,
    args: McpArgs,
) -> Result<ExitCode> {
    let cfg = Config::load(&paths.config_file).ok();
    let default_peer = cfg.as_ref().and_then(|c| c.device.default_peer.clone());
    let (spec, config) = match args.action {
        None => (args.spec, None),
        Some(McpCmd::Config { spec, agent, name }) => (spec, Some((agent, name))),
    };
    let r = match spec.resolve(device.as_deref(), default_peer.as_deref()) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("enishi mcp: {e}");
            return Ok(code(exit::USAGE));
        }
    };
    if let Some(cfg) = &cfg
        && !cfg.peers.contains_key(&r.device)
    {
        eprintln!(
            "enishi mcp: no peer named {:?} in {}",
            r.device,
            paths.config_file.display()
        );
        return Ok(code(exit::USAGE));
    }
    if let Some((agent, name)) = config {
        // Prefer the bare `enishi` when that is what PATH resolves to: readable configs and
        // they survive upgrades/reinstalls. Fall back to the absolute path otherwise.
        let exe = std::env::current_exe().and_then(|p| p.canonicalize()).ok();
        let on_path = exe.as_ref().is_some_and(|exe| {
            std::env::var_os("PATH").is_some_and(|path| {
                std::env::split_paths(&path).any(|dir| {
                    ["enishi", "enishi.exe"]
                        .iter()
                        .any(|n| dir.join(n).canonicalize().is_ok_and(|c| &c == exe))
                })
            })
        });
        let command = match exe {
            Some(p) if !on_path => strip_verbatim(p.display().to_string()),
            _ => "enishi".into(),
        };
        let mut env = Vec::new();
        if config_override.is_some() {
            // Agents start the shim with their own environment: pin the config used here.
            let p = paths
                .config_file
                .canonicalize()
                .unwrap_or(paths.config_file);
            env.push((
                "ENISHI_CONFIG".into(),
                strip_verbatim(p.display().to_string()),
            ));
        }
        let entry = snippet::Entry {
            name: name.unwrap_or_else(|| format!("{}-{}", r.device, r.default_name())),
            command,
            args: r.cli_args(),
            env,
        };
        print!("{}", snippet::render(agent, &entry));
        return Ok(ExitCode::SUCCESS);
    }
    let file = enishi_mcp::cache::cache_dir().map(|d| match &r.target {
        Target::Service { name, args } => enishi_mcp::cache::cache_file(&d, &r.device, name, args),
        Target::Command(c) => enishi_mcp::cache::command_cache_file(
            &d,
            &r.device,
            &c.command,
            c.cwd.as_deref(),
            &c.env,
        ),
    });
    let cache = enishi_mcp::Cache::open(file);
    let label = format!("{}/{}", r.device, r.target.label());
    let conn = enishi_mcp::DaemonConnector {
        paths,
        device: r.device,
        target: r.target,
    };
    enishi_mcp::run(conn, cache, label, tokio::io::stdin(), tokio::io::stdout()).await?;
    // stdin is at EOF and replies are flushed; don't wait on runtime teardown.
    std::process::exit(0)
}

/// `canonicalize` on Windows yields `\\?\C:\…`; agents want the plain path.
fn strip_verbatim(p: String) -> String {
    match p.strip_prefix(r"\\?\") {
        Some(rest) if !rest.starts_with("UNC\\") => rest.to_string(),
        _ => p,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct T {
        #[arg(short = 'd', global = true)]
        device: Option<String>,
        #[command(flatten)]
        mcp: McpArgs,
    }

    fn parse(a: &[&str]) -> (Option<String>, McpArgs) {
        let t = T::try_parse_from(std::iter::once("x").chain(a.iter().copied())).unwrap();
        (t.device, t.mcp)
    }

    fn v(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn published_and_transparent() {
        let (d, a) = parse(&["pc/fs"]);
        let r = a.spec.resolve(d.as_deref(), None).unwrap();
        assert_eq!(r.device, "pc");
        assert_eq!(
            r.target,
            Target::Service {
                name: "fs".into(),
                args: vec![]
            }
        );
        assert_eq!(r.cli_args(), v(&["mcp", "pc/fs"]));

        let (d, a) = parse(&["fs", "--", "/x"]);
        let r = a.spec.resolve(d.as_deref(), Some("pc")).unwrap();
        assert_eq!(r.cli_args(), v(&["mcp", "pc/fs", "--", "/x"]));

        let (d, a) = parse(&[
            "-d",
            "win",
            "--env",
            "K=v",
            "--",
            "npx",
            "-y",
            "@scope/server-x",
            "C:\\a b",
        ]);
        let r = a.spec.resolve(d.as_deref(), Some("pc")).unwrap();
        assert_eq!(r.device, "win");
        match &r.target {
            Target::Command(c) => {
                assert_eq!(c.command, v(&["npx", "-y", "@scope/server-x", "C:\\a b"]));
                assert!(c.raw);
                assert_eq!(c.env, vec![("K".to_string(), "v".to_string())]);
            }
            t => panic!("{t:?}"),
        }
        assert_eq!(
            r.cli_args(),
            v(&[
                "mcp",
                "-d",
                "win",
                "--env",
                "K=v",
                "--",
                "npx",
                "-y",
                "@scope/server-x",
                "C:\\a b"
            ])
        );
        assert_eq!(r.default_name(), "server-x");

        // Errors: nothing given; --env with a published service; no device.
        let (d, a) = parse(&[]);
        assert!(a.spec.resolve(d.as_deref(), Some("pc")).is_err());
        let (d, a) = parse(&["pc/fs", "--env", "K=v"]);
        assert!(a.spec.resolve(d.as_deref(), None).is_err());
        let (d, a) = parse(&["--", "npx", "x"]);
        assert!(a.spec.resolve(d.as_deref(), None).is_err());
    }

    #[test]
    fn config_subcommand() {
        let (d, a) = parse(&["config", "-d", "win", "--for", "hermes", "--", "uvx", "srv"]);
        assert_eq!(d.as_deref(), Some("win"));
        match a.action {
            Some(McpCmd::Config { spec, agent, name }) => {
                assert_eq!(agent, Agent::Hermes);
                assert!(name.is_none());
                assert_eq!(spec.args, v(&["uvx", "srv"]));
            }
            other => panic!("{other:?}"),
        }
        let (_, a) = parse(&["config", "pc/fs", "--for", "claude", "--name", "files"]);
        assert!(matches!(
            a.action,
            Some(McpCmd::Config {
                agent: Agent::Claude,
                ..
            })
        ));
        assert!(T::try_parse_from(["x", "config", "pc/fs", "--for", "vim"]).is_err());
    }
}

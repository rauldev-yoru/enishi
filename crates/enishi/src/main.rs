use std::{path::PathBuf, process::ExitCode};

use anyhow::Result;
use clap::{Parser, Subcommand};
use enishi_core::{
    config::Config,
    identity,
    ipc::{self, ClientError},
    paths::Paths,
};
use enishi_proto::{
    exit,
    ipc::{IpcRequest, IpcResponse, Presence, StatusReport},
    session::ExecRequest,
};

mod exec;

/// 縁 enishi — a native link between your devices.
#[derive(Parser)]
#[command(version, about)]
struct Cli {
    /// Target device (defaults to `default_peer` in enishi.toml)
    #[arg(short = 'd', long = "device", global = true)]
    device: Option<String>,
    /// Path to enishi.toml (default: ~/.config/enishi/enishi.toml, %APPDATA%\enishi\enishi.toml)
    #[arg(long, global = true, env = "ENISHI_CONFIG")]
    config: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the daemon in the foreground
    Up,
    /// Print this device's public id
    Id,
    /// Pair with another device
    Pair {
        #[command(subcommand)]
        action: PairCmd,
    },
    /// Peers, presence and latency (all peers, or only -d <device>)
    Status,
    /// Run a native command on the device
    ///
    /// The command words are joined into one command line for the device's shell
    /// (PowerShell on Windows, $SHELL or sh elsewhere); --raw runs them as argv. stdin is
    /// streamed, stdout/stderr stay separate, the exit code is the remote one. enishi's own
    /// failures: 255 peer offline, 254 not allowed, 252 daemon not running, 124 timeout.
    Exec {
        /// Shell on the device. Default: pwsh/powershell on Windows, $SHELL or sh elsewhere
        #[arg(long, value_parser = ["pwsh", "powershell", "cmd", "bash", "sh"])]
        shell: Option<String>,
        /// Spawn the command directly, without a shell
        #[arg(long, conflicts_with = "shell")]
        raw: bool,
        /// Working directory on the device (a leading ~ is the device user's home)
        #[arg(long)]
        cwd: Option<String>,
        /// KEY=VALUE, repeatable
        #[arg(long = "env", value_name = "KEY=VALUE", value_parser = parse_env)]
        env: Vec<(String, String)>,
        /// Seconds before the remote process tree is killed (exit 124)
        #[arg(long, value_name = "SECS")]
        timeout: Option<u64>,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        command: Vec<String>,
    },
    /// Pipe local stdio to a service published by the device
    Run { service: String },
    /// Expose a remote MCP service as a local stdio MCP server: `enishi mcp windows-home/fs`
    Mcp {
        target: String,
        #[arg(last = true)]
        args: Vec<String>,
    },
    /// List services the device publishes to us
    Services,
}

#[derive(Subcommand)]
enum PairCmd {
    /// Print a one-time invite code
    Invite,
    /// Join using an invite code
    Join { code: String },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let rt = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("enishi: {e}");
            return ExitCode::FAILURE;
        }
    };
    match rt.block_on(run(cli)) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("enishi: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn code(c: i32) -> ExitCode {
    ExitCode::from(c as u8)
}

async fn run(cli: Cli) -> Result<ExitCode> {
    let paths = Paths::resolve(cli.config.as_deref())?;
    match cli.cmd {
        Cmd::Up => {
            init_logging();
            let cfg = Config::load(&paths.config_file)?;
            let key = identity::load_or_create(&paths.identity_key())?;
            enishi_core::daemon::run(cfg, paths, key).await?;
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Id => {
            let key = identity::load_or_create(&paths.identity_key())?;
            println!("{}", key.public());
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Status => status(&paths, cli.device).await,
        Cmd::Exec {
            shell,
            raw,
            cwd,
            env,
            timeout,
            command,
        } => {
            let request = ExecRequest {
                command,
                shell,
                raw,
                cwd,
                env,
                timeout_secs: timeout,
            };
            let c = exec::run(&paths, cli.device, request).await;
            // Exit at once: a blocking stdin read must not keep the process alive, and the
            // remote code may not fit an ExitCode (Windows).
            std::process::exit(c)
        }
        other => {
            let name = match other {
                Cmd::Pair { .. } => "pair",
                Cmd::Run { .. } => "run",
                Cmd::Mcp { .. } => "mcp",
                Cmd::Services => "services",
                Cmd::Up | Cmd::Id | Cmd::Status | Cmd::Exec { .. } => unreachable!(),
            };
            eprintln!("enishi {name}: not implemented yet");
            Ok(code(exit::USAGE))
        }
    }
}

fn parse_env(s: &str) -> Result<(String, String), String> {
    match s.split_once('=') {
        Some((k, v)) if !k.is_empty() => Ok((k.into(), v.into())),
        _ => Err(format!("expected KEY=VALUE, got {s:?}")),
    }
}

fn init_logging() {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_env("ENISHI_LOG")
        .unwrap_or_else(|_| EnvFilter::new("warn,enishi=info,enishi_core=info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
}

async fn status(paths: &Paths, device: Option<String>) -> Result<ExitCode> {
    match ipc::request(paths, &IpcRequest::Status { device }).await {
        Ok(IpcResponse::Status(report)) => {
            print!("{}", render_status(&report));
            Ok(ExitCode::SUCCESS)
        }
        Ok(IpcResponse::Error { code: c, message }) => {
            eprintln!("enishi: {message}");
            Ok(code(c))
        }
        Err(e @ ClientError::DaemonDown(_)) => {
            eprintln!("enishi: {e}");
            Ok(code(exit::DAEMON_DOWN))
        }
        Err(ClientError::Other(e)) => Err(e),
        Ok(other) => anyhow::bail!("unexpected reply {other:?}"),
    }
}

fn human(ms: u64) -> String {
    let s = ms / 1000;
    match s {
        0..60 => format!("{:.1}s", ms as f64 / 1000.0),
        60..3600 => format!("{}m{:02}s", s / 60, s % 60),
        3600..86400 => format!("{}h{:02}m", s / 3600, (s % 3600) / 60),
        _ => format!("{}d{:02}h", s / 86400, (s % 86400) / 3600),
    }
}

fn render_status(r: &StatusReport) -> String {
    let mut out = format!("{} {} (enishi {})\n", r.device, r.id, r.version);
    if r.peers.is_empty() {
        out.push_str("no peers configured\n");
        return out;
    }
    let w = r
        .peers
        .iter()
        .map(|p| p.name.len())
        .max()
        .unwrap_or(4)
        .max(4);
    out.push_str(&format!(
        "{:<w$}  {:<7}  {:>8}  {:>9}  {:>8}\n",
        "PEER", "STATE", "SINCE", "LAST SEEN", "RTT"
    ));
    for p in &r.peers {
        let state = match p.presence {
            Presence::Online => "online",
            Presence::Offline => "offline",
        };
        let seen = p
            .last_seen_ms
            .map(|ms| format!("{} ago", human(ms)))
            .unwrap_or_else(|| "never".into());
        let rtt = p
            .rtt_us
            .map(|us| format!("{:.1} ms", us as f64 / 1000.0))
            .unwrap_or_else(|| "-".into());
        out.push_str(&format!(
            "{:<w$}  {:<7}  {:>8}  {:>9}  {:>8}\n",
            p.name,
            state,
            human(p.since_ms),
            seen,
            rtt
        ));
        if p.presence == Presence::Offline
            && let Some(e) = &p.last_error
        {
            out.push_str(&format!("{:<w$}  last error: {e}\n", ""));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use enishi_proto::ipc::PeerStatus;

    #[test]
    fn env_and_exec_args() {
        assert_eq!(parse_env("A=1=2").unwrap(), ("A".into(), "1=2".into()));
        assert_eq!(parse_env("A=").unwrap(), ("A".into(), "".into()));
        assert!(parse_env("=1").is_err() && parse_env("A").is_err());
        let cli = Cli::try_parse_from([
            "enishi",
            "-d",
            "pc",
            "exec",
            "--env",
            "K=V",
            "--timeout",
            "5",
            "--",
            "ls",
            "-la",
        ])
        .unwrap();
        assert_eq!(cli.device.as_deref(), Some("pc"));
        match cli.cmd {
            Cmd::Exec {
                command,
                env,
                timeout,
                raw,
                ..
            } => {
                assert_eq!(command, vec!["ls", "-la"]);
                assert_eq!(env, vec![("K".to_string(), "V".to_string())]);
                assert_eq!(timeout, Some(5));
                assert!(!raw);
            }
            _ => panic!("not exec"),
        }
        assert!(Cli::try_parse_from(["enishi", "exec", "--shell", "zsh", "--", "ls"]).is_err());
        assert!(
            Cli::try_parse_from(["enishi", "exec", "--raw", "--shell", "sh", "--", "ls"]).is_err()
        );
        assert!(Cli::try_parse_from(["enishi", "exec"]).is_err());
    }

    #[test]
    fn human_durations() {
        assert_eq!(human(0), "0.0s");
        assert_eq!(human(1234), "1.2s");
        assert_eq!(human(125_000), "2m05s");
        assert_eq!(human(3_660_000), "1h01m");
        assert_eq!(human(90_000_000), "1d01h");
    }

    #[test]
    fn renders_table() {
        let r = StatusReport {
            device: "server".into(),
            id: "abc".into(),
            version: "0.0.1".into(),
            peers: vec![
                PeerStatus {
                    name: "windows-home".into(),
                    id: "x".into(),
                    presence: Presence::Online,
                    since_ms: 61_000,
                    last_seen_ms: Some(400),
                    rtt_us: Some(1_250),
                    last_error: None,
                },
                PeerStatus {
                    name: "pc".into(),
                    id: "y".into(),
                    presence: Presence::Offline,
                    since_ms: 2_000,
                    last_seen_ms: None,
                    rtt_us: None,
                    last_error: Some("heartbeat timeout".into()),
                },
            ],
        };
        let s = render_status(&r);
        assert!(s.contains("windows-home  online      1m01s"), "{s}");
        assert!(s.contains("0.4s ago"), "{s}");
        assert!(s.contains("1.2 ms") || s.contains("1.3 ms"), "{s}");
        assert!(s.contains("pc            offline"), "{s}");
        assert!(s.contains("last error: heartbeat timeout"), "{s}");
    }
}

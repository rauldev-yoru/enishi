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
};

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
    Exec {
        /// Shell to use (pwsh, powershell, cmd, bash, sh). Default: platform shell
        #[arg(long)]
        shell: Option<String>,
        /// Spawn the process directly, without a shell
        #[arg(long)]
        raw: bool,
        #[arg(long)]
        cwd: Option<String>,
        /// KEY=VALUE, repeatable
        #[arg(long = "env")]
        env: Vec<String>,
        /// Seconds before the remote process is killed
        #[arg(long)]
        timeout: Option<u64>,
        #[arg(trailing_var_arg = true, required = true)]
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
        other => {
            let name = match other {
                Cmd::Pair { .. } => "pair",
                Cmd::Exec { .. } => "exec",
                Cmd::Run { .. } => "run",
                Cmd::Mcp { .. } => "mcp",
                Cmd::Services => "services",
                Cmd::Up | Cmd::Id | Cmd::Status => unreachable!(),
            };
            eprintln!("enishi {name}: not implemented yet");
            Ok(code(exit::USAGE))
        }
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

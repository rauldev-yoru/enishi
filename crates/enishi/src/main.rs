use std::{path::PathBuf, process::ExitCode, time::Duration};

use anyhow::Result;
use clap::{Parser, Subcommand};
use enishi_core::{
    config::Config,
    identity,
    ipc::{self, ClientError},
    pair::parse_ttl,
    paths::Paths,
};
use enishi_proto::{
    exit,
    ipc::{IpcRequest, IpcResponse, Paired, Presence, ReloadReport, StatusReport},
    session::{ExecRequest, ServiceInfo, ServiceKind},
};

mod exec;
mod manage;

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
    /// Re-read enishi.toml in the running daemon (peers, grants, services)
    Reload,
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
    ///
    /// Raw stdin/stdout/stderr and the service's exit code, like exec. Extra args after
    /// `--` only if the service has allow_args. 253 no such service, 254 not allowed.
    Run {
        service: String,
        #[arg(last = true)]
        args: Vec<String>,
    },
    /// Expose a remote MCP service as a local stdio MCP server: `enishi mcp windows-home/fs`
    ///
    /// Offline-aware: initialize and the list methods are answered from a local cache
    /// while the device is unreachable (or the local daemon is down), other requests fail
    /// at once, and the session resumes by itself when the device is back.
    Mcp {
        /// <service> or <device>/<service>
        target: String,
        #[arg(last = true)]
        args: Vec<String>,
    },
    /// List services the device publishes to us
    Services,
    /// Manage the services this device publishes (edits enishi.toml, then reloads)
    Service {
        #[command(subcommand)]
        action: manage::ServiceCmd,
    },
    /// Show or change what a peer may do here (edits enishi.toml, then reloads)
    Grant(manage::GrantArgs),
}

#[derive(Subcommand)]
enum PairCmd {
    /// Print a one-time invite code (run on the device being joined)
    Invite {
        /// Name this device will give the joiner (default: the joiner's device name)
        #[arg(long)]
        name: Option<String>,
        /// How long the code stays valid: 90s, 10m, 1h (max 24h)
        #[arg(long, value_parser = parse_ttl, default_value = "10m")]
        ttl: Duration,
    },
    /// Pair with the device that printed <code> (both daemons must be running)
    Join {
        code: String,
        /// Local name for the other device (default: its device name)
        #[arg(long)]
        name: Option<String>,
    },
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
        Cmd::Reload => reload(&paths).await,
        Cmd::Pair { action } => pair(&paths, action).await,
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
            let c = exec::run(
                &paths,
                IpcRequest::Exec {
                    device: cli.device,
                    request,
                },
            )
            .await;
            // Exit at once: a blocking stdin read must not keep the process alive, and the
            // remote code may not fit an ExitCode (Windows).
            std::process::exit(c)
        }
        Cmd::Run { service, args } => {
            let req = IpcRequest::Run {
                device: cli.device,
                service,
                args,
            };
            std::process::exit(exec::run(&paths, req).await)
        }
        Cmd::Services => services(&paths, cli.device).await,
        Cmd::Mcp { target, args } => mcp(paths, cli.device, target, args).await,
        Cmd::Service { action } => manage::service(&paths, action).await,
        Cmd::Grant(args) => manage::grant(&paths, args).await,
    }
}

async fn services(paths: &Paths, device: Option<String>) -> Result<ExitCode> {
    let req = IpcRequest::Services { device };
    match ask(paths, &req, ipc::CLIENT_TIMEOUT).await? {
        Ok(IpcResponse::Services { device, services }) => {
            print!("{}", render_services(&device, &services));
            Ok(ExitCode::SUCCESS)
        }
        Ok(other) => anyhow::bail!("unexpected reply {other:?}"),
        Err(c) => Ok(c),
    }
}

fn render_services(device: &str, services: &[ServiceInfo]) -> String {
    if services.is_empty() {
        return format!("{device} publishes no services to this device\n");
    }
    let w = services
        .iter()
        .map(|s| s.name.len())
        .max()
        .unwrap_or(0)
        .max(7);
    let mut out = format!("{:<w$}  {:<5}  {:<4}  USE\n", "SERVICE", "KIND", "ARGS");
    for s in services {
        let usage = match s.kind {
            ServiceKind::Mcp => format!("enishi mcp {device}/{}", s.name),
            ServiceKind::Stdio => format!("enishi -d {device} run {}", s.name),
        };
        out.push_str(&format!(
            "{:<w$}  {:<5}  {:<4}  {usage}\n",
            s.name,
            s.kind.as_str(),
            if s.allow_args { "yes" } else { "no" },
        ));
    }
    out
}

/// `enishi mcp`: serve stdin/stdout until the client closes stdin. Works without a
/// running daemon (cache + offline errors), so the device comes from the target, -d, or
/// `default_peer` read straight from the config file.
async fn mcp(
    paths: Paths,
    device: Option<String>,
    target: String,
    args: Vec<String>,
) -> Result<ExitCode> {
    let cfg = Config::load(&paths.config_file).ok();
    let default_peer = cfg.as_ref().and_then(|c| c.device.default_peer.clone());
    let (device, service) =
        match enishi_mcp::parse_target(&target, device.as_deref(), default_peer.as_deref()) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("enishi mcp: {e}");
                return Ok(code(exit::USAGE));
            }
        };
    if let Some(cfg) = &cfg
        && !cfg.peers.contains_key(&device)
    {
        eprintln!(
            "enishi mcp: no peer named {device:?} in {}",
            paths.config_file.display()
        );
        return Ok(code(exit::USAGE));
    }
    let file = enishi_mcp::cache::cache_dir()
        .map(|d| enishi_mcp::cache::cache_file(&d, &device, &service, &args));
    let cache = enishi_mcp::Cache::open(file);
    let label = format!("{device}/{service}");
    let conn = enishi_mcp::DaemonConnector {
        paths,
        device,
        service,
        args,
    };
    enishi_mcp::run(conn, cache, label, tokio::io::stdin(), tokio::io::stdout()).await?;
    // stdin is at EOF and replies are flushed; don't wait on runtime teardown.
    std::process::exit(0)
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

/// One request; daemon/IPC errors become the exit code.
async fn ask(
    paths: &Paths,
    req: &IpcRequest,
    timeout: Duration,
) -> Result<Result<IpcResponse, ExitCode>> {
    match ipc::request_timeout(paths, req, timeout).await {
        Ok(IpcResponse::Error { code: c, message }) => {
            eprintln!("enishi: {message}");
            Ok(Err(code(c)))
        }
        Ok(r) => Ok(Ok(r)),
        Err(e @ ClientError::DaemonDown(_)) => {
            eprintln!("enishi: {e}");
            Ok(Err(code(exit::DAEMON_DOWN)))
        }
        Err(ClientError::Other(e)) => Err(e),
    }
}

async fn pair(paths: &Paths, action: PairCmd) -> Result<ExitCode> {
    let cfg_path = paths.config_file.display().to_string();
    match action {
        PairCmd::Invite { name, ttl } => {
            let req = IpcRequest::PairInvite {
                name: name.clone(),
                ttl_secs: ttl.as_secs(),
            };
            match ask(paths, &req, ipc::CLIENT_TIMEOUT).await? {
                Ok(IpcResponse::Invite {
                    code,
                    ttl_secs,
                    addrs,
                }) => {
                    // The code alone on stdout (scriptable); guidance on stderr.
                    println!("{code}");
                    eprintln!(
                        "\nOne-time invite, valid {}. On the other device run:\n  \
                         enishi pair join {code}\nAddress hints: {}\n\
                         The new peer gets no rights: to let it run commands here add\n  \
                         [grants.{}]\n  exec = true\nto {cfg_path} and run `enishi reload`.",
                        human(ttl_secs * 1000),
                        if addrs.is_empty() {
                            "none (discovery only)".into()
                        } else {
                            addrs.join(", ")
                        },
                        name.as_deref().unwrap_or("<its name>"),
                    );
                    Ok(ExitCode::SUCCESS)
                }
                Ok(other) => anyhow::bail!("unexpected reply {other:?}"),
                Err(c) => Ok(c),
            }
        }
        PairCmd::Join { code: invite, name } => {
            let req = IpcRequest::PairJoin { code: invite, name };
            // The daemon bounds the network part (10 s); leave room for it.
            match ask(paths, &req, Duration::from_secs(20)).await? {
                Ok(IpcResponse::Paired(p)) => {
                    print!("{}", render_paired(&p));
                    Ok(ExitCode::SUCCESS)
                }
                Ok(other) => anyhow::bail!("unexpected reply {other:?}"),
                Err(c) => Ok(c),
            }
        }
    }
}

fn render_paired(p: &Paired) -> String {
    let mut out = format!(
        "paired with {:?} as peer {:?} ({})\n",
        p.device, p.name, p.id
    );
    if p.default_peer {
        out.push_str(&format!("{} is now the default peer\n", p.name));
    }
    out.push_str(&format!(
        "It has no rights here. To let it run commands on this device, add\n  \
         [grants.{}]\n  exec = true\nto {} and run `enishi reload`.\n",
        p.name, p.config
    ));
    out
}

async fn reload(paths: &Paths) -> Result<ExitCode> {
    match ask(paths, &IpcRequest::Reload, ipc::CLIENT_TIMEOUT).await? {
        Ok(IpcResponse::Reloaded(r)) => {
            print!("{}", render_reload(&r));
            Ok(ExitCode::SUCCESS)
        }
        Ok(other) => anyhow::bail!("unexpected reply {other:?}"),
        Err(c) => Ok(c),
    }
}

fn render_reload(r: &ReloadReport) -> String {
    let mut out = String::from("config reloaded (grants and services apply to new sessions)\n");
    for (what, list) in [
        ("added", &r.added),
        ("removed", &r.removed),
        ("changed", &r.changed),
    ] {
        if !list.is_empty() {
            out.push_str(&format!("peers {what}: {}\n", list.join(", ")));
        }
    }
    if !r.needs_restart.is_empty() {
        out.push_str(&format!(
            "needs a daemon restart to take effect: {}\n",
            r.needs_restart.join(", ")
        ));
    }
    out
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
    use enishi_core::pair::DEFAULT_TTL;
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
    fn pair_args() {
        let cli = Cli::try_parse_from(["enishi", "pair", "invite", "--ttl", "2s", "--name", "pc"])
            .unwrap();
        match cli.cmd {
            Cmd::Pair {
                action: PairCmd::Invite { name, ttl },
            } => {
                assert_eq!(name.as_deref(), Some("pc"));
                assert_eq!(ttl, Duration::from_secs(2));
            }
            _ => panic!("not pair invite"),
        }
        match Cli::try_parse_from(["enishi", "pair", "invite"])
            .unwrap()
            .cmd
        {
            Cmd::Pair {
                action: PairCmd::Invite { ttl, name: None },
            } => assert_eq!(ttl, DEFAULT_TTL),
            _ => panic!("not pair invite"),
        }
        assert!(Cli::try_parse_from(["enishi", "pair", "invite", "--ttl", "0s"]).is_err());
        match Cli::try_parse_from(["enishi", "pair", "join", "enishi1-abc", "--name", "srv"])
            .unwrap()
            .cmd
        {
            Cmd::Pair {
                action: PairCmd::Join { code, name },
            } => {
                assert_eq!(code, "enishi1-abc");
                assert_eq!(name.as_deref(), Some("srv"));
            }
            _ => panic!("not pair join"),
        }
        let s = render_paired(&Paired {
            name: "srv".into(),
            device: "server".into(),
            id: "abc".into(),
            default_peer: true,
            config: "/x/enishi.toml".into(),
        });
        assert!(s.contains("srv is now the default peer"), "{s}");
        assert!(
            s.contains("[grants.srv]\n  exec = true\nto /x/enishi.toml"),
            "{s}"
        );
        let r = render_reload(&ReloadReport {
            added: vec!["a".into()],
            needs_restart: vec!["[device] name".into()],
            ..Default::default()
        });
        assert!(
            r.contains("peers added: a\n") && r.contains("restart"),
            "{r}"
        );
    }

    #[test]
    fn service_args() {
        match Cli::try_parse_from(["enishi", "-d", "pc", "run", "wc", "--", "-l", "x"])
            .unwrap()
            .cmd
        {
            Cmd::Run { service, args } => {
                assert_eq!(service, "wc");
                assert_eq!(args, vec!["-l", "x"]);
            }
            _ => panic!("not run"),
        }
        match Cli::try_parse_from(["enishi", "mcp", "pc/fs"]).unwrap().cmd {
            Cmd::Mcp { target, args } => {
                assert_eq!(target, "pc/fs");
                assert!(args.is_empty());
            }
            _ => panic!("not mcp"),
        }
        let s = render_services(
            "pc",
            &[
                ServiceInfo {
                    name: "fs".into(),
                    kind: ServiceKind::Mcp,
                    allow_args: false,
                },
                ServiceInfo {
                    name: "wc".into(),
                    kind: ServiceKind::Stdio,
                    allow_args: true,
                },
            ],
        );
        assert!(
            s.contains("fs       mcp    no    enishi mcp pc/fs\n"),
            "{s}"
        );
        assert!(
            s.contains("wc       stdio  yes   enishi -d pc run wc\n"),
            "{s}"
        );
        assert!(render_services("pc", &[]).contains("no services"));
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

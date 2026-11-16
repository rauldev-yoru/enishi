use clap::{Parser, Subcommand};

/// 縁 enishi — a native link between your devices.
#[derive(Parser)]
#[command(version, about)]
struct Cli {
    /// Target device (defaults to `default_peer` in enishi.toml)
    #[arg(short = 'd', long = "device", global = true)]
    device: Option<String>,
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
    /// Peers, presence and latency
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

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let name = match &cli.cmd {
        Cmd::Up => "up",
        Cmd::Id => "id",
        Cmd::Pair { .. } => "pair",
        Cmd::Status => "status",
        Cmd::Exec { .. } => "exec",
        Cmd::Run { .. } => "run",
        Cmd::Mcp { .. } => "mcp",
        Cmd::Services => "services",
    };
    eprintln!(
        "enishi {name}: not implemented yet (device: {})",
        cli.device.as_deref().unwrap_or("<default>")
    );
    std::process::exit(2);
}

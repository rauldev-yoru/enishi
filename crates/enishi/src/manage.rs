//! Management commands that edit the local `enishi.toml` (`enishi service …`, `enishi
//! grant`) and then ask the running daemon, if any, to reload it.

use std::{collections::BTreeMap, path::Path, process::ExitCode};

use anyhow::{Context, Result, bail};
use clap::Subcommand;
use enishi_core::{
    config::{Config, ServiceKind, write_atomic},
    config_edit::{self, GrantEdit, ServiceSpec},
    import::{Importers, sanitize_name},
    ipc::{self, ClientError},
    paths::Paths,
    policy::ExecPolicy,
};
use enishi_proto::ipc::{IpcRequest, IpcResponse};

#[derive(Subcommand)]
pub enum ServiceCmd {
    /// Publish a local command as a service: `enishi service add fs --mcp -- npx -y pkg dir`
    ///
    /// The command after `--` is spawned directly (argv, no shell) for each session; on
    /// Windows `npx`, `uvx`… are found through PATH + PATHEXT. Peers still need it in
    /// their grant: `enishi grant <peer> --service <name>` sets both.
    Add {
        name: String,
        /// The command is an MCP server (`enishi mcp <device>/<name>` on the consumer)
        #[arg(long, conflicts_with = "stdio")]
        mcp: bool,
        /// Plain stdio service (`enishi run <name>`); the default
        #[arg(long)]
        stdio: bool,
        /// Peer allowed to use it (repeatable). It also needs the service granted.
        #[arg(long = "allow", value_name = "PEER")]
        allow: Vec<String>,
        /// Let clients append extra args after the command
        #[arg(long)]
        allow_args: bool,
        /// Working directory (a leading ~ is this user's home)
        #[arg(long)]
        cwd: Option<String>,
        /// KEY=VALUE, repeatable
        #[arg(long = "env", value_name = "KEY=VALUE", value_parser = crate::parse_env)]
        env: Vec<(String, String)>,
        /// Replace an existing service with the same name
        #[arg(long)]
        force: bool,
        #[arg(last = true, required = true, value_name = "COMMAND")]
        command: Vec<String>,
    },
    /// List the services this device publishes (from the local config)
    List,
    /// Remove a service (and every grant of it)
    Remove { name: String },
    /// Import services from another tool's config: `enishi service import mcp claude.json`
    Import {
        /// Source format (`mcp`: mcpServers JSON of Claude Desktop, Cursor, VS Code…)
        format: String,
        file: std::path::PathBuf,
        /// Peer allowed to use the imported services (repeatable)
        #[arg(long = "allow", value_name = "PEER")]
        allow: Vec<String>,
        /// Import only these entries (comma-separated source names)
        #[arg(long, value_delimiter = ',', value_name = "NAME,...")]
        only: Vec<String>,
        /// Print the TOML that would be added; change nothing
        #[arg(long)]
        dry_run: bool,
        /// Replace services that already exist
        #[arg(long)]
        force: bool,
    },
}

/// What a peer may do here: `enishi grant laptop --service fs --exec`.
#[derive(clap::Args)]
pub struct GrantArgs {
    pub peer: String,
    /// Let the peer run any command (`enishi exec`)
    #[arg(long, overrides_with = "no_exec")]
    pub exec: bool,
    #[arg(long)]
    pub no_exec: bool,
    /// Grant a service: added to the grant and the peer to the service's allow list
    #[arg(long = "service", value_name = "SERVICE")]
    pub services: Vec<String>,
    /// Revoke a service (both places)
    #[arg(long = "revoke-service", value_name = "SERVICE")]
    pub revoke: Vec<String>,
}

/// The import formats this build knows. Each layer contributes its own.
pub fn importers() -> Importers {
    Importers::new().with(enishi_mcp::import::McpServersImporter)
}

fn read_config(paths: &Paths) -> Result<String> {
    let path = &paths.config_file;
    std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))
}

/// Write the edited config and ask the daemon to pick it up.
async fn commit(paths: &Paths, text: &str) -> Result<()> {
    write_atomic(&paths.config_file, text)?;
    match ipc::request_timeout(paths, &IpcRequest::Reload, ipc::CLIENT_TIMEOUT).await {
        Ok(IpcResponse::Reloaded(_)) => println!("daemon reloaded"),
        Ok(IpcResponse::Error { message, .. }) => {
            eprintln!("enishi: config written, but the daemon refused to reload it: {message}")
        }
        Ok(other) => eprintln!("enishi: unexpected reload reply {other:?}"),
        Err(ClientError::DaemonDown(_)) => {
            println!("daemon not running; the change applies when it starts")
        }
        Err(ClientError::Other(e)) => {
            eprintln!("enishi: config written, but reloading failed: {e:#}")
        }
    }
    Ok(())
}

fn kind_of(mcp: bool) -> ServiceKind {
    if mcp {
        ServiceKind::Mcp
    } else {
        ServiceKind::Stdio
    }
}

/// Peers in `allow` that still lack the service in their grant.
fn ungranted(text: &str, service: &str, allow: &[String]) -> Vec<String> {
    let Ok(cfg) = text.parse::<Config>() else {
        return vec![];
    };
    allow
        .iter()
        .filter(|p| !cfg.grant(p).services.iter().any(|s| s == service))
        .cloned()
        .collect()
}

fn grant_hint(service: &str, peers: &[String]) -> String {
    peers
        .iter()
        .map(|p| format!("note: {p} also needs the grant: enishi grant {p} --service {service}\n"))
        .collect()
}

pub async fn service(paths: &Paths, cmd: ServiceCmd) -> Result<ExitCode> {
    match cmd {
        ServiceCmd::Add {
            name,
            mcp,
            stdio: _,
            allow,
            allow_args,
            cwd,
            env,
            force,
            command,
        } => {
            let text = read_config(paths)?;
            let spec = ServiceSpec {
                name: name.clone(),
                kind: kind_of(mcp),
                command,
                allow: allow.clone(),
                allow_args,
                cwd,
                env: env.into_iter().collect(),
                ..Default::default()
            };
            let out = config_edit::add_service(&text, &spec, force)?;
            println!(
                "service {name} ({}) written to {}",
                spec.kind.as_str(),
                paths.config_file.display()
            );
            print!("{}", grant_hint(&name, &ungranted(&out, &name, &allow)));
            commit(paths, &out).await?;
        }
        ServiceCmd::List => {
            let cfg = Config::load(&paths.config_file)?;
            print!("{}", render_local_services(&cfg));
        }
        ServiceCmd::Remove { name } => {
            let (out, grants) = config_edit::remove_service(&read_config(paths)?, &name)?;
            println!("service {name} removed");
            if !grants.is_empty() {
                println!("also revoked from: {}", grants.join(", "));
            }
            commit(paths, &out).await?;
        }
        ServiceCmd::Import {
            format,
            file,
            allow,
            only,
            dry_run,
            force,
        } => return import(paths, &format, &file, &allow, &only, dry_run, force).await,
    }
    Ok(ExitCode::SUCCESS)
}

async fn import(
    paths: &Paths,
    format: &str,
    file: &Path,
    allow: &[String],
    only: &[String],
    dry_run: bool,
    force: bool,
) -> Result<ExitCode> {
    let registry = importers();
    let importer = registry.lookup(format)?;
    let src =
        std::fs::read_to_string(file).with_context(|| format!("reading {}", file.display()))?;
    let set = importer
        .parse(&src)
        .with_context(|| format!("{} is not a valid {format} file", file.display()))?;
    for o in only {
        let found = set
            .services
            .iter()
            .any(|s| &s.name == o || &sanitize_name(&s.name) == o)
            || set.skipped.iter().any(|(n, _)| n == o);
        if !found {
            bail!("--only {o:?}: no such entry in {}", file.display());
        }
    }
    let selected =
        |n: &str| only.is_empty() || only.iter().any(|o| o == n || *o == sanitize_name(n));
    for (n, why) in set.skipped.iter().filter(|(n, _)| selected(n)) {
        println!("skip   {n}: {why}");
    }
    let mut text = read_config(paths)?;
    let existing = text
        .parse::<Config>()
        .map(|c| c.services)
        .unwrap_or_default();
    let mut added = Vec::new();
    let mut snippets = String::new();
    let mut env_names = BTreeMap::new();
    for s in set.services.iter().filter(|s| selected(&s.name)) {
        let name = sanitize_name(&s.name);
        if name.is_empty() {
            println!("skip   {}: no usable name (letters, digits, - _ .)", s.name);
            continue;
        }
        if added.contains(&name) {
            println!("skip   {}: duplicate name {name:?} after renaming", s.name);
            continue;
        }
        if existing.contains_key(&name) && !force {
            println!("skip   {name}: already a service here (--force replaces it)");
            continue;
        }
        let spec = ServiceSpec {
            name: name.clone(),
            kind: s.kind,
            command: s.command.clone(),
            allow: allow.to_vec(),
            cwd: s.cwd.clone(),
            env: s.env.clone(),
            ..Default::default()
        };
        text = config_edit::add_service(&text, &spec, force)
            .with_context(|| format!("importing {}", s.name))?;
        if !s.env.is_empty() {
            env_names.insert(name.clone(), s.env.keys().cloned().collect::<Vec<_>>());
        }
        let renamed = if name != s.name {
            format!(" (from {:?})", s.name)
        } else {
            String::new()
        };
        println!(
            "{} {name}{renamed}: {}",
            if dry_run { "would add" } else { "add   " },
            s.command.join(" ")
        );
        snippets.push_str(&config_edit::render_service(&spec));
        snippets.push('\n');
        added.push(name);
    }
    if added.is_empty() {
        println!("nothing to import");
        return Ok(ExitCode::SUCCESS);
    }
    if !env_names.is_empty() {
        let list: Vec<String> = env_names
            .iter()
            .map(|(n, keys)| format!("{n} ({})", keys.join(", ")))
            .collect();
        println!(
            "note: env values were copied verbatim into enishi.toml (plain text): {}",
            list.join("; ")
        );
    }
    if dry_run {
        print!(
            "\n# --dry-run: would add to {}\n{snippets}",
            paths.config_file.display()
        );
        return Ok(ExitCode::SUCCESS);
    }
    for name in &added {
        print!("{}", grant_hint(name, &ungranted(&text, name, allow)));
    }
    commit(paths, &text).await?;
    Ok(ExitCode::SUCCESS)
}

pub async fn grant(paths: &Paths, args: GrantArgs) -> Result<ExitCode> {
    let edit = GrantEdit {
        exec: match (args.exec, args.no_exec) {
            (true, _) => Some(true),
            (_, true) => Some(false),
            _ => None,
        },
        services: args.services,
        revoke: args.revoke,
    };
    let text = read_config(paths)?;
    if edit == GrantEdit::default() {
        // Nothing to change: show what the peer has.
        let cfg: Config = text.parse()?;
        if !cfg.peers.contains_key(&args.peer) {
            bail!("no peer named {:?} in this config", args.peer);
        }
        print!("{}", render_grant(&cfg, &args.peer));
        return Ok(ExitCode::SUCCESS);
    }
    let (out, changes) = config_edit::edit_grant(&text, &args.peer, &edit)?;
    if changes.is_empty() {
        println!("nothing to change");
        return Ok(ExitCode::SUCCESS);
    }
    for c in &changes {
        println!("{c}");
    }
    commit(paths, &out).await?;
    Ok(ExitCode::SUCCESS)
}

fn render_grant(cfg: &Config, peer: &str) -> String {
    let g = cfg.grant(peer);
    let services: Vec<&str> = g
        .services
        .iter()
        .filter(|s| {
            cfg.services
                .get(*s)
                .is_some_and(|svc| svc.allow.iter().any(|p| p == peer))
        })
        .map(String::as_str)
        .collect();
    let exec = match &g.exec {
        ExecPolicy::Flag(true) => "yes (any command)",
        ExecPolicy::Rules { .. } => "rules (not supported yet: denied)",
        _ => "no",
    };
    format!(
        "peer {peer}\n  exec:     {exec}\n  services: {}\n",
        if services.is_empty() {
            "none".into()
        } else {
            services.join(", ")
        }
    )
}

fn render_local_services(cfg: &Config) -> String {
    if cfg.services.is_empty() {
        return "this device publishes no services (add one: enishi service add)\n".into();
    }
    let w = cfg
        .services
        .keys()
        .map(String::len)
        .max()
        .unwrap_or(0)
        .max(7);
    let mut out = format!(
        "{:<w$}  {:<5}  {:<4}  {:<20}  COMMAND\n",
        "SERVICE", "KIND", "ARGS", "ALLOW"
    );
    for (name, s) in &cfg.services {
        let allow: Vec<String> = s
            .allow
            .iter()
            .map(|p| {
                if cfg.grant(p).services.iter().any(|g| g == name) {
                    p.clone()
                } else {
                    format!("{p}(no grant)")
                }
            })
            .collect();
        let allow = if allow.is_empty() {
            "-".into()
        } else {
            allow.join(",")
        };
        out.push_str(&format!(
            "{name:<w$}  {:<5}  {:<4}  {allow:<20}  {}\n",
            s.kind.as_str(),
            if s.allow_args { "yes" } else { "no" },
            shown_command(&s.command),
        ));
    }
    out
}

/// argv for display: words with spaces or quotes are quoted.
fn shown_command(argv: &[String]) -> String {
    argv.iter()
        .map(|a| {
            if a.is_empty() || a.contains([' ', '"', '\'']) {
                format!("{a:?}")
            } else {
                a.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_services_table() {
        let id = |n: u8| iroh_id(n);
        let cfg: Config = format!(
            "[device]\nname = \"x\"\n[peers.a]\nid = \"{}\"\n[peers.b]\nid = \"{}\"\n\
             [services.fs]\nkind = \"mcp\"\ncommand = [\"npx\", \"-y\", \"a b\"]\nallow = [\"a\", \"b\"]\n\
             [grants.a]\nservices = [\"fs\"]\n",
            id(1),
            id(2)
        )
        .parse()
        .unwrap();
        let t = render_local_services(&cfg);
        assert_eq!(
            t,
            "SERVICE  KIND   ARGS  ALLOW                 COMMAND\n\
             fs       mcp    no    a,b(no grant)         npx -y \"a b\"\n"
        );
        assert!(render_grant(&cfg, "a").contains("services: fs"));
        assert!(render_grant(&cfg, "b").contains("services: none"));
    }

    fn iroh_id(n: u8) -> String {
        // Any 32-byte ed25519 public key works; derive from a fixed secret via `enishi id`
        // logic would need iroh here, so use known-valid keys from the core tests.
        [
            "8a88e3dd7409f195fd52db2d3cba5d72ca6709bf1d94121bf3748801b40f6f5c",
            "8139770ea87d175f56a35466c34c7ecccb8d8a91b4ee37a25df60f5b8fc9b394",
        ][n as usize - 1]
            .to_string()
    }

    #[test]
    fn registry_has_mcp() {
        assert!(importers().lookup("mcp").is_ok());
        let e = importers().lookup("yaml").err().unwrap().to_string();
        assert!(e.contains("mcp"), "{e}");
    }
}

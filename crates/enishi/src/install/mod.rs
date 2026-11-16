//! `enishi install` / `enishi uninstall` / `enishi firewall`: per-user installation as a
//! systemd user unit (Linux) or a logon scheduled task (Windows). No admin rights needed;
//! the firewall step asks for them once (Windows: one UAC prompt; Linux: an offered `sudo`).

use std::{
    path::{Path, PathBuf},
    process::ExitCode,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use clap::Args;
use enishi_core::{ipc, paths::Paths};
use enishi_proto::ipc::{IpcRequest, IpcResponse, StatusReport};
use serde::{Deserialize, Serialize};

#[cfg(target_os = "linux")]
mod linux;
// Pure generators and decisions for every platform (tested everywhere); each OS uses its
// half.
#[allow(dead_code)]
pub mod firewall;
#[allow(dead_code)]
pub mod render;
#[cfg(windows)]
mod windows;

#[cfg(target_os = "linux")]
use linux as platform;
#[cfg(windows)]
use windows as platform;

#[derive(Args, Debug)]
pub struct InstallArgs {
    /// Directory for the binary [default: ~/.local/bin, %LOCALAPPDATA%\Programs\enishi]
    #[arg(long, value_name = "DIR")]
    pub prefix: Option<PathBuf>,
    /// Name of the systemd unit / scheduled task (several instances can coexist)
    #[arg(long, default_value = "enishi", value_parser = parse_name)]
    pub name: String,
    /// Register autostart but do not (re)start the daemon now
    #[arg(long)]
    pub no_start: bool,
    /// Skip the firewall step (Windows: the UAC prompt for the Private-only rule; Linux:
    /// the firewalld/ufw check)
    #[arg(long)]
    pub no_firewall: bool,
    /// Kept for compatibility: the firewall step is now the default
    #[arg(long, hide = true, conflicts_with = "no_firewall")]
    pub firewall: bool,
}

#[derive(Args, Debug)]
pub struct UninstallArgs {
    /// Name given at install time
    #[arg(long, default_value = "enishi", value_parser = parse_name)]
    pub name: String,
    /// Also delete the config dir (enishi.toml, identity key, audit log, daemon log) and
    /// the MCP cache — needs --yes
    #[arg(long)]
    pub purge: bool,
    /// Confirm --purge
    #[arg(long, requires = "purge")]
    pub yes: bool,
    /// Windows: do not ask (UAC) to remove the firewall rules; print the command instead
    #[arg(long)]
    pub no_firewall: bool,
}

#[derive(Args, Debug)]
pub struct FirewallArgs {
    /// Install name (the rule is named after it)
    #[arg(long, default_value = "enishi", value_parser = parse_name)]
    pub name: String,
    /// Install directory [default: the one recorded at install, else the default]
    #[arg(long, value_name = "DIR")]
    pub prefix: Option<PathBuf>,
    /// Remove the rules instead
    #[arg(long)]
    pub remove: bool,
    /// Only report whether the rules are as enishi wants them (exit 1 if not); no changes
    #[arg(long, conflicts_with = "remove")]
    pub check: bool,
    /// Internal: this is the elevated helper; never re-elevate
    #[arg(long, hide = true)]
    pub elevated: bool,
}

fn parse_name(s: &str) -> Result<String, String> {
    if !s.is_empty()
        && s.len() <= 64
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
        && !s.starts_with('.')
    {
        Ok(s.to_string())
    } else {
        Err("use 1-64 characters from [A-Za-z0-9._-]".into())
    }
}

/// What an install did, so uninstall can reverse exactly that.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Manifest {
    pub name: String,
    pub version: String,
    /// The CLI binary (`enishi` / `enishi.exe`).
    pub bin: PathBuf,
    /// Windows: the windowless daemon copy (`enishiw.exe`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daemon_bin: Option<PathBuf>,
    /// Non-default config pinned into the unit / task.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<PathBuf>,
    /// The config file existed only because install created it.
    #[serde(default)]
    pub created_config: bool,
    /// Linux: the unit file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<PathBuf>,
    /// Windows: the install dir was added to the user PATH by us.
    #[serde(default)]
    pub path_added: bool,
}

impl Manifest {
    fn file(name: &str) -> Result<PathBuf> {
        Ok(data_dir()?.join("installs").join(format!("{name}.json")))
    }

    pub fn load(name: &str) -> Result<Option<Manifest>> {
        let f = Self::file(name)?;
        match std::fs::read_to_string(&f) {
            Ok(t) => Ok(Some(
                serde_json::from_str(&t).with_context(|| format!("invalid {}", f.display()))?,
            )),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("reading {}", f.display())),
        }
    }

    fn save(&self) -> Result<PathBuf> {
        let f = Self::file(&self.name)?;
        std::fs::create_dir_all(f.parent().expect("has parent"))?;
        std::fs::write(&f, serde_json::to_string_pretty(self)? + "\n")
            .with_context(|| format!("writing {}", f.display()))?;
        Ok(f)
    }

    fn remove(name: &str) -> Result<()> {
        let f = Self::file(name)?;
        match std::fs::remove_file(&f) {
            Ok(()) => {
                // Drop the now-empty dirs, never anything else in them.
                if let Some(d) = f.parent() {
                    let _ = std::fs::remove_dir(d);
                    if let Some(dd) = d.parent() {
                        let _ = std::fs::remove_dir(dd);
                    }
                }
                Ok(())
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).with_context(|| format!("removing {}", f.display())),
        }
    }
}

/// Per-user data dir for install manifests: `$XDG_DATA_HOME/enishi` (`~/.local/share`),
/// `%LOCALAPPDATA%\enishi`.
fn data_dir() -> Result<PathBuf> {
    let var = |k: &str| {
        std::env::var_os(k)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    };
    if cfg!(windows) {
        var("LOCALAPPDATA")
            .map(|d| d.join("enishi"))
            .context("LOCALAPPDATA is not set")
    } else {
        var("XDG_DATA_HOME")
            .or_else(|| var("HOME").map(|h| h.join(".local").join("share")))
            .map(|d| d.join("enishi"))
            .context("HOME is not set")
    }
}

/// Context shared by the platform implementations.
pub struct Ctx {
    /// Config to pin into the unit/task (non-default only).
    pub pinned_config: Option<PathBuf>,
    pub current_exe: PathBuf,
    /// `[device] listen_port` of the config the service runs (Linux firewall check).
    #[cfg_attr(windows, allow(dead_code))]
    pub listen_port: Option<u16>,
}

pub async fn install(paths: &Paths, args: InstallArgs) -> Result<ExitCode> {
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        let _ = (paths, args);
        bail!("enishi install supports Linux (systemd) and Windows for now");
    }
    #[cfg(any(target_os = "linux", windows))]
    {
        let current_exe = std::env::current_exe()
            .and_then(|p| p.canonicalize().or(Ok(p)))
            .context("cannot locate the running enishi binary")?;
        let created_config = ensure_config(paths)?;
        let listen_port = enishi_core::config::Config::load(&paths.config_file)
            .ok()
            .and_then(|c| c.device.listen_port);
        let ctx = Ctx {
            pinned_config: paths.custom.then(|| paths.config_file.clone()),
            current_exe,
            listen_port,
        };
        let previous = Manifest::load(&args.name)?;
        let mut m = platform::install(&ctx, &args, previous.as_ref())?;
        m.created_config = created_config || previous.is_some_and(|p| p.created_config);
        let mf = m.save()?;
        println!("install record: {}", mf.display());
        let mut ok = true;
        if !args.no_start {
            ok = wait_for_daemon(paths, &args.name).await;
        }
        platform::after_install(&ctx, &args, &m);
        Ok(if ok {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        })
    }
}

pub async fn uninstall(paths: &Paths, args: UninstallArgs) -> Result<ExitCode> {
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        let _ = (paths, args);
        bail!("enishi uninstall supports Linux (systemd) and Windows for now");
    }
    #[cfg(any(target_os = "linux", windows))]
    {
        let m = Manifest::load(&args.name)?;
        if args.purge && !args.yes {
            let cfg_dir = purge_targets(paths, m.as_ref());
            eprintln!("--purge would delete, after uninstalling:");
            for d in &cfg_dir {
                eprintln!("  {}", d.display());
            }
            eprintln!("including this device's identity key (peers must pair again).");
            eprintln!("Re-run with --purge --yes to confirm.");
            return Ok(ExitCode::from(2));
        }
        if m.is_none() {
            println!(
                "no install record for {:?}; removing what exists under the defaults",
                args.name
            );
        }
        platform::uninstall(&args.name, m.as_ref(), args.no_firewall)?;
        Manifest::remove(&args.name)?;
        if args.purge {
            for d in purge_targets(paths, m.as_ref()) {
                match std::fs::remove_dir_all(&d) {
                    Ok(()) => println!("deleted {}", d.display()),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => eprintln!("enishi: could not delete {}: {e}", d.display()),
                }
            }
        }
        println!("uninstalled {}", args.name);
        Ok(ExitCode::SUCCESS)
    }
}

pub fn firewall(args: FirewallArgs) -> Result<ExitCode> {
    #[cfg(windows)]
    {
        windows::firewall_cmd(&args)
    }
    #[cfg(not(windows))]
    {
        let _ = args;
        println!(
            "enishi firewall only applies to Windows; on Linux `enishi install` checks firewalld/ufw"
        );
        Ok(ExitCode::SUCCESS)
    }
}

/// The config dir of the installed instance, plus the MCP cache when it is safe to remove
/// (the default instance, or an explicit `ENISHI_CACHE_DIR`; a custom-config instance
/// shares the per-user cache with the default one, so it is left alone).
fn purge_targets(paths: &Paths, m: Option<&Manifest>) -> Vec<PathBuf> {
    let cfg = m
        .and_then(|m| m.config.clone())
        .map(|c| Paths::resolve(Some(&c)))
        .unwrap_or_else(|| Ok(paths.clone()));
    let mut out = Vec::new();
    let custom = match cfg {
        Ok(p) => {
            out.push(p.dir.clone());
            p.custom
        }
        Err(_) => true,
    };
    let explicit_cache = std::env::var_os("ENISHI_CACHE_DIR").is_some_and(|v| !v.is_empty());
    if (!custom || explicit_cache)
        && let Some(c) = enishi_mcp::cache::cache_dir()
    {
        out.push(c);
    }
    out
}

/// A first install on a device with no config writes a minimal one (device name = host
/// name) so the daemon can start; pairing fills in the rest. Returns whether it did.
fn ensure_config(paths: &Paths) -> Result<bool> {
    if paths.config_file.exists() {
        return Ok(false);
    }
    std::fs::create_dir_all(&paths.dir)
        .with_context(|| format!("creating {}", paths.dir.display()))?;
    let name = host_name();
    let text = format!(
        "# Created by `enishi install`. See https://github.com/rauldev-yoru/enishi\n\
         [device]\nname = \"{name}\"\n# listen_port = 7821   # optional fixed UDP port\n"
    );
    std::fs::write(&paths.config_file, text)
        .with_context(|| format!("writing {}", paths.config_file.display()))?;
    println!(
        "created {} (device name {name:?})",
        paths.config_file.display()
    );
    Ok(true)
}

fn host_name() -> String {
    let raw = if cfg!(windows) {
        std::env::var("COMPUTERNAME").unwrap_or_default()
    } else {
        std::fs::read_to_string("/proc/sys/kernel/hostname")
            .or_else(|_| std::fs::read_to_string("/etc/hostname"))
            .unwrap_or_default()
    };
    let s: String = raw
        .trim()
        .to_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '-'
            }
        })
        .collect();
    if s.is_empty() { "device".into() } else { s }
}

/// Poll the freshly started daemon over IPC until it answers (or 20 s pass).
async fn wait_for_daemon(paths: &Paths, name: &str) -> bool {
    let t0 = Instant::now();
    loop {
        if let Ok(IpcResponse::Status(r)) = ipc::request_timeout(
            paths,
            &IpcRequest::Status { device: None },
            Duration::from_secs(2),
        )
        .await
        {
            println!(
                "daemon up after {:.1}s: {}",
                t0.elapsed().as_secs_f64(),
                describe(&r)
            );
            return true;
        }
        if t0.elapsed() > Duration::from_secs(20) {
            eprintln!(
                "enishi: the daemon did not answer within 20 s; see `enishi{} logs`{}",
                if paths.custom {
                    format!(" --config {}", paths.config_file.display())
                } else {
                    String::new()
                },
                if cfg!(windows) {
                    String::new()
                } else {
                    format!(" or `journalctl --user -u {name}`")
                }
            );
            return false;
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
}

fn describe(r: &StatusReport) -> String {
    format!(
        "device {:?} (enishi {}), {} peer(s)",
        r.device,
        r.version,
        r.peers.len()
    )
}

/// Copy `src` to `dst` atomically: write `dst.new` next to it, then rename over. On Linux
/// this works while the old binary runs; on Windows the caller moves a locked file aside.
pub fn copy_atomic(src_bytes: &[u8], dst: &Path) -> Result<()> {
    let dir = dst.parent().context("destination has no parent")?;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let mut tmp = dst.as_os_str().to_owned();
    tmp.push(".new");
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, src_bytes).with_context(|| format!("writing {}", tmp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
    }
    std::fs::rename(&tmp, dst).with_context(|| format!("replacing {}", dst.display()))?;
    Ok(())
}

pub fn same_file(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

/// Run a helper program; error with its output if it fails.
pub fn run_cmd(cmd: &mut std::process::Command) -> Result<String> {
    let desc = format!("{cmd:?}");
    let out = cmd.output().with_context(|| format!("cannot run {desc}"))?;
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    if !out.status.success() {
        bail!(
            "{desc} failed ({}): {}{}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim(),
            stdout.trim()
        );
    }
    Ok(stdout)
}

#[cfg(test)]
mod tests {
    use super::*;
    use enishi_core::paths::CONFIG_FILE;

    #[test]
    fn names() {
        assert!(parse_name("enishi-test_2.x").is_ok());
        assert!(parse_name("").is_err());
        assert!(parse_name("a/b").is_err());
        assert!(parse_name(".hidden").is_err());
        assert!(parse_name("a b").is_err());
    }

    #[test]
    fn manifest_round_trip() {
        let m = Manifest {
            name: "enishi-test".into(),
            version: "0.0.1".into(),
            bin: "/x/enishi".into(),
            daemon_bin: None,
            config: Some("/c/enishi.toml".into()),
            created_config: false,
            unit: Some("/u/enishi-test.service".into()),
            path_added: false,
        };
        let s = serde_json::to_string(&m).unwrap();
        assert!(!s.contains("daemon_bin"));
        assert_eq!(serde_json::from_str::<Manifest>(&s).unwrap(), m);
    }

    #[test]
    fn purge_keeps_shared_cache_for_custom_instances() {
        let dir = std::env::temp_dir().join("enishi-purge-test");
        let p = Paths::resolve(Some(&dir.join(CONFIG_FILE))).unwrap();
        let t = purge_targets(&p, None);
        assert_eq!(t[0], dir);
        if std::env::var_os("ENISHI_CACHE_DIR").is_none() {
            assert_eq!(t.len(), 1, "{t:?}");
        }
    }
}

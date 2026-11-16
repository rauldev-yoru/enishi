//! Linux: binary in `~/.local/bin`, `systemd --user` unit, linger hint, firewalld/ufw check.

use std::{
    io::{BufRead, IsTerminal, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use anyhow::{Context, Result};

use super::{
    Ctx, InstallArgs, Manifest, copy_atomic,
    firewall::{self, Detected, LinuxPlan, Tool},
    render, run_cmd, same_file,
};

const MARKER: &str = "Written by `enishi install`";

fn home() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .context("HOME is not set")
}

fn default_prefix() -> Result<PathBuf> {
    Ok(home()?.join(".local").join("bin"))
}

fn unit_dir() -> Result<PathBuf> {
    let base = match std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        Some(x) => PathBuf::from(x),
        None => home()?.join(".config"),
    };
    Ok(base.join("systemd").join("user"))
}

fn systemctl(args: &[&str]) -> Result<String> {
    run_cmd(Command::new("systemctl").arg("--user").args(args))
}

/// What `install` did to the unit: an upgrade over a running daemon restarts it.
fn started_word(was_active: bool) -> &'static str {
    if was_active { "restarted" } else { "started" }
}

fn is_ours(unit: &Path) -> bool {
    std::fs::read_to_string(unit).is_ok_and(|t| t.contains(MARKER))
}

pub fn install(ctx: &Ctx, args: &InstallArgs, previous: Option<&Manifest>) -> Result<Manifest> {
    let prefix = match (&args.prefix, previous) {
        (Some(p), _) => std::path::absolute(p)?,
        (None, Some(m)) => m
            .bin
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or(default_prefix()?),
        (None, None) => default_prefix()?,
    };
    let bin = prefix.join("enishi");
    let unit_name = format!("{}.service", args.name);
    let unit = unit_dir()?.join(&unit_name);
    if unit.exists() && !is_ours(&unit) {
        anyhow::bail!(
            "{} exists and was not written by enishi install; pick another --name",
            unit.display()
        );
    }

    // 1. Binary (rename over the old one: safe while it runs).
    let mut changed = false;
    if same_file(&ctx.current_exe, &bin) {
        println!("binary: {} (already the running one)", bin.display());
    } else {
        changed = true;
        let upgrading = bin.exists();
        let bytes = std::fs::read(&ctx.current_exe)
            .with_context(|| format!("reading {}", ctx.current_exe.display()))?;
        copy_atomic(&bytes, &bin)?;
        println!(
            "binary: {} ({})",
            bin.display(),
            if upgrading { "replaced" } else { "installed" }
        );
    }
    if let Some(old) = previous.map(|m| &m.bin)
        && old != &bin
        && old.exists()
    {
        let _ = std::fs::remove_file(old);
        println!("removed previous binary {}", old.display());
    }

    // 2. Unit.
    let text = render::systemd_unit(&bin, ctx.pinned_config.as_deref());
    if std::fs::read_to_string(&unit).ok().as_deref() != Some(text.as_str()) {
        std::fs::create_dir_all(unit.parent().expect("has parent"))?;
        std::fs::write(&unit, &text).with_context(|| format!("writing {}", unit.display()))?;
    }
    println!("unit: {}", unit.display());
    systemctl(&["daemon-reload"])?;
    systemctl(&["enable", &unit_name])?;
    if args.no_start {
        let active = systemctl(&["is-active", &unit_name]).is_ok();
        println!(
            "enabled (starts at login); not started now{}",
            if active && changed {
                format!(
                    " — the running daemon keeps the old binary until `systemctl --user restart {unit_name}`"
                )
            } else {
                String::new()
            }
        );
    } else {
        // restart, not `enable --now`: an upgrade must pick up the new binary.
        let was_active = systemctl(&["is-active", &unit_name]).is_ok();
        systemctl(&["restart", &unit_name])?;
        println!("{} {unit_name}", started_word(was_active));
    }
    Ok(Manifest {
        name: args.name.clone(),
        version: env!("CARGO_PKG_VERSION").into(),
        bin,
        daemon_bin: None,
        config: ctx.pinned_config.clone(),
        created_config: false,
        unit: Some(unit),
        path_added: false,
    })
}

fn linger_enabled() -> Option<bool> {
    let user = std::env::var("USER").ok()?;
    let out = Command::new("loginctl")
        .args(["show-user", &user, "-p", "Linger", "--value"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim() == "yes")
}

pub fn after_install(ctx: &Ctx, args: &InstallArgs, m: &Manifest) {
    if !args.no_firewall {
        firewall_step(ctx.listen_port);
    }
    let user = std::env::var("USER").unwrap_or_else(|_| "$USER".into());
    match linger_enabled() {
        Some(true) => println!("linger: on (the daemon runs without an open session)"),
        Some(false) => println!(
            "linger: off — the daemon stops when you log out. For a headless server run:\n  \
             sudo loginctl enable-linger {user}"
        ),
        None => {}
    }
    if let Some(dir) = m.bin.parent() {
        let on_path =
            std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d == dir));
        if !on_path {
            println!(
                "note: {} is not on PATH; add it to your shell profile to run `enishi` directly",
                dir.display()
            );
        }
    }
    let cfg = ctx
        .pinned_config
        .as_ref()
        .map(|c| format!(" --config {}", c.display()))
        .unwrap_or_default();
    println!(
        "manage: systemctl --user status|restart|stop {n} · journalctl --user -u {n} · enishi{cfg} logs -f",
        n = args.name
    );
}

/// Stdout of a probe command, if it ran and exited 0 (stderr discarded, stdin closed).
fn probe(program: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

fn unit_active(unit: &str) -> bool {
    probe("systemctl", &["is-active", "--quiet", unit]).is_some()
}

/// Find an active firewalld or ufw and whether `port`/udp is open where it applies. No
/// sudo: firewalld answers queries unprivileged (`--state` may need polkit, so the unit
/// state is the fallback); ufw lists rules only for root, so its answer may be unknown.
fn detect(port: Option<u16>) -> Option<Detected> {
    let firewalld = probe("firewall-cmd", &["--state"]).is_some_and(|s| s.trim() == "running")
        || (unit_active("firewalld") && probe("firewall-cmd", &["--get-default-zone"]).is_some());
    if firewalld {
        // The zone of the default route's interface, else the default zone.
        let dev = probe("ip", &["route", "show", "default"])
            .and_then(|o| firewall::default_route_dev(&o));
        let zone = dev
            .and_then(|d| probe("firewall-cmd", &[&format!("--get-zone-of-interface={d}")]))
            .or_else(|| probe("firewall-cmd", &["--get-default-zone"]))
            .map(|z| z.trim().to_string())
            .filter(|z| !z.is_empty());
        let open = match (port, &zone) {
            (Some(p), Some(z)) => {
                let zone_arg = format!("--zone={z}");
                let query = Command::new("firewall-cmd")
                    .args([zone_arg.as_str(), &format!("--query-port={p}/udp")])
                    .stdin(Stdio::null())
                    .stderr(Stdio::null())
                    .output()
                    .ok();
                match query.map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()) {
                    Some(s) if s == "yes" => Some(true),
                    // `--query-port` matches ranges; check the list too in case of an
                    // older firewalld that does not.
                    Some(s) if s == "no" => Some(
                        probe("firewall-cmd", &[&zone_arg, "--list-ports"])
                            .is_some_and(|l| firewall::ports_cover(&l, p, "udp")),
                    ),
                    _ => None,
                }
            }
            _ => None,
        };
        return Some(Detected {
            tool: Tool::Firewalld,
            zone,
            open,
        });
    }
    let ufw_enabled = std::fs::read_to_string("/etc/ufw/ufw.conf").is_ok_and(|t| {
        t.lines()
            .any(|l| l.trim().eq_ignore_ascii_case("ENABLED=yes"))
    });
    let status = probe("ufw", &["status", "verbose"]);
    let active = match &status {
        Some(s) => firewall::ufw_port_open(s, 0).is_some(),
        None => ufw_enabled && unit_active("ufw"),
    };
    if !active {
        return None;
    }
    Some(Detected {
        tool: Tool::Ufw,
        zone: None,
        open: match (&status, port) {
            (Some(s), Some(p)) => firewall::ufw_port_open(s, p),
            _ => None,
        },
    })
}

fn firewall_step(port: Option<u16>) {
    let found = detect(port);
    match firewall::linux_plan(found.as_ref(), port) {
        LinuxPlan::NoFirewall => {}
        LinuxPlan::NoListenPort(tool) => println!(
            "firewall: {} is active and no [device] listen_port is set, so inbound can't be \
             opened; links still work (outbound-initiated). Set listen_port to allow inbound.",
            tool.label()
        ),
        LinuxPlan::AlreadyOpen { tool, zone, port } => println!(
            "firewall: {} allows UDP {port}{}",
            tool.label(),
            zone.map(|z| format!(" (zone {z})")).unwrap_or_default()
        ),
        LinuxPlan::Open {
            tool,
            port,
            known_closed,
            cmds,
        } => {
            let line = firewall::shell_line(&cmds);
            let state = if known_closed {
                format!("{} blocks inbound UDP {port}", tool.label())
            } else {
                format!(
                    "{} is active; could not read whether UDP {port} is open without root",
                    tool.label()
                )
            };
            if !std::io::stdin().is_terminal() {
                println!(
                    "firewall: {state}. Links still work outbound-initiated; to allow inbound run:\n  {line}"
                );
                return;
            }
            print!(
                "firewall: {state}.\nOpen UDP {port} in {}? [y/N] ",
                tool.label()
            );
            let _ = std::io::stdout().flush();
            let mut answer = String::new();
            let _ = std::io::stdin().lock().read_line(&mut answer);
            if !matches!(answer.trim(), "y" | "Y" | "yes" | "Yes") {
                println!("firewall: left as is; to allow inbound later run:\n  {line}");
                return;
            }
            for c in &cmds {
                println!("+ {}", firewall::shell_line(std::slice::from_ref(c)));
                // Interactive sudo: it may prompt for the password on this terminal.
                let ok = Command::new(&c[0])
                    .args(&c[1..])
                    .status()
                    .is_ok_and(|s| s.success());
                if !ok {
                    eprintln!("enishi: that failed; run it yourself:\n  {line}");
                    return;
                }
            }
            println!("firewall: opened UDP {port} in {}", tool.label());
        }
    }
}

pub fn uninstall(name: &str, m: Option<&Manifest>, _no_firewall: bool) -> Result<()> {
    let unit_name = format!("{name}.service");
    let unit = match m.and_then(|m| m.unit.clone()) {
        Some(u) => u,
        None => unit_dir()?.join(&unit_name),
    };
    if unit.exists() {
        if !is_ours(&unit) {
            anyhow::bail!(
                "{} was not written by enishi install; leaving it alone",
                unit.display()
            );
        }
        let _ = systemctl(&["disable", "--now", &unit_name]);
        std::fs::remove_file(&unit).with_context(|| format!("removing {}", unit.display()))?;
        println!("stopped and removed {}", unit.display());
        systemctl(&["daemon-reload"])?;
        let _ = systemctl(&["reset-failed", &unit_name]);
    } else {
        println!("no unit {} (already removed)", unit.display());
    }
    match m {
        Some(m) => {
            match std::fs::remove_file(&m.bin) {
                Ok(()) => println!("removed {}", m.bin.display()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e).with_context(|| format!("removing {}", m.bin.display())),
            }
            // A custom --prefix dir goes too if we left it empty (never ~/.local/bin).
            if let Some(dir) = m.bin.parent()
                && default_prefix().ok().as_deref() != Some(dir)
            {
                let _ = std::fs::remove_dir(dir);
            }
        }
        None => println!("binary left in place (no install record says where it came from)"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upgrade_says_restarted() {
        assert_eq!(started_word(true), "restarted");
        assert_eq!(started_word(false), "started");
    }
}

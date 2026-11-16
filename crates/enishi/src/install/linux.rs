//! Linux: binary in `~/.local/bin`, `systemd --user` unit, linger hint.

use std::{
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result};

use super::{Ctx, InstallArgs, Manifest, copy_atomic, render, run_cmd, same_file};

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
        systemctl(&["restart", &unit_name])?;
        println!("started {unit_name}");
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

pub fn uninstall(name: &str, m: Option<&Manifest>) -> Result<()> {
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

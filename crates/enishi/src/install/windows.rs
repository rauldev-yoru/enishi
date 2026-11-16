//! Windows: `%LOCALAPPDATA%\Programs\enishi\{enishi.exe, enishiw.exe}`, user PATH, a logon
//! scheduled task running the windowless copy in the user's session, and the Private-only
//! firewall rule (one UAC prompt: an elevated helper of the installed `enishi.exe`).

use std::{
    ffi::OsStr,
    os::windows::{ffi::OsStrExt, process::CommandExt},
    path::{Path, PathBuf},
    process::{Command, ExitCode},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};

use super::{
    Ctx, FirewallArgs, InstallArgs, Manifest, copy_atomic,
    firewall::{self as fw, WinFwState, WinRule},
    render, run_cmd, same_file,
};

pub const CLI_EXE: &str = "enishi.exe";
/// The same binary with the GUI subsystem bit (no console window): what the task runs.
pub const DAEMON_EXE: &str = "enishiw.exe";
const MARKER: &str = "Written by `enishi install`";
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
/// How long to wait for the UAC prompt to be answered (and the helper to finish) before
/// carrying on as if it had been declined.
const ELEVATION_TIMEOUT: Duration = Duration::from_secs(60);

fn env_path(k: &str) -> Result<PathBuf> {
    std::env::var_os(k)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .with_context(|| format!("{k} is not set"))
}

fn default_prefix() -> Result<PathBuf> {
    Ok(env_path("LOCALAPPDATA")?.join("Programs").join("enishi"))
}

fn system32(exe: &str) -> PathBuf {
    let root = std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into());
    PathBuf::from(root).join("System32").join(exe)
}

fn hidden(program: &str) -> Command {
    let mut c = Command::new(system32(program));
    c.creation_flags(CREATE_NO_WINDOW);
    c
}

fn schtasks(args: &[&str]) -> Result<String> {
    run_cmd(hidden("schtasks.exe").args(args))
}

fn task_xml_of(name: &str) -> Option<String> {
    schtasks(&["/Query", "/XML", "/TN", name]).ok()
}

fn current_user() -> String {
    let user = std::env::var("USERNAME").unwrap_or_default();
    match std::env::var("USERDOMAIN") {
        Ok(d) if !d.is_empty() => format!(r"{d}\{user}"),
        _ => user,
    }
}

fn resolve_prefix(arg: Option<&PathBuf>, previous: Option<&Manifest>) -> Result<PathBuf> {
    Ok(match (arg, previous) {
        (Some(p), _) => std::path::absolute(p)?,
        (None, Some(m)) => m
            .bin
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or(default_prefix()?),
        (None, None) => default_prefix()?,
    })
}

pub fn install(ctx: &Ctx, args: &InstallArgs, previous: Option<&Manifest>) -> Result<Manifest> {
    let prefix = resolve_prefix(args.prefix.as_ref(), previous)?;
    let cli = prefix.join(CLI_EXE);
    let daemon = prefix.join(DAEMON_EXE);
    let existing = task_xml_of(&args.name);
    if let Some(x) = &existing
        && !x.contains(MARKER)
    {
        bail!(
            "a scheduled task {:?} exists and was not created by enishi install; pick another --name",
            args.name
        );
    }
    std::fs::create_dir_all(&prefix).with_context(|| format!("creating {}", prefix.display()))?;
    remove_stale(&prefix);

    // 1. Stop the installed daemon (its exe is locked while it runs).
    if existing.is_some() {
        let _ = schtasks(&["/End", "/TN", &args.name]);
    }
    let killed = kill_by_path(&daemon);
    if killed > 0 {
        println!("stopped the running daemon ({killed} process)");
    }

    // 2. Binaries: the CLI copy and the windowless daemon copy.
    let bytes = std::fs::read(&ctx.current_exe)
        .with_context(|| format!("reading {}", ctx.current_exe.display()))?;
    if same_file(&ctx.current_exe, &cli) {
        println!("binary: {} (already the running one)", cli.display());
    } else {
        let upgrading = cli.exists();
        place(&bytes, &cli)?;
        println!(
            "binary: {} ({})",
            cli.display(),
            if upgrading { "replaced" } else { "installed" }
        );
    }
    let mut gui = bytes;
    render::pe_to_gui(&mut gui)?;
    place(&gui, &daemon)?;
    println!("daemon: {} (no console window)", daemon.display());

    // 3. User PATH.
    let added_now = path_add(&prefix)?;
    if added_now {
        println!(
            "PATH: added {} to the user PATH (new terminals see it)",
            prefix.display()
        );
    }
    let path_added = added_now || previous.is_some_and(|m| m.path_added);

    // 4. Logon task.
    let home = env_path("USERPROFILE").unwrap_or_else(|_| prefix.clone());
    let xml = render::task_xml(
        &args.name,
        &current_user(),
        &daemon,
        &render::task_arguments(ctx.pinned_config.as_deref()),
        &home,
    );
    let tmp = std::env::temp_dir().join(format!("enishi-task-{}.xml", std::process::id()));
    std::fs::write(&tmp, render::utf16le_bom(&xml))?;
    let created = schtasks(&[
        "/Create",
        "/TN",
        &args.name,
        "/XML",
        &tmp.to_string_lossy(),
        "/F",
    ]);
    let _ = std::fs::remove_file(&tmp);
    created.context("registering the logon task")?;
    println!(
        "task: {:?} (at logon of {}, in your session, no time limit, restart on failure)",
        args.name,
        current_user()
    );
    // 5. Firewall, before the daemon first listens: once our rule exists Windows never
    // shows its own "allow access" prompt (which leaves Block rules when ignored).
    if args.no_firewall {
        println!("firewall: skipped (--no-firewall)");
    } else {
        ensure_firewall(&args.name, &prefix, &cli);
    }
    if args.no_start {
        println!("not started now (--no-start); it starts at your next logon");
    } else {
        schtasks(&["/Run", "/TN", &args.name]).context("starting the task")?;
        println!("started task {:?}", args.name);
    }
    Ok(Manifest {
        name: args.name.clone(),
        version: env!("CARGO_PKG_VERSION").into(),
        bin: cli,
        daemon_bin: Some(daemon),
        config: ctx.pinned_config.clone(),
        created_config: false,
        unit: None,
        path_added,
    })
}

pub fn after_install(ctx: &Ctx, args: &InstallArgs, _m: &Manifest) {
    let cfg = ctx
        .pinned_config
        .as_ref()
        .map(|c| format!(" --config \"{}\"", c.display()))
        .unwrap_or_default();
    println!(
        "manage: schtasks /Query /TN {n} /V /FO LIST · schtasks /End|/Run /TN {n} · enishi{cfg} logs -f",
        n = args.name
    );
}

pub fn uninstall(name: &str, m: Option<&Manifest>, no_firewall: bool) -> Result<()> {
    match task_xml_of(name) {
        Some(x) if !x.contains(MARKER) => {
            bail!("scheduled task {name:?} was not created by enishi install; leaving it alone")
        }
        Some(_) => {
            let _ = schtasks(&["/End", "/TN", name]);
            schtasks(&["/Delete", "/TN", name, "/F"])?;
            println!("removed scheduled task {name:?}");
        }
        None => println!("no scheduled task {name:?} (already removed)"),
    }
    let Some(m) = m else {
        println!("binaries and PATH left in place (no install record says what was installed)");
        return Ok(());
    };
    let prefix = m.bin.parent().map(Path::to_path_buf).unwrap_or_default();
    let daemon = m
        .daemon_bin
        .clone()
        .unwrap_or_else(|| prefix.join(DAEMON_EXE));
    let killed = kill_by_path(&daemon);
    if killed > 0 {
        println!("stopped the running daemon ({killed} process)");
    }
    // Before the binaries go: the elevated helper is the installed enishi.exe.
    remove_firewall_rules(name, &prefix, &m.bin, no_firewall);
    if m.path_added && path_remove(&prefix)? {
        println!("PATH: removed {}", prefix.display());
    }
    let me = std::env::current_exe().ok();
    let mut deferred = Vec::new();
    for f in [&daemon, &m.bin] {
        match std::fs::remove_file(f) {
            Ok(()) => println!("removed {}", f.display()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                let running_me = me.as_ref().is_some_and(|me| same_file(me, f));
                if running_me {
                    deferred.push(f.clone());
                } else {
                    return Err(e).with_context(|| format!("removing {}", f.display()));
                }
            }
        }
    }
    remove_stale(&prefix);
    if deferred.is_empty() {
        let _ = std::fs::remove_dir(&prefix);
    } else {
        delete_after_exit(&deferred, &prefix);
        println!(
            "{} is this running program; it is deleted right after it exits",
            deferred[0].display()
        );
    }
    Ok(())
}

/// Put `bytes` at `dst`; a locked (running) `dst` is first renamed aside, which Windows
/// allows for a running image, and cleaned up by a later install/uninstall.
fn place(bytes: &[u8], dst: &Path) -> Result<()> {
    if dst.exists() && std::fs::remove_file(dst).is_err() {
        let ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let mut aside = dst.as_os_str().to_owned();
        aside.push(format!(".old-{ms}"));
        std::fs::rename(dst, &aside)
            .with_context(|| format!("{} is locked and cannot be moved aside", dst.display()))?;
        println!(
            "{} was in use: moved aside to {}",
            dst.display(),
            Path::new(&aside).display()
        );
    }
    copy_atomic(bytes, dst)
}

/// Best-effort removal of `*.old-*` / `*.new` leftovers from earlier upgrades.
fn remove_stale(prefix: &Path) {
    let Ok(rd) = std::fs::read_dir(prefix) else {
        return;
    };
    for e in rd.flatten() {
        let n = e.file_name().to_string_lossy().to_lowercase();
        let ours = n.starts_with("enishi.exe.") || n.starts_with("enishiw.exe.");
        if ours && (n.contains(".old-") || n.ends_with(".new")) {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

fn delete_after_exit(files: &[PathBuf], dir: &Path) {
    let mut line = String::from("/d /c ping -n 3 127.0.0.1 >nul");
    for f in files {
        line.push_str(&format!(" & del /f /q \"{}\"", f.display()));
    }
    line.push_str(&format!(
        " & del /f /q \"{}\\enishi*.exe.old-*\" & rd \"{}\"",
        dir.display(),
        dir.display()
    ));
    let spawn = |flags: u32| {
        Command::new(system32("cmd.exe"))
            .raw_arg(&line)
            .creation_flags(flags)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
    };
    if spawn(CREATE_NO_WINDOW | CREATE_BREAKAWAY_FROM_JOB).is_err() {
        let _ = spawn(CREATE_NO_WINDOW);
    }
}

// ---------- firewall ----------

fn netsh(line: &str) -> Result<String> {
    run_cmd(hidden("netsh.exe").raw_arg(line))
}

fn firewall_programs(prefix: &Path) -> [PathBuf; 2] {
    [prefix.join(DAEMON_EXE), prefix.join(CLI_EXE)]
}

/// Inbound UDP (QUIC), Private profile only, scoped to the two enishi binaries; first
/// removes every rule bound to those paths (what Windows' "allow access" prompt creates:
/// allow or block, TCP+UDP, often Public too) and any previous rule of ours.
fn firewall_add(rule: &str, prefix: &Path) -> Result<()> {
    firewall_remove_all(rule, prefix);
    for p in firewall_programs(prefix) {
        netsh(&format!(
            "advfirewall firewall add rule name=\"{rule}\" dir=in action=allow \
             program=\"{}\" protocol=UDP profile=private enable=yes \
             description=\"enishi device link (QUIC). Written by enishi install.\"",
            p.display()
        ))?;
    }
    Ok(())
}

/// Our named rule plus every rule bound to the enishi binaries (best effort).
fn firewall_remove_all(rule: &str, prefix: &Path) {
    let _ = netsh(&format!("advfirewall firewall delete rule name=\"{rule}\""));
    for p in firewall_programs(prefix) {
        let _ = netsh(&format!(
            "advfirewall firewall delete rule name=all program=\"{}\"",
            p.display()
        ));
    }
}

/// Every rule bound to the enishi binaries or named `rule` (read-only, works unelevated,
/// locale-independent unlike `netsh show`). `None` if the query failed.
fn query_rules(rule: &str, prefix: &Path) -> Option<Vec<WinRule>> {
    let progs: Vec<String> = firewall_programs(prefix)
        .iter()
        .map(|p| render::ps_quote(&p.to_string_lossy()))
        .collect();
    let script = format!(
        "$ErrorActionPreference='SilentlyContinue'; \
         [Console]::OutputEncoding = New-Object System.Text.UTF8Encoding($false); \
         $rules = @(); \
         foreach ($p in @({progs})) {{ $rules += @(Get-NetFirewallApplicationFilter -Program $p | Get-NetFirewallRule) }}; \
         $rules += @(Get-NetFirewallRule -DisplayName {rule}); \
         $seen = @{{}}; \
         foreach ($r in $rules) {{ if ($r -and -not $seen.ContainsKey($r.Name)) {{ $seen[$r.Name] = 1; \
           $prog = ($r | Get-NetFirewallApplicationFilter).Program; \
           $proto = ($r | Get-NetFirewallPortFilter).Protocol; \
           \"$($r.DisplayName)`t$($r.Profile)`t$proto`t$($r.Direction)`t$($r.Action)`t$prog`t$($r.Enabled)\" }} }}; \
         '{ok}'",
        progs = progs.join(","),
        rule = render::ps_quote(rule),
        ok = fw::WIN_QUERY_OK,
    );
    let out = hidden(r"WindowsPowerShell\v1.0\powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .output()
        .ok()?;
    fw::parse_win_rules(&String::from_utf8_lossy(&out.stdout))
}

fn rule_state(rule: &str, prefix: &Path) -> Option<(WinFwState, Vec<WinRule>)> {
    let rules = query_rules(rule, prefix)?;
    let [d, c] = firewall_programs(prefix);
    let (d, c) = (d.to_string_lossy(), c.to_string_lossy());
    Some((fw::win_rule_state(&rules, rule, &[&d, &c]), rules))
}

/// The one-line hint printed when the rule could not be set.
fn later_hint(name: &str) -> String {
    let n = if name == "enishi" {
        String::new()
    } else {
        format!(" --name {name}")
    };
    format!(
        "enishi works without it (links start outbound); to add it later run `enishi firewall{n}`"
    )
}

/// Install step: make the rules exactly ours, asking for elevation once if needed.
fn ensure_firewall(name: &str, prefix: &Path, cli: &Path) {
    let state = rule_state(name, prefix);
    if let Some((WinFwState::Correct, _)) = state {
        println!("firewall: rule {name:?} already in place (UDP in, Private only)");
        return;
    }
    if let Some((WinFwState::Wrong(p), _)) = &state {
        println!("firewall: fixing the rules for enishi:");
        for l in p {
            println!("  {l}");
        }
    }
    if is_elevated() {
        match firewall_add(name, prefix) {
            Ok(()) => println!("firewall: rule {name:?} (UDP in, Private profile only)"),
            Err(e) => eprintln!("enishi: firewall rule failed: {e:#}; {}", later_hint(name)),
        }
        return;
    }
    println!("firewall: asking Windows for admin rights once (UAC) to add the Private-only rule…");
    match elevate(cli, &fw::helper_args(name, prefix, false)) {
        Elevation::Done(0) => match rule_state(name, prefix) {
            Some((WinFwState::Correct, _)) | None => {
                println!("firewall: rule {name:?} (UDP in, Private profile only)")
            }
            Some((s, _)) => eprintln!(
                "enishi: firewall rules still not as expected ({s:?}); {}",
                later_hint(name)
            ),
        },
        other => println!("firewall: {}; {}", other.describe(), later_hint(name)),
    }
}

/// Uninstall step: remove our rule and Windows' rules for the binaries (one UAC prompt).
fn remove_firewall_rules(name: &str, prefix: &Path, cli: &Path, no_firewall: bool) {
    let rules = query_rules(name, prefix);
    if rules.as_ref().is_some_and(|r| r.is_empty()) {
        return;
    }
    if is_elevated() {
        firewall_remove_all(name, prefix);
        println!("removed the firewall rules for enishi");
        return;
    }
    let print_cmd = |why: &str| {
        let [d, c] = firewall_programs(prefix);
        println!(
            "firewall rules for enishi remain ({why}); remove them from PowerShell (UAC prompt):\n  {}",
            render::elevated_firewall_remove_cmd(name, &[&d, &c])
        );
    };
    if no_firewall {
        print_cmd("--no-firewall");
        return;
    }
    let exe = if cli.exists() {
        cli.to_path_buf()
    } else {
        match std::env::current_exe() {
            Ok(e) => e,
            Err(_) => return print_cmd("no enishi.exe to elevate"),
        }
    };
    println!("firewall: asking Windows for admin rights once (UAC) to remove enishi's rules…");
    match elevate(&exe, &fw::helper_args(name, prefix, true)) {
        Elevation::Done(0) => println!("removed the firewall rules for enishi"),
        other => print_cmd(&other.describe()),
    }
}

pub fn firewall_cmd(args: &FirewallArgs) -> Result<ExitCode> {
    let previous = Manifest::load(&args.name)?;
    let prefix = resolve_prefix(args.prefix.as_ref(), previous.as_ref())?;
    let name = &args.name;
    if args.check {
        return Ok(match rule_state(name, &prefix) {
            Some((WinFwState::Correct, _)) => {
                println!(
                    "firewall: rule {name:?} is in place (UDP in, Private only), nothing else"
                );
                ExitCode::SUCCESS
            }
            Some((WinFwState::Missing, _)) => {
                println!("firewall: no rules for enishi under {}", prefix.display());
                ExitCode::FAILURE
            }
            Some((WinFwState::Wrong(p), _)) => {
                println!("firewall: needs fixing (`enishi firewall`):");
                for l in p {
                    println!("  {l}");
                }
                ExitCode::FAILURE
            }
            None => {
                eprintln!("enishi: could not query the firewall rules");
                ExitCode::from(2)
            }
        });
    }
    if is_elevated() {
        if args.remove {
            firewall_remove_all(name, &prefix);
            println!(
                "removed the firewall rules for enishi under {}",
                prefix.display()
            );
        } else {
            firewall_add(name, &prefix)?;
            println!(
                "firewall rule {name:?}: UDP in, Private profile only, for {} and {}",
                prefix.join(DAEMON_EXE).display(),
                prefix.join(CLI_EXE).display()
            );
        }
        return Ok(ExitCode::SUCCESS);
    }
    if args.elevated {
        eprintln!("enishi: the elevated helper is not elevated; giving up");
        return Ok(ExitCode::from(3));
    }
    // Nothing to do without elevation?
    match (args.remove, rule_state(name, &prefix)) {
        (false, Some((WinFwState::Correct, _))) => {
            println!("firewall: rule {name:?} already in place (UDP in, Private only)");
            return Ok(ExitCode::SUCCESS);
        }
        (true, Some((_, rules))) if rules.is_empty() => {
            println!("firewall: no rules for enishi to remove");
            return Ok(ExitCode::SUCCESS);
        }
        _ => {}
    }
    let installed = prefix.join(CLI_EXE);
    let exe = if installed.exists() {
        installed
    } else {
        std::env::current_exe().context("cannot locate the running enishi binary")?
    };
    println!("firewall: asking Windows for admin rights (UAC)…");
    match elevate(&exe, &fw::helper_args(name, &prefix, args.remove)) {
        Elevation::Done(0) => {
            if args.remove {
                println!("removed the firewall rules for enishi");
            } else {
                println!("firewall rule {name:?}: UDP in, Private profile only");
            }
            Ok(ExitCode::SUCCESS)
        }
        other => {
            eprintln!("enishi: firewall unchanged: {}", other.describe());
            Ok(ExitCode::FAILURE)
        }
    }
}

// ---------- elevation ----------

#[derive(Debug)]
enum Elevation {
    /// The elevated helper ran; its exit code.
    Done(u32),
    /// The user said no to the UAC prompt.
    Declined,
    /// Nobody answered (or the helper hung) within ELEVATION_TIMEOUT.
    TimedOut,
    Failed(String),
}

impl Elevation {
    fn describe(&self) -> String {
        match self {
            Elevation::Done(c) => format!("the elevated helper failed (exit {c})"),
            Elevation::Declined => "UAC prompt declined".into(),
            Elevation::TimedOut => format!(
                "no answer to the UAC prompt within {} s",
                ELEVATION_TIMEOUT.as_secs()
            ),
            Elevation::Failed(e) => format!("elevation failed: {e}"),
        }
    }
}

/// Run `exe args` elevated (`runas`: the UAC prompt), hidden, and wait for it. The prompt
/// blocks `ShellExecuteExW`, so it runs on its own thread and is abandoned after
/// ELEVATION_TIMEOUT (a late "yes" still runs the helper, which only sets the rules right).
fn elevate(exe: &Path, args: &str) -> Elevation {
    let (tx, rx) = std::sync::mpsc::channel();
    let exe = exe.to_path_buf();
    let args = args.to_string();
    let spawned = std::thread::Builder::new()
        .name("uac".into())
        .spawn(move || {
            let _ = tx.send(elevate_blocking(&exe, &args));
        });
    if let Err(e) = spawned {
        return Elevation::Failed(e.to_string());
    }
    rx.recv_timeout(ELEVATION_TIMEOUT)
        .unwrap_or(Elevation::TimedOut)
}

fn elevate_blocking(exe: &Path, args: &str) -> Elevation {
    use windows_sys::Win32::{
        Foundation::{CloseHandle, ERROR_CANCELLED, GetLastError, WAIT_OBJECT_0},
        System::{
            Com::{COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoInitializeEx},
            Threading::{GetExitCodeProcess, WaitForSingleObject},
        },
        UI::{
            Shell::{
                SEE_MASK_FLAG_NO_UI, SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW,
                ShellExecuteExW,
            },
            WindowsAndMessaging::SW_HIDE,
        },
    };
    let verb = wide(OsStr::new("runas"));
    let file = wide(exe.as_os_str());
    let params = wide(OsStr::new(args));
    unsafe {
        CoInitializeEx(
            std::ptr::null(),
            (COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) as u32,
        );
        let mut info: SHELLEXECUTEINFOW = std::mem::zeroed();
        info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
        info.fMask = SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC | SEE_MASK_FLAG_NO_UI;
        info.lpVerb = verb.as_ptr();
        info.lpFile = file.as_ptr();
        info.lpParameters = params.as_ptr();
        info.nShow = SW_HIDE;
        if ShellExecuteExW(&mut info) == 0 {
            let err = GetLastError();
            return if err == ERROR_CANCELLED {
                Elevation::Declined
            } else {
                Elevation::Failed(format!(
                    "{} (error {err})",
                    std::io::Error::from_raw_os_error(err as i32)
                ))
            };
        }
        if info.hProcess.is_null() {
            return Elevation::Failed("no process handle".into());
        }
        let ms = ELEVATION_TIMEOUT.as_millis() as u32;
        let r = if WaitForSingleObject(info.hProcess, ms) == WAIT_OBJECT_0 {
            let mut code = 0u32;
            if GetExitCodeProcess(info.hProcess, &mut code) != 0 {
                Elevation::Done(code)
            } else {
                Elevation::Failed("cannot read the helper's exit code".into())
            }
        } else {
            Elevation::TimedOut
        };
        CloseHandle(info.hProcess);
        r
    }
}

// ---------- Win32 helpers ----------

fn wide(s: &OsStr) -> Vec<u16> {
    s.encode_wide().chain(Some(0)).collect()
}

pub fn is_elevated() -> bool {
    use windows_sys::Win32::{
        Foundation::{CloseHandle, HANDLE},
        Security::{GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation},
        System::Threading::{GetCurrentProcess, OpenProcessToken},
    };
    unsafe {
        let mut token: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return false;
        }
        let mut elev = TOKEN_ELEVATION { TokenIsElevated: 0 };
        let mut len = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            (&mut elev as *mut TOKEN_ELEVATION).cast(),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut len,
        );
        CloseHandle(token);
        ok != 0 && elev.TokenIsElevated != 0
    }
}

/// Terminate every process whose image is exactly `exe`, waiting for each to exit.
fn kill_by_path(exe: &Path) -> usize {
    use windows_sys::Win32::{
        Foundation::{CloseHandle, INVALID_HANDLE_VALUE},
        System::{
            Diagnostics::ToolHelp::{
                CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
                TH32CS_SNAPPROCESS,
            },
            Threading::{
                OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
                PROCESS_SYNCHRONIZE, PROCESS_TERMINATE, QueryFullProcessImageNameW,
                TerminateProcess, WaitForSingleObject,
            },
        },
    };
    let want = exe.to_string_lossy().to_lowercase();
    let file_name = exe
        .file_name()
        .map(|f| f.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let me = std::process::id();
    let mut killed = 0;
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap == INVALID_HANDLE_VALUE {
            return 0;
        }
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut more = Process32FirstW(snap, &mut entry) != 0;
        while more {
            let pid = entry.th32ProcessID;
            let len = entry
                .szExeFile
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(entry.szExeFile.len());
            let name = String::from_utf16_lossy(&entry.szExeFile[..len]).to_lowercase();
            if pid != me && name == file_name {
                let h = OpenProcess(
                    PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_TERMINATE | PROCESS_SYNCHRONIZE,
                    0,
                    pid,
                );
                if !h.is_null() {
                    let mut buf = [0u16; 1024];
                    let mut n = buf.len() as u32;
                    if QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, buf.as_mut_ptr(), &mut n)
                        != 0
                        && String::from_utf16_lossy(&buf[..n as usize]).to_lowercase() == want
                        && TerminateProcess(h, 1) != 0
                    {
                        WaitForSingleObject(h, 5000);
                        killed += 1;
                    }
                    CloseHandle(h);
                }
            }
            more = Process32NextW(snap, &mut entry) != 0;
        }
        CloseHandle(snap);
    }
    if killed > 0 {
        // Let the loader release the image file.
        std::thread::sleep(Duration::from_millis(200));
    }
    killed
}

fn norm_dir(s: &str) -> String {
    s.trim().trim_end_matches(['\\', '/']).to_lowercase()
}

/// `HKCU\Environment\Path`, unexpanded, with its value type.
fn read_user_path() -> Result<(String, u32)> {
    use windows_sys::Win32::{
        Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS},
        System::Registry::{
            HKEY_CURRENT_USER, REG_EXPAND_SZ, RRF_NOEXPAND, RRF_RT_REG_EXPAND_SZ, RRF_RT_REG_SZ,
            RegGetValueW,
        },
    };
    let key = wide(OsStr::new("Environment"));
    let val = wide(OsStr::new("Path"));
    let flags = RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ | RRF_NOEXPAND;
    unsafe {
        let mut ty = 0u32;
        let mut size = 0u32;
        let rc = RegGetValueW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            val.as_ptr(),
            flags,
            &mut ty,
            std::ptr::null_mut(),
            &mut size,
        );
        if rc == ERROR_FILE_NOT_FOUND {
            return Ok((String::new(), REG_EXPAND_SZ));
        }
        if rc != ERROR_SUCCESS {
            bail!("reading HKCU\\Environment\\Path failed (error {rc})");
        }
        let mut buf = vec![0u16; (size as usize).div_ceil(2) + 1];
        let mut size = (buf.len() * 2) as u32;
        let rc = RegGetValueW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            val.as_ptr(),
            flags,
            &mut ty,
            buf.as_mut_ptr().cast(),
            &mut size,
        );
        if rc != ERROR_SUCCESS {
            bail!("reading HKCU\\Environment\\Path failed (error {rc})");
        }
        let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        Ok((String::from_utf16_lossy(&buf[..len]), ty))
    }
}

fn write_user_path(value: &str, ty: u32) -> Result<()> {
    use windows_sys::Win32::{
        Foundation::ERROR_SUCCESS,
        System::Registry::{
            HKEY, HKEY_CURRENT_USER, KEY_SET_VALUE, RegCloseKey, RegOpenKeyExW, RegSetValueExW,
        },
        UI::WindowsAndMessaging::{
            HWND_BROADCAST, SMTO_ABORTIFHUNG, SendMessageTimeoutW, WM_SETTINGCHANGE,
        },
    };
    let key = wide(OsStr::new("Environment"));
    let name = wide(OsStr::new("Path"));
    let data = wide(OsStr::new(value));
    unsafe {
        let mut hkey: HKEY = std::ptr::null_mut();
        let rc = RegOpenKeyExW(HKEY_CURRENT_USER, key.as_ptr(), 0, KEY_SET_VALUE, &mut hkey);
        if rc != ERROR_SUCCESS {
            bail!("opening HKCU\\Environment failed (error {rc})");
        }
        let rc = RegSetValueExW(
            hkey,
            name.as_ptr(),
            0,
            ty,
            data.as_ptr().cast(),
            (data.len() * 2) as u32,
        );
        RegCloseKey(hkey);
        if rc != ERROR_SUCCESS {
            bail!("writing HKCU\\Environment\\Path failed (error {rc})");
        }
        // Tell Explorer (and so new terminals) that the environment changed.
        let env = wide(OsStr::new("Environment"));
        let mut res = 0usize;
        SendMessageTimeoutW(
            HWND_BROADCAST,
            WM_SETTINGCHANGE,
            0,
            env.as_ptr() as isize,
            SMTO_ABORTIFHUNG,
            3000,
            &mut res,
        );
    }
    Ok(())
}

/// Append `dir` to the user PATH unless already there. Returns whether it was added.
fn path_add(dir: &Path) -> Result<bool> {
    let (cur, ty) = read_user_path()?;
    let d = dir.to_string_lossy();
    if cur.split(';').any(|e| norm_dir(e) == norm_dir(&d)) {
        return Ok(false);
    }
    let new = if cur.trim_end_matches(';').is_empty() {
        d.to_string()
    } else {
        format!("{};{d}", cur.trim_end_matches(';'))
    };
    write_user_path(&new, ty)?;
    Ok(true)
}

/// Remove `dir` from the user PATH. Returns whether it was there.
fn path_remove(dir: &Path) -> Result<bool> {
    let (cur, ty) = read_user_path()?;
    let d = norm_dir(&dir.to_string_lossy());
    let kept: Vec<&str> = cur.split(';').filter(|e| norm_dir(e) != d).collect();
    let new = kept.join(";");
    if new == cur {
        return Ok(false);
    }
    write_user_path(&new, ty)?;
    Ok(true)
}

/// Is this process a descendant of the installed daemon (`enishiw.exe`), e.g. an `exec`
/// child? Ending the logon task kills its whole job, so such a process cannot replace the
/// daemon and live to start it again (bug #11).
pub fn inside_daemon(m: &Manifest) -> bool {
    let Some(daemon) = &m.daemon_bin else {
        return false;
    };
    let want = daemon.to_string_lossy().to_lowercase();
    ancestors().iter().any(|exe| exe.to_lowercase() == want)
}

/// Image paths of this process's ancestors (nearest first, up to 32).
fn ancestors() -> Vec<String> {
    use windows_sys::Win32::{
        Foundation::{CloseHandle, INVALID_HANDLE_VALUE},
        System::{
            Diagnostics::ToolHelp::{
                CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
                TH32CS_SNAPPROCESS,
            },
            Threading::{
                OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
                QueryFullProcessImageNameW,
            },
        },
    };
    let mut parent = std::collections::HashMap::new();
    // SAFETY: snapshot handle checked and closed; entry sized as required.
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap == INVALID_HANDLE_VALUE {
            return Vec::new();
        }
        let mut e: PROCESSENTRY32W = std::mem::zeroed();
        e.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut more = Process32FirstW(snap, &mut e) != 0;
        while more {
            parent.insert(e.th32ProcessID, e.th32ParentProcessID);
            more = Process32NextW(snap, &mut e) != 0;
        }
        CloseHandle(snap);
    }
    let mut out = Vec::new();
    let mut pid = std::process::id();
    for _ in 0..32 {
        let Some(&pp) = parent.get(&pid) else { break };
        if pp == 0 || pp == pid {
            break;
        }
        // SAFETY: handle checked and closed; buffer length passed in/out.
        unsafe {
            let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pp);
            if h.is_null() {
                break;
            }
            let mut buf = [0u16; 1024];
            let mut n = buf.len() as u32;
            if QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, buf.as_mut_ptr(), &mut n) != 0 {
                out.push(String::from_utf16_lossy(&buf[..n as usize]));
            }
            CloseHandle(h);
        }
        pid = pp;
    }
    out
}

pub fn stop(m: &Manifest) -> Result<String> {
    let ended = schtasks(&["/End", "/TN", &m.name]).is_ok();
    let killed = m.daemon_bin.as_deref().map(kill_by_path).unwrap_or(0);
    Ok(format!(
        "schtasks /End /TN {}{}; {killed} daemon process(es) terminated",
        m.name,
        if ended { "" } else { " (not running)" }
    ))
}

pub fn start(m: &Manifest) -> Result<String> {
    schtasks(&["/Run", "/TN", &m.name]).context("starting the task")?;
    Ok(format!("schtasks /Run /TN {}", m.name))
}

/// Write an installed binary (a running one is moved aside first).
pub fn put(bytes: &[u8], dst: &Path) -> Result<()> {
    place(bytes, dst)
}

/// Point stdout/stderr of this (windowless) updater at the log, so the install steps it
/// reuses leave their output there.
pub fn redirect_stdio(log: &Path) {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::System::Console::{STD_ERROR_HANDLE, STD_OUTPUT_HANDLE, SetStdHandle};
    if let Ok(f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)
    {
        let h = f.as_raw_handle();
        // SAFETY: the handle stays valid for the life of the process (leaked below).
        unsafe {
            SetStdHandle(STD_OUTPUT_HANDLE, h as _);
            SetStdHandle(STD_ERROR_HANDLE, h as _);
        }
        std::mem::forget(f);
    }
}

fn in_job(process: windows_sys::Win32::Foundation::HANDLE) -> Option<bool> {
    use windows_sys::Win32::System::JobObjects::IsProcessInJob;
    let mut b = 0;
    // SAFETY: valid process handle; null job = "any job".
    (unsafe { IsProcessInJob(process, std::ptr::null_mut(), &mut b) } != 0).then_some(b != 0)
}

pub fn diagnostics() -> String {
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    // SAFETY: pseudo handle of this process.
    let job = in_job(unsafe { GetCurrentProcess() });
    format!(
        "in a job object: {}",
        match job {
            Some(true) => "yes",
            Some(false) => "no",
            None => "unknown",
        }
    )
}

pub fn delete_task(name: &str) {
    let _ = schtasks(&["/Delete", "/TN", name, "/F"]);
}

/// Start `exe args` outside the daemon's lifetime: broken away from every job object
/// (suspended first, to check it really left them all), else via a one-shot scheduled
/// task that deletes itself.
pub fn spawn_detached(exe: &Path, args: &[String], _log: &Path, unit: &str) -> Result<String> {
    let why = match spawn_breakaway(exe, args) {
        Ok(()) => return Ok("process broken away from the daemon's job objects".into()),
        Err(e) => format!("{e:#}"),
    };
    let mut a: Vec<String> = args.to_vec();
    a.extend(["--task".into(), unit.into()]);
    let line = a
        .iter()
        .map(|s| render::win_arg(s))
        .collect::<Vec<_>>()
        .join(" ");
    let workdir = exe.parent().map(Path::to_path_buf).unwrap_or_default();
    let xml = render::update_task_xml(&current_user(), exe, &line, &workdir);
    let tmp = std::env::temp_dir().join(format!("enishi-update-{}.xml", std::process::id()));
    std::fs::write(&tmp, render::utf16le_bom(&xml))?;
    let created = schtasks(&["/Create", "/TN", unit, "/XML", &tmp.to_string_lossy(), "/F"]);
    let _ = std::fs::remove_file(&tmp);
    created.context("registering the one-shot updater task")?;
    if let Err(e) = schtasks(&["/Run", "/TN", unit]) {
        delete_task(unit);
        return Err(e.context("starting the one-shot updater task"));
    }
    Ok(format!("one-shot task {unit:?} (breakaway failed: {why})"))
}

fn spawn_breakaway(exe: &Path, args: &[String]) -> Result<()> {
    use windows_sys::Win32::{
        Foundation::CloseHandle,
        System::Threading::{
            CREATE_NEW_PROCESS_GROUP, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessW,
            DETACHED_PROCESS, PROCESS_INFORMATION, ResumeThread, STARTUPINFOW, TerminateProcess,
        },
    };
    let mut line: Vec<u16> = std::iter::once(render::win_arg(&exe.to_string_lossy()))
        .chain(args.iter().map(|a| render::win_arg(a)))
        .collect::<Vec<_>>()
        .join(" ")
        .encode_utf16()
        .chain([0])
        .collect();
    let app = wide(exe.as_os_str());
    let cwd = exe.parent().map(|d| wide(d.as_os_str()));
    let flags = CREATE_BREAKAWAY_FROM_JOB
        | DETACHED_PROCESS
        | CREATE_NEW_PROCESS_GROUP
        | CREATE_NO_WINDOW
        | CREATE_SUSPENDED
        | CREATE_UNICODE_ENVIRONMENT;
    // SAFETY: valid NUL-terminated buffers; handles from CreateProcessW are closed.
    unsafe {
        let mut si: STARTUPINFOW = std::mem::zeroed();
        si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
        let mut pi: PROCESS_INFORMATION = std::mem::zeroed();
        let ok = CreateProcessW(
            app.as_ptr(),
            line.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            0,
            flags,
            std::ptr::null(),
            cwd.as_ref().map_or(std::ptr::null(), |c| c.as_ptr()),
            &si,
            &mut pi,
        );
        if ok == 0 {
            bail!(
                "CreateProcess with CREATE_BREAKAWAY_FROM_JOB: {}",
                std::io::Error::last_os_error()
            );
        }
        let still = in_job(pi.hProcess);
        if still != Some(false) {
            TerminateProcess(pi.hProcess, 1);
            CloseHandle(pi.hThread);
            CloseHandle(pi.hProcess);
            bail!("the new process is still in a job object (a parent job forbids breakaway)");
        }
        ResumeThread(pi.hThread);
        CloseHandle(pi.hThread);
        CloseHandle(pi.hProcess);
    }
    Ok(())
}

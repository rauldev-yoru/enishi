//! `enishi update`: replace the installed enishi with a release (or a given binary) through
//! a *detached* updater, so the update survives stopping the daemon it may have been
//! started from (bug #11, design D16/D17).
//!
//! Stage 1 (`enishi update`, in the caller's process tree — possibly an `exec` child or
//! the daemon itself): pick the source, download + verify (SHA256SUMS, `--version`
//! probe), stage the new binary and a copy of the updater, then launch stage 2 outside the
//! daemon's lifetime (Linux: a transient `systemd-run --user` unit, outside the daemon's
//! cgroup; Windows: a process broken away from every job object, or a one-shot scheduled
//! task). Stage 2 (`update --stage2`, hidden) stops the daemon, swaps the binary, starts it
//! and waits for it to answer with the new version; on any failure after stopping it puts
//! the old binary back and restarts the daemon, so a device is never left without one.
//! Both stages record their phases in `update.json` (and `update.log`) next to the config.

use std::{
    path::{Path, PathBuf},
    process::ExitCode,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use clap::Args;
use enishi_core::{daemon::UpdateOrder, ipc, paths::Paths};
use enishi_proto::exit;
use enishi_proto::ipc::{IpcRequest, IpcResponse, StatusReport};
use serde::{Deserialize, Serialize};

use super::{Ctx, InstallArgs, Manifest, data_dir, describe, fetch, parse_name, platform};

#[derive(Args, Debug, Default)]
pub struct UpdateArgs {
    /// Release tag to install [default: the newest release, pre-releases included]
    #[arg(long, value_name = "vX.Y.Z", conflicts_with = "from_file")]
    pub version: Option<String>,
    /// Install this binary instead of downloading one (a path on the device being updated)
    #[arg(long, value_name = "PATH")]
    pub from_file: Option<PathBuf>,
    /// Only report the installed, running and newest versions
    #[arg(long, conflicts_with_all = ["from_file", "status"])]
    pub check: bool,
    /// Show the last update's phases and result
    #[arg(long)]
    pub status: bool,
    /// Install name [default: the install this binary belongs to, else "enishi"]
    #[arg(long, value_parser = parse_name)]
    pub name: Option<String>,
    /// Update even when the new binary is identical to the installed one
    #[arg(long)]
    pub force: bool,
    /// Do not wait for the updater to finish
    #[arg(long)]
    pub no_wait: bool,
    /// How long to wait for the result, in seconds (remote: including the device coming
    /// back online)
    #[arg(long, default_value_t = 180, value_name = "SECS")]
    pub timeout: u64,
    /// Internal: run as the detached updater
    #[arg(long, hide = true)]
    pub stage2: bool,
    #[arg(long, hide = true)]
    pub id: Option<String>,
    #[arg(long, hide = true)]
    pub origin: Option<String>,
    #[arg(long, hide = true)]
    pub expect_version: Option<String>,
    #[arg(long, hide = true, default_value_t = 1000)]
    pub grace_ms: u64,
    /// Internal (Windows): the one-shot task that runs this updater, deleted when done.
    #[arg(long, hide = true)]
    pub task: Option<String>,
    /// Testing only: fail stage 2 at stop|swap|start|health (also `ENISHI_UPDATE_FAIL_AT`).
    #[arg(long, hide = true)]
    pub fail_at: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Running,
    Done,
    Failed,
    RolledBack,
    RollbackFailed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Phase {
    pub phase: String,
    /// Unix time, milliseconds.
    pub at: u64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub detail: String,
}

/// `update.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    pub id: String,
    pub name: String,
    /// `local`, `exec`/`daemon` (started from inside the daemon), `peer:<name>`.
    pub origin: String,
    pub state: State,
    pub from_version: String,
    #[serde(default)]
    pub to_version: Option<String>,
    #[serde(default)]
    pub source: String,
    pub phases: Vec<Phase>,
    #[serde(default)]
    pub error: Option<String>,
}

impl Status {
    pub fn finished(&self) -> bool {
        self.state != State::Running
    }

    fn last_at(&self) -> u64 {
        self.phases.last().map(|p| p.at).unwrap_or(0)
    }
}

/// An update still marked running this long after its last phase is considered dead.
const STALE_MS: u64 = 15 * 60 * 1000;
/// How long stage 2 waits for the restarted daemon to answer.
const HEALTH_TIMEOUT: Duration = Duration::from_secs(30);

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn new_id() -> String {
    format!("{:x}-{:x}", now_ms(), std::process::id())
}

/// UTC `HH:MM:SS.mmm` of a unix-ms time, for the log.
fn clock(ms: u64) -> String {
    let s = ms / 1000;
    format!(
        "{:02}:{:02}:{:02}.{:03}Z",
        (s / 3600) % 24,
        (s / 60) % 60,
        s % 60,
        ms % 1000
    )
}

/// Reads and writes `update.json` and appends to `update.log`.
struct Tracker {
    file: PathBuf,
    log: PathBuf,
    st: Status,
    /// Echo phases to stderr (an interactive stage 1).
    echo: bool,
}

pub fn read_status(paths: &Paths) -> Option<Status> {
    let text = std::fs::read_to_string(paths.update_status()).ok()?;
    serde_json::from_str(&text).ok()
}

/// Refuse to start while another update of this instance is in flight.
fn check_not_running(paths: &Paths, own_id: Option<&str>) -> Result<()> {
    if let Some(s) = read_status(paths)
        && s.state == State::Running
        && Some(s.id.as_str()) != own_id
        && now_ms().saturating_sub(s.last_at()) < STALE_MS
    {
        bail!(
            "another update ({}) is in progress since {}; see `enishi update --status` (it is considered dead after 15 min)",
            s.id,
            clock(s.phases.first().map(|p| p.at).unwrap_or(0))
        );
    }
    Ok(())
}

impl Tracker {
    fn create(paths: &Paths, st: Status, echo: bool) -> Result<Tracker> {
        std::fs::create_dir_all(&paths.dir)?;
        let t = Tracker {
            file: paths.update_status(),
            log: paths.update_log(),
            st,
            echo,
        };
        t.save()?;
        Ok(t)
    }

    /// Continue the record `id` (written by an earlier stage), or start one.
    fn resume(
        paths: &Paths,
        id: &str,
        fresh: impl FnOnce() -> Status,
        echo: bool,
    ) -> Result<Tracker> {
        match read_status(paths) {
            Some(st) if st.id == id => Ok(Tracker {
                file: paths.update_status(),
                log: paths.update_log(),
                st,
                echo,
            }),
            _ => Tracker::create(paths, fresh(), echo),
        }
    }

    fn reload(&mut self) {
        if let Ok(t) = std::fs::read_to_string(&self.file)
            && let Ok(st) = serde_json::from_str::<Status>(&t)
            && st.id == self.st.id
        {
            self.st = st;
        }
    }

    fn save(&self) -> Result<()> {
        let tmp = self.file.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(&self.st)? + "\n")
            .with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, &self.file)
            .with_context(|| format!("writing {}", self.file.display()))?;
        Ok(())
    }

    fn log(&self, line: &str) {
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log)
        {
            let _ = writeln!(f, "{} [{}] {line}", clock(now_ms()), self.st.id);
        }
    }

    fn phase(&mut self, phase: &str, detail: impl Into<String>) {
        let p = Phase {
            phase: phase.into(),
            at: now_ms(),
            detail: detail.into(),
        };
        self.log(&format!("{}: {}", p.phase, p.detail));
        if self.echo {
            eprintln!("  {:<13} {}", p.phase, p.detail);
        }
        self.st.phases.push(p);
        if let Err(e) = self.save() {
            self.log(&format!("cannot write the status file: {e:#}"));
        }
    }

    fn end(&mut self, state: State, phase: &str, detail: impl Into<String>) {
        let detail = detail.into();
        if state != State::Done {
            self.st.error = Some(self.st.error.clone().unwrap_or_else(|| detail.clone()));
        }
        self.st.state = state;
        self.phase(phase, detail);
    }
}

/// `enishi [-d dev] update …`.
pub async fn run(paths: &Paths, device: Option<String>, mut args: UpdateArgs) -> Result<ExitCode> {
    if args.fail_at.is_none() {
        args.fail_at = std::env::var("ENISHI_UPDATE_FAIL_AT")
            .ok()
            .filter(|v| !v.is_empty());
    }
    if args.stage2 {
        return stage2(paths, args).await;
    }
    if let Some(d) = device {
        return remote(paths, d, args).await;
    }
    let m = resolve_install(args.name.as_deref())?;
    let ipaths = instance_paths(&m)?;
    if args.status {
        return Ok(match read_status(&ipaths) {
            Some(s) => {
                print_status(&s);
                result_code(&s)
            }
            None => {
                println!("{}: no update recorded", m.name);
                ExitCode::SUCCESS
            }
        });
    }
    if args.check {
        let running = running_version(&ipaths).await;
        return check_report(&m.name, &m.version, running.as_deref()).await;
    }
    let origin = args.origin.clone().unwrap_or_else(|| {
        if platform::inside_daemon(&m) {
            "exec".into()
        } else {
            "local".into()
        }
    });
    stage1(&m, &ipaths, args, origin).await
}

/// The install `name`, or the one whose binary is running, or `enishi`.
pub(super) fn resolve_install(name: Option<&str>) -> Result<Manifest> {
    let name = match name {
        Some(n) => n.to_string(),
        None => own_install()
            .map(|m| m.name)
            .unwrap_or_else(|| "enishi".into()),
    };
    Manifest::load(&name)?.with_context(|| {
        format!(
            "no install record for {name:?}: install with `enishi install`{}",
            if name == "enishi" {
                " (or pick another with --name)"
            } else {
                ""
            }
        )
    })
}

/// The install whose CLI or daemon binary is the running executable.
pub fn own_install() -> Option<Manifest> {
    let exe = std::env::current_exe().ok()?;
    let dir = data_dir().ok()?.join("installs");
    std::fs::read_dir(dir).ok()?.flatten().find_map(|e| {
        let name = e.path().file_stem()?.to_string_lossy().into_owned();
        let m = Manifest::load(&name).ok()??;
        let mine = super::same_file(&exe, &m.bin)
            || m.daemon_bin
                .as_deref()
                .is_some_and(|d| super::same_file(&exe, d));
        mine.then_some(m)
    })
}

/// Paths of the config the installed daemon runs.
pub(super) fn instance_paths(m: &Manifest) -> Result<Paths> {
    Paths::resolve(m.config.as_deref())
}

async fn running_version(paths: &Paths) -> Option<String> {
    match ipc::request_timeout(
        paths,
        &IpcRequest::Status { device: None },
        Duration::from_secs(3),
    )
    .await
    {
        Ok(IpcResponse::Status(r)) => Some(r.version),
        _ => None,
    }
}

async fn check_report(who: &str, installed: &str, running: Option<&str>) -> Result<ExitCode> {
    let latest = fetch::latest_tag().await?;
    println!("{who}: installed {installed}");
    match running {
        Some(v) => println!("  daemon running {v}"),
        None => println!("  daemon not answering"),
    }
    let note = match fetch::compare(&latest, installed) {
        Some(std::cmp::Ordering::Greater) | None if fetch::tag_of(installed) != latest => {
            format!(" — update with `enishi update` (or `--version {latest}`)")
        }
        Some(std::cmp::Ordering::Less) => " — the installed version is newer".into(),
        _ => " — up to date".into(),
    };
    println!("  newest release {latest}{note}");
    Ok(ExitCode::SUCCESS)
}

/// The staging dir of install `name`: new binary, updater copy, backup.
fn staging_dir(name: &str) -> Result<PathBuf> {
    Ok(data_dir()?.join("update").join(name))
}

/// `enishi uninstall`: drop the update staging dir (new binary, updater copy, backups).
pub fn remove_staging(name: &str) {
    let Ok(dir) = staging_dir(name) else { return };
    match std::fs::remove_dir_all(&dir) {
        Ok(()) => println!("deleted {}", dir.display()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => eprintln!("enishi: could not delete {}: {e}", dir.display()),
    }
}

const NEW_EXE: &str = if cfg!(windows) {
    "enishi-new.exe"
} else {
    "enishi-new"
};
const UPDATER_EXE: &str = if cfg!(windows) {
    "enishi-updater.exe"
} else {
    "enishi-updater"
};

/// `<bin> --version` → `0.1.1`.
fn probe_version(bin: &Path) -> Result<String> {
    let mut cmd = std::process::Command::new(bin);
    cmd.arg("--version")
        .stdin(std::process::Stdio::null())
        .env_remove("ENISHI_CONFIG");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let out = cmd
        .output()
        .with_context(|| format!("{} does not run", bin.display()))?;
    let text = String::from_utf8_lossy(&out.stdout);
    match text.trim().strip_prefix("enishi ") {
        Some(v) if out.status.success() && !v.is_empty() && !v.contains(char::is_whitespace) => {
            Ok(v.to_string())
        }
        _ => bail!(
            "{} is not an enishi binary (`--version` printed {:?})",
            bin.display(),
            text.trim()
        ),
    }
}

/// Stage 1: fetch, verify, stage, detach.
async fn stage1(
    m: &Manifest,
    ipaths: &Paths,
    args: UpdateArgs,
    origin: String,
) -> Result<ExitCode> {
    let interactive = origin == "local" && !args.no_wait;
    let id = match &args.id {
        Some(id) => id.clone(),
        None => {
            check_not_running(ipaths, None)?;
            new_id()
        }
    };
    let fresh = || Status {
        id: id.clone(),
        name: m.name.clone(),
        origin: origin.clone(),
        state: State::Running,
        from_version: m.version.clone(),
        to_version: None,
        source: String::new(),
        phases: Vec::new(),
        error: None,
    };
    let mut tr = Tracker::resume(ipaths, &id, fresh, interactive)?;
    if interactive {
        eprintln!("updating {} (enishi {}) — id {id}", m.name, m.version);
    }
    match prepare_and_detach(m, ipaths, &args, &origin, &mut tr).await {
        Ok(Detached::UpToDate) => Ok(ExitCode::SUCCESS),
        Ok(Detached::Started(method)) => {
            if interactive {
                Ok(wait_local(ipaths, &mut tr, Duration::from_secs(args.timeout)).await)
            } else {
                println!(
                    "updater for {} started ({method}), id {id}: it stops the daemon in ~{:.0} s, swaps the binary and starts it again{}",
                    m.name,
                    if origin == "exec" { 2.0 } else { 1.0 },
                    if origin == "exec" {
                        ", which ends this session; follow with `enishi -d <this device> update --status`"
                    } else {
                        "; follow with `enishi update --status`"
                    }
                );
                Ok(ExitCode::SUCCESS)
            }
        }
        Err(e) => {
            tr.end(State::Failed, "failed", format!("{e:#}"));
            Err(e)
        }
    }
}

enum Detached {
    UpToDate,
    Started(String),
}

async fn prepare_and_detach(
    m: &Manifest,
    ipaths: &Paths,
    args: &UpdateArgs,
    origin: &str,
    tr: &mut Tracker,
) -> Result<Detached> {
    let stage = staging_dir(&m.name)?;
    std::fs::create_dir_all(&stage).with_context(|| format!("creating {}", stage.display()))?;
    let new_path = stage.join(NEW_EXE);

    // 1. Source.
    let (bytes, tag) = match &args.from_file {
        Some(f) => {
            let f = std::path::absolute(f)?;
            let bytes = std::fs::read(&f).with_context(|| format!("reading {}", f.display()))?;
            tr.st.source = format!("file {}", f.display());
            (bytes, None)
        }
        None => {
            let tag = match &args.version {
                Some(v) => fetch::tag_of(v),
                None => fetch::latest_tag()
                    .await
                    .context("finding the newest release")?,
            };
            let d = fetch::download(&tag).await?;
            tr.st.source = format!("release {tag} ({})", d.url);
            (d.bytes, Some(tag))
        }
    };
    let sha = fetch::sha256_hex(&bytes);
    tr.phase(
        "downloaded",
        format!(
            "{} — {} bytes, sha256 {}{}",
            tr.st.source,
            bytes.len(),
            &sha[..16],
            if tag.is_some() {
                " (matches SHA256SUMS)"
            } else {
                ""
            }
        ),
    );
    if super::same_file(&new_path, &m.bin) {
        bail!("the staging file is the installed binary");
    }
    super::copy_atomic(&bytes, &new_path)?;

    // 2. Verify it is an enishi that runs here.
    let version = probe_version(&new_path)?;
    if let Some(tag) = &tag
        && fetch::tag_of(&version) != *tag
    {
        bail!("release {tag} contains enishi {version}");
    }
    tr.st.to_version = Some(version.clone());
    let installed_same = std::fs::read(&m.bin)
        .map(|b| fetch::sha256_hex(&b) == sha)
        .unwrap_or(false);
    if installed_same && !args.force {
        let _ = std::fs::remove_file(&new_path);
        tr.end(
            State::Done,
            "done",
            format!("already up to date (enishi {version}, same binary); --force reinstalls"),
        );
        return Ok(Detached::UpToDate);
    }
    tr.phase("verified", format!("runs as enishi {version}"));

    // 3. The updater: a copy of this binary, so the installed one can be replaced (and,
    // on Windows, windowless so a scheduled task shows no console).
    let updater = stage.join(UPDATER_EXE);
    let exe = std::env::current_exe().context("cannot locate the running enishi binary")?;
    #[allow(unused_mut)]
    let mut me = std::fs::read(&exe).with_context(|| format!("reading {}", exe.display()))?;
    #[cfg(windows)]
    super::render::pe_to_gui(&mut me)?;
    if !super::same_file(&exe, &updater) {
        let _ = std::fs::remove_file(&updater);
        super::copy_atomic(&me, &updater)?;
    }

    // 4. Detach.
    let grace = if origin == "local" { 1000 } else { 2000 };
    let mut a: Vec<String> = vec![
        "--config".into(),
        ipaths.config_file.to_string_lossy().into_owned(),
        "update".into(),
        "--stage2".into(),
        "--name".into(),
        m.name.clone(),
        "--id".into(),
        tr.st.id.clone(),
        "--from-file".into(),
        new_path.to_string_lossy().into_owned(),
        "--expect-version".into(),
        version,
        "--grace-ms".into(),
        grace.to_string(),
    ];
    if let Some(f) = &args.fail_at {
        a.extend(["--fail-at".into(), f.clone()]);
    }
    let unit = format!(
        "{}-update-{}",
        m.name,
        tr.st.id.split('-').next().unwrap_or("x")
    );
    let method = platform::spawn_detached(&updater, &a, &ipaths.update_log(), &unit)
        .context("starting the detached updater")?;
    tr.phase("detached", method.clone());
    Ok(Detached::Started(method))
}

/// Interactive local update: follow the status file until the updater finishes.
async fn wait_local(ipaths: &Paths, tr: &mut Tracker, timeout: Duration) -> ExitCode {
    let t0 = Instant::now();
    let mut shown = tr.st.phases.len();
    loop {
        tokio::time::sleep(Duration::from_millis(300)).await;
        tr.reload();
        for p in &tr.st.phases[shown.min(tr.st.phases.len())..] {
            eprintln!("  {:<13} {}", p.phase, p.detail);
        }
        shown = tr.st.phases.len();
        if tr.st.finished() {
            print_result(&tr.st, None);
            return result_code(&tr.st);
        }
        if t0.elapsed() > timeout {
            eprintln!(
                "enishi: still running after {} s; follow with `enishi update --status` ({})",
                timeout.as_secs(),
                ipaths.update_log().display()
            );
            return ExitCode::FAILURE;
        }
    }
}

fn result_code(s: &Status) -> ExitCode {
    match s.state {
        State::Done => ExitCode::SUCCESS,
        State::Running => ExitCode::from(3),
        _ => ExitCode::FAILURE,
    }
}

fn print_result(s: &Status, version: Option<&str>) {
    let to = s.to_version.as_deref().unwrap_or("?");
    match s.state {
        State::Done => println!(
            "updated {}: {} → {to}{}",
            s.name,
            s.from_version,
            version
                .map(|v| format!(" (daemon reports {v})"))
                .unwrap_or_default()
        ),
        State::Running => println!("update {} still running", s.id),
        State::Failed => println!(
            "update failed, nothing changed: {}",
            s.error.as_deref().unwrap_or("?")
        ),
        State::RolledBack => println!(
            "update to {to} failed and was rolled back to {}: {}",
            s.from_version,
            s.error.as_deref().unwrap_or("?")
        ),
        State::RollbackFailed => println!(
            "update to {to} failed AND the rollback failed — the daemon may be down: {}",
            s.error.as_deref().unwrap_or("?")
        ),
    }
}

/// Phase timeline relative to the first phase.
pub fn print_status(s: &Status) {
    println!(
        "update {} of {} ({}, from {}): {}",
        s.id,
        s.name,
        s.origin,
        s.from_version,
        serde_json::to_value(s.state)
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_default()
    );
    let t0 = s.phases.first().map(|p| p.at).unwrap_or(0);
    for p in &s.phases {
        println!(
            "  +{:>6.1}s  {:<13} {}",
            p.at.saturating_sub(t0) as f64 / 1000.0,
            p.phase,
            p.detail
        );
    }
}

/// `enishi install` run from inside the installed daemon (an `exec` child): stopping the
/// daemon would kill this process, so hand the upgrade to the detached updater.
pub async fn install_detached(
    paths: &Paths,
    args: &InstallArgs,
    prev: &Manifest,
) -> Result<ExitCode> {
    let exe = std::env::current_exe().context("cannot locate the running enishi binary")?;
    if let Some(p) = &args.prefix
        && std::path::absolute(p).ok().as_deref() != prev.bin.parent()
    {
        bail!(
            "this runs inside the installed daemon ({}); --prefix cannot change from here — run the install outside enishi, or `enishi update --from-file` to upgrade in place",
            prev.name
        );
    }
    let ipaths = instance_paths(prev)?;
    if paths.config_file != ipaths.config_file {
        eprintln!(
            "enishi: note: the install keeps its config {}",
            ipaths.config_file.display()
        );
    }
    println!(
        "running inside the installed daemon {:?}: handing the upgrade to a detached updater (bug #11)",
        prev.name
    );
    let up = UpdateArgs {
        from_file: Some(exe),
        name: Some(prev.name.clone()),
        force: true,
        timeout: 180,
        grace_ms: 2000,
        ..UpdateArgs::default()
    };
    stage1(prev, &ipaths, up, "exec".into()).await
}

/// Daemon hook: a peer asked for an update (already authorized). Start stage 1 as a child
/// and return the id at once.
pub fn spawn_for_peer(paths: &Paths, order: UpdateOrder) -> Result<String, String> {
    let m = own_install()
        .ok_or("this daemon does not run an installed enishi (no install record for its binary)")?;
    let ipaths = instance_paths(&m).map_err(|e| format!("{e:#}"))?;
    if ipaths.config_file != paths.config_file {
        return Err(format!(
            "this daemon runs {} but install {:?} is pinned to {}",
            paths.config_file.display(),
            m.name,
            ipaths.config_file.display()
        ));
    }
    check_not_running(&ipaths, None).map_err(|e| format!("{e:#}"))?;
    let id = new_id();
    let origin = format!("peer:{}", order.peer);
    let mut tr = Tracker::create(
        &ipaths,
        Status {
            id: id.clone(),
            name: m.name.clone(),
            origin: origin.clone(),
            state: State::Running,
            from_version: m.version.clone(),
            to_version: None,
            source: String::new(),
            phases: Vec::new(),
            error: None,
        },
        false,
    )
    .map_err(|e| format!("{e:#}"))?;
    tr.phase(
        "requested",
        format!(
            "by {} ({})",
            order.peer,
            order
                .from_file
                .as_ref()
                .map(|f| format!("file {f}"))
                .or_else(|| order.version.as_ref().map(|v| format!("release {v}")))
                .unwrap_or_else(|| "newest release".into())
        ),
    );
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("--config").arg(&ipaths.config_file).args([
        "update",
        "--no-wait",
        "--name",
        &m.name,
        "--id",
        &id,
        "--origin",
        &origin,
    ]);
    if let Some(v) = &order.version {
        cmd.args(["--version", v]);
    }
    if let Some(f) = &order.from_file {
        cmd.args(["--from-file", f]);
    }
    cmd.stdin(std::process::Stdio::null());
    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(ipaths.update_log())
    {
        Ok(f) => {
            let f2 = f.try_clone().map_err(|e| e.to_string())?;
            cmd.stdout(f).stderr(f2);
        }
        Err(_) => {
            cmd.stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    match cmd.spawn() {
        Ok(mut child) => {
            // Reap it in the background (it exits once stage 2 is detached).
            std::thread::spawn(move || child.wait());
            Ok(id)
        }
        Err(e) => {
            tr.end(
                State::Failed,
                "failed",
                format!("cannot start the updater: {e}"),
            );
            Err(format!("cannot start the updater: {e}"))
        }
    }
}

/// Stage 2: the detached updater.
async fn stage2(paths: &Paths, args: UpdateArgs) -> Result<ExitCode> {
    platform::redirect_stdio(&paths.update_log());
    let name = args.name.clone().context("--stage2 needs --name")?;
    let id = args.id.clone().context("--stage2 needs --id")?;
    let new_path = args
        .from_file
        .clone()
        .context("--stage2 needs --from-file")?;
    let expect = args
        .expect_version
        .clone()
        .context("--stage2 needs --expect-version")?;
    tokio::time::sleep(Duration::from_millis(args.grace_ms)).await;
    let m = Manifest::load(&name)?.with_context(|| format!("no install record for {name:?}"))?;
    let mut tr = Tracker::resume(
        paths,
        &id,
        || Status {
            id: id.clone(),
            name: name.clone(),
            origin: "local".into(),
            state: State::Running,
            from_version: m.version.clone(),
            to_version: Some(expect.clone()),
            source: format!("file {}", new_path.display()),
            phases: Vec::new(),
            error: None,
        },
        false,
    )?;
    tr.log(&format!(
        "stage 2: pid {} — {}",
        std::process::id(),
        platform::diagnostics()
    ));
    let stage = staging_dir(&name)?;
    let code = match swap(
        paths,
        &m,
        &new_path,
        &expect,
        &stage,
        args.fail_at.as_deref(),
        &mut tr,
    )
    .await
    {
        Ok(r) => {
            tr.end(
                State::Done,
                "done",
                format!("daemon answers: {}", describe(&r)),
            );
            ExitCode::SUCCESS
        }
        Err(Failure::BeforeStop(e)) => {
            tr.end(State::Failed, "failed", format!("{e:#} (daemon untouched)"));
            ExitCode::FAILURE
        }
        Err(Failure::AfterStop(e, backup)) => {
            let e = format!("{e:#}");
            tr.st.error = Some(e.clone());
            tr.phase("rolling_back", e);
            match rollback(paths, &m, &backup).await {
                Ok(r) => tr.end(
                    State::RolledBack,
                    "rolled_back",
                    format!("previous binary restored, daemon answers: {}", describe(&r)),
                ),
                Err(e2) => tr.end(State::RollbackFailed, "rollback_failed", format!("{e2:#}")),
            }
            ExitCode::FAILURE
        }
    };
    let _ = std::fs::remove_file(&new_path);
    let _ = std::fs::remove_dir_all(stage.join("backup"));
    if let Some(task) = &args.task {
        platform::delete_task(task);
    }
    Ok(code)
}

enum Failure {
    BeforeStop(anyhow::Error),
    AfterStop(anyhow::Error, Vec<(PathBuf, PathBuf)>),
}

fn fail_point(fail_at: Option<&str>, here: &str) -> Result<()> {
    if fail_at == Some(here) {
        bail!("simulated failure at {here} (ENISHI_UPDATE_FAIL_AT, testing only)");
    }
    Ok(())
}

async fn swap(
    paths: &Paths,
    m: &Manifest,
    new_path: &Path,
    expect: &str,
    stage: &Path,
    fail_at: Option<&str>,
    tr: &mut Tracker,
) -> Result<StatusReport, Failure> {
    // Back up what is installed (pairs of installed → backup).
    let backup_dir = stage.join("backup");
    let mut backup = Vec::new();
    let prep = (|| -> Result<()> {
        if !new_path.exists() {
            bail!("{} is gone", new_path.display());
        }
        std::fs::create_dir_all(&backup_dir)?;
        for f in std::iter::once(&m.bin).chain(m.daemon_bin.as_ref()) {
            let b = backup_dir.join(f.file_name().context("binary has no file name")?);
            std::fs::copy(f, &b)
                .with_context(|| format!("backing up {} to {}", f.display(), b.display()))?;
            backup.push((f.clone(), b));
        }
        fail_point(fail_at, "stop")
    })();
    if let Err(e) = prep {
        return Err(Failure::BeforeStop(e));
    }

    let stopped = platform::stop(m);
    tr.phase(
        "stopped",
        match &stopped {
            Ok(d) => d.clone(),
            Err(e) => format!("{e:#} (continuing)"),
        },
    );
    let after = |e: anyhow::Error| Failure::AfterStop(e, backup.clone());

    let ctx = Ctx {
        pinned_config: m.config.clone(),
        current_exe: new_path.to_path_buf(),
        listen_port: enishi_core::config::Config::load(&paths.config_file)
            .ok()
            .and_then(|c| c.device.listen_port),
    };
    let iargs = InstallArgs {
        prefix: m.bin.parent().map(Path::to_path_buf),
        name: m.name.clone(),
        no_start: true,
        no_firewall: true,
        firewall: false,
    };
    let mut nm = platform::install(&ctx, &iargs, Some(m)).map_err(after)?;
    nm.version = expect.to_string();
    nm.created_config = m.created_config;
    nm.path_added = nm.path_added || m.path_added;
    nm.save().map_err(after)?;
    tr.phase(
        "swapped",
        format!("{} is enishi {expect}", nm.bin.display()),
    );
    fail_point(fail_at, "swap").map_err(after)?;

    let started = platform::start(&nm).map_err(after)?;
    tr.phase("started", started);
    fail_point(fail_at, "start").map_err(after)?;

    let r = wait_version(paths, Some(expect), HEALTH_TIMEOUT)
        .await
        .map_err(after)?;
    fail_point(fail_at, "health").map_err(after)?;
    Ok(r)
}

/// Put the backed-up binaries and the old record back, then start the daemon again.
async fn rollback(
    paths: &Paths,
    m: &Manifest,
    backup: &[(PathBuf, PathBuf)],
) -> Result<StatusReport> {
    let _ = platform::stop(m);
    for (dst, b) in backup {
        let bytes = std::fs::read(b).with_context(|| format!("reading backup {}", b.display()))?;
        platform::put(&bytes, dst)?;
    }
    m.save()?;
    platform::start(m)?;
    wait_version(paths, None, HEALTH_TIMEOUT).await
}

/// Wait until the local daemon answers (with `want` as its version, if given).
async fn wait_version(
    paths: &Paths,
    want: Option<&str>,
    timeout: Duration,
) -> Result<StatusReport> {
    let t0 = Instant::now();
    let mut last = None;
    loop {
        if let Ok(IpcResponse::Status(r)) = ipc::request_timeout(
            paths,
            &IpcRequest::Status { device: None },
            Duration::from_secs(2),
        )
        .await
        {
            if want.is_none_or(|w| w == r.version) {
                return Ok(r);
            }
            last = Some(r.version);
        }
        if t0.elapsed() > timeout {
            match last {
                Some(v) => bail!(
                    "the daemon answers as enishi {v}, expected {}",
                    want.unwrap_or("?")
                ),
                None => bail!("the daemon did not answer within {} s", timeout.as_secs()),
            }
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
}

/// `enishi -d <dev> update …`.
async fn remote(paths: &Paths, device: String, args: UpdateArgs) -> Result<ExitCode> {
    let status_req = IpcRequest::UpdateStatus {
        device: Some(device.clone()),
    };
    if args.status || args.check {
        return match ipc::request_timeout(paths, &status_req, Duration::from_secs(20)).await? {
            IpcResponse::UpdateStatus {
                device,
                version,
                status,
            } => {
                if args.check {
                    return check_report(&device, &version, Some(&version)).await;
                }
                println!("{device}: enishi {version}");
                match status.as_deref().map(serde_json::from_str::<Status>) {
                    Some(Ok(s)) => {
                        print_status(&s);
                        Ok(result_code(&s))
                    }
                    Some(Err(e)) => bail!("{device}: unreadable update status: {e}"),
                    None => {
                        println!("  no update recorded");
                        Ok(ExitCode::SUCCESS)
                    }
                }
            }
            IpcResponse::Error { code, message } => {
                eprintln!("enishi: {message}");
                Ok(crate::code(code))
            }
            other => bail!("unexpected reply {other:?}"),
        };
    }
    if args.name.is_some() || args.force {
        bail!("--name/--force apply to local updates only");
    }
    let req = IpcRequest::Update {
        device: Some(device.clone()),
        version: args.version.clone(),
        from_file: args
            .from_file
            .as_ref()
            .map(|f| f.to_string_lossy().into_owned()),
    };
    let t0 = Instant::now();
    let (device, id) = match ipc::request_timeout(paths, &req, Duration::from_secs(30)).await? {
        IpcResponse::UpdateStarted { device, id } => (device, id),
        IpcResponse::Error { code, message } => {
            eprintln!("enishi: {message}");
            return Ok(crate::code(code));
        }
        other => bail!("unexpected reply {other:?}"),
    };
    println!("{device}: updater started (id {id})");
    if args.no_wait {
        println!("follow with `enishi -d {device} update --status`");
        return Ok(ExitCode::SUCCESS);
    }
    let timeout = Duration::from_secs(args.timeout);
    let el = |t0: Instant| format!("[{:>5.1}s]", t0.elapsed().as_secs_f64());
    let mut shown = 0usize;
    let mut online = true;
    loop {
        tokio::time::sleep(Duration::from_millis(1000)).await;
        match ipc::request_timeout(paths, &status_req, Duration::from_secs(10)).await {
            Ok(IpcResponse::UpdateStatus {
                version, status, ..
            }) => {
                if !online {
                    println!("{} {device} is back online (enishi {version})", el(t0));
                    online = true;
                }
                let st = status
                    .as_deref()
                    .and_then(|s| serde_json::from_str::<Status>(s).ok());
                if let Some(s) = st.filter(|s| s.id == id) {
                    // Device-side timestamps: phases logged while the link was down arrive
                    // together but keep their real offsets.
                    let p0 = s.phases.first().map(|p| p.at).unwrap_or(0);
                    for p in &s.phases[shown.min(s.phases.len())..] {
                        println!(
                            "  +{:>6.1}s  {:<13} {}",
                            p.at.saturating_sub(p0) as f64 / 1000.0,
                            p.phase,
                            p.detail
                        );
                    }
                    shown = s.phases.len();
                    if s.finished() {
                        print_result(&s, Some(&version));
                        if s.state == State::Done && s.to_version.as_deref() != Some(&version) {
                            eprintln!("enishi: but {device} reports enishi {version}");
                            return Ok(ExitCode::FAILURE);
                        }
                        return Ok(result_code(&s));
                    }
                }
            }
            Ok(IpcResponse::Error { code, message }) => {
                if online {
                    println!(
                        "{} link down ({message}) — expected while the daemon restarts",
                        el(t0)
                    );
                    online = false;
                }
                if code == exit::USAGE {
                    eprintln!("enishi: {message}");
                    return Ok(crate::code(code));
                }
            }
            Ok(other) => bail!("unexpected reply {other:?}"),
            Err(e) => {
                if online {
                    println!("{} local daemon not answering: {e}", el(t0));
                    online = false;
                }
            }
        }
        if t0.elapsed() > timeout {
            eprintln!(
                "enishi: no result after {} s; check later with `enishi -d {device} update --status`",
                timeout.as_secs()
            );
            return Ok(ExitCode::FAILURE);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_roundtrip_and_states() {
        let s = Status {
            id: "abc-1".into(),
            name: "enishi".into(),
            origin: "peer:server".into(),
            state: State::RolledBack,
            from_version: "0.1.0".into(),
            to_version: Some("0.1.1".into()),
            source: "file /tmp/x".into(),
            phases: vec![Phase {
                phase: "stopped".into(),
                at: 5,
                detail: String::new(),
            }],
            error: Some("boom".into()),
        };
        let j = serde_json::to_string(&s).unwrap();
        assert!(j.contains("\"rolled_back\""), "{j}");
        assert!(!j.contains("\"detail\""), "empty detail omitted: {j}");
        assert_eq!(serde_json::from_str::<Status>(&j).unwrap(), s);
        assert!(s.finished());
        assert_eq!(result_code(&s), ExitCode::FAILURE);
        assert_eq!(clock(3_723_004), "01:02:03.004Z");
    }

    #[test]
    fn fail_points() {
        assert!(fail_point(Some("swap"), "swap").is_err());
        assert!(fail_point(Some("swap"), "stop").is_ok());
        assert!(fail_point(None, "start").is_ok());
    }

    #[test]
    fn tracker_resume_and_stale_guard() {
        let dir = std::env::temp_dir().join(format!("enishi-upd-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let paths = Paths::resolve(Some(&dir.join("enishi.toml"))).unwrap();
        let fresh = |id: &str| Status {
            id: id.into(),
            name: "t".into(),
            origin: "local".into(),
            state: State::Running,
            from_version: "0.1.0".into(),
            to_version: None,
            source: String::new(),
            phases: vec![],
            error: None,
        };
        let mut t = Tracker::create(&paths, fresh("a"), false).unwrap();
        t.phase("downloaded", "x");
        assert!(check_not_running(&paths, None).is_err(), "running blocks");
        assert!(
            check_not_running(&paths, Some("a")).is_ok(),
            "own id passes"
        );
        let mut t2 = Tracker::resume(&paths, "a", || fresh("zzz"), false).unwrap();
        assert_eq!(t2.st.phases.len(), 1, "resumed the same record");
        t2.end(State::Done, "done", "ok");
        assert!(
            check_not_running(&paths, None).is_ok(),
            "finished does not block"
        );
        t.reload();
        assert_eq!(t.st.state, State::Done);
        let t3 = Tracker::resume(&paths, "b", || fresh("b"), false).unwrap();
        assert!(t3.st.phases.is_empty(), "other id starts fresh");
        assert!(
            std::fs::read_to_string(paths.update_log())
                .unwrap()
                .contains("downloaded: x")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

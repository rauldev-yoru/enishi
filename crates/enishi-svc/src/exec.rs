//! `exec` sessions on the executing device: shell selection, spawn, stdio pump, timeout and
//! process-tree kill (design §3.3, D2).
//!
//! Transport-agnostic: the daemon feeds stdin chunks through a channel (closing it = EOF)
//! and receives [`ExecFrame::Stdout`] / [`ExecFrame::Stderr`] through another. Dropping the
//! output receiver means the caller is gone: the process tree is killed.

use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::OnceLock,
    time::Duration,
};

use enishi_proto::{
    exit,
    session::{ExecFrame, ExecRequest, MAX_CHUNK},
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    process::{Child, Command},
    sync::mpsc,
    task::JoinHandle,
    time::Instant,
};

/// After the process exits, output still arriving from pipes held open by background
/// children is forwarded until it goes quiet for this long.
pub const DRAIN_GRACE: Duration = Duration::from_millis(500);

/// Shells accepted by `--shell`.
/// Carries a `--shell cmd` line to the inner cmd (see [`plan`]).
const CMD_LINE_VAR: &str = "ENISHI_CMD_LINE";

/// Prepended to every PowerShell command line so its output is UTF-8 (see `plan`).
pub const PS_UTF8_PRELUDE: &str = "$__u=[System.Text.UTF8Encoding]::new($false); [Console]::OutputEncoding=$__u; [Console]::InputEncoding=$__u; $OutputEncoding=$__u; Remove-Variable __u; ";

pub const SHELLS: &[&str] = &["pwsh", "powershell", "cmd", "bash", "sh"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Shell {
    Pwsh,
    /// Windows PowerShell 5.1.
    Powershell,
    Cmd,
    Bash,
    Sh,
    /// A POSIX shell by path (`$SHELL`).
    Posix(String),
}

impl Shell {
    pub fn parse(name: &str) -> Option<Shell> {
        Some(match name {
            "pwsh" => Shell::Pwsh,
            "powershell" => Shell::Powershell,
            "cmd" => Shell::Cmd,
            "bash" => Shell::Bash,
            "sh" => Shell::Sh,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Os {
    Windows,
    Unix,
}

impl Os {
    pub const fn current() -> Os {
        if cfg!(windows) { Os::Windows } else { Os::Unix }
    }
}

/// D2: Windows → `pwsh` if on PATH, else Windows PowerShell; unix → `$SHELL`, else `/bin/sh`.
pub fn default_shell(os: Os, shell_env: Option<&str>, pwsh_on_path: bool) -> Shell {
    match os {
        Os::Windows if pwsh_on_path => Shell::Pwsh,
        Os::Windows => Shell::Powershell,
        Os::Unix => match shell_env.map(str::trim).filter(|s| !s.is_empty()) {
            Some(s) => Shell::Posix(s.into()),
            None => Shell::Posix("/bin/sh".into()),
        },
    }
}

/// [`default_shell`] for this machine (looked up once).
pub fn local_default_shell() -> Shell {
    static SHELL: OnceLock<Shell> = OnceLock::new();
    SHELL
        .get_or_init(|| {
            let os = Os::current();
            let env = std::env::var("SHELL").ok();
            let pwsh = os == Os::Windows && on_path("pwsh.exe");
            default_shell(os, env.as_deref(), pwsh)
        })
        .clone()
}

fn on_path(exe: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(exe).is_file()))
}

/// Default `PATHEXT` when the variable is missing.
pub const DEFAULT_PATHEXT: &str = ".COM;.EXE;.BAT;.CMD";

/// Resolve a raw-mode program the way a Windows shell does: a bare name is looked up in
/// each `PATH` directory, trying every `PATHEXT` extension unless the name already has one
/// of them; a name with a directory part is only completed with the extensions. Returns
/// `None` when nothing matches (the spawn then reports "not found").
///
/// Needed because Rust's `Command` only appends `.exe`, so the `.cmd` shims that `npx`,
/// `pnpm`, `yarn`… install are not found by name. The current directory is deliberately
/// not searched (cmd.exe does; that is how a planted `npx.cmd` in a cwd gets run).
/// Pure (filesystem access through `is_file`) so it is unit-tested on every OS.
pub fn resolve_windows_program(
    program: &str,
    path: Option<&std::ffi::OsStr>,
    pathext: Option<&str>,
    is_file: impl Fn(&Path) -> bool,
) -> Option<PathBuf> {
    let exts: Vec<String> = pathext
        .filter(|v| !v.trim().is_empty())
        .unwrap_or(DEFAULT_PATHEXT)
        .split(';')
        .map(|e| e.trim().to_string())
        .filter(|e| e.starts_with('.') && e.len() > 1)
        .collect();
    let has_ext = Path::new(program)
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| exts.iter().any(|x| x[1..].eq_ignore_ascii_case(e)));
    let candidates = |base: PathBuf| -> Vec<PathBuf> {
        if has_ext {
            return vec![base];
        }
        exts.iter()
            .map(|e| {
                let mut s = base.clone().into_os_string();
                s.push(e);
                PathBuf::from(s)
            })
            .collect()
    };
    if program.contains(['/', '\\', ':']) {
        return candidates(PathBuf::from(program))
            .into_iter()
            .find(|c| is_file(c));
    }
    let path = path?;
    std::env::split_paths(path)
        .filter(|d| !d.as_os_str().is_empty())
        .flat_map(|d| candidates(d.join(program)))
        .find(|c| is_file(c))
}

/// The program to hand to `Command::new` for a raw plan: on Windows resolved through
/// `PATH` (the request's own `PATH`, if it sets one) + `PATHEXT`, so `.cmd`/`.bat` shims
/// work. A resolved batch file is then started by Rust's std, which (since 1.77, the
/// BatBadBut fix CVE-2024-24576) runs it through `cmd.exe /e:ON /v:OFF /d /c` with every
/// argument quoted and `%`/`"` escaped for cmd, and refuses arguments it cannot pass
/// safely (`\r`, `\n`) with `InvalidInput` instead of letting cmd re-parse them.
fn raw_program(program: &str, _env: &[(String, String)]) -> std::ffi::OsString {
    #[cfg(windows)]
    {
        let path = _env
            .iter()
            .rev()
            .find(|(k, _)| k.eq_ignore_ascii_case("PATH"))
            .map(|(_, v)| std::ffi::OsString::from(v))
            .or_else(|| std::env::var_os("PATH"));
        let pathext = std::env::var("PATHEXT").ok();
        if let Some(p) =
            resolve_windows_program(program, path.as_deref(), pathext.as_deref(), |p| {
                p.is_file()
            })
        {
            return p.into_os_string();
        }
    }
    program.into()
}

/// How to start a process: `program args… [raw_tail]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub program: String,
    pub args: Vec<String>,
    /// Appended verbatim to the Windows command line (cmd.exe's own quoting rules); a
    /// plain extra argument elsewhere.
    pub raw_tail: Option<String>,
    /// Extra environment for the process, applied after the request's `env`.
    pub env: Vec<(String, String)>,
}

/// A request that can't be started; `code` is one of [`exit`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecError {
    pub code: i32,
    pub message: String,
}

impl ExecError {
    fn new(code: i32, message: impl Into<String>) -> Self {
        ExecError {
            code,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for ExecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ExecError {}

/// Build the process to start. Shell mode joins the words into one command string;
/// `raw` uses them as argv.
pub fn plan(req: &ExecRequest, default: impl FnOnce() -> Shell) -> Result<Plan, ExecError> {
    if req.command.is_empty() || req.command[0].is_empty() {
        return Err(ExecError::new(exit::USAGE, "empty command"));
    }
    if req.raw {
        if req.shell.is_some() {
            return Err(ExecError::new(
                exit::USAGE,
                "--raw and --shell are exclusive",
            ));
        }
        return Ok(Plan {
            program: req.command[0].clone(),
            args: req.command[1..].to_vec(),
            raw_tail: None,
            env: Vec::new(),
        });
    }
    let shell = match &req.shell {
        Some(name) => Shell::parse(name).ok_or_else(|| {
            ExecError::new(
                exit::USAGE,
                format!("unknown shell {name:?} (one of {})", SHELLS.join(", ")),
            )
        })?,
        None => default(),
    };
    let line = req.command.join(" ");
    // PowerShell writes to a redirected stdout using [Console]::OutputEncoding, which
    // defaults to the OEM code page (accents and `€` arrive broken). Force UTF-8 without
    // BOM for output and native-command pipes, then run the line. Piped stdin is already
    // decoded (OEM) before this runs, so non-ASCII stdin to PowerShell stays a known gap.
    let ps_line = format!("{PS_UTF8_PRELUDE}{line}");
    let ps = |program: &str| Plan {
        program: program.into(),
        args: ["-NoProfile", "-NoLogo", "-NonInteractive", "-Command"]
            .map(String::from)
            .into_iter()
            .chain([ps_line.clone()])
            .collect(),
        raw_tail: None,
        env: Vec::new(),
    };
    let posix = |program: &str| Plan {
        program: program.into(),
        args: vec!["-c".into(), line.clone()],
        raw_tail: None,
        env: Vec::new(),
    };
    Ok(match shell {
        Shell::Pwsh => ps("pwsh"),
        Shell::Powershell => ps("powershell"),
        // `/s` makes cmd strip exactly the outer quotes and run the rest verbatim.
        //
        // Output encoding: cmd's built-ins (echo, dir, ver…) and most console programs
        // encode for the console *output code page*, which defaults to the OEM page
        // (cp850/cp437…), so "Versión" arrived as `Versi\xa2n` and `€` as `?` (lost).
        // Rejected: `cmd /u` (built-ins only, and UTF-16, so mixed encodings in one
        // stream); decoding OEM ourselves (MultiByteToWideChar(CP_OEMCP)) can't recover
        // characters outside the OEM page and would corrupt programs that already write
        // UTF-8 (git, python, cargo); `chcp 65001 & <line>` in the same cmd (measured: cmd
        // caches the code page per parsed line, so the line's built-ins still emit OEM).
        // So an outer cmd switches its (hidden, per-session — CREATE_NO_WINDOW) console
        // to UTF-8 and only then starts the real cmd, which inherits the console and
        // runs the line verbatim. The line travels in an environment variable read with
        // delayed expansion (`!VAR!`, expanded after the outer cmd has parsed its own
        // line), so the outer cmd never interprets its quotes, `&`, `|`, `%`… The inner
        // cmd has default settings (delayed expansion off, `%VAR%` sees the caller's
        // env), and its exit code is the outer one's. chcp failing (no console) is
        // ignored. Cost: chcp + a second cmd.exe (measured ~30 ms: `echo hi` 55 ms vs
        // 25 ms for a bare `cmd /c`), acceptable for a non-default shell. Both are by
        // absolute path so a `cmd.exe`/`chcp.com` in --cwd can't hijack them.
        Shell::Cmd => Plan {
            program: "cmd".into(),
            args: vec!["/d".into(), "/v:on".into(), "/s".into(), "/c".into()],
            raw_tail: Some(format!(
                "\"%SystemRoot%\\System32\\chcp.com 65001 >nul 2>&1 & \
                 \"%SystemRoot%\\System32\\cmd.exe\" /d /s /c !{CMD_LINE_VAR}!\""
            )),
            env: vec![(CMD_LINE_VAR.into(), format!("\"{line}\""))],
        },
        Shell::Bash => posix("bash"),
        Shell::Sh => posix("sh"),
        Shell::Posix(path) => posix(&path),
    })
}

/// Expand a leading `~` to the executing user's home directory.
pub fn expand_home(dir: &str, home: Option<&Path>) -> PathBuf {
    if let Some(home) = home {
        if dir == "~" {
            return home.to_path_buf();
        }
        if let Some(rest) = dir.strip_prefix("~/").or_else(|| dir.strip_prefix("~\\")) {
            return home.join(rest);
        }
    }
    PathBuf::from(dir)
}

fn home_dir() -> Option<PathBuf> {
    let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var_os(var)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// How a session ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum End {
    Exited(i32),
    /// `timeout_secs` expired; the process tree was killed.
    TimedOut,
    /// The output receiver was dropped (caller gone); the process tree was killed.
    Disconnected,
}

/// Result and timings of a session (all relative to the spawn call).
#[derive(Debug, Clone)]
pub struct Outcome {
    pub end: End,
    pub spawn: Duration,
    pub first_out: Option<Duration>,
    pub total: Duration,
    /// When both stdout and stderr reached EOF, if they did.
    pub eof: Option<Duration>,
    /// When the process' exit status was collected, if it was.
    pub exited: Option<Duration>,
    pub out_bytes: u64,
    pub err_bytes: u64,
}

/// A spawned exec process.
pub struct Running {
    child: Child,
    tree: tree::Tree,
    t0: Instant,
    spawn: Duration,
    pub plan: Plan,
}

/// Start the process described by `req` (stdin/stdout/stderr piped, killed on drop; on
/// Windows without a console window and inside a job object; on unix in its own process
/// group, so the whole tree can be killed).
pub fn spawn(req: &ExecRequest) -> Result<Running, ExecError> {
    let t0 = Instant::now();
    let plan = plan(req, local_default_shell)?;
    let program = if req.raw {
        raw_program(&plan.program, &req.env)
    } else {
        plan.program.clone().into()
    };
    let mut cmd = Command::new(&program);
    cmd.args(&plan.args);
    if let Some(tail) = &plan.raw_tail {
        #[cfg(windows)]
        cmd.raw_arg(tail);
        #[cfg(not(windows))]
        cmd.arg(tail);
    }
    if let Some(dir) = &req.cwd {
        let dir = expand_home(dir, home_dir().as_deref());
        if !dir.is_dir() {
            return Err(ExecError::new(
                exit::CANNOT_EXECUTE,
                format!("cwd {} is not a directory", dir.display()),
            ));
        }
        cmd.current_dir(dir);
    }
    cmd.envs(req.env.iter().map(|(k, v)| (k, v)))
        .envs(plan.env.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(unix)]
    cmd.process_group(0);
    let child = cmd.spawn().map_err(|e| {
        let code = if e.kind() == std::io::ErrorKind::NotFound {
            exit::COMMAND_NOT_FOUND
        } else {
            exit::CANNOT_EXECUTE
        };
        let shown = program.to_string_lossy();
        ExecError::new(code, format!("{shown}: {e}"))
    })?;
    let tree = tree::Tree::attach(&child);
    Ok(Running {
        child,
        tree,
        spawn: t0.elapsed(),
        t0,
        plan,
    })
}

async fn until(deadline: Option<Instant>) {
    match deadline {
        Some(d) => tokio::time::sleep_until(d).await,
        None => std::future::pending().await,
    }
}

fn reader<R>(mut r: R, tx: mpsc::Sender<ExecFrame>, stderr: bool) -> JoinHandle<()>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut buf = vec![0u8; MAX_CHUNK];
        loop {
            match r.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let data = buf[..n].to_vec();
                    let f = if stderr {
                        ExecFrame::Stderr(data)
                    } else {
                        ExecFrame::Stdout(data)
                    };
                    if tx.send(f).await.is_err() {
                        break;
                    }
                }
            }
        }
    })
}

fn exit_code(status: std::io::Result<std::process::ExitStatus>) -> i32 {
    let Ok(status) = status else { return -1 };
    if let Some(c) = status.code() {
        return c;
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(sig) = status.signal() {
            return 128 + sig;
        }
    }
    -1
}

impl Running {
    pub fn id(&self) -> Option<u32> {
        self.child.id()
    }

    /// Pump stdio until the process exits (and its output is drained), the timeout
    /// expires, or `out` is closed by the receiver.
    pub async fn run(
        mut self,
        mut stdin: mpsc::Receiver<Vec<u8>>,
        out: mpsc::Sender<ExecFrame>,
        timeout: Option<Duration>,
    ) -> Outcome {
        let deadline = timeout.map(|d| self.t0 + self.spawn + d);
        let mut cin = self.child.stdin.take();
        let stdin_task = tokio::spawn(async move {
            while let Some(buf) = stdin.recv().await {
                if let Some(w) = cin.as_mut()
                    && w.write_all(&buf).await.is_err()
                {
                    // The process closed its stdin; keep draining so the sender never blocks.
                    cin = None;
                }
            }
            // Channel closed = EOF: dropping `cin` closes the process's stdin.
        });
        let (tx, mut rx) = mpsc::channel(8);
        let mut readers = Vec::new();
        if let Some(o) = self.child.stdout.take() {
            readers.push(reader(o, tx.clone(), false));
        }
        if let Some(e) = self.child.stderr.take() {
            readers.push(reader(e, tx.clone(), true));
        }
        drop(tx);

        let (mut out_bytes, mut err_bytes) = (0u64, 0u64);
        let mut first_out = None;
        let mut status: Option<i32> = None;
        let mut drained = false;
        let (mut eof, mut exited) = (None, None);
        let mut quiet_since = Instant::now();
        let end = loop {
            if let (Some(code), true) = (status, drained) {
                break End::Exited(code);
            }
            tokio::select! {
                biased;
                _ = out.closed() => break End::Disconnected,
                _ = until(deadline) => break End::TimedOut,
                s = self.child.wait(), if status.is_none() => {
                    status = Some(exit_code(s));
                    exited = Some(self.t0.elapsed());
                    quiet_since = Instant::now();
                }
                f = rx.recv(), if !drained => match f {
                    None => {
                        drained = true;
                        eof = Some(self.t0.elapsed());
                    }
                    Some(f) => {
                        first_out.get_or_insert_with(|| self.t0.elapsed());
                        match &f {
                            ExecFrame::Stdout(b) => out_bytes += b.len() as u64,
                            ExecFrame::Stderr(b) => err_bytes += b.len() as u64,
                            _ => {}
                        }
                        tokio::select! {
                            r = out.send(f) => if r.is_err() { break End::Disconnected },
                            _ = until(deadline) => break End::TimedOut,
                        }
                        quiet_since = Instant::now();
                    }
                },
                // Exited, but background children still hold the pipes and went quiet.
                _ = tokio::time::sleep_until(quiet_since + DRAIN_GRACE), if status.is_some() => {
                    break End::Exited(status.unwrap_or(-1));
                }
            }
        };
        if matches!(end, End::TimedOut | End::Disconnected) {
            self.tree.kill(&self.child);
            let _ = self.child.kill().await;
        }
        stdin_task.abort();
        for r in readers {
            r.abort();
        }
        Outcome {
            end,
            spawn: self.spawn,
            first_out,
            total: self.t0.elapsed(),
            eof,
            exited,
            out_bytes,
            err_bytes,
        }
    }
}

#[cfg(unix)]
mod tree {
    use tokio::process::Child;

    /// The child leads its own process group (`process_group(0)`).
    pub struct Tree(Option<i32>);

    impl Tree {
        pub fn attach(child: &Child) -> Tree {
            Tree(child.id().map(|p| p as i32))
        }

        pub fn kill(&self, _child: &Child) {
            if let Some(pgid) = self.0 {
                // SAFETY: plain syscall; a stale group id only yields ESRCH.
                unsafe {
                    libc::kill(-pgid, libc::SIGKILL);
                }
            }
        }
    }
}

#[cfg(windows)]
mod tree {
    use tokio::process::Child;
    use windows_sys::Win32::{
        Foundation::{CloseHandle, HANDLE},
        System::JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_BREAKAWAY_OK,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
            SetInformationJobObject, TerminateJobObject,
        },
    };

    /// A job object holding the child and everything it starts (assigned right after
    /// spawn). Closing the handle does NOT kill the job: processes deliberately started in
    /// the background outlive a normal exit; only timeout/disconnect terminate the job.
    pub struct Tree(HANDLE);

    // SAFETY: a job handle is a kernel object handle, usable from any thread.
    unsafe impl Send for Tree {}
    unsafe impl Sync for Tree {}

    impl Tree {
        pub fn attach(child: &Child) -> Tree {
            // SAFETY: null attributes/name create an anonymous job; handles are checked.
            unsafe {
                let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
                if job.is_null() {
                    return Tree(std::ptr::null_mut());
                }
                // Let a child leave the job on purpose (CREATE_BREAKAWAY_FROM_JOB): the
                // detached updater must outlive this exec and the daemon (bug #11).
                let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_BREAKAWAY_OK;
                SetInformationJobObject(
                    job,
                    JobObjectExtendedLimitInformation,
                    (&raw const info).cast(),
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                );
                match child.raw_handle() {
                    Some(h) if AssignProcessToJobObject(job, h as HANDLE) != 0 => Tree(job),
                    _ => {
                        CloseHandle(job);
                        Tree(std::ptr::null_mut())
                    }
                }
            }
        }

        pub fn kill(&self, _child: &Child) {
            if !self.0.is_null() {
                // SAFETY: valid job handle owned by self.
                unsafe {
                    TerminateJobObject(self.0, 1);
                }
            }
        }
    }

    impl Drop for Tree {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: valid job handle owned by self, closed once.
                unsafe {
                    CloseHandle(self.0);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(words: &[&str]) -> ExecRequest {
        ExecRequest {
            command: words.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn default_shell_per_platform() {
        assert_eq!(default_shell(Os::Windows, None, true), Shell::Pwsh);
        assert_eq!(
            default_shell(Os::Windows, Some("/bin/zsh"), false),
            Shell::Powershell
        );
        assert_eq!(
            default_shell(Os::Unix, Some("/usr/bin/fish"), true),
            Shell::Posix("/usr/bin/fish".into())
        );
        assert_eq!(
            default_shell(Os::Unix, None, false),
            Shell::Posix("/bin/sh".into())
        );
        assert_eq!(
            default_shell(Os::Unix, Some("  "), false),
            Shell::Posix("/bin/sh".into())
        );
    }

    #[test]
    fn shell_mode_joins_words() {
        let p = plan(&req(&["echo", "a b", "|", "wc"]), || Shell::Bash).unwrap();
        assert_eq!(p.program, "bash");
        assert_eq!(p.args, vec!["-c", "echo a b | wc"]);
        let p = plan(&req(&["ls"]), || Shell::Posix("/bin/zsh".into())).unwrap();
        assert_eq!(p.program, "/bin/zsh");
        assert_eq!(p.args, vec!["-c", "ls"]);
    }

    #[test]
    fn powershell_and_cmd_args() {
        let p = plan(&req(&["Get-ChildItem", "C:\\"]), || Shell::Pwsh).unwrap();
        assert_eq!(p.program, "pwsh");
        assert_eq!(
            p.args,
            vec![
                "-NoProfile",
                "-NoLogo",
                "-NonInteractive",
                "-Command",
                &format!("{PS_UTF8_PRELUDE}Get-ChildItem C:\\")
            ]
        );
        let mut r = req(&["dir", "/b"]);
        r.shell = Some("powershell".into());
        assert_eq!(plan(&r, || unreachable!()).unwrap().program, "powershell");
        r.shell = Some("cmd".into());
        let p = plan(&r, || unreachable!()).unwrap();
        assert_eq!(p.program, "cmd");
        assert_eq!(p.args, vec!["/d", "/v:on", "/s", "/c"]);
        assert_eq!(
            p.raw_tail.as_deref(),
            Some(
                "\"%SystemRoot%\\System32\\chcp.com 65001 >nul 2>&1 & \
                 \"%SystemRoot%\\System32\\cmd.exe\" /d /s /c !ENISHI_CMD_LINE!\""
            )
        );
        assert_eq!(p.env, vec![("ENISHI_CMD_LINE".into(), "\"dir /b\"".into())]);
    }

    #[test]
    fn raw_mode_and_errors() {
        let mut r = req(&["/usr/bin/env", "A=1", "x y"]);
        r.raw = true;
        let p = plan(&r, || unreachable!()).unwrap();
        assert_eq!(p.program, "/usr/bin/env");
        assert_eq!(p.args, vec!["A=1", "x y"]);
        r.shell = Some("sh".into());
        assert_eq!(plan(&r, || Shell::Sh).unwrap_err().code, exit::USAGE);
        let mut r = req(&["ls"]);
        r.shell = Some("zsh".into());
        assert_eq!(plan(&r, || Shell::Sh).unwrap_err().code, exit::USAGE);
        assert_eq!(plan(&req(&[]), || Shell::Sh).unwrap_err().code, exit::USAGE);
    }

    #[test]
    fn windows_program_resolution() {
        use std::ffi::OsString;
        let files = [
            "/c/node/npx.cmd",
            "/c/node/node.exe",
            "/c/tools/uvx.exe",
            "/c/tools/npx.ps1",
            "/c/py/script.bat",
            "/c/rel/tool.CMD",
        ];
        // NTFS lookups are case-insensitive.
        let is_file = |p: &Path| {
            let p = p.to_string_lossy().replace('\\', "/").to_lowercase();
            files.iter().any(|f| f.to_lowercase() == p)
        };
        let sep = if cfg!(windows) { ";" } else { ":" };
        let path = OsString::from(["/c/tools", "", "/c/node", "/c/py"].join(sep));
        let r = |p: &str, ext: Option<&str>| {
            resolve_windows_program(p, Some(&path), ext, is_file)
                .map(|p| p.to_string_lossy().replace('\\', "/"))
        };
        // npx → npx.cmd: the first PATH dir only has npx.ps1 (not in PATHEXT). Candidates
        // carry PATHEXT's spelling (Windows matches file names case-insensitively).
        assert_eq!(r("npx", None).as_deref(), Some("/c/node/npx.CMD"));
        assert_eq!(r("node", None).as_deref(), Some("/c/node/node.EXE"));
        assert_eq!(r("uvx", Some(".exe")).as_deref(), Some("/c/tools/uvx.exe"));
        // A known extension is used as-is (matched case-insensitively against PATHEXT).
        assert_eq!(r("script.BAT", None).as_deref(), Some("/c/py/script.BAT"));
        assert_eq!(r("npx.ps1", None), None, ".ps1 is not in PATHEXT");
        // PATHEXT contents and order are honoured; blank PATHEXT = default.
        assert_eq!(r("npx", Some(".EXE;.COM")), None);
        assert_eq!(r("npx", Some(" ")).as_deref(), Some("/c/node/npx.CMD"));
        // Directory part: only the extensions are tried, never PATH.
        assert_eq!(
            r("/c/rel/tool", Some(".exe;.cmd")).as_deref(),
            Some("/c/rel/tool.cmd")
        );
        assert_eq!(r("/c/node/npx", None).as_deref(), Some("/c/node/npx.CMD"));
        assert_eq!(r("./npx", None), None, "cwd is never searched");
        assert_eq!(r("missing", None), None);
        assert_eq!(resolve_windows_program("npx", None, None, is_file), None);
    }

    #[test]
    fn home_expansion() {
        let h = Path::new("/home/u");
        assert_eq!(expand_home("~", Some(h)), PathBuf::from("/home/u"));
        assert_eq!(expand_home("~/p", Some(h)), PathBuf::from("/home/u/p"));
        assert_eq!(expand_home("/tmp", Some(h)), PathBuf::from("/tmp"));
        assert_eq!(expand_home("~x", Some(h)), PathBuf::from("~x"));
        assert_eq!(expand_home("~/p", None), PathBuf::from("~/p"));
    }

    /// Run `req` locally, feeding `input`; returns (stdout, stderr, end).
    async fn run_local(r: ExecRequest, input: &[u8]) -> (Vec<u8>, Vec<u8>, Outcome) {
        let running = spawn(&r).unwrap();
        let (in_tx, in_rx) = mpsc::channel(4);
        let (out_tx, mut out_rx) = mpsc::channel(4);
        let timeout = r.timeout_secs.map(Duration::from_secs);
        let task = tokio::spawn(running.run(in_rx, out_tx, timeout));
        if !input.is_empty() {
            in_tx.send(input.to_vec()).await.unwrap();
        }
        drop(in_tx);
        let (mut o, mut e) = (Vec::new(), Vec::new());
        while let Some(f) = out_rx.recv().await {
            match f {
                ExecFrame::Stdout(b) => o.extend(b),
                ExecFrame::Stderr(b) => e.extend(b),
                other => panic!("unexpected {other:?}"),
            }
        }
        (o, e, task.await.unwrap())
    }

    fn shell_req(unix: &str, windows: &str) -> ExecRequest {
        req(&[if cfg!(windows) { windows } else { unix }])
    }

    #[tokio::test]
    async fn separate_streams_and_exit_code() {
        let r = shell_req(
            "echo out; echo err 1>&2; exit 7",
            "Write-Output out; [Console]::Error.WriteLine('err'); exit 7",
        );
        let (o, e, out) = run_local(r, b"").await;
        assert_eq!(String::from_utf8_lossy(&o).trim(), "out");
        assert_eq!(String::from_utf8_lossy(&e).trim(), "err");
        assert_eq!(out.end, End::Exited(7));
        assert!(out.first_out.is_some());
    }

    #[tokio::test]
    async fn stdin_is_streamed_with_eof() {
        let r = shell_req("cat", "$input | ForEach-Object { $_ }");
        let (o, _, out) = run_local(r, b"hello\n").await;
        assert_eq!(String::from_utf8_lossy(&o).trim(), "hello");
        assert_eq!(out.end, End::Exited(0));
    }

    #[tokio::test]
    async fn env_cwd_and_raw() {
        let mut r = shell_req(
            "echo $FOO; pwd",
            "Write-Output $env:FOO; (Get-Location).Path",
        );
        r.env = vec![("FOO".into(), "bar".into())];
        let dir = std::env::temp_dir().canonicalize().unwrap();
        r.cwd = Some(dir.to_string_lossy().into());
        let (o, _, _) = run_local(r, b"").await;
        let s = String::from_utf8_lossy(&o);
        assert!(s.contains("bar"), "{s}");
        let last = s.lines().last().unwrap_or("").trim().to_string();
        assert_eq!(
            Path::new(&last).canonicalize().unwrap(),
            dir,
            "cwd output {s:?}"
        );

        let mut r = if cfg!(windows) {
            req(&["cmd", "/c", "echo", "raw"])
        } else {
            req(&["printf", "%s|", "a b", "c"])
        };
        r.raw = true;
        let (o, _, out) = run_local(r, b"").await;
        let s = String::from_utf8_lossy(&o);
        if cfg!(windows) {
            assert_eq!(s.trim(), "raw");
        } else {
            assert_eq!(s, "a b|c|");
        }
        assert_eq!(out.end, End::Exited(0));
    }

    /// `--shell cmd` output must be UTF-8, not the OEM code page (cp850 turned
    /// "Versión" into `Versi\xa2n`); operators and the exit code must survive the
    /// chcp prefix.
    #[cfg(windows)]
    #[tokio::test]
    async fn cmd_output_is_utf8() {
        let mut r = req(&["echo versíón ñ€ \"a|b\" %OS% !x! && (echo x | findstr y || exit /b 3)"]);
        r.shell = Some("cmd".into());
        let (o, e, out) = run_local(r, b"").await;
        assert_eq!(
            String::from_utf8(o.clone()).map_err(|_| o).unwrap().trim(),
            "versíón ñ€ \"a|b\" Windows_NT !x!"
        );
        assert!(e.is_empty(), "{}", String::from_utf8_lossy(&e));
        assert_eq!(out.end, End::Exited(3));
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn powershell_output_is_utf8() {
        for shell in ["pwsh", "powershell"] {
            if shell == "pwsh" && !on_path("pwsh.exe") {
                continue;
            }
            // Output only: PowerShell decodes piped stdin with the OEM page before our
            // prelude runs, so non-ASCII *stdin* is a known v0.1 limitation (design §3.3).
            let mut r = req(&["Write-Output 'versíón ñ€'"]);
            r.shell = Some(shell.into());
            let (o, _e, out) = run_local(r, b"").await;
            let text = String::from_utf8(o.clone()).map_err(|_| o).unwrap();
            assert_eq!(text.trim(), "versíón ñ€", "{shell}");
            assert_eq!(out.end, End::Exited(0), "{shell}");
        }
    }

    /// Raw spawn finds `.cmd` shims by name (`npx` → `npx.cmd`), like MCP docs expect.
    #[cfg(windows)]
    #[tokio::test]
    async fn raw_npx_resolves_cmd_shim() {
        let found = std::env::var_os("PATH").and_then(|p| {
            resolve_windows_program(
                "npx",
                Some(&p),
                std::env::var("PATHEXT").ok().as_deref(),
                |f| f.is_file(),
            )
        });
        let Some(found) = found else {
            eprintln!("skipped: npx not on PATH");
            return;
        };
        let mut r = req(&["npx", "--version"]);
        r.raw = true;
        let (o, e, out) = run_local(r, b"").await;
        assert_eq!(
            out.end,
            End::Exited(0),
            "{found:?} stderr {}",
            String::from_utf8_lossy(&e)
        );
        let v = String::from_utf8_lossy(&o);
        assert!(
            v.trim().chars().next().is_some_and(|c| c.is_ascii_digit()),
            "{v:?}"
        );
    }

    /// Arguments reach the program behind a `.cmd` intact: spaces, quotes, `&`, `%`, `^`
    /// are not re-interpreted by cmd.exe (Rust std's BatBadBut escaping), and a newline
    /// is refused at spawn instead of truncating the command line.
    #[cfg(windows)]
    #[tokio::test]
    async fn cmd_shim_args_survive() {
        let have_node = std::env::var_os("PATH").is_some_and(|p| {
            resolve_windows_program("node", Some(&p), None, |f| f.is_file()).is_some()
        });
        if !have_node {
            eprintln!("skipped: node not on PATH");
            return;
        }
        let dir = std::env::temp_dir().join(format!("enishi-bat-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("argv.js"),
            "console.log(JSON.stringify(process.argv.slice(2)))",
        )
        .unwrap();
        std::fs::write(dir.join("echoargs.cmd"), "@node \"%~dp0argv.js\" %*\r\n").unwrap();
        let args = [
            "a b",
            "say \"hi\"",
            "x&echo INJECTED",
            "100%",
            "^caret",
            "|pipe>",
            "",
        ];
        let mut words = vec!["echoargs".to_string()];
        words.extend(args.iter().map(|s| s.to_string()));
        let mut r = ExecRequest {
            command: words,
            raw: true,
            env: vec![(
                "PATH".into(),
                format!("{};{}", dir.display(), std::env::var("PATH").unwrap()),
            )],
            ..Default::default()
        };
        let (o, e, out) = run_local(r.clone(), b"").await;
        assert_eq!(out.end, End::Exited(0), "{}", String::from_utf8_lossy(&e));
        let got: Vec<String> = serde_json_lite(&String::from_utf8_lossy(&o));
        assert_eq!(got, args, "stdout {:?}", String::from_utf8_lossy(&o));
        r.command.push("two\nlines".into());
        assert_eq!(spawn(&r).err().map(|e| e.code), Some(exit::CANNOT_EXECUTE));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Minimal parser for a JSON array of strings (keeps serde_json out of this crate).
    #[cfg(windows)]
    fn serde_json_lite(s: &str) -> Vec<String> {
        let s = s
            .trim()
            .strip_prefix('[')
            .and_then(|s| s.strip_suffix(']'))
            .unwrap_or("");
        let mut out = Vec::new();
        let mut cur = None::<String>;
        let mut chars = s.chars();
        while let Some(c) = chars.next() {
            match (&mut cur, c) {
                (None, '"') => cur = Some(String::new()),
                (Some(_), '"') => out.push(cur.take().unwrap()),
                (Some(v), '\\') => match chars.next() {
                    Some('n') => v.push('\n'),
                    Some(o) => v.push(o),
                    None => {}
                },
                (Some(v), c) => v.push(c),
                (None, _) => {}
            }
        }
        out
    }

    #[tokio::test]
    async fn spawn_errors_map_to_codes() {
        let mut r = req(&["enishi-no-such-binary-xyz"]);
        r.raw = true;
        assert_eq!(spawn(&r).err().unwrap().code, exit::COMMAND_NOT_FOUND);
        let mut r = req(&["echo"]);
        r.cwd = Some("/no/such/dir/xyz".into());
        assert_eq!(spawn(&r).err().unwrap().code, exit::CANNOT_EXECUTE);
    }

    #[tokio::test]
    async fn timeout_kills_tree() {
        // The shell starts a grandchild sleeper; the whole tree must die at the timeout.
        let mut r = shell_req("sleep 30 & sleep 30; echo never", "Start-Sleep 30; 'never'");
        r.timeout_secs = Some(1);
        let t = std::time::Instant::now();
        let (o, _, out) = run_local(r, b"").await;
        assert_eq!(out.end, End::TimedOut);
        assert!(o.is_empty());
        assert!(t.elapsed() < Duration::from_secs(5), "{:?}", t.elapsed());
    }

    #[tokio::test]
    async fn dropped_receiver_kills() {
        let r = shell_req("sleep 30", "Start-Sleep 30");
        let running = spawn(&r).unwrap();
        let (_in_tx, in_rx) = mpsc::channel(1);
        let (out_tx, out_rx) = mpsc::channel(1);
        let task = tokio::spawn(running.run(in_rx, out_tx, None));
        tokio::time::sleep(Duration::from_millis(200)).await;
        drop(out_rx);
        let out = tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("killed promptly")
            .unwrap();
        assert_eq!(out.end, End::Disconnected);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn background_child_does_not_hang_session() {
        // `sleep 30 &` keeps stdout open after the shell exits; the drain grace ends it.
        let r = req(&["sleep 30 & echo done"]);
        let t = std::time::Instant::now();
        let (o, _, out) = run_local(r, b"").await;
        assert_eq!(String::from_utf8_lossy(&o).trim(), "done");
        assert_eq!(out.end, End::Exited(0));
        assert!(t.elapsed() < Duration::from_secs(5));
    }
}

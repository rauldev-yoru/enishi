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

/// How to start a process: `program args… [raw_tail]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub program: String,
    pub args: Vec<String>,
    /// Appended verbatim to the Windows command line (cmd.exe's own quoting rules); a
    /// plain extra argument elsewhere.
    pub raw_tail: Option<String>,
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
    let ps = |program: &str| Plan {
        program: program.into(),
        args: ["-NoProfile", "-NoLogo", "-NonInteractive", "-Command"]
            .map(String::from)
            .into_iter()
            .chain([line.clone()])
            .collect(),
        raw_tail: None,
    };
    let posix = |program: &str| Plan {
        program: program.into(),
        args: vec!["-c".into(), line.clone()],
        raw_tail: None,
    };
    Ok(match shell {
        Shell::Pwsh => ps("pwsh"),
        Shell::Powershell => ps("powershell"),
        // `/s` makes cmd strip exactly the outer quotes and run the rest verbatim.
        Shell::Cmd => Plan {
            program: "cmd".into(),
            args: vec!["/d".into(), "/s".into(), "/c".into()],
            raw_tail: Some(format!("\"{line}\"")),
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
    let mut cmd = Command::new(&plan.program);
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
        ExecError::new(code, format!("{}: {e}", plan.program))
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
                    quiet_since = Instant::now();
                }
                f = rx.recv(), if !drained => match f {
                    None => drained = true,
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
        System::JobObjects::{AssignProcessToJobObject, CreateJobObjectW, TerminateJobObject},
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
                "Get-ChildItem C:\\"
            ]
        );
        let mut r = req(&["dir", "/b"]);
        r.shell = Some("powershell".into());
        assert_eq!(plan(&r, || unreachable!()).unwrap().program, "powershell");
        r.shell = Some("cmd".into());
        let p = plan(&r, || unreachable!()).unwrap();
        assert_eq!(p.program, "cmd");
        assert_eq!(p.args, vec!["/d", "/s", "/c"]);
        assert_eq!(p.raw_tail.as_deref(), Some("\"dir /b\""));
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

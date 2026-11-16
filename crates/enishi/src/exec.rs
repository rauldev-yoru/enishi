//! `enishi exec` / `enishi run`: the CLI end of a session (exec or published service),
//! relayed by the local daemon.

use enishi_core::{ipc, paths::Paths, wire};
use enishi_proto::{
    SessionKind, exit,
    ipc::IpcRequest,
    session::{ExecFrame, MAX_CHUNK},
};
use tokio::io::{AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Run the session (`IpcRequest::Exec` or `IpcRequest::Run`); returns the process exit
/// code for `enishi`.
pub async fn run(paths: &Paths, req: IpcRequest) -> i32 {
    let code = match session(paths, req).await {
        Ok(c) => {
            // A Windows process killed or crashed: say so (the code alone, truncated to
            // 8 bits by the shell on unix, is meaningless). The code is still returned.
            if let Some(what) = exit::ntstatus(c) {
                eprintln!("enishi: remote process exited with {what}");
            }
            c
        }
        Err((c, msg)) => {
            eprintln!("enishi: {msg}");
            c
        }
    };
    let _ = tokio::io::stdout().flush().await;
    let _ = tokio::io::stderr().flush().await;
    code
}

async fn session(paths: &Paths, req: IpcRequest) -> Result<i32, (i32, String)> {
    let stream = match &req {
        // A published service: resumable (design D18) when the daemon is ≥ 0.1.2.
        IpcRequest::Run {
            device,
            service,
            args,
        } => {
            let header = SessionKind::Resumable(Box::new(SessionKind::Service {
                name: service.clone(),
                args: args.clone(),
            }));
            let open = IpcRequest::Open {
                device: device.clone(),
                header,
            };
            ipc::open_session_or(paths, &open, &req).await?
        }
        _ => ipc::open_session(paths, &req).await?,
    };
    let lost = |e: anyhow::Error| (exit::DAEMON_DOWN, format!("daemon connection lost: {e:#}"));
    let (mut r, mut w) = tokio::io::split(stream);
    // stdin → daemon. The write half stays open after EOF: closing the connection would
    // tell the daemon that we are gone and the remote process must be killed.
    let stdin_task = tokio::spawn(async move {
        let mut stdin = tokio::io::stdin();
        let mut buf = vec![0u8; MAX_CHUNK];
        loop {
            match stdin.read(&mut buf).await {
                Ok(0) | Err(_) => {
                    let _ = wire::write_frame(&mut w, &ExecFrame::StdinEof).await;
                    break;
                }
                Ok(n) => {
                    if wire::write_frame(&mut w, &ExecFrame::Stdin(buf[..n].to_vec()))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            }
        }
        std::future::pending::<()>().await;
        drop(w);
    });

    let mut stdout = tokio::io::stdout();
    let mut stderr = tokio::io::stderr();
    let result = loop {
        match wire::read_frame::<_, ExecFrame>(&mut r).await {
            Ok(ExecFrame::Stdout(b)) => put(&mut stdout, &b).await,
            Ok(ExecFrame::Stderr(b)) => put(&mut stderr, &b).await,
            Ok(ExecFrame::Exit { code }) => break Ok(code),
            Ok(ExecFrame::Error { code, message }) => break Err((code, message)),
            Ok(ExecFrame::Detached { message }) => eprintln!("enishi: {message}"),
            Ok(ExecFrame::Resumed) => eprintln!("enishi: session resumed"),
            Ok(other) => break Err((exit::DAEMON_DOWN, format!("unexpected frame {other:?}"))),
            Err(e) => break Err(lost(e)),
        }
    };
    stdin_task.abort();
    result
}

/// Write remote output as-is; a Windows console rejects invalid UTF-8, so fall back to a
/// lossy conversion there instead of dropping the chunk.
async fn put<W: AsyncWrite + Unpin>(w: &mut W, b: &[u8]) {
    if w.write_all(b).await.is_err() {
        let _ = w.write_all(String::from_utf8_lossy(b).as_bytes()).await;
    }
    let _ = w.flush().await;
}

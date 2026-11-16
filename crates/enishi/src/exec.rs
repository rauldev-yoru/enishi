//! `enishi exec`: the CLI end of an exec session, relayed by the local daemon.

use enishi_core::{
    ipc::{self, ClientError},
    paths::Paths,
    wire,
};
use enishi_proto::{
    exit,
    ipc::{IpcRequest, IpcResponse},
    session::{ExecFrame, ExecRequest, MAX_CHUNK},
};
use tokio::io::{AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Run the session; returns the process exit code for `enishi`.
pub async fn run(paths: &Paths, device: Option<String>, request: ExecRequest) -> i32 {
    let code = match session(paths, device, request).await {
        Ok(c) => c,
        Err((c, msg)) => {
            eprintln!("enishi: {msg}");
            c
        }
    };
    let _ = tokio::io::stdout().flush().await;
    let _ = tokio::io::stderr().flush().await;
    code
}

async fn session(
    paths: &Paths,
    device: Option<String>,
    request: ExecRequest,
) -> Result<i32, (i32, String)> {
    let mut stream = ipc::connect(paths).await.map_err(|e| match e {
        e @ ClientError::DaemonDown(_) => (exit::DAEMON_DOWN, e.to_string()),
        ClientError::Other(e) => (exit::DAEMON_DOWN, format!("{e:#}")),
    })?;
    let lost = |e: anyhow::Error| (exit::DAEMON_DOWN, format!("daemon connection lost: {e:#}"));
    wire::write_frame(&mut stream, &IpcRequest::Exec { device, request })
        .await
        .map_err(lost)?;
    let resp = tokio::time::timeout(
        ipc::CLIENT_TIMEOUT,
        wire::read_frame::<_, IpcResponse>(&mut stream),
    )
    .await
    .map_err(|_| {
        (
            exit::DAEMON_DOWN,
            format!("daemon did not answer within {:?}", ipc::CLIENT_TIMEOUT),
        )
    })?
    .map_err(lost)?;
    match resp {
        IpcResponse::ExecStarted => {}
        IpcResponse::Error { code, message } => return Err((code, message)),
        other => return Err((exit::DAEMON_DOWN, format!("unexpected reply {other:?}"))),
    }

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

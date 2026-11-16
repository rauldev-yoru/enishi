//! Local IPC between the CLI and the daemon: a unix socket (0600) or a named pipe.
//! One [`IpcRequest`] frame in, one [`IpcResponse`] frame out per connection.

use std::time::Duration;

use anyhow::Result;
use enishi_proto::ipc::{IpcRequest, IpcResponse};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::{paths::Paths, wire};

/// Bound on a whole CLI request, so a wedged daemon never hangs the caller.
pub const CLIENT_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug)]
pub enum ClientError {
    /// No daemon is listening on this instance's IPC endpoint.
    DaemonDown(String),
    Other(anyhow::Error),
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientError::DaemonDown(m) => write!(f, "daemon not running ({m})"),
            ClientError::Other(e) => write!(f, "{e:#}"),
        }
    }
}

impl std::error::Error for ClientError {}

/// Connect to the local daemon (for streaming requests such as exec).
pub async fn connect(paths: &Paths) -> Result<ClientStream, ClientError> {
    sys::connect(paths)
        .await
        .map_err(|e| ClientError::DaemonDown(format!("{}: {e}", paths.ipc_display())))
}

/// Send one request to the local daemon.
pub async fn request(paths: &Paths, req: &IpcRequest) -> Result<IpcResponse, ClientError> {
    let mut stream = connect(paths).await?;
    let exchange = async {
        wire::write_frame(&mut stream, req).await?;
        wire::read_frame::<_, IpcResponse>(&mut stream).await
    };
    match tokio::time::timeout(CLIENT_TIMEOUT, exchange).await {
        Ok(r) => r.map_err(ClientError::Other),
        Err(_) => Err(ClientError::Other(anyhow::anyhow!(
            "daemon did not answer within {CLIENT_TIMEOUT:?}"
        ))),
    }
}

/// Serve one client connection with `handle`.
pub async fn serve_one<S, F>(mut stream: S, handle: F) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
    F: FnOnce(IpcRequest) -> IpcResponse,
{
    let req: IpcRequest =
        tokio::time::timeout(CLIENT_TIMEOUT, wire::read_frame(&mut stream)).await??;
    let resp = handle(req);
    wire::write_frame(&mut stream, &resp).await?;
    Ok(())
}

pub use sys::{ClientStream, Listener};

#[cfg(unix)]
mod sys {
    use std::path::PathBuf;

    use anyhow::{Context, Result, bail};
    use tokio::net::{UnixListener, UnixStream};

    use crate::paths::Paths;

    pub type ClientStream = UnixStream;

    pub async fn connect(paths: &Paths) -> std::io::Result<UnixStream> {
        UnixStream::connect(paths.ipc_socket()).await
    }

    pub struct Listener {
        inner: UnixListener,
        path: PathBuf,
    }

    impl Listener {
        /// Fails if another daemon of this instance is already listening.
        pub async fn bind(paths: &Paths) -> Result<Listener> {
            use std::os::unix::fs::PermissionsExt;
            let path = paths.ipc_socket();
            if path.exists() {
                if UnixStream::connect(&path).await.is_ok() {
                    bail!("a daemon is already running (socket {})", path.display());
                }
                std::fs::remove_file(&path)
                    .with_context(|| format!("removing stale socket {}", path.display()))?;
            }
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let inner =
                UnixListener::bind(&path).with_context(|| format!("binding {}", path.display()))?;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
            Ok(Listener { inner, path })
        }

        pub async fn accept(&mut self) -> std::io::Result<UnixStream> {
            Ok(self.inner.accept().await?.0)
        }

        pub fn display(&self) -> String {
            self.path.display().to_string()
        }
    }

    impl Drop for Listener {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

#[cfg(windows)]
mod sys {
    use std::time::Duration;

    use anyhow::{Context, Result};
    use tokio::net::windows::named_pipe::{
        ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions,
    };

    use crate::paths::Paths;

    const ERROR_PIPE_BUSY: i32 = 231;

    pub type ClientStream = NamedPipeClient;

    pub async fn connect(paths: &Paths) -> std::io::Result<NamedPipeClient> {
        let name = paths.ipc_pipe_name();
        let mut tries = 0;
        loop {
            match ClientOptions::new().open(&name) {
                Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY) && tries < 20 => {
                    tries += 1;
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                r => return r,
            }
        }
    }

    pub struct Listener {
        name: String,
        next: NamedPipeServer,
    }

    impl Listener {
        /// `first_pipe_instance` fails if another daemon of this instance owns the pipe.
        pub async fn bind(paths: &Paths) -> Result<Listener> {
            let name = paths.ipc_pipe_name();
            let next = ServerOptions::new()
                .first_pipe_instance(true)
                .reject_remote_clients(true)
                .create(&name)
                .with_context(|| format!("creating pipe {name} (is a daemon already running?)"))?;
            Ok(Listener { name, next })
        }

        pub async fn accept(&mut self) -> std::io::Result<NamedPipeServer> {
            self.next.connect().await?;
            let fresh = ServerOptions::new()
                .reject_remote_clients(true)
                .create(&self.name)?;
            Ok(std::mem::replace(&mut self.next, fresh))
        }

        pub fn display(&self) -> String {
            self.name.clone()
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use enishi_proto::exit;

    #[tokio::test]
    async fn client_server_roundtrip_and_daemon_down() {
        let dir = std::env::temp_dir().join(format!("enishi-ipc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let paths = Paths::resolve(Some(&dir.join("enishi.toml"))).unwrap();

        match request(&paths, &IpcRequest::Status { device: None }).await {
            Err(ClientError::DaemonDown(_)) => {}
            other => panic!("expected DaemonDown, got {other:?}"),
        }

        let mut l = Listener::bind(&paths).await.unwrap();
        assert!(
            Listener::bind(&paths).await.is_err(),
            "second daemon refused"
        );
        let server = tokio::spawn(async move {
            // First connection is the liveness probe of the refused second bind: it sends
            // nothing and hangs up, which must only fail that one client.
            let probe = l.accept().await.unwrap();
            assert!(serve_one(probe, |_| unreachable!()).await.is_err());
            let s = l.accept().await.unwrap();
            serve_one(s, |req| match req {
                IpcRequest::Status { device } => IpcResponse::Error {
                    code: exit::NOT_ALLOWED,
                    message: format!("{device:?}"),
                },
                IpcRequest::Exec { .. } => unreachable!(),
            })
            .await
            .unwrap();
            l
        });
        let resp = request(
            &paths,
            &IpcRequest::Status {
                device: Some("pc".into()),
            },
        )
        .await
        .unwrap();
        assert_eq!(
            resp,
            IpcResponse::Error {
                code: exit::NOT_ALLOWED,
                message: "Some(\"pc\")".into()
            }
        );
        let l = server.await.unwrap();
        let sock = paths.ipc_socket();
        assert!(sock.exists());
        drop(l);
        assert!(!sock.exists(), "socket removed on drop");
        std::fs::remove_dir_all(dir).unwrap();
    }
}

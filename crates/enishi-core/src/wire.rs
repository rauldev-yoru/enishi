//! Async read/write of [`enishi_proto::frame`] frames over any byte stream
//! (QUIC streams, unix sockets, named pipes).

use anyhow::{Context, Result};
use enishi_proto::frame;
use serde::{Serialize, de::DeserializeOwned};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub async fn write_frame<W, T>(w: &mut W, msg: &T) -> Result<()>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let bytes = frame::encode(msg)?;
    w.write_all(&bytes).await.context("writing frame")?;
    w.flush().await.context("flushing frame")?;
    Ok(())
}

pub async fn read_frame<R, T>(r: &mut R) -> Result<T>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    let mut header = [0u8; frame::HEADER_LEN];
    r.read_exact(&mut header)
        .await
        .context("reading frame header")?;
    let n = frame::body_len(header)?;
    let mut body = vec![0u8; n];
    r.read_exact(&mut body)
        .await
        .context("reading frame body")?;
    Ok(frame::decode_body(&body)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use enishi_proto::ipc::{IpcRequest, IpcResponse};

    #[tokio::test]
    async fn roundtrip_over_duplex() {
        let (mut a, mut b) = tokio::io::duplex(64);
        let req = IpcRequest::Status {
            device: Some("pc".into()),
        };
        let w = tokio::spawn(async move {
            write_frame(&mut a, &req).await.unwrap();
            write_frame(
                &mut a,
                &IpcResponse::Error {
                    code: 2,
                    message: "x".repeat(200),
                },
            )
            .await
            .unwrap();
        });
        let got: IpcRequest = read_frame(&mut b).await.unwrap();
        assert_eq!(
            got,
            IpcRequest::Status {
                device: Some("pc".into())
            }
        );
        let got: IpcResponse = read_frame(&mut b).await.unwrap();
        assert!(matches!(got, IpcResponse::Error { code: 2, .. }));
        w.await.unwrap();
    }

    #[tokio::test]
    async fn eof_mid_frame_is_error() {
        let (mut a, mut b) = tokio::io::duplex(64);
        a.write_all(&[0, 0, 0, 9, 1, 2]).await.unwrap();
        drop(a);
        assert!(read_frame::<_, IpcRequest>(&mut b).await.is_err());
    }
}

//! Length-prefixed postcard frames (design D3).
//!
//! A frame is a 4-byte big-endian body length followed by the postcard-encoded body.
//! This module only does the byte layout; reading/writing streams lives in the daemon.

use serde::{Serialize, de::DeserializeOwned};

/// Size of the length prefix.
pub const HEADER_LEN: usize = 4;
/// Largest accepted body. Control and IPC messages are tiny; this bounds memory per frame.
pub const MAX_FRAME: usize = 1 << 20;

#[derive(Debug)]
pub enum FrameError {
    /// Declared or encoded body length exceeds [`MAX_FRAME`].
    TooLarge(usize),
    /// Fewer bytes than the header announces.
    Truncated { expected: usize, got: usize },
    Codec(postcard::Error),
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FrameError::TooLarge(n) => write!(f, "frame too large ({n} bytes, max {MAX_FRAME})"),
            FrameError::Truncated { expected, got } => {
                write!(f, "truncated frame ({got} of {expected} bytes)")
            }
            FrameError::Codec(e) => write!(f, "frame codec: {e}"),
        }
    }
}

impl std::error::Error for FrameError {}

impl From<postcard::Error> for FrameError {
    fn from(e: postcard::Error) -> Self {
        FrameError::Codec(e)
    }
}

/// Encode `msg` as a complete frame (header + body).
pub fn encode<T: Serialize>(msg: &T) -> Result<Vec<u8>, FrameError> {
    let mut out = vec![0u8; HEADER_LEN];
    out = postcard::to_extend(msg, out)?;
    let body = out.len() - HEADER_LEN;
    if body > MAX_FRAME {
        return Err(FrameError::TooLarge(body));
    }
    out[..HEADER_LEN].copy_from_slice(&(body as u32).to_be_bytes());
    Ok(out)
}

/// Body length announced by a header, validated against [`MAX_FRAME`].
pub fn body_len(header: [u8; HEADER_LEN]) -> Result<usize, FrameError> {
    let n = u32::from_be_bytes(header) as usize;
    if n > MAX_FRAME {
        return Err(FrameError::TooLarge(n));
    }
    Ok(n)
}

/// Decode a frame body (without the header).
pub fn decode_body<T: DeserializeOwned>(body: &[u8]) -> Result<T, FrameError> {
    Ok(postcard::from_bytes(body)?)
}

/// Decode one complete frame from the start of `buf`; returns the message and bytes consumed.
pub fn decode<T: DeserializeOwned>(buf: &[u8]) -> Result<(T, usize), FrameError> {
    if buf.len() < HEADER_LEN {
        return Err(FrameError::Truncated {
            expected: HEADER_LEN,
            got: buf.len(),
        });
    }
    let n = body_len(buf[..HEADER_LEN].try_into().expect("header slice"))?;
    let end = HEADER_LEN + n;
    if buf.len() < end {
        return Err(FrameError::Truncated {
            expected: end,
            got: buf.len(),
        });
    }
    Ok((decode_body(&buf[HEADER_LEN..end])?, end))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ctl::{CtlRequest, Hello};

    fn hello() -> CtlRequest {
        CtlRequest::Hello(Hello {
            device: "server".into(),
            version: "0.0.1".into(),
            instance: 42,
            resume: true,
        })
    }

    #[test]
    fn roundtrip() {
        let bytes = encode(&hello()).unwrap();
        let n = u32::from_be_bytes(bytes[..4].try_into().unwrap()) as usize;
        assert_eq!(n, bytes.len() - HEADER_LEN);
        let (back, used): (CtlRequest, usize) = decode(&bytes).unwrap();
        assert_eq!(back, hello());
        assert_eq!(used, bytes.len());
    }

    #[test]
    fn back_to_back_frames() {
        let mut buf = encode(&CtlRequest::Ping { seq: 1 }).unwrap();
        buf.extend(encode(&CtlRequest::Ping { seq: 2 }).unwrap());
        let (a, used): (CtlRequest, usize) = decode(&buf).unwrap();
        let (b, _): (CtlRequest, usize) = decode(&buf[used..]).unwrap();
        assert_eq!(a, CtlRequest::Ping { seq: 1 });
        assert_eq!(b, CtlRequest::Ping { seq: 2 });
    }

    #[test]
    fn truncated_and_oversized() {
        let bytes = encode(&hello()).unwrap();
        assert!(matches!(
            decode::<CtlRequest>(&bytes[..bytes.len() - 1]),
            Err(FrameError::Truncated { .. })
        ));
        assert!(matches!(
            decode::<CtlRequest>(&bytes[..2]),
            Err(FrameError::Truncated { .. })
        ));
        let huge = ((MAX_FRAME + 1) as u32).to_be_bytes();
        assert!(matches!(body_len(huge), Err(FrameError::TooLarge(_))));
        assert!(matches!(
            encode(&vec![0u8; MAX_FRAME + 1]),
            Err(FrameError::TooLarge(_))
        ));
    }

    #[test]
    fn garbage_body_is_codec_error() {
        let mut bytes = 1u32.to_be_bytes().to_vec();
        bytes.push(0xff);
        assert!(matches!(
            decode::<CtlRequest>(&bytes),
            Err(FrameError::Codec(_))
        ));
    }
}

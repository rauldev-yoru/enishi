//! `enishi/pair/1`: one-time pairing (design §6).
//!
//! The joining device dials the inviting one with the address hints of the invite code and
//! opens one bi-stream: [`PairRequest`] in, [`PairResponse`] out. The joiner's
//! `EndpointId` is taken from the TLS handshake, never from the message.

use std::net::SocketAddr;

use serde::{Deserialize, Serialize};

/// Bytes of the random one-time invite secret.
pub const SECRET_LEN: usize = 16;

/// Longest device name accepted from the other side (names are informational).
pub const MAX_DEVICE_NAME: usize = 64;
/// Most address hints accepted from the other side.
pub const MAX_ADDRS: usize = 8;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PairRequest {
    /// The invite secret from the code.
    pub secret: [u8; SECRET_LEN],
    /// The joiner's `[device] name`.
    pub device: String,
    /// Stable direct addresses of the joiner (only when it has a fixed `listen_port`).
    pub addrs: Vec<SocketAddr>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum PairResponse {
    /// Paired: the inviter stored the joiner as a peer.
    Ok {
        /// The inviter's `[device] name`.
        device: String,
        /// Stable direct addresses of the inviter (only with a fixed `listen_port`).
        addrs: Vec<SocketAddr>,
    },
    Err {
        message: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::{decode, encode};

    #[test]
    fn pair_frames_roundtrip() {
        let req = PairRequest {
            secret: [7; SECRET_LEN],
            device: "pc".into(),
            addrs: vec![
                "100.1.2.3:7812".parse().unwrap(),
                "[::1]:9".parse().unwrap(),
            ],
        };
        let (back, _): (PairRequest, usize) = decode(&encode(&req).unwrap()).unwrap();
        assert_eq!(back, req);
        for r in [
            PairResponse::Ok {
                device: "server".into(),
                addrs: vec![],
            },
            PairResponse::Err {
                message: "expired".into(),
            },
        ] {
            let (back, _): (PairResponse, usize) = decode(&encode(&r).unwrap()).unwrap();
            assert_eq!(back, r);
        }
    }
}

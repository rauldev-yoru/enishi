//! enishi daemon (layer L2): config, identity, peers, presence, local IPC, sessions.

pub mod audit;
pub mod config;
pub mod daemon;
pub mod identity;
pub mod ipc;
pub mod pair;
mod pairing;
pub mod paths;
pub mod policy;
pub mod session;
pub mod wire;

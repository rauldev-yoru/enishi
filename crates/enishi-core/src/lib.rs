//! enishi daemon (layer L2): config, identity, peers, presence, local IPC, sessions.

pub mod audit;
pub mod config;
pub mod config_edit;
pub mod daemon;
mod forward;
pub mod identity;
pub mod import;
pub mod ipc;
pub mod pair;
mod pairing;
pub mod paths;
pub mod policy;
pub mod session;
pub mod wire;

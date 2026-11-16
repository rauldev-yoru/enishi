//! MCP shim (layer L3): exposes a remote MCP service as a local, offline-aware stdio
//! server — `enishi mcp` (design §3.4).
//!
//! The shim only knows MCP's framing (newline-delimited JSON-RPC 2.0) and its handshake
//! (`initialize` + `notifications/initialized`); it never looks at what a given server's
//! tools do. Online it is a pass-through that remembers the results of `initialize` and of
//! the list methods; offline it answers those from the on-disk cache and fails every
//! other request at once; when the service comes back mid-session it replays the
//! handshake before forwarding the next message.

pub mod cache;
pub mod daemon;
pub mod import;
pub mod shim;

pub use cache::Cache;
pub use daemon::DaemonConnector;
pub use shim::{Connector, OpenError, Remote, run};

/// Map an `enishi mcp` target to `(device, service)`: `dev/service` or just `service`
/// (device from `-d` or the default peer). `-d` and `dev/` must agree when both given.
pub fn parse_target(
    target: &str,
    device_flag: Option<&str>,
    default_peer: Option<&str>,
) -> Result<(String, String), String> {
    let (dev, service) = match target.split_once('/') {
        Some((d, s)) => (Some(d), s),
        None => (None, target),
    };
    if service.is_empty() || dev.is_some_and(str::is_empty) || service.contains('/') {
        return Err(format!(
            "invalid target {target:?}: expected <service> or <device>/<service>"
        ));
    }
    let device = match (dev, device_flag) {
        (Some(a), Some(b)) if a != b => {
            return Err(format!("-d {b} conflicts with target device {a:?}"));
        }
        (Some(d), _) | (None, Some(d)) => d,
        (None, None) => default_peer
            .ok_or("no device given (<device>/<service> or -d) and no default_peer in config")?,
    };
    Ok((device.to_string(), service.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets() {
        let p = parse_target;
        assert_eq!(p("pc/fs", None, None).unwrap(), ("pc".into(), "fs".into()));
        assert_eq!(
            p("fs", Some("pc"), Some("x")).unwrap(),
            ("pc".into(), "fs".into())
        );
        assert_eq!(p("fs", None, Some("x")).unwrap(), ("x".into(), "fs".into()));
        assert_eq!(
            p("pc/fs", Some("pc"), None).unwrap(),
            ("pc".into(), "fs".into())
        );
        assert!(p("pc/fs", Some("other"), None).is_err());
        assert!(p("fs", None, None).is_err());
        for bad in ["", "pc/", "/fs", "a/b/c"] {
            assert!(p(bad, Some("pc"), None).is_err(), "{bad}");
        }
    }
}

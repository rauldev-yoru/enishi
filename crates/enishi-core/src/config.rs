//! `enishi.toml` (design §4). Everything is denied unless granted, per peer.

use std::{collections::BTreeMap, net::SocketAddr, path::Path, str::FromStr};

use anyhow::{Context, Result, bail};
use iroh::EndpointId;
use serde::{Deserialize, Deserializer};

use crate::policy::ExecPolicy;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub device: DeviceConfig,
    #[serde(default)]
    pub peers: BTreeMap<String, PeerConfig>,
    /// What this device publishes, and to whom. Parsed and validated; served in a later step.
    #[serde(default)]
    pub services: BTreeMap<String, ServiceConfig>,
    /// What each peer may do on this device.
    #[serde(default)]
    pub grants: BTreeMap<String, Grant>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceConfig {
    pub name: String,
    #[serde(default)]
    pub default_peer: Option<String>,
    /// Fixed UDP port; helps direct dialing and firewall rules. Random when absent.
    #[serde(default)]
    pub listen_port: Option<u16>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PeerConfig {
    #[serde(deserialize_with = "endpoint_id")]
    pub id: EndpointId,
    /// Direct address hints (Tailscale/LAN); discovery is used otherwise.
    #[serde(default)]
    pub addrs: Vec<SocketAddr>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ServiceKind {
    Mcp,
    Stdio,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceConfig {
    pub command: Vec<String>,
    pub kind: ServiceKind,
    /// Peer names allowed to use this service.
    #[serde(default)]
    pub allow: Vec<String>,
    /// Forward extra client args to the command.
    #[serde(default)]
    pub allow_args: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Grant {
    #[serde(default)]
    pub exec: ExecPolicy,
    #[serde(default)]
    pub services: Vec<String>,
}

fn endpoint_id<'de, D: Deserializer<'de>>(d: D) -> Result<EndpointId, D::Error> {
    let s = String::deserialize(d)?;
    EndpointId::from_str(s.trim())
        .map_err(|e| serde::de::Error::custom(format!("invalid peer id {s:?}: {e}")))
}

impl FromStr for Config {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        let cfg: Config = toml::from_str(s)?;
        cfg.validate()?;
        Ok(cfg)
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Config> {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => bail!(
                "no config at {} — create it (see docs/design.md §4)",
                path.display()
            ),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        text.parse()
            .with_context(|| format!("invalid config {}", path.display()))
    }

    fn validate(&self) -> Result<()> {
        if self.device.name.trim().is_empty() {
            bail!("[device] name must not be empty");
        }
        let mut ids = BTreeMap::new();
        for (name, peer) in &self.peers {
            check_name("peer", name)?;
            if let Some(other) = ids.insert(peer.id, name) {
                bail!("peers {other:?} and {name:?} have the same id");
            }
        }
        if let Some(d) = &self.device.default_peer
            && !self.peers.contains_key(d)
        {
            bail!("default_peer {d:?} is not a configured peer");
        }
        for (name, svc) in &self.services {
            check_name("service", name)?;
            if svc.command.is_empty() || svc.command[0].is_empty() {
                bail!("service {name:?}: command must not be empty");
            }
            for p in &svc.allow {
                if !self.peers.contains_key(p) {
                    bail!("service {name:?}: allow lists unknown peer {p:?}");
                }
            }
        }
        for (peer, grant) in &self.grants {
            if !self.peers.contains_key(peer) {
                bail!("[grants.{peer}] refers to an unknown peer");
            }
            for s in &grant.services {
                if !self.services.contains_key(s) {
                    bail!("[grants.{peer}] grants unknown service {s:?}");
                }
            }
        }
        Ok(())
    }

    /// Grant for `peer`; an absent grant denies everything.
    pub fn grant(&self, peer: &str) -> Grant {
        self.grants.get(peer).cloned().unwrap_or_default()
    }

    /// Resolve `-d/--device`, falling back to `default_peer`.
    pub fn target_peer<'a>(&'a self, device: Option<&'a str>) -> Result<&'a str> {
        let name = match device.or(self.device.default_peer.as_deref()) {
            Some(n) => n,
            None => bail!("no device given (-d <name>) and no default_peer in config"),
        };
        if !self.peers.contains_key(name) {
            bail!("unknown device {name:?}");
        }
        Ok(name)
    }
}

/// Names are used in CLI targets like `device/service`, so keep them simple.
fn check_name(what: &str, name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.');
    if !ok {
        bail!("{what} name {name:?} may only contain letters, digits, '-', '_' and '.'");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use iroh::SecretKey;

    fn id(n: u8) -> String {
        SecretKey::from_bytes(&[n; 32]).public().to_string()
    }

    fn full() -> String {
        format!(
            r#"
[device]
name = "server"
default_peer = "windows-home"
listen_port = 7801

[peers.windows-home]
id = "{}"
addrs = ["100.71.180.5:7802"]

[peers.laptop]
id = "{}"

[services.fs]
command = ["node", "C:/x/index.js", "C:/Users/raulr/Projects"]
kind = "mcp"
allow = ["windows-home"]
allow_args = false

[services.ps]
command = ["pwsh"]
kind = "stdio"

[grants.windows-home]
exec = true
services = ["fs", "ps"]

[grants.laptop]
exec = {{ allow = ["git *"], deny = ["rm *"] }}
"#,
            id(1),
            id(2)
        )
    }

    #[test]
    fn parses_design_example() {
        let c: Config = full().parse().unwrap();
        assert_eq!(c.device.name, "server");
        assert_eq!(c.device.default_peer.as_deref(), Some("windows-home"));
        assert_eq!(c.device.listen_port, Some(7801));
        let p = &c.peers["windows-home"];
        assert_eq!(p.id.to_string(), id(1));
        assert_eq!(p.addrs, vec!["100.71.180.5:7802".parse().unwrap()]);
        assert!(c.peers["laptop"].addrs.is_empty());
        assert_eq!(c.services["fs"].kind, ServiceKind::Mcp);
        assert_eq!(c.services["ps"].kind, ServiceKind::Stdio);
        assert!(!c.services["ps"].allow_args);
        assert_eq!(c.grant("windows-home").exec, ExecPolicy::Flag(true));
        assert_eq!(c.grant("windows-home").services, vec!["fs", "ps"]);
        assert!(matches!(c.grant("laptop").exec, ExecPolicy::Rules { .. }));
        assert_eq!(c.target_peer(None).unwrap(), "windows-home");
        assert_eq!(c.target_peer(Some("laptop")).unwrap(), "laptop");
        assert!(c.target_peer(Some("nope")).is_err());
    }

    #[test]
    fn minimal_config_denies_everything() {
        let c: Config = "[device]\nname = \"pc\"\n".parse().unwrap();
        assert!(c.peers.is_empty() && c.services.is_empty() && c.grants.is_empty());
        assert_eq!(c.device.listen_port, None);
        let g = c.grant("anyone");
        assert_eq!(g.exec, ExecPolicy::Deny);
        assert!(g.services.is_empty());
        assert!(c.target_peer(None).is_err());
    }

    fn err(toml: &str) -> String {
        format!("{:#}", toml.parse::<Config>().unwrap_err())
    }

    #[test]
    fn rejects_invalid_configs() {
        let peer = format!("[peers.a]\nid = \"{}\"\n", id(1));
        assert!(err("[device]\nname = \"x\"\nbogus = 1\n").contains("bogus"));
        assert!(err("").contains("device"));
        assert!(err("[device]\nname = \"\"\n").contains("name"));
        assert!(
            err("[device]\nname = \"x\"\n[peers.a]\nid = \"zzz\"\n").contains("invalid peer id")
        );
        assert!(err("[device]\nname = \"x\"\ndefault_peer = \"b\"\n").contains("default_peer"));
        assert!(
            err(&format!(
                "[device]\nname = \"x\"\n{peer}[grants.b]\nexec = true\n"
            ))
            .contains("unknown peer")
        );
        assert!(
            err(&format!(
                "[device]\nname = \"x\"\n{peer}[grants.a]\nservices = [\"fs\"]\n"
            ))
            .contains("unknown service")
        );
        assert!(
            err("[device]\nname = \"x\"\n[services.fs]\ncommand = []\nkind = \"mcp\"\n")
                .contains("command")
        );
        assert!(
            err("[device]\nname = \"x\"\n[services.fs]\ncommand = [\"a\"]\nkind = \"http\"\n")
                .contains("http")
        );
        assert!(err(&format!("[device]\nname = \"x\"\n{peer}[services.fs]\ncommand = [\"a\"]\nkind = \"mcp\"\nallow = [\"b\"]\n")).contains("unknown peer"));
        let dup = format!(
            "[device]\nname = \"x\"\n{peer}[peers.b]\nid = \"{}\"\n",
            id(1)
        );
        assert!(err(&dup).contains("same id"));
        let bad_name = format!(
            "[device]\nname = \"x\"\n[peers.\"a/b\"]\nid = \"{}\"\n",
            id(1)
        );
        assert!(err(&bad_name).contains("may only contain"));
        let bad_addr = format!("[device]\nname = \"x\"\n{peer}addrs = [\"nope\"]\n");
        assert!(toml::from_str::<Config>(&bad_addr).is_err());
    }

    #[test]
    fn load_reports_missing_file() {
        let e = Config::load(Path::new("/nonexistent/enishi.toml")).unwrap_err();
        assert!(format!("{e:#}").contains("no config at"));
    }
}

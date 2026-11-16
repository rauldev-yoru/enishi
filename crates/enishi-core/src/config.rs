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

pub use enishi_proto::session::ServiceKind;

/// A published stdio service (`[services.<name>]`). Spawned directly from `command` (argv,
/// no shell) for every session; see [`crate::policy::authorize_service`] for who may open it.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceConfig {
    pub command: Vec<String>,
    pub kind: ServiceKind,
    /// Peer names allowed to use this service (the peer also needs it in
    /// `[grants.<peer>] services`).
    #[serde(default)]
    pub allow: Vec<String>,
    /// Forward extra client args to the command.
    #[serde(default)]
    pub allow_args: bool,
    /// Working directory (a leading `~` is this device user's home). Default: the daemon's.
    #[serde(default)]
    pub cwd: Option<String>,
    /// Extra environment for the process.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
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

/// Append `[peers.<name>]` to the config text, keeping everything else (comments,
/// order, formatting) as it was; also set `[device] default_peer` when it is unset.
/// Returns the new text and whether `default_peer` was set. The caller validates the
/// result by parsing it before writing it.
pub fn add_peer_to_toml(
    text: &str,
    name: &str,
    id: &EndpointId,
    addrs: &[SocketAddr],
) -> Result<(String, bool)> {
    use toml_edit::{Array, DocumentMut, Item, Table, value};
    check_name("peer", name)?;
    let mut doc: DocumentMut = text.parse().context("config is not valid TOML")?;
    if doc.get("peers").is_none() {
        let mut t = Table::new();
        t.set_implicit(true);
        doc.insert("peers", Item::Table(t));
    }
    let peers = doc["peers"]
        .as_table_mut()
        .context("`peers` must be a table ([peers.<name>] sections)")?;
    if peers.contains_key(name) {
        bail!("a peer named {name:?} already exists");
    }
    let mut t = Table::new();
    t.decor_mut().set_prefix("\n");
    t.insert("id", value(id.to_string()));
    if !addrs.is_empty() {
        t.insert(
            "addrs",
            value(addrs.iter().map(|a| a.to_string()).collect::<Array>()),
        );
    }
    peers.insert(name, Item::Table(t));
    let device = doc
        .get_mut("device")
        .and_then(Item::as_table_mut)
        .context("config has no [device] table")?;
    let set_default = !device.contains_key("default_peer");
    if set_default {
        device.insert("default_peer", value(name));
    }
    let mut out = doc.to_string();
    if !out.ends_with('\n') {
        out.push('\n');
    }
    Ok((out, set_default))
}

/// Replace `path` with `text` atomically (temp file in the same directory + rename), so a
/// crash never leaves a half-written config.
pub fn write_atomic(path: &Path, text: &str) -> Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = std::path::PathBuf::from(tmp);
    std::fs::write(&tmp, text).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))
}

/// Names are used in CLI targets like `device/service`, so keep them simple.
pub(crate) fn check_name(what: &str, name: &str) -> Result<()> {
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
cwd = "~"
env = {{ A = "1" }}

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
        assert_eq!(c.services["ps"].cwd.as_deref(), Some("~"));
        assert_eq!(c.services["ps"].env["A"], "1");
        assert!(c.services["fs"].env.is_empty() && c.services["fs"].cwd.is_none());
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
    fn add_peer_keeps_comments_and_sets_default() {
        let text = format!(
            "# my config\n[device]\nname = \"pc\" # inline comment\nlisten_port = 7812\n\n\
             # the server\n[peers.server]\nid = \"{}\"\n\n[grants.server]\nexec = true\n",
            id(1)
        );
        // default_peer unset → set to the new peer.
        let new = EndpointId::from_str(&id(2)).unwrap();
        let addrs: Vec<SocketAddr> = vec!["100.1.2.3:7811".parse().unwrap()];
        let (out, set) = add_peer_to_toml(&text, "laptop", &new, &addrs).unwrap();
        assert!(set);
        for keep in [
            "# my config",
            "# inline comment",
            "# the server",
            "[grants.server]",
        ] {
            assert!(out.contains(keep), "lost {keep:?}:\n{out}");
        }
        assert!(
            out.starts_with(&text[..text.find("listen_port").unwrap()]),
            "{out}"
        );
        let cfg: Config = out.parse().unwrap();
        assert_eq!(cfg.device.default_peer.as_deref(), Some("laptop"));
        assert_eq!(cfg.peers["laptop"].id, new);
        assert_eq!(cfg.peers["laptop"].addrs, addrs);
        assert_eq!(cfg.peers["server"].id.to_string(), id(1));
        assert_eq!(
            cfg.grant("laptop").exec,
            ExecPolicy::Deny,
            "pairing grants nothing"
        );
        assert!(out.contains("\n\n[peers.laptop]\nid = "), "{out}");

        // default_peer already set → untouched; no addrs → no addrs key.
        let (out2, set2) =
            add_peer_to_toml(&out, "tablet", &EndpointId::from_str(&id(3)).unwrap(), &[]).unwrap();
        assert!(!set2);
        let cfg2: Config = out2.parse().unwrap();
        assert_eq!(cfg2.device.default_peer.as_deref(), Some("laptop"));
        assert!(cfg2.peers["tablet"].addrs.is_empty());
        // toml_edit groups the new table after the existing [peers.*] ones; nothing else moves.
        let block = format!("\n[peers.tablet]\nid = \"{}\"\n", id(3));
        assert_eq!(out2.replacen(&block, "", 1), out, "only inserted:\n{out2}");

        // Collisions and bad names are refused.
        let e = add_peer_to_toml(&out, "server", &new, &[]).unwrap_err();
        assert!(e.to_string().contains("already exists"), "{e}");
        assert!(add_peer_to_toml(&out, "a b", &new, &[]).is_err());

        // A config without any [peers] gets the table.
        let (out3, _) = add_peer_to_toml("[device]\nname = \"x\"\n", "pc", &new, &[]).unwrap();
        let cfg3: Config = out3.parse().unwrap();
        assert_eq!(cfg3.peers.len(), 1);
    }

    #[test]
    fn write_atomic_replaces() {
        let dir = std::env::temp_dir().join(format!("enishi-wa-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("enishi.toml");
        std::fs::write(&p, "old").unwrap();
        write_atomic(&p, "new").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "new");
        assert!(!dir.join("enishi.toml.tmp").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn load_reports_missing_file() {
        let e = Config::load(Path::new("/nonexistent/enishi.toml")).unwrap_err();
        assert!(format!("{e:#}").contains("no config at"));
    }
}

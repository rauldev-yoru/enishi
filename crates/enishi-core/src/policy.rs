//! Authorization. Every exec request goes through [`authorize_exec`] (design D5).

use serde::Deserialize;

/// `exec` grant for a peer. v0.1 supports allow/deny; `Rules` is reserved for allowlists.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(untagged)]
pub enum ExecPolicy {
    #[default]
    #[serde(skip)]
    Deny,
    Flag(bool),
    Rules {
        allow: Vec<String>,
        #[serde(default)]
        deny: Vec<String>,
    },
}

pub fn authorize_exec(policy: &ExecPolicy, _command: &[String]) -> Result<(), &'static str> {
    match policy {
        ExecPolicy::Flag(true) => Ok(()),
        ExecPolicy::Flag(false) | ExecPolicy::Deny => Err("exec not granted to this peer"),
        ExecPolicy::Rules { .. } => Err("exec rules are not supported yet"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Deserialize)]
    struct G {
        #[serde(default)]
        exec: ExecPolicy,
    }

    #[test]
    fn parses_bool_and_rules_and_default() {
        let g: G = toml::from_str("exec = true").unwrap();
        assert_eq!(g.exec, ExecPolicy::Flag(true));
        let g: G = toml::from_str("").unwrap();
        assert_eq!(g.exec, ExecPolicy::Deny);
        let g: G = toml::from_str(r#"exec = { allow = ["git *"] }"#).unwrap();
        assert!(matches!(g.exec, ExecPolicy::Rules { .. }));
    }

    #[test]
    fn default_is_deny() {
        assert!(authorize_exec(&ExecPolicy::default(), &["ls".into()]).is_err());
        assert!(authorize_exec(&ExecPolicy::Flag(true), &["ls".into()]).is_ok());
    }
}

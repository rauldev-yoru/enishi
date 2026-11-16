//! Authorization. Every exec request goes through [`authorize_exec`] (design D5), every
//! service session through [`authorize_service`].

use enishi_proto::{
    exit,
    session::{ExecRequest, ServiceInfo},
};
use serde::Deserialize;

use crate::config::{Config, ServiceConfig};

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

/// `mcp` grant for a peer: may it start arbitrary MCP server commands here with
/// `enishi mcp -d <this device> -- <command…>` (transparent mode)? Same shape as
/// [`ExecPolicy`] (`true`/`false` now, `{ allow = [...] }` command rules later), checked
/// only by [`authorize_mcp`]. Independent of `exec` (design D13).
pub type McpPolicy = ExecPolicy;

/// The single check for transparent MCP sessions; the request is then spawned raw.
pub fn authorize_mcp(policy: &McpPolicy, _command: &[String]) -> Result<(), &'static str> {
    match policy {
        ExecPolicy::Flag(true) => Ok(()),
        ExecPolicy::Flag(false) | ExecPolicy::Deny => {
            Err("mcp commands not granted to this peer (needs [grants.<peer>] mcp = true)")
        }
        ExecPolicy::Rules { .. } => Err("mcp rules are not supported yet"),
    }
}

/// What actually runs for a transparent MCP session: the argv spawned directly, never a
/// shell, never a timeout (the session lives as long as the client).
pub fn mcp_command(mut req: ExecRequest) -> ExecRequest {
    req.raw = true;
    req.shell = None;
    req.timeout_secs = None;
    req
}

/// A refused session: `code` is [`exit::NO_SUCH_SERVICE`] or [`exit::NOT_ALLOWED`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub code: i32,
    pub message: String,
}

/// May `peer` open service `name` (with `args`)? Default deny. The service must exist
/// ([`exit::NO_SUCH_SERVICE`] otherwise), list `peer` in its `allow`, appear in
/// `[grants.<peer>] services`, and accept args if any are given ([`exit::NOT_ALLOWED`]).
pub fn authorize_service<'a>(
    cfg: &'a Config,
    peer: &str,
    name: &str,
    args: &[String],
) -> Result<&'a ServiceConfig, Refusal> {
    let Some(svc) = cfg.services.get(name) else {
        return Err(Refusal {
            code: exit::NO_SUCH_SERVICE,
            message: format!("no service {name:?} on this device"),
        });
    };
    let deny = |why: String| Refusal {
        code: exit::NOT_ALLOWED,
        message: why,
    };
    if !svc.allow.iter().any(|p| p == peer) {
        return Err(deny(format!("service {name:?} does not allow this peer")));
    }
    if !cfg.grant(peer).services.iter().any(|s| s == name) {
        return Err(deny(format!(
            "service {name:?} is not in this peer's grants"
        )));
    }
    if !args.is_empty() && !svc.allow_args {
        return Err(deny(format!(
            "service {name:?} does not accept extra args (allow_args = false)"
        )));
    }
    Ok(svc)
}

/// The services `peer` may open, as listed by `enishi services`.
pub fn visible_services(cfg: &Config, peer: &str) -> Vec<ServiceInfo> {
    cfg.services
        .iter()
        .filter(|(name, _)| authorize_service(cfg, peer, name, &[]).is_ok())
        .map(|(name, s)| ServiceInfo {
            name: name.clone(),
            kind: s.kind,
            allow_args: s.allow_args,
            port: s.target_port(),
        })
        .collect()
}

/// The process for a service session: its argv (+ client args) spawned directly, never
/// through a shell, with the service's cwd and env.
pub fn service_command(svc: &ServiceConfig, args: &[String]) -> ExecRequest {
    ExecRequest {
        command: svc.command.iter().chain(args).cloned().collect(),
        shell: None,
        raw: true,
        cwd: svc.cwd.clone(),
        env: svc
            .env
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
        timeout_secs: None,
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
        assert!(authorize_exec(&ExecPolicy::Flag(false), &["ls".into()]).is_err());
        // Rules are parsed but not enforced yet: fail closed, even for an allowed pattern.
        let rules = ExecPolicy::Rules {
            allow: vec!["*".into()],
            deny: vec![],
        };
        assert!(authorize_exec(&rules, &["ls".into()]).is_err());
    }

    fn cfg() -> Config {
        let id = |n: u8| iroh::SecretKey::from_bytes(&[n; 32]).public().to_string();
        format!(
            r#"
[device]
name = "pc"
[peers.a]
id = "{}"
[peers.b]
id = "{}"
[peers.c]
id = "{}"
[services.fs]
command = ["node", "fs.js", "/root"]
kind = "mcp"
allow = ["a", "b"]
[services.wc]
command = ["wc", "-c"]
kind = "stdio"
allow = ["a"]
allow_args = true
cwd = "/tmp"
env = {{ K = "v" }}
[services.hidden]
command = ["x"]
kind = "stdio"
[grants.a]
services = ["fs", "wc", "hidden"]
[grants.b]
exec = true
[grants.c]
services = ["fs"]
"#,
            id(1),
            id(2),
            id(3)
        )
        .parse()
        .unwrap()
    }

    #[test]
    fn service_permission_matrix() {
        let c = cfg();
        let code = |peer: &str, name: &str, args: &[&str]| {
            let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
            authorize_service(&c, peer, name, &args)
                .err()
                .map(|r| r.code)
        };
        // allow ∧ grant → ok.
        assert_eq!(code("a", "fs", &[]), None);
        assert_eq!(code("a", "wc", &[]), None);
        // allow but no grant (b), grant but not in allow (c), neither (b on wc).
        assert_eq!(code("b", "fs", &[]), Some(exit::NOT_ALLOWED));
        assert_eq!(code("c", "fs", &[]), Some(exit::NOT_ALLOWED));
        assert_eq!(code("b", "wc", &[]), Some(exit::NOT_ALLOWED));
        // granted but `allow` empty.
        assert_eq!(code("a", "hidden", &[]), Some(exit::NOT_ALLOWED));
        // unknown service / unknown peer.
        assert_eq!(code("a", "nope", &[]), Some(exit::NO_SUCH_SERVICE));
        assert_eq!(code("zz", "fs", &[]), Some(exit::NOT_ALLOWED));
        // args only with allow_args.
        assert_eq!(code("a", "fs", &["/etc"]), Some(exit::NOT_ALLOWED));
        assert_eq!(code("a", "wc", &["-l"]), None);
        // exec grant does not imply services.
        assert!(visible_services(&c, "b").is_empty());
        let names = |p: &str| -> Vec<String> {
            visible_services(&c, p)
                .into_iter()
                .map(|s| s.name)
                .collect()
        };
        assert_eq!(names("a"), vec!["fs", "wc"]);
        assert!(names("c").is_empty());
    }

    #[test]
    fn mcp_grant_is_separate_from_exec() {
        let c = cfg();
        let argv = vec!["npx".to_string(), "-y".into(), "pkg".into()];
        // b has exec = true but no mcp grant: denied (D13).
        assert!(authorize_mcp(&c.grant("b").mcp, &argv).is_err());
        assert!(authorize_mcp(&McpPolicy::Flag(true), &argv).is_ok());
        assert!(authorize_mcp(&McpPolicy::Flag(false), &argv).is_err());
        let rules = McpPolicy::Rules {
            allow: vec!["npx *".into()],
            deny: vec![],
        };
        assert!(authorize_mcp(&rules, &argv).is_err(), "rules fail closed");
        let g: crate::config::Grant = toml::from_str("mcp = true").unwrap();
        assert_eq!(g.mcp, McpPolicy::Flag(true));
        assert_eq!(g.exec, ExecPolicy::Deny, "mcp does not imply exec");
        let r = mcp_command(ExecRequest {
            command: argv.clone(),
            shell: Some("bash".into()),
            raw: false,
            timeout_secs: Some(3),
            ..Default::default()
        });
        assert!(r.raw && r.shell.is_none() && r.timeout_secs.is_none());
        assert_eq!(r.command, argv);
    }

    #[test]
    fn service_command_is_raw_argv() {
        let c = cfg();
        let r = service_command(&c.services["wc"], &["-l".into()]);
        assert_eq!(r.command, vec!["wc", "-c", "-l"]);
        assert!(r.raw && r.shell.is_none() && r.timeout_secs.is_none());
        assert_eq!(r.cwd.as_deref(), Some("/tmp"));
        assert_eq!(r.env, vec![("K".to_string(), "v".to_string())]);
    }
}

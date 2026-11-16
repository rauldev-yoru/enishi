//! Pure firewall logic for `enishi install` (tested on every OS): the Windows rule-state
//! decision and the elevated helper's arguments, and the Linux firewalld/ufw parsing,
//! decision and the exact `sudo` commands.

use std::path::Path;

use super::render::win_arg;

// ---------- Windows ----------

/// One Windows Firewall rule bound to an enishi binary (or named like our rule), as the
/// `Get-NetFirewallRule` query prints it: tab-separated
/// `DisplayName, Profile, Protocol, Direction, Action, Program, Enabled`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WinRule {
    pub name: String,
    pub profile: String,
    pub protocol: String,
    pub direction: String,
    pub action: String,
    pub program: String,
    pub enabled: bool,
}

impl WinRule {
    pub fn describe(&self) -> String {
        format!(
            "{} [{}] {} {} {}{}",
            self.name,
            self.profile,
            self.protocol,
            self.direction,
            self.action,
            if self.enabled { "" } else { " (disabled)" }
        )
    }
}

/// Sentinel the query prints last, so an empty rule list is told apart from a failed query.
pub const WIN_QUERY_OK: &str = "ENISHI-QUERY-OK";

/// Parse the query output; `None` if the query did not finish (no sentinel).
pub fn parse_win_rules(out: &str) -> Option<Vec<WinRule>> {
    let mut rules = Vec::new();
    let mut ok = false;
    for line in out.lines() {
        let line = line.trim_end_matches('\r');
        if line.trim() == WIN_QUERY_OK {
            ok = true;
            continue;
        }
        let f: Vec<&str> = line.split('\t').collect();
        if f.len() != 7 {
            continue;
        }
        rules.push(WinRule {
            name: f[0].trim().into(),
            profile: f[1].trim().into(),
            protocol: f[2].trim().into(),
            direction: f[3].trim().into(),
            action: f[4].trim().into(),
            program: f[5].trim().into(),
            enabled: f[6].trim().eq_ignore_ascii_case("true"),
        });
    }
    ok.then_some(rules)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WinFwState {
    /// Exactly our rule: one inbound UDP allow, Private only, per enishi binary; nothing else.
    Correct,
    /// No rule at all for the enishi binaries.
    Missing,
    /// Something to fix: missing pieces and/or other rules (Windows' auto-created allow or
    /// block rules, a broader profile, a disabled rule…), one line each.
    Wrong(Vec<String>),
}

impl WinFwState {
    pub fn needs_fix(&self) -> bool {
        *self != WinFwState::Correct
    }
}

fn same_path(a: &str, b: &str) -> bool {
    a.trim().eq_ignore_ascii_case(b.trim())
}

/// Is `r` our scoped rule for `program`?
fn is_scoped(r: &WinRule, rule: &str, program: &str) -> bool {
    r.name == rule
        && same_path(&r.program, program)
        && r.profile.eq_ignore_ascii_case("Private")
        && r.protocol.eq_ignore_ascii_case("UDP")
        && r.direction.eq_ignore_ascii_case("Inbound")
        && r.action.eq_ignore_ascii_case("Allow")
        && r.enabled
}

/// Decide whether the firewall needs the elevated fix, from every rule bound to the
/// enishi binaries or named `rule`.
pub fn win_rule_state(rules: &[WinRule], rule: &str, programs: &[&str]) -> WinFwState {
    if rules.is_empty() {
        return WinFwState::Missing;
    }
    let mut problems = Vec::new();
    for p in programs {
        let n = rules.iter().filter(|r| is_scoped(r, rule, p)).count();
        if n == 0 {
            problems.push(format!(
                "missing: {rule} [Private] UDP Inbound Allow for {p}"
            ));
        } else if n > 1 {
            problems.push(format!("duplicate: {rule} for {p}"));
        }
    }
    for r in rules {
        if !programs.iter().any(|p| is_scoped(r, rule, p)) {
            problems.push(format!("other: {}", r.describe()));
        }
    }
    if problems.is_empty() {
        WinFwState::Correct
    } else {
        WinFwState::Wrong(problems)
    }
}

/// Arguments for the elevated helper (`enishi.exe firewall …` run with the `runas` verb).
pub fn helper_args(name: &str, prefix: &Path, remove: bool) -> String {
    let mut a = format!(
        "firewall --elevated --name {} --prefix {}",
        win_arg(name),
        win_arg(&prefix.to_string_lossy())
    );
    if remove {
        a.push_str(" --remove");
    }
    a
}

// ---------- Linux ----------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    Firewalld,
    Ufw,
}

impl Tool {
    pub fn label(self) -> &'static str {
        match self {
            Tool::Firewalld => "firewalld",
            Tool::Ufw => "ufw",
        }
    }
}

/// What the probes found: the active firewall, the zone that applies (firewalld) and
/// whether the port is open (`None`: could not tell, e.g. ufw needs root to list rules).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Detected {
    pub tool: Tool,
    pub zone: Option<String>,
    pub open: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinuxPlan {
    /// No active firewalld/ufw: nothing to do.
    NoFirewall,
    /// A firewall is active but no fixed `listen_port`: inbound can't be opened.
    NoListenPort(Tool),
    AlreadyOpen {
        tool: Tool,
        zone: Option<String>,
        port: u16,
    },
    /// Closed (or unknown): these commands open it.
    Open {
        tool: Tool,
        port: u16,
        known_closed: bool,
        cmds: Vec<Vec<String>>,
    },
}

pub fn linux_plan(found: Option<&Detected>, port: Option<u16>) -> LinuxPlan {
    let Some(d) = found else {
        return LinuxPlan::NoFirewall;
    };
    let Some(port) = port else {
        return LinuxPlan::NoListenPort(d.tool);
    };
    if d.open == Some(true) {
        return LinuxPlan::AlreadyOpen {
            tool: d.tool,
            zone: d.zone.clone(),
            port,
        };
    }
    LinuxPlan::Open {
        tool: d.tool,
        port,
        known_closed: d.open == Some(false),
        cmds: open_cmds(d.tool, d.zone.as_deref(), port),
    }
}

/// The sudo commands that open `port`/udp: firewalld runtime + permanent (no reload, so
/// other runtime-only changes survive); ufw one idempotent rule.
pub fn open_cmds(tool: Tool, zone: Option<&str>, port: u16) -> Vec<Vec<String>> {
    let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    let spec = format!("{port}/udp");
    match tool {
        Tool::Firewalld => {
            let zone_arg = zone.map(|z| format!("--zone={z}"));
            let mut rt = s(&["sudo", "firewall-cmd"]);
            let mut perm = s(&["sudo", "firewall-cmd", "--permanent"]);
            for c in [&mut rt, &mut perm] {
                if let Some(z) = &zone_arg {
                    c.push(z.clone());
                }
                c.push(format!("--add-port={spec}"));
            }
            vec![rt, perm]
        }
        Tool::Ufw => vec![s(&["sudo", "ufw", "allow", &spec, "comment", "enishi"])],
    }
}

fn sh_quote(s: &str) -> String {
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./=:,+@%".contains(c))
    {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

/// The commands as one line to paste into a shell.
pub fn shell_line(cmds: &[Vec<String>]) -> String {
    cmds.iter()
        .map(|c| c.iter().map(|a| sh_quote(a)).collect::<Vec<_>>().join(" "))
        .collect::<Vec<_>>()
        .join(" && ")
}

/// `dev` of the default route from `ip route show default`.
pub fn default_route_dev(ip_route: &str) -> Option<String> {
    ip_route.lines().find_map(|l| {
        let w: Vec<&str> = l.split_whitespace().collect();
        if w.first() != Some(&"default") {
            return None;
        }
        w.windows(2)
            .find(|p| p[0] == "dev")
            .map(|p| p[1].to_string())
    })
}

/// Does a firewalld `--list-ports` line (`1025-65535/udp 22/tcp`) cover `port`/`proto`?
pub fn ports_cover(list: &str, port: u16, proto: &str) -> bool {
    list.split_whitespace().any(|e| {
        let Some((range, p)) = e.split_once('/') else {
            return false;
        };
        p.eq_ignore_ascii_case(proto) && range_covers(range, '-', port)
    })
}

fn range_covers(range: &str, sep: char, port: u16) -> bool {
    match range.split_once(sep) {
        Some((a, b)) => match (a.trim().parse::<u16>(), b.trim().parse::<u16>()) {
            (Ok(a), Ok(b)) => (a..=b).contains(&port),
            _ => false,
        },
        None => range.trim().parse::<u16>() == Ok(port),
    }
}

/// From `ufw status verbose`: `None` if ufw is inactive, else whether `port`/udp is let in
/// (default incoming policy allow, or an ALLOW rule covering it; ports may be `N`, `N/udp`,
/// `A:B/udp`, `N,M/udp`).
pub fn ufw_port_open(status: &str, port: u16) -> Option<bool> {
    let mut active = false;
    for line in status.lines() {
        let l = line.trim();
        if let Some(s) = l.strip_prefix("Status:") {
            active = s.trim().eq_ignore_ascii_case("active");
            if !active {
                return None;
            }
            continue;
        }
        if let Some(d) = l.strip_prefix("Default:") {
            if d.split(',')
                .any(|p| p.trim().starts_with("allow (incoming)"))
            {
                return Some(true);
            }
            continue;
        }
        let mut w = l.split_whitespace();
        let (Some(to), Some(action)) = (w.next(), w.next()) else {
            continue;
        };
        if !action.eq_ignore_ascii_case("ALLOW") {
            continue;
        }
        if w.next().is_some_and(|x| x.eq_ignore_ascii_case("OUT")) {
            continue;
        }
        let (ports, proto) = match to.split_once('/') {
            Some((a, p)) => (a, Some(p)),
            None => (to, None),
        };
        if proto.is_some_and(|p| !p.eq_ignore_ascii_case("udp")) {
            continue;
        }
        if ports.split(',').any(|r| range_covers(r, ':', port)) {
            return Some(true);
        }
    }
    active.then_some(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    const D: &str = r"C:\P\enishiw.exe";
    const C: &str = r"C:\P\enishi.exe";

    fn rule(name: &str, profile: &str, proto: &str, action: &str, prog: &str) -> WinRule {
        WinRule {
            name: name.into(),
            profile: profile.into(),
            protocol: proto.into(),
            direction: "Inbound".into(),
            action: action.into(),
            program: prog.into(),
            enabled: true,
        }
    }

    #[test]
    fn windows_query_parse() {
        let out = "enishi\tPrivate\tUDP\tInbound\tAllow\tC:\\P\\enishiw.exe\tTrue\r\n\
                   junk line\r\n\
                   enishiw.exe\tPrivate, Public\tTCP\tInbound\tBlock\tC:\\P\\enishiw.exe\tFalse\r\n\
                   ENISHI-QUERY-OK\r\n";
        let r = parse_win_rules(out).unwrap();
        assert_eq!(r.len(), 2);
        assert_eq!(r[0], rule("enishi", "Private", "UDP", "Allow", D));
        assert_eq!(r[1].profile, "Private, Public");
        assert!(!r[1].enabled);
        assert_eq!(
            parse_win_rules("enishi\tPrivate\tUDP\tInbound\tAllow\tx\tTrue"),
            None
        );
        assert_eq!(parse_win_rules("ENISHI-QUERY-OK\n"), Some(vec![]));
    }

    #[test]
    fn windows_rule_state() {
        let ours = vec![
            rule("enishi", "Private", "UDP", "Allow", D),
            rule("enishi", "Private", "UDP", "Allow", &C.to_lowercase()),
        ];
        let progs = [D, C];
        assert_eq!(win_rule_state(&ours, "enishi", &progs), WinFwState::Correct);
        assert_eq!(win_rule_state(&[], "enishi", &progs), WinFwState::Missing);
        assert!(!WinFwState::Correct.needs_fix() && WinFwState::Missing.needs_fix());

        // Windows' prompt rules next to ours (allow on Public, block when dismissed).
        let mut auto = ours.clone();
        auto.push(rule("enishiw.exe", "Public", "TCP", "Allow", D));
        auto.push(rule("enishiw.exe", "Private", "UDP", "Block", D));
        let WinFwState::Wrong(p) = win_rule_state(&auto, "enishi", &progs) else {
            panic!()
        };
        assert_eq!(p.len(), 2, "{p:?}");
        assert!(p[1].contains("Block"));

        // Only Windows' rules, ours absent.
        let only_auto = vec![rule("enishiw.exe", "Private, Public", "UDP", "Allow", D)];
        let WinFwState::Wrong(p) = win_rule_state(&only_auto, "enishi", &progs) else {
            panic!()
        };
        assert_eq!(p.len(), 3, "{p:?}");

        // Our name but too broad / disabled / one binary missing.
        let broad = vec![
            rule("enishi", "Private, Public", "UDP", "Allow", D),
            rule("enishi", "Private", "UDP", "Allow", C),
        ];
        assert!(win_rule_state(&broad, "enishi", &progs).needs_fix());
        let mut off = ours.clone();
        off[0].enabled = false;
        assert!(win_rule_state(&off, "enishi", &progs).needs_fix());
        assert!(win_rule_state(&ours[..1], "enishi", &progs).needs_fix());
        // Another install name's rule on the same binaries is "other".
        assert!(win_rule_state(&ours, "enishi-test", &progs).needs_fix());
    }

    #[test]
    fn windows_helper_args() {
        assert_eq!(
            helper_args(
                "enishi",
                Path::new(r"C:\Users\r\AppData\Local\Programs\enishi"),
                false
            ),
            r"firewall --elevated --name enishi --prefix C:\Users\r\AppData\Local\Programs\enishi"
        );
        assert_eq!(
            helper_args("enishi-test", Path::new(r"C:\my dir\"), true),
            r#"firewall --elevated --name enishi-test --prefix "C:\my dir\\" --remove"#
        );
    }

    #[test]
    fn linux_decision() {
        let fwd = |open| Detected {
            tool: Tool::Firewalld,
            zone: Some("FedoraServer".into()),
            open,
        };
        assert_eq!(linux_plan(None, Some(7821)), LinuxPlan::NoFirewall);
        assert_eq!(
            linux_plan(Some(&fwd(Some(false))), None),
            LinuxPlan::NoListenPort(Tool::Firewalld)
        );
        assert!(matches!(
            linux_plan(Some(&fwd(Some(true))), Some(7821)),
            LinuxPlan::AlreadyOpen { port: 7821, .. }
        ));
        // The closed case.
        let LinuxPlan::Open {
            tool,
            port,
            known_closed,
            cmds,
        } = linux_plan(Some(&fwd(Some(false))), Some(7821))
        else {
            panic!()
        };
        assert_eq!((tool, port, known_closed), (Tool::Firewalld, 7821, true));
        assert_eq!(
            shell_line(&cmds),
            "sudo firewall-cmd --zone=FedoraServer --add-port=7821/udp && \
             sudo firewall-cmd --permanent --zone=FedoraServer --add-port=7821/udp"
        );
        // ufw whose rules we could not read (needs root): still offered, not "closed".
        let ufw = Detected {
            tool: Tool::Ufw,
            zone: None,
            open: None,
        };
        let LinuxPlan::Open {
            known_closed, cmds, ..
        } = linux_plan(Some(&ufw), Some(7821))
        else {
            panic!()
        };
        assert!(!known_closed);
        assert_eq!(shell_line(&cmds), "sudo ufw allow 7821/udp comment enishi");
        assert_eq!(
            shell_line(&open_cmds(Tool::Firewalld, Some("my zone"), 1)),
            "sudo firewall-cmd '--zone=my zone' --add-port=1/udp && \
             sudo firewall-cmd --permanent '--zone=my zone' --add-port=1/udp"
        );
    }

    #[test]
    fn linux_parsers() {
        assert_eq!(
            default_route_dev(
                "default via 192.168.1.1 dev enp4s0 proto dhcp src 192.168.1.42 metric 100 \n"
            )
            .as_deref(),
            Some("enp4s0")
        );
        assert_eq!(default_route_dev(""), None);
        let fedora = "1025-8764/tcp 8766-8776/tcp 8778-65535/tcp 1025-65535/udp";
        assert!(ports_cover(fedora, 7831, "udp"));
        assert!(!ports_cover(fedora, 80, "udp"));
        assert!(!ports_cover("7821/tcp", 7821, "udp"));
        assert!(ports_cover("22/tcp 7821/udp", 7821, "udp"));
    }

    #[test]
    fn ufw_status() {
        let closed = "Status: active\nLogging: on (low)\n\
                      Default: deny (incoming), allow (outgoing), disabled (routed)\n\
                      New profiles: skip\n\n\
                      To                         Action      From\n\
                      --                         ------      ----\n\
                      22/tcp                     ALLOW IN    Anywhere\n\
                      7821/udp                   DENY IN     Anywhere\n\
                      7821/udp                   ALLOW OUT   Anywhere\n\
                      OpenSSH                    ALLOW IN    Anywhere\n";
        assert_eq!(ufw_port_open(closed, 7821), Some(false));
        for open in [
            "7821/udp                   ALLOW IN    Anywhere",
            "7821                       ALLOW       Anywhere",
            "7000:8000/udp              ALLOW IN    Anywhere",
            "53,7821/udp                ALLOW IN    Anywhere (v6)",
        ] {
            assert_eq!(
                ufw_port_open(&format!("{closed}{open}\n"), 7821),
                Some(true),
                "{open}"
            );
        }
        assert_eq!(ufw_port_open("Status: inactive\n", 7821), None);
        assert_eq!(
            ufw_port_open(
                "Status: active\nDefault: allow (incoming), allow (outgoing)\n",
                7821
            ),
            Some(true)
        );
    }
}

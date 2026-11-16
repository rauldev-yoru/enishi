//! Edits of `enishi.toml` made by the management commands (`enishi service add|remove|
//! import`, `enishi grant`). Every edit goes through `toml_edit`, so comments, order and
//! formatting of everything else are kept, and the result is validated by parsing it as a
//! [`Config`] before the caller writes it (see [`crate::config::write_atomic`]).

use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};
use toml_edit::{Array, DocumentMut, InlineTable, Item, Table, Value, value};

use crate::config::{Config, ServiceKind, check_name};

/// A service to write as `[services.<name>]`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ServiceSpec {
    pub name: String,
    pub kind: ServiceKind,
    /// argv (stdio and mcp services).
    pub command: Vec<String>,
    /// `host:port` (tcp services).
    pub target: Option<String>,
    pub allow: Vec<String>,
    pub allow_args: bool,
    pub cwd: Option<String>,
    pub env: BTreeMap<String, String>,
}

fn parse_doc(text: &str) -> Result<DocumentMut> {
    text.parse().context("config is not valid TOML")
}

fn finish(doc: &DocumentMut) -> Result<String> {
    let mut out = doc.to_string();
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out.parse::<Config>()
        .map_err(|e| anyhow::anyhow!("{e:#}"))
        .context("the edited config would be invalid")?;
    Ok(out)
}

/// The `[<key>]` table holding `[<key>.<name>]` sub-tables, created (implicit) if absent.
fn section<'a>(doc: &'a mut DocumentMut, key: &str) -> Result<&'a mut Table> {
    if doc.get(key).is_none() {
        let mut t = Table::new();
        t.set_implicit(true);
        doc.insert(key, Item::Table(t));
    }
    doc[key]
        .as_table_mut()
        .with_context(|| format!("`{key}` must be a table ([{key}.<name>] sections)"))
}

fn strings(items: &[String]) -> Array {
    items.iter().map(String::as_str).collect()
}

/// Render a spec as a fresh table.
fn service_table(spec: &ServiceSpec) -> Table {
    let mut t = Table::new();
    t.decor_mut().set_prefix("\n");
    t.insert("kind", value(spec.kind.as_str()));
    if !spec.command.is_empty() {
        t.insert("command", value(strings(&spec.command)));
    }
    if let Some(target) = &spec.target {
        t.insert("target", value(target.as_str()));
    }
    t.insert("allow", value(strings(&spec.allow)));
    if spec.allow_args {
        t.insert("allow_args", value(true));
    }
    if let Some(cwd) = &spec.cwd {
        t.insert("cwd", value(cwd.as_str()));
    }
    if !spec.env.is_empty() {
        let mut env = InlineTable::new();
        for (k, v) in &spec.env {
            env.insert(k, Value::from(v.as_str()));
        }
        t.insert("env", value(env));
    }
    t
}

/// TOML text of `[services.<name>]` alone (for `--dry-run`).
pub fn render_service(spec: &ServiceSpec) -> String {
    let mut doc = DocumentMut::new();
    let mut services = Table::new();
    services.set_implicit(true);
    let mut t = service_table(spec);
    t.decor_mut().set_prefix("");
    services.insert(&spec.name, Item::Table(t));
    doc.insert("services", Item::Table(services));
    doc.to_string()
}

/// Add `[services.<name>]`. An existing service is refused unless `force`, which replaces
/// its table (keeping the comment above it) — grants that name it stay as they are.
pub fn add_service(text: &str, spec: &ServiceSpec, force: bool) -> Result<String> {
    check_name("service", &spec.name)?;
    let mut doc = parse_doc(text)?;
    let services = section(&mut doc, "services")?;
    let mut t = service_table(spec);
    if let Some(old) = services.get(&spec.name) {
        if !force {
            bail!(
                "a service named {:?} already exists (use --force to replace it)",
                spec.name
            );
        }
        if let Some(old) = old.as_table() {
            t.decor_mut().clone_from(old.decor());
        }
    }
    services.insert(&spec.name, Item::Table(t));
    finish(&doc)
}

/// Remove `[services.<name>]` and every grant of it. Returns the new text and the peers
/// whose `[grants.<peer>] services` lost it.
pub fn remove_service(text: &str, name: &str) -> Result<(String, Vec<String>)> {
    let mut doc = parse_doc(text)?;
    let removed = doc
        .get_mut("services")
        .and_then(Item::as_table_mut)
        .and_then(|s| s.remove(name));
    if removed.is_none() {
        bail!("no service named {name:?} in this config");
    }
    let mut touched = Vec::new();
    if let Some(grants) = doc.get_mut("grants").and_then(Item::as_table_mut) {
        for (peer, g) in grants.iter_mut() {
            if let Some(arr) = g.get_mut("services").and_then(Item::as_array_mut)
                && remove_str(arr, name)
            {
                touched.push(peer.get().to_string());
            }
        }
    }
    Ok((finish(&doc)?, touched))
}

fn remove_str(arr: &mut Array, s: &str) -> bool {
    let before = arr.len();
    arr.retain(|v| v.as_str() != Some(s));
    arr.len() != before
}

/// Append `s` unless present; true if it was added.
fn push_str(arr: &mut Array, s: &str) -> bool {
    if arr.iter().any(|v| v.as_str() == Some(s)) {
        return false;
    }
    arr.push(s);
    true
}

/// The array `key` of `table`, created empty if absent.
fn array_mut<'a>(table: &'a mut Table, key: &str, what: &str) -> Result<&'a mut Array> {
    if !table.contains_key(key) {
        table.insert(key, value(Array::new()));
    }
    table[key]
        .as_array_mut()
        .with_context(|| format!("{what}: `{key}` must be an array"))
}

/// What `enishi grant` should change for one peer. `None` = leave as is.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GrantEdit {
    pub exec: Option<bool>,
    /// Transparent MCP (`mcp = true|false`), see [`crate::policy::authorize_mcp`].
    pub mcp: Option<bool>,
    /// Grant these services: added to `[grants.<peer>] services` **and** the peer added to
    /// each service's `allow` (both gates of D9 in one command).
    pub services: Vec<String>,
    /// Revoke: removed from the grant and the peer removed from the service's `allow`.
    pub revoke: Vec<String>,
}

/// Human-readable changes made by [`edit_grant`] (empty = nothing changed).
pub type Changes = Vec<String>;

pub fn edit_grant(text: &str, peer: &str, edit: &GrantEdit) -> Result<(String, Changes)> {
    let mut doc = parse_doc(text)?;
    let known_peer = doc
        .get("peers")
        .and_then(Item::as_table)
        .is_some_and(|p| p.contains_key(peer));
    if !known_peer {
        bail!("no peer named {peer:?} in this config (pair it first: enishi pair)");
    }
    for s in edit.services.iter().chain(&edit.revoke) {
        let exists = doc
            .get("services")
            .and_then(Item::as_table)
            .is_some_and(|t| t.contains_key(s));
        if !exists {
            bail!("no service named {s:?} in this config (see enishi service list)");
        }
    }
    if let Some(s) = edit.services.iter().find(|s| edit.revoke.contains(s)) {
        bail!("service {s:?} is both granted and revoked");
    }
    let mut changes = Vec::new();
    // Services' allow lists.
    {
        let services = section(&mut doc, "services")?;
        for s in &edit.services {
            let t = services[s.as_str()]
                .as_table_mut()
                .with_context(|| format!("[services.{s}] must be a table"))?;
            if push_str(array_mut(t, "allow", &format!("[services.{s}]"))?, peer) {
                changes.push(format!("[services.{s}] allow += {peer}"));
            }
        }
        for s in &edit.revoke {
            let t = services[s.as_str()]
                .as_table_mut()
                .with_context(|| format!("[services.{s}] must be a table"))?;
            if let Some(arr) = t.get_mut("allow").and_then(Item::as_array_mut)
                && remove_str(arr, peer)
            {
                changes.push(format!("[services.{s}] allow -= {peer}"));
            }
        }
    }
    // The grant itself.
    let grants = section(&mut doc, "grants")?;
    if !grants.contains_key(peer) {
        let mut t = Table::new();
        t.decor_mut().set_prefix("\n");
        grants.insert(peer, Item::Table(t));
    }
    let g = grants[peer]
        .as_table_mut()
        .with_context(|| format!("[grants.{peer}] must be a table"))?;
    for (key, want) in [("exec", edit.exec), ("mcp", edit.mcp)] {
        let Some(want) = want else { continue };
        let current = g.get(key).and_then(Item::as_bool);
        if current != Some(want) {
            if g.get(key).is_some_and(|v| v.as_bool().is_none()) {
                changes.push(format!("[grants.{peer}] {key}: rules replaced by {want}"));
            } else {
                changes.push(format!("[grants.{peer}] {key} = {want}"));
            }
            g.insert(key, value(want));
        }
    }
    if !edit.services.is_empty() || g.contains_key("services") {
        let what = format!("[grants.{peer}]");
        let arr = array_mut(g, "services", &what)?;
        for s in &edit.services {
            if push_str(arr, s) {
                changes.push(format!("{what} services += {s}"));
            }
        }
        for s in &edit.revoke {
            if remove_str(arr, s) {
                changes.push(format!("{what} services -= {s}"));
            }
        }
    }
    Ok((finish(&doc)?, changes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::ExecPolicy;

    fn id(n: u8) -> String {
        iroh::SecretKey::from_bytes(&[n; 32]).public().to_string()
    }

    fn base() -> String {
        format!(
            "# top comment\n[device]\nname = \"pc\" # me\n\n# the server\n[peers.srv]\nid = \"{}\"\n\n\
             [peers.lap]\nid = \"{}\"\n\n# keep me\n[services.old]\ncommand = [\"cat\"] # cat!\n\
             allow = [\"srv\"]\n\n[grants.srv]\nexec = true # yes\nservices = [\"old\"]\n",
            id(1),
            id(2)
        )
    }

    fn spec(name: &str) -> ServiceSpec {
        ServiceSpec {
            name: name.into(),
            kind: ServiceKind::Mcp,
            command: vec!["npx".into(), "-y".into(), "a b".into()],
            allow: vec!["lap".into()],
            env: [("K".to_string(), "v".to_string())].into(),
            cwd: Some("~".into()),
            ..Default::default()
        }
    }

    fn keeps_comments(out: &str) {
        for c in [
            "# top comment",
            "# me",
            "# the server",
            "# keep me",
            "# cat!",
            "# yes",
        ] {
            assert!(out.contains(c), "lost {c:?}:\n{out}");
        }
    }

    #[test]
    fn add_service_keeps_comments_and_refuses_duplicates() {
        let out = add_service(&base(), &spec("fs"), false).unwrap();
        keeps_comments(&out);
        let block = "[services.fs]\nkind = \"mcp\"\ncommand = [\"npx\", \"-y\", \"a b\"]\n\
                     allow = [\"lap\"]\ncwd = \"~\"\nenv = { K = \"v\" }\n\n";
        // Placed after the other services, everything else byte-identical.
        assert!(
            out.contains(&format!("allow = [\"srv\"]\n\n{block}[grants.srv]")),
            "{out}"
        );
        assert_eq!(out.replace(block, ""), base());
        let cfg: Config = out.parse().unwrap();
        let fs = &cfg.services["fs"];
        assert_eq!(fs.kind, ServiceKind::Mcp);
        assert_eq!(fs.command, vec!["npx", "-y", "a b"]);
        assert_eq!(fs.allow, vec!["lap"]);
        assert_eq!(fs.env["K"], "v");
        assert_eq!(fs.cwd.as_deref(), Some("~"));
        assert!(!fs.allow_args);

        let e = add_service(&out, &spec("fs"), false).unwrap_err();
        assert!(e.to_string().contains("--force"), "{e}");
        let mut s2 = spec("old");
        s2.kind = ServiceKind::Stdio;
        s2.command = vec!["wc".into()];
        s2.allow_args = true;
        let out2 = add_service(&out, &s2, true).unwrap();
        assert!(
            out2.contains("# top comment") && out2.contains("# yes"),
            "{out2}"
        );
        assert!(out2.contains("# keep me\n[services.old]"), "{out2}");
        let cfg2: Config = out2.parse().unwrap();
        assert_eq!(cfg2.services["old"].command, vec!["wc"]);
        assert!(cfg2.services["old"].allow_args);

        // Unknown peer in allow → the result is refused.
        let mut bad = spec("x");
        bad.allow = vec!["nobody".into()];
        let e = add_service(&base(), &bad, false).unwrap_err();
        assert!(format!("{e:#}").contains("unknown peer"), "{e:#}");
        assert!(add_service(&base(), &spec("a/b"), false).is_err());
        // No [services] yet.
        let minimal = format!("[device]\nname = \"x\"\n[peers.lap]\nid = \"{}\"\n", id(2));
        let out = add_service(&minimal, &spec("fs"), false).unwrap();
        assert!(out.parse::<Config>().unwrap().services.contains_key("fs"));
    }

    #[test]
    fn remove_service_drops_grants() {
        let (out, touched) = remove_service(&base(), "old").unwrap();
        assert_eq!(touched, vec!["srv"]);
        let cfg: Config = out.parse().unwrap();
        assert!(cfg.services.is_empty());
        assert!(cfg.grant("srv").services.is_empty());
        assert_eq!(cfg.grant("srv").exec, ExecPolicy::Flag(true));
        assert!(
            out.contains("# top comment") && out.contains("# yes"),
            "{out}"
        );
        assert!(remove_service(&base(), "nope").is_err());
    }

    #[test]
    fn grant_edits_both_gates() {
        let text = add_service(&base(), &spec("fs"), false).unwrap();
        let edit = GrantEdit {
            exec: Some(false),
            mcp: Some(true),
            services: vec!["old".into(), "fs".into()],
            revoke: vec![],
        };
        let (out, changes) = edit_grant(&text, "lap", &edit).unwrap();
        keeps_comments(&out);
        let cfg: Config = out.parse().unwrap();
        let g = cfg.grant("lap");
        assert_eq!(g.services, vec!["old", "fs"]);
        assert_eq!(g.exec, ExecPolicy::Flag(false));
        assert_eq!(cfg.services["old"].allow, vec!["srv", "lap"]);
        assert_eq!(cfg.services["fs"].allow, vec!["lap"], "not duplicated");
        assert!(changes.contains(&"[services.old] allow += lap".to_string()));
        assert!(!changes.iter().any(|c| c.contains("[services.fs] allow")));
        assert!(crate::policy::authorize_service(&cfg, "lap", "old", &[]).is_ok());
        assert_eq!(g.mcp, ExecPolicy::Flag(true));
        assert!(
            changes.contains(&"[grants.lap] mcp = true".to_string()),
            "{changes:?}"
        );

        // Idempotent.
        let (again, none) = edit_grant(&out, "lap", &edit).unwrap();
        assert_eq!(again, out);
        assert!(none.is_empty(), "{none:?}");

        // Revoke removes both gates; exec of another peer untouched; comment kept.
        let revoke = GrantEdit {
            revoke: vec!["old".into()],
            ..Default::default()
        };
        let (out2, ch) = edit_grant(&out, "srv", &revoke).unwrap();
        let cfg2: Config = out2.parse().unwrap();
        assert!(cfg2.grant("srv").services.is_empty());
        assert_eq!(cfg2.services["old"].allow, vec!["lap"]);
        assert_eq!(cfg2.grant("srv").exec, ExecPolicy::Flag(true));
        assert!(out2.contains("exec = true # yes"), "{out2}");
        assert_eq!(ch.len(), 2, "{ch:?}");

        // Errors.
        assert!(edit_grant(&text, "nobody", &edit).is_err());
        let unknown = GrantEdit {
            services: vec!["nope".into()],
            ..Default::default()
        };
        assert!(edit_grant(&text, "lap", &unknown).is_err());
        let both = GrantEdit {
            services: vec!["fs".into()],
            revoke: vec!["fs".into()],
            ..Default::default()
        };
        assert!(edit_grant(&text, "lap", &both).is_err());
    }

    #[test]
    fn render_service_snippet() {
        let s = render_service(&spec("fs"));
        assert!(s.starts_with("[services.fs]\nkind = \"mcp\"\n"), "{s}");
        assert!(s.contains("env = { K = \"v\" }"), "{s}");
    }
}

//! `enishi service import <format> <file>`: turn another tool's server list into
//! `[services.<name>]` entries. The generic layer only defines the [`Importer`] trait and a
//! registry; each format lives with the layer that understands it (the `mcp` format —
//! Claude Desktop / Cursor / VS Code `mcpServers` JSON — is in `enishi-mcp`) and is
//! registered by the CLI, so adding a format never touches this module or the others.

use std::collections::BTreeMap;

use anyhow::{Result, bail};

use crate::config::ServiceKind;

/// One service found in an imported file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportedService {
    /// Name as written in the source (may need [`sanitize_name`]).
    pub name: String,
    pub kind: ServiceKind,
    pub command: Vec<String>,
    pub cwd: Option<String>,
    pub env: BTreeMap<String, String>,
}

/// What a file contained: services to import and entries skipped with the reason.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImportSet {
    pub services: Vec<ImportedService>,
    pub skipped: Vec<(String, String)>,
}

/// A source format. Implementations parse only; validation, naming, `allow` and writing
/// are done by the caller for every format alike.
pub trait Importer: Send + Sync {
    /// Name used on the command line (`enishi service import <format>`).
    fn format(&self) -> &'static str;
    /// One line for `--help`/errors, e.g. which tools write this format.
    fn description(&self) -> &'static str;
    fn parse(&self, text: &str) -> Result<ImportSet>;
}

/// Registered formats, in registration order.
#[derive(Default)]
pub struct Importers(Vec<Box<dyn Importer>>);

impl Importers {
    pub fn new() -> Importers {
        Importers::default()
    }

    /// Register a format (a later one with the same name is ignored).
    pub fn with(mut self, importer: impl Importer + 'static) -> Importers {
        if self.get(importer.format()).is_none() {
            self.0.push(Box::new(importer));
        }
        self
    }

    pub fn get(&self, format: &str) -> Option<&dyn Importer> {
        self.0
            .iter()
            .find(|i| i.format() == format)
            .map(|b| b.as_ref())
    }

    /// `(format, description)` of every registered importer.
    pub fn formats(&self) -> Vec<(&'static str, &'static str)> {
        self.0
            .iter()
            .map(|i| (i.format(), i.description()))
            .collect()
    }

    /// The importer for `format`, or an error listing the available ones.
    pub fn lookup(&self, format: &str) -> Result<&dyn Importer> {
        match self.get(format) {
            Some(i) => Ok(i),
            None => {
                let list = self
                    .formats()
                    .iter()
                    .map(|(f, d)| format!("  {f:<8} {d}"))
                    .collect::<Vec<_>>()
                    .join("\n");
                bail!("unknown import format {format:?}; available formats:\n{list}")
            }
        }
    }
}

/// Map a foreign server name onto enishi's name rules (letters, digits, `-`, `_`, `.`):
/// anything else becomes `-`, runs collapse, and the ends are trimmed.
pub fn sanitize_name(name: &str) -> String {
    let mut out = String::new();
    for c in name.chars() {
        let ok = c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.');
        if ok {
            out.push(c);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_matches('-').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fake(&'static str);
    impl Importer for Fake {
        fn format(&self) -> &'static str {
            self.0
        }
        fn description(&self) -> &'static str {
            "fake format"
        }
        fn parse(&self, _: &str) -> Result<ImportSet> {
            Ok(ImportSet::default())
        }
    }

    #[test]
    fn registry_lookup_lists_formats() {
        let r = Importers::new()
            .with(Fake("a"))
            .with(Fake("b"))
            .with(Fake("a"));
        assert_eq!(r.formats().len(), 2);
        assert!(r.lookup("b").is_ok());
        let e = r.lookup("yaml").err().unwrap().to_string();
        assert!(e.contains("unknown import format \"yaml\""), "{e}");
        assert!(e.contains("  a        fake format\n  b"), "{e}");
    }

    #[test]
    fn sanitizes_names() {
        assert_eq!(sanitize_name("filesystem"), "filesystem");
        assert_eq!(sanitize_name("My Server (v2)"), "My-Server-v2");
        assert_eq!(sanitize_name("@scope/pkg"), "scope-pkg");
        assert_eq!(sanitize_name("a.b_c-d"), "a.b_c-d");
        assert_eq!(sanitize_name("日本"), "");
    }
}

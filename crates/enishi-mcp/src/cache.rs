//! On-disk cache of a service's handshake and list results, one JSON file per device +
//! service (+ extra args): `{"initialize": {...}, "tools/list": {...}, ...}`.
//!
//! Location: `$ENISHI_CACHE_DIR`, else `%LOCALAPPDATA%\enishi\cache` (Windows) or
//! `$XDG_CACHE_HOME/enishi` / `~/.cache/enishi` (elsewhere); file
//! `mcp/<device>/<service>[.<args hash>].json`, or `mcp/<device>/cmd~<argv hash>.json`
//! for transparent mode (`enishi mcp -d dev -- cmd…`; `~` never appears in service names).

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

pub struct Cache {
    path: Option<PathBuf>,
    data: Map<String, Value>,
}

/// Base cache directory for this OS (see module docs).
pub fn cache_dir() -> Option<PathBuf> {
    let var = |k: &str| {
        std::env::var_os(k)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    };
    if let Some(d) = var("ENISHI_CACHE_DIR") {
        return Some(d);
    }
    if cfg!(windows) {
        var("LOCALAPPDATA").map(|d| d.join("enishi").join("cache"))
    } else {
        var("XDG_CACHE_HOME")
            .or_else(|| var("HOME").map(|h| h.join(".cache")))
            .map(|d| d.join("enishi"))
    }
}

/// Keep a name usable as one path component.
fn component(s: &str) -> String {
    let mut out: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if out.is_empty() || out.starts_with('.') {
        out.insert(0, '_');
    }
    out
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// Cache file for `device`/`service` started with `args`, under `base`.
pub fn cache_file(base: &Path, device: &str, service: &str, args: &[String]) -> PathBuf {
    let mut name = component(service);
    if !args.is_empty() {
        // Different args may expose different tools.
        name.push_str(&format!(".{:016x}", fnv1a(args.join("\0").as_bytes())));
    }
    name.push_str(".json");
    base.join("mcp").join(component(device)).join(name)
}

/// Cache file for the transparent-mode command `argv` on `device` (cwd and env are part
/// of the key too: they can change what the server exposes).
pub fn command_cache_file(
    base: &Path,
    device: &str,
    argv: &[String],
    cwd: Option<&str>,
    env: &[(String, String)],
) -> PathBuf {
    let mut key = argv.join("\0");
    if let Some(c) = cwd {
        key.push_str(&format!("\0cwd={c}"));
    }
    for (k, v) in env {
        key.push_str(&format!("\0env={k}={v}"));
    }
    let name = format!("cmd~{:016x}.json", fnv1a(key.as_bytes()));
    base.join("mcp").join(component(device)).join(name)
}

impl Cache {
    /// Load the cache file (missing or unreadable = empty).
    pub fn open(path: Option<PathBuf>) -> Cache {
        let data = path
            .as_ref()
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
            .and_then(|v| match v {
                Value::Object(m) => Some(m),
                _ => None,
            })
            .unwrap_or_default();
        Cache { path, data }
    }

    /// In-memory only (tests, or no cache directory on this system).
    pub fn memory() -> Cache {
        Cache::open(None)
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        self.data.get(key)
    }

    /// Store and persist (atomically: temp file + rename). Write errors are reported once
    /// per call on stderr and otherwise ignored: the cache is an optimisation.
    pub fn put(&mut self, key: &str, value: Value) {
        if self.data.get(key) == Some(&value) {
            return;
        }
        self.data.insert(key.to_string(), value);
        let Some(path) = &self.path else { return };
        let write = || -> std::io::Result<()> {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let mut tmp = path.as_os_str().to_owned();
            tmp.push(format!(".{}.tmp", std::process::id()));
            let bytes = serde_json::to_vec_pretty(&self.data).map_err(std::io::Error::other)?;
            std::fs::write(&tmp, bytes)?;
            std::fs::rename(&tmp, path)
        };
        if let Err(e) = write() {
            eprintln!("enishi mcp: cannot write cache {}: {e}", path.display());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn file_names() {
        let b = Path::new("/c");
        assert_eq!(
            cache_file(b, "pc", "fs", &[]),
            Path::new("/c/mcp/pc/fs.json")
        );
        let a = cache_file(b, "pc", "fs", &["x".into()]);
        assert_ne!(a, cache_file(b, "pc", "fs", &["y".into()]));
        assert_eq!(a.parent(), Some(Path::new("/c/mcp/pc")));
        assert!(a.file_name().unwrap().to_string_lossy().starts_with("fs."));
        assert_eq!(
            cache_file(b, "..", "a/b", &[]),
            Path::new("/c/mcp/_../a_b.json")
        );
    }

    #[test]
    fn command_file_names() {
        let b = Path::new("/c");
        let argv = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let f = command_cache_file(b, "pc", &argv(&["npx", "-y", "x"]), None, &[]);
        assert!(f.starts_with("/c/mcp/pc"));
        let name = f.file_name().unwrap().to_str().unwrap();
        assert!(
            name.starts_with("cmd~") && name.ends_with(".json"),
            "{name}"
        );
        assert_eq!(
            f,
            command_cache_file(b, "pc", &argv(&["npx", "-y", "x"]), None, &[])
        );
        assert_ne!(
            f,
            command_cache_file(b, "pc", &argv(&["npx", "-y", "y"]), None, &[])
        );
        assert_ne!(
            f,
            command_cache_file(b, "pc", &argv(&["npx -y", "x"]), None, &[])
        );
        assert_ne!(
            f,
            command_cache_file(b, "pc", &argv(&["npx", "-y", "x"]), Some("/"), &[])
        );
        assert_ne!(
            f,
            command_cache_file(b, "lap", &argv(&["npx", "-y", "x"]), None, &[])
        );
        let env = [("K".to_string(), "v".to_string())];
        assert_ne!(
            f,
            command_cache_file(b, "pc", &argv(&["npx", "-y", "x"]), None, &env)
        );
        assert_ne!(f, cache_file(b, "pc", "cmd", &argv(&["npx", "-y", "x"])));
    }

    #[test]
    fn persists_and_reloads() {
        let dir = std::env::temp_dir().join(format!("enishi-mcp-cache-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let f = cache_file(&dir, "pc", "fs", &[]);
        let mut c = Cache::open(Some(f.clone()));
        assert!(c.get("initialize").is_none());
        c.put("initialize", json!({"serverInfo": {"name": "x"}}));
        c.put("tools/list", json!({"tools": []}));
        let c2 = Cache::open(Some(f.clone()));
        assert_eq!(c2.get("tools/list"), Some(&json!({"tools": []})));
        assert_eq!(c2.get("initialize").unwrap()["serverInfo"]["name"], "x");
        std::fs::write(&f, "not json").unwrap();
        assert!(Cache::open(Some(f)).get("initialize").is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }
}

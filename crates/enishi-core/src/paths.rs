//! Where enishi keeps its files, per OS (design §4, §7).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

pub const CONFIG_FILE: &str = "enishi.toml";
pub const IDENTITY_FILE: &str = "identity.key";
#[cfg(unix)]
const SOCKET_FILE: &str = "enishi.sock";

/// Resolved locations for one enishi instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    /// `enishi.toml`.
    pub config_file: PathBuf,
    /// Directory holding the config and `identity.key`.
    pub dir: PathBuf,
    /// The config location was overridden (`--config` / `ENISHI_CONFIG`) to a non-default
    /// file. Such an instance gets its own IPC endpoint, so several daemons can run on one
    /// host (tests, multiple identities).
    pub custom: bool,
}

impl Paths {
    /// `config` is the `--config` / `ENISHI_CONFIG` override, if any.
    pub fn resolve(config: Option<&Path>) -> Result<Paths> {
        let default_file = default_config_dir().map(|d| d.join(CONFIG_FILE));
        match config {
            Some(p) => {
                let file = std::path::absolute(p)
                    .with_context(|| format!("invalid config path {}", p.display()))?;
                let Some(dir) = file.parent().map(Path::to_path_buf) else {
                    bail!("invalid config path {}", file.display());
                };
                let custom = default_file.as_ref().ok() != Some(&file);
                Ok(Paths {
                    config_file: file,
                    dir,
                    custom,
                })
            }
            None => {
                let file = default_file?;
                let dir = file
                    .parent()
                    .expect("config file has a parent")
                    .to_path_buf();
                Ok(Paths {
                    config_file: file,
                    dir,
                    custom: false,
                })
            }
        }
    }

    pub fn identity_key(&self) -> PathBuf {
        self.dir.join(IDENTITY_FILE)
    }

    /// Unix socket of the local daemon: `$XDG_RUNTIME_DIR/enishi.sock`, falling back to the
    /// config dir. Custom instances always use their config dir.
    #[cfg(unix)]
    pub fn ipc_socket(&self) -> PathBuf {
        if !self.custom
            && let Some(rt) = std::env::var_os("XDG_RUNTIME_DIR").filter(|v| !v.is_empty())
        {
            return PathBuf::from(rt).join(SOCKET_FILE);
        }
        self.dir.join(SOCKET_FILE)
    }

    /// Named pipe of the local daemon: `\\.\pipe\enishi-<user>`; custom instances get a
    /// suffix derived from their config path.
    pub fn ipc_pipe_name(&self) -> String {
        let user = std::env::var("USERNAME")
            .or_else(|_| std::env::var("USER"))
            .unwrap_or_else(|_| "user".into());
        if self.custom {
            let h = fnv1a(self.config_file.to_string_lossy().as_bytes());
            format!(r"\\.\pipe\enishi-{user}-{h:016x}")
        } else {
            format!(r"\\.\pipe\enishi-{user}")
        }
    }

    /// Human-readable IPC endpoint for messages.
    pub fn ipc_display(&self) -> String {
        #[cfg(unix)]
        {
            self.ipc_socket().display().to_string()
        }
        #[cfg(not(unix))]
        {
            self.ipc_pipe_name()
        }
    }
}

/// Linux/macOS: `$XDG_CONFIG_HOME/enishi` or `~/.config/enishi`. Windows: `%APPDATA%\enishi`.
pub fn default_config_dir() -> Result<PathBuf> {
    #[cfg(windows)]
    {
        let appdata = std::env::var_os("APPDATA").context("APPDATA is not set")?;
        Ok(PathBuf::from(appdata).join("enishi"))
    }
    #[cfg(not(windows))]
    {
        if let Some(x) = std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
            return Ok(PathBuf::from(x).join("enishi"));
        }
        let home = std::env::var_os("HOME").context("HOME is not set")?;
        Ok(PathBuf::from(home).join(".config").join("enishi"))
    }
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn override_sets_dir_and_custom() {
        let base = std::env::temp_dir().join("enishi-paths-test");
        let p = Paths::resolve(Some(&base.join("a").join(CONFIG_FILE))).unwrap();
        assert_eq!(p.dir, base.join("a"));
        assert!(p.custom);
        assert_eq!(p.identity_key(), base.join("a").join(IDENTITY_FILE));
        let q = Paths::resolve(Some(&base.join("b").join(CONFIG_FILE))).unwrap();
        assert_ne!(p.ipc_pipe_name(), q.ipc_pipe_name());
        #[cfg(unix)]
        assert_eq!(p.ipc_socket(), base.join("a").join(SOCKET_FILE));
    }

    #[test]
    fn relative_override_is_absolutized() {
        let p = Paths::resolve(Some(Path::new(CONFIG_FILE))).unwrap();
        assert!(p.config_file.is_absolute());
        assert!(p.dir.is_absolute());
    }

    #[test]
    fn default_is_not_custom() {
        if let Ok(p) = Paths::resolve(None) {
            assert!(!p.custom);
            assert!(p.dir.ends_with("enishi"));
            let same = Paths::resolve(Some(&p.config_file)).unwrap();
            assert!(!same.custom, "explicit default path is not custom");
        }
    }
}

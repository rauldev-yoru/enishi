//! Device identity: an Ed25519 key (the iroh `EndpointId` is its public half), stored as 32
//! raw bytes in `identity.key` next to the config, readable by the user only.

use std::{io::Write, path::Path};

use anyhow::{Context, Result};
use iroh::SecretKey;

/// Load the key at `path`, creating it (and its directory) on first use.
pub fn load_or_create(path: &Path) -> Result<SecretKey> {
    match read(path) {
        Ok(Some(k)) => return Ok(k),
        Ok(None) => {}
        Err(e) => return Err(e),
    }
    if let Some(dir) = path.parent() {
        create_private_dir(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let key = SecretKey::generate();
    match create_private_file(path) {
        Ok(mut f) => {
            f.write_all(&key.to_bytes())?;
            f.sync_all()?;
            Ok(key)
        }
        // Another process created it first: use theirs.
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            read(path)?.context("identity key vanished")
        }
        Err(e) => Err(e).with_context(|| format!("creating {}", path.display())),
    }
}

fn read(path: &Path) -> Result<Option<SecretKey>> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    check_permissions(path)?;
    let arr: [u8; 32] = bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("{} is not a valid identity key", path.display()))?;
    Ok(Some(SecretKey::from_bytes(&arr)))
}

#[cfg(unix)]
fn check_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(path)?.permissions().mode();
    if mode & 0o077 != 0 {
        anyhow::bail!(
            "{} is accessible by other users (mode {:o}); run: chmod 600 {}",
            path.display(),
            mode & 0o777,
            path.display()
        );
    }
    Ok(())
}

/// `%APPDATA%` is already private to the user; a tighter ACL is set by the installer.
#[cfg(not(unix))]
fn check_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn create_private_file(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

#[cfg(not(unix))]
fn create_private_file(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
}

#[cfg(unix)]
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
}

#[cfg(not(unix))]
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("enishi-id-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn creates_once_then_reloads() {
        let dir = scratch("reload");
        let path = dir.join("sub").join("identity.key");
        let a = load_or_create(&path).unwrap();
        let b = load_or_create(&path).unwrap();
        assert_eq!(a.public(), b.public());
        assert_eq!(std::fs::read(&path).unwrap().len(), 32);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
            let dmode = std::fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(dmode & 0o777, 0o700);
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn rejects_bad_key_file() {
        let dir = scratch("bad");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("identity.key");
        create_private_file(&path)
            .unwrap()
            .write_all(b"short")
            .unwrap();
        assert!(load_or_create(&path).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn rejects_world_readable_key() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("perm");
        let path = dir.join("identity.key");
        load_or_create(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let e = load_or_create(&path).unwrap_err();
        assert!(format!("{e:#}").contains("chmod 600"));
        std::fs::remove_dir_all(dir).unwrap();
    }
}

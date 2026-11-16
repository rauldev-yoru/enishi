//! Release downloads for `enishi update`: the same choices as `install.sh`/`install.ps1`
//! (newest release via `releases?per_page=1`, pre-releases included, or a pinned tag;
//! the asset for this OS; its line of `SHA256SUMS` must match).

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

pub const REPO: &str = "rauldev-yoru/enishi";

/// The release asset for this platform.
pub fn asset_name() -> Result<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => Ok("enishi-x86_64-unknown-linux-gnu"),
        ("windows", "x86_64") => Ok("enishi-x86_64-pc-windows-msvc.exe"),
        (os, arch) => bail!("no enishi release asset for {os}/{arch}; use --from-file"),
    }
}

/// Releases list endpoint (newest first). `ENISHI_RELEASES_API` overrides it (mirrors,
/// tests).
fn api_url() -> String {
    std::env::var("ENISHI_RELEASES_API")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| format!("https://api.github.com/repos/{REPO}/releases?per_page=1"))
}

/// Base of the download URLs (`<base>/<tag>/<asset>`). `ENISHI_RELEASES_URL` overrides it.
fn download_base() -> String {
    std::env::var("ENISHI_RELEASES_URL")
        .ok()
        .filter(|v| !v.is_empty())
        .map(|v| v.trim_end_matches('/').to_string())
        .unwrap_or_else(|| format!("https://github.com/{REPO}/releases/download"))
}

fn client() -> Result<reqwest::Client> {
    // reqwest is built without a bundled crypto provider (iroh brings ring).
    let _ = rustls::crypto::ring::default_provider().install_default();
    reqwest::Client::builder()
        .user_agent(concat!("enishi/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(std::time::Duration::from_secs(15))
        .timeout(std::time::Duration::from_secs(300))
        .build()
        .context("building the HTTP client")
}

async fn get(client: &reqwest::Client, url: &str) -> Result<Vec<u8>> {
    let resp = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    let status = resp.status();
    if !status.is_success() {
        bail!("GET {url}: HTTP {status}");
    }
    Ok(resp
        .bytes()
        .await
        .with_context(|| format!("GET {url}"))?
        .to_vec())
}

/// `v`-prefixed tag of the newest release (pre-releases included, like the installers).
pub async fn latest_tag() -> Result<String> {
    let body = get(&client()?, &api_url()).await?;
    parse_latest(&body)
}

pub fn parse_latest(body: &[u8]) -> Result<String> {
    let v: serde_json::Value = serde_json::from_slice(body).context("invalid releases JSON")?;
    v.get(0)
        .and_then(|r| r.get("tag_name"))
        .and_then(|t| t.as_str())
        .map(str::to_string)
        .context("no release found")
}

/// `0.1.1` / `v0.1.1` → `v0.1.1`.
pub fn tag_of(version: &str) -> String {
    if version.starts_with('v') {
        version.to_string()
    } else {
        format!("v{version}")
    }
}

/// SemVer precedence of two versions (`v` prefix and build metadata ignored); `None` when
/// one does not parse.
pub fn compare(a: &str, b: &str) -> Option<std::cmp::Ordering> {
    fn parse(v: &str) -> Option<(Vec<u64>, Option<String>)> {
        let v = v.trim().trim_start_matches('v');
        let v = v.split('+').next()?;
        let (core, pre) = match v.split_once('-') {
            Some((c, p)) => (c, Some(p.to_string())),
            None => (v, None),
        };
        let nums = core
            .split('.')
            .map(|n| n.parse().ok())
            .collect::<Option<Vec<u64>>>()?;
        (nums.len() == 3).then_some((nums, pre))
    }
    fn cmp_pre(a: &str, b: &str) -> std::cmp::Ordering {
        let (mut x, mut y) = (a.split('.'), b.split('.'));
        loop {
            match (x.next(), y.next()) {
                (None, None) => return std::cmp::Ordering::Equal,
                (None, Some(_)) => return std::cmp::Ordering::Less,
                (Some(_), None) => return std::cmp::Ordering::Greater,
                (Some(p), Some(q)) => {
                    let o = match (p.parse::<u64>(), q.parse::<u64>()) {
                        (Ok(m), Ok(n)) => m.cmp(&n),
                        (Ok(_), Err(_)) => std::cmp::Ordering::Less,
                        (Err(_), Ok(_)) => std::cmp::Ordering::Greater,
                        (Err(_), Err(_)) => p.cmp(q),
                    };
                    if o.is_ne() {
                        return o;
                    }
                }
            }
        }
    }
    let (an, ap) = parse(a)?;
    let (bn, bp) = parse(b)?;
    Some(an.cmp(&bn).then_with(|| match (ap, bp) {
        (None, None) => std::cmp::Ordering::Equal,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (Some(_), None) => std::cmp::Ordering::Less,
        (Some(p), Some(q)) => cmp_pre(&p, &q),
    }))
}

/// A downloaded, checksum-verified release binary.
pub struct Download {
    pub url: String,
    pub bytes: Vec<u8>,
}

/// Download this platform's asset of `tag` and check it against the release's SHA256SUMS.
pub async fn download(tag: &str) -> Result<Download> {
    let asset = asset_name()?;
    let client = client()?;
    let base = format!("{}/{tag}", download_base());
    let url = format!("{base}/{asset}");
    let bytes = get(&client, &url).await?;
    let sums = get(&client, &format!("{base}/SHA256SUMS")).await?;
    let want = expected_sum(&String::from_utf8_lossy(&sums), asset)
        .with_context(|| format!("{asset} is not listed in {base}/SHA256SUMS"))?;
    let got = sha256_hex(&bytes);
    if got != want {
        bail!("checksum mismatch for {asset}: SHA256SUMS says {want}, downloaded {got}");
    }
    Ok(Download { url, bytes })
}

/// The hash `sha256sum` printed for `asset` (`<hex>  <name>` or `<hex> *<name>`).
pub fn expected_sum(sums: &str, asset: &str) -> Option<String> {
    sums.lines().find_map(|l| {
        let (hex, name) = l.trim().split_once(char::is_whitespace)?;
        let name = name.trim_start().trim_start_matches('*');
        (name == asset && hex.len() == 64).then(|| hex.to_ascii_lowercase())
    })
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_precedence() {
        use std::cmp::Ordering::*;
        assert_eq!(compare("0.1.1", "v0.1.0"), Some(Greater));
        assert_eq!(compare("v0.1.0", "0.1.0"), Some(Equal));
        assert_eq!(compare("0.1.1-a", "0.1.1"), Some(Less));
        assert_eq!(compare("0.2.0-rc.2", "0.2.0-rc.10"), Some(Less));
        assert_eq!(compare("0.10.0", "0.9.9"), Some(Greater));
        assert_eq!(compare("garbage", "0.1.0"), None);
    }

    #[test]
    fn sums_and_tags() {
        let sums = "3f0a  other\nABCDEF0123456789abcdef0123456789abcdef0123456789abcdef0123456789  enishi-x86_64-unknown-linux-gnu\n\
                    1111111111111111111111111111111111111111111111111111111111111111 *enishi-x86_64-pc-windows-msvc.exe\n";
        assert_eq!(
            expected_sum(sums, "enishi-x86_64-unknown-linux-gnu").as_deref(),
            Some("abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789")
        );
        assert!(expected_sum(sums, "enishi-x86_64-pc-windows-msvc.exe").is_some());
        assert!(expected_sum(sums, "other").is_none(), "short hash ignored");
        assert!(expected_sum(sums, "enishi").is_none());
        assert_eq!(tag_of("0.1.1"), "v0.1.1");
        assert_eq!(tag_of("v0.2.0-rc.1"), "v0.2.0-rc.1");
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let body = br#"[{"tag_name":"v0.1.1","prerelease":false},{"tag_name":"v0.1.0"}]"#;
        assert_eq!(parse_latest(body).unwrap(), "v0.1.1");
        assert!(parse_latest(b"[]").is_err());
        assert!(asset_name().is_ok() || !cfg!(target_arch = "x86_64"));
    }
}

//! Audit log (design §6): one JSON line per exec session in `<config dir>/audit.jsonl`,
//! on both ends (`dir` = `in` for sessions a peer ran here, `out` for ours on a peer).

use std::{
    io::Write,
    path::PathBuf,
    sync::Mutex,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use enishi_proto::session::ExecRequest;
use serde::Serialize;
use tracing::warn;

pub const AUDIT_FILE: &str = "audit.jsonl";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    /// A peer ran a command on this device.
    In,
    /// This device ran a command on a peer.
    Out,
}

#[derive(Debug, Serialize)]
pub struct Entry<'a> {
    /// UTC, RFC 3339 with milliseconds.
    pub ts: String,
    pub dir: Direction,
    pub peer: &'a str,
    pub cmd: &'a [String],
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shell: Option<&'a str>,
    pub raw: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<&'a str>,
    /// Only the variable names: values may be secrets.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub env: Vec<&'a str>,
    /// Remote exit code, `None` when the session ended without one.
    pub exit: Option<i32>,
    /// Why there is no exit code (denied, timeout, link lost…).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<&'a str>,
    pub duration_ms: u64,
}

impl<'a> Entry<'a> {
    pub fn exec(dir: Direction, peer: &'a str, req: &'a ExecRequest) -> Entry<'a> {
        Entry {
            ts: rfc3339(SystemTime::now()),
            dir,
            peer,
            cmd: &req.command,
            shell: req.shell.as_deref(),
            raw: req.raw,
            cwd: req.cwd.as_deref(),
            env: req.env.iter().map(|(k, _)| k.as_str()).collect(),
            exit: None,
            error: None,
            duration_ms: 0,
        }
    }

    pub fn finish(mut self, exit: Option<i32>, error: Option<&'a str>, took: Duration) -> Self {
        self.exit = exit;
        self.error = error;
        self.duration_ms = took.as_millis() as u64;
        self
    }
}

pub struct Audit {
    path: PathBuf,
    lock: Mutex<()>,
}

impl Audit {
    pub fn new(path: PathBuf) -> Audit {
        Audit {
            path,
            lock: Mutex::new(()),
        }
    }

    /// Append one line; failures are logged, never fatal for the session.
    pub fn record(&self, entry: &Entry<'_>) {
        let line = match serde_json::to_string(entry) {
            Ok(mut l) => {
                l.push('\n');
                l
            }
            Err(e) => return warn!("audit: {e}"),
        };
        let _g = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let res = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .and_then(|mut f| f.write_all(line.as_bytes()));
        if let Err(e) = res {
            warn!("audit {}: {e}", self.path.display());
        }
    }
}

/// `2026-11-15T20:01:02.345Z` without a date-time dependency.
pub fn rfc3339(t: SystemTime) -> String {
    let d = t.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = d.as_secs();
    let (days, rem) = (secs / 86_400, secs % 86_400);
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60,
        d.subsec_millis()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339_known_dates() {
        assert_eq!(rfc3339(UNIX_EPOCH), "1970-01-01T00:00:00.000Z");
        let t = UNIX_EPOCH + Duration::from_millis(1_794_772_862_345);
        assert_eq!(rfc3339(t), "2026-11-15T20:01:02.345Z");
        let leap = UNIX_EPOCH + Duration::from_secs(951_782_400); // 2000-02-29
        assert_eq!(rfc3339(leap), "2000-02-29T00:00:00.000Z");
    }

    #[test]
    fn appends_json_lines_without_env_values() {
        let dir = std::env::temp_dir().join(format!("enishi-audit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(AUDIT_FILE);
        let _ = std::fs::remove_file(&path);
        let audit = Audit::new(path.clone());
        let req = ExecRequest {
            command: vec!["echo".into(), "hi".into()],
            env: vec![("TOKEN".into(), "s3cret".into())],
            ..Default::default()
        };
        audit.record(&Entry::exec(Direction::In, "pc", &req).finish(
            Some(0),
            None,
            Duration::from_millis(12),
        ));
        audit.record(&Entry::exec(Direction::Out, "pc", &req).finish(
            None,
            Some("not allowed"),
            Duration::ZERO,
        ));
        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<serde_json::Value> = text
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["dir"], "in");
        assert_eq!(lines[0]["peer"], "pc");
        assert_eq!(lines[0]["cmd"][1], "hi");
        assert_eq!(lines[0]["exit"], 0);
        assert_eq!(lines[0]["duration_ms"], 12);
        assert_eq!(lines[0]["env"][0], "TOKEN");
        assert!(!text.contains("s3cret"));
        assert_eq!(lines[1]["exit"], serde_json::Value::Null);
        assert_eq!(lines[1]["error"], "not allowed");
        std::fs::remove_dir_all(dir).unwrap();
    }
}

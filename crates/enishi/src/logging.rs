//! Daemon logging: stderr always, plus a size-rotated file when the daemon runs as the
//! systemd user unit / logon task (`enishi up --log-file`), and `enishi logs [-f]`.

use std::{
    fs::{File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    process::ExitCode,
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Context, Result};

/// Rotate when the file would grow past this; one previous file (`enishi.log.1`) is kept.
pub const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;

pub fn init(log_file: Option<&Path>) -> Result<()> {
    use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};
    let filter = EnvFilter::try_from_env("ENISHI_LOG")
        .unwrap_or_else(|_| EnvFilter::new("warn,enishi=info,enishi_core=info"));
    let stderr = fmt::layer().with_writer(io::stderr);
    let file = match log_file {
        Some(p) => {
            let rot = RotatingFile::open(p, MAX_LOG_BYTES)?;
            let shared = SharedFile(Arc::new(Mutex::new(rot)));
            Some(fmt::layer().with_ansi(false).with_writer(shared))
        }
        None => None,
    };
    tracing_subscriber::registry()
        .with(filter)
        .with(stderr)
        .with(file)
        .init();
    if log_file.is_some() {
        // A windowless daemon has no stderr: make panics reach the file too.
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            tracing::error!("panic: {info}");
            prev(info);
        }));
    }
    Ok(())
}

/// Append-only log file that renames itself to `<file>.1` when it reaches `max` bytes.
pub struct RotatingFile {
    path: PathBuf,
    file: Option<File>,
    len: u64,
    max: u64,
}

impl RotatingFile {
    pub fn open(path: &Path, max: u64) -> Result<RotatingFile> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("creating log dir {}", dir.display()))?;
        }
        let file = append(path).with_context(|| format!("opening log {}", path.display()))?;
        let len = file.metadata().map(|m| m.len()).unwrap_or(0);
        Ok(RotatingFile {
            path: path.to_path_buf(),
            file: Some(file),
            len,
            max,
        })
    }

    pub fn rotated_path(path: &Path) -> PathBuf {
        let mut s = path.as_os_str().to_owned();
        s.push(".1");
        PathBuf::from(s)
    }

    fn rotate(&mut self) -> io::Result<()> {
        self.file = None; // close before renaming (Windows)
        let _ = std::fs::rename(&self.path, Self::rotated_path(&self.path));
        self.file = Some(append(&self.path)?);
        self.len = 0;
        Ok(())
    }
}

fn append(path: &Path) -> io::Result<File> {
    OpenOptions::new().create(true).append(true).open(path)
}

impl Write for RotatingFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.len > 0 && self.len + buf.len() as u64 > self.max {
            self.rotate()?;
        }
        let f = match &mut self.file {
            Some(f) => f,
            None => {
                self.file = Some(append(&self.path)?);
                self.file.as_mut().expect("just set")
            }
        };
        f.write_all(buf)?;
        self.len += buf.len() as u64;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        match &mut self.file {
            Some(f) => f.flush(),
            None => Ok(()),
        }
    }
}

#[derive(Clone)]
struct SharedFile(Arc<Mutex<RotatingFile>>);

pub struct SharedGuard<'a>(std::sync::MutexGuard<'a, RotatingFile>);

impl Write for SharedGuard<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for SharedFile {
    type Writer = SharedGuard<'a>;
    fn make_writer(&'a self) -> Self::Writer {
        SharedGuard(self.0.lock().unwrap_or_else(|e| e.into_inner()))
    }
}

/// `enishi logs [-n N] [-f]`.
pub fn show(path: &Path, lines: usize, follow: bool) -> Result<ExitCode> {
    let mut file = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound && !follow => {
            eprintln!(
                "enishi: no log at {} — it is written when the daemon runs as the installed \
                 service (`enishi install`) or with `enishi up --log-file`",
                path.display()
            );
            return Ok(ExitCode::FAILURE);
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            eprintln!("enishi: waiting for {} …", path.display());
            loop {
                std::thread::sleep(Duration::from_millis(500));
                if let Ok(f) = File::open(path) {
                    break f;
                }
            }
        }
        Err(e) => return Err(e).with_context(|| format!("opening {}", path.display())),
    };
    let mut pos = print_tail(&mut file, lines)?;
    if !follow {
        return Ok(ExitCode::SUCCESS);
    }
    let mut out = io::stdout();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n > 0 {
            out.write_all(&buf[..n])?;
            out.flush()?;
            pos += n as u64;
            continue;
        }
        std::thread::sleep(Duration::from_millis(500));
        // Rotated (the path now names a new, shorter file): start over on it.
        if let Ok(meta) = std::fs::metadata(path)
            && meta.len() < pos
            && let Ok(f) = File::open(path)
        {
            file = f;
            pos = 0;
        }
    }
}

/// Print the last `lines` lines (0 = everything); returns the offset reached (EOF).
fn print_tail(file: &mut File, lines: usize) -> Result<u64> {
    let len = file.metadata()?.len();
    let start = if lines == 0 {
        0
    } else {
        tail_offset(file, len, lines)?
    };
    file.seek(SeekFrom::Start(start))?;
    let mut out = io::stdout().lock();
    let copied = io::copy(&mut file.take(len - start), &mut out)?;
    out.flush()?;
    Ok(start + copied)
}

/// Offset where the last `lines` lines of the first `len` bytes start.
fn tail_offset<R: Read + Seek>(r: &mut R, len: u64, lines: usize) -> io::Result<u64> {
    const CHUNK: u64 = 8192;
    let mut end = len;
    // A trailing newline terminates the last line; it does not start a new one.
    let mut newlines = 0usize;
    let mut skip_last = true;
    let mut buf = vec![0u8; CHUNK as usize];
    while end > 0 {
        let start = end.saturating_sub(CHUNK);
        let n = (end - start) as usize;
        r.seek(SeekFrom::Start(start))?;
        r.read_exact(&mut buf[..n])?;
        for i in (0..n).rev() {
            if buf[i] == b'\n' {
                if skip_last && start + i as u64 == len - 1 {
                    continue;
                }
                newlines += 1;
                if newlines == lines {
                    return Ok(start + i as u64 + 1);
                }
            }
        }
        skip_last = false;
        end = start;
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn tail_offsets() {
        let s = b"a\nbb\nccc\n";
        let mut c = Cursor::new(&s[..]);
        let len = s.len() as u64;
        assert_eq!(tail_offset(&mut c, len, 1).unwrap(), 5);
        assert_eq!(tail_offset(&mut c, len, 2).unwrap(), 2);
        assert_eq!(tail_offset(&mut c, len, 3).unwrap(), 0);
        assert_eq!(tail_offset(&mut c, len, 10).unwrap(), 0);
        let s = b"a\nbb\nccc"; // no trailing newline
        let mut c = Cursor::new(&s[..]);
        assert_eq!(tail_offset(&mut c, s.len() as u64, 1).unwrap(), 5);
        let big: Vec<u8> = (0..5000)
            .flat_map(|i| format!("{i}\n").into_bytes())
            .collect();
        let mut c = Cursor::new(&big[..]);
        let off = tail_offset(&mut c, big.len() as u64, 3).unwrap() as usize;
        assert_eq!(&big[off..], b"4997\n4998\n4999\n");
    }

    #[test]
    fn rotates_by_size() {
        let dir = std::env::temp_dir().join(format!("enishi-log-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("enishi.log");
        let mut f = RotatingFile::open(&path, 12).unwrap();
        f.write_all(b"12345678\n").unwrap();
        f.write_all(b"abcdefgh\n").unwrap(); // would exceed 12 → rotate first
        f.flush().unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"abcdefgh\n");
        assert_eq!(
            std::fs::read(RotatingFile::rotated_path(&path)).unwrap(),
            b"12345678\n"
        );
        f.write_all(b"x\n").unwrap(); // 11 ≤ 12: no rotation
        drop(f);
        // Re-opening appends and knows the current size.
        let mut f = RotatingFile::open(&path, 12).unwrap();
        f.write_all(b"yy\n").unwrap(); // 11 + 3 > 12 → rotate
        drop(f);
        assert_eq!(std::fs::read(&path).unwrap(), b"yy\n");
        assert_eq!(
            std::fs::read(RotatingFile::rotated_path(&path)).unwrap(),
            b"abcdefgh\nx\n"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

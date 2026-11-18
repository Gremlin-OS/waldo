//! Sessions on disk (DESIGN.md section 1.4).
//!
//! Each session is a directory under the base directory. A session is alive
//! while its host holds a lock on the `lock` file in it; the kernel drops the
//! lock however the host dies.

use serde_json::Value;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

/// Where sessions live: `$WALDO_DIR`, else `$XDG_RUNTIME_DIR/waldo`, else
/// `/tmp/waldo-<uid>`.
pub fn base_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("WALDO_DIR").filter(|d| !d.is_empty()) {
        return PathBuf::from(d);
    }
    if let Some(d) = std::env::var_os("XDG_RUNTIME_DIR").filter(|d| !d.is_empty()) {
        return PathBuf::from(d).join("waldo");
    }
    PathBuf::from(format!("/tmp/waldo-{}", unsafe { libc::getuid() }))
}

/// Create the base directory if needed and check that it is safe to use:
/// a real directory, owned by us, mode 0700.
pub fn ensure_base_dir() -> Result<PathBuf, String> {
    let dir = base_dir();
    if fs::symlink_metadata(&dir).is_err() {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)
            .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }
    check_base_dir(&dir)?;
    Ok(dir)
}

fn check_base_dir(dir: &Path) -> Result<(), String> {
    let md = fs::symlink_metadata(dir).map_err(|e| format!("cannot use {}: {e}", dir.display()))?;
    if md.file_type().is_symlink() || !md.is_dir() {
        return Err(format!("{} is not a directory; set WALDO_DIR to one you own", dir.display()));
    }
    if md.uid() != unsafe { libc::getuid() } {
        return Err(format!("{} is owned by another user; set WALDO_DIR to a directory you own", dir.display()));
    }
    if md.permissions().mode() & 0o077 != 0 {
        return Err(format!("{} is accessible to other users; run: chmod 700 {}", dir.display(), dir.display()));
    }
    Ok(())
}

/// Session names: 1 to 32 of `a-z 0-9 _ -`, starting with a letter or digit.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 32
        && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
        && name.as_bytes()[0].is_ascii_alphanumeric()
}

/// Whether a host holds the lock in this session directory.
pub fn lock_is_held(dir: &Path) -> bool {
    let Ok(f) = fs::File::open(dir.join("lock")) else {
        return false;
    };
    // The host holds an exclusive lock, so a shared lock fails while it lives.
    let r = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) };
    if r == 0 {
        unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_UN) };
        false
    } else {
        true
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum State {
    /// The host is running.
    Live,
    /// The session ended and its will can be read from the directory.
    Exited,
    /// The session ended and the will could not be delivered; it is in
    /// `will.json`.
    Undelivered,
    /// The host died without leaving a will.
    Dead,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            State::Live => "live",
            State::Exited => "exited",
            State::Undelivered => "undelivered",
            State::Dead => "dead",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Session {
    pub id: String,
    pub dir: PathBuf,
    pub state: State,
    pub meta: Option<Value>,
}

impl Session {
    pub fn socket(&self) -> PathBuf {
        self.dir.join("sock")
    }

    /// Seconds since the directory was last changed.
    pub fn age_secs(&self) -> i64 {
        let m = fs::metadata(self.dir.join("meta.json")).or_else(|_| fs::metadata(&self.dir));
        match m {
            Ok(md) => (crate::util::unix_now() - md.mtime()).max(0),
            Err(_) => 0,
        }
    }
}

fn read_json(path: &Path) -> Option<Value> {
    serde_json::from_slice(&fs::read(path).ok()?).ok()
}

/// The last line of a file, reading at most the final 16 MiB.
fn last_line(path: &Path) -> Option<String> {
    let mut f = fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    let start = len.saturating_sub(16 << 20);
    f.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf);
    text.lines().rev().find(|l| !l.trim().is_empty()).map(|l| l.to_string())
}

/// The session's exit message, if it can be found in the session directory:
/// in `will.json`, or as the last line of a `file:` destination.
pub fn read_will(dir: &Path, meta: Option<&Value>) -> Option<Value> {
    if let Some(v) = read_json(&dir.join("will.json")) {
        return Some(v);
    }
    let dest = meta.and_then(|m| m.get("dest")).and_then(|d| d.as_str())?;
    let path = dest.strip_prefix("file:")?;
    let v: Value = serde_json::from_str(&last_line(Path::new(path))?).ok()?;
    if v.get("trigger").and_then(|t| t.as_str()) == Some("exit") {
        Some(v)
    } else {
        None
    }
}

fn inspect(dir: PathBuf) -> Option<Session> {
    let id = dir.file_name()?.to_str()?.to_string();
    if !dir.join("lock").exists() {
        return None;
    }
    let meta = read_json(&dir.join("meta.json"));
    let state = if lock_is_held(&dir) {
        State::Live
    } else if dir.join("will.json").exists() {
        State::Undelivered
    } else if read_will(&dir, meta.as_ref()).is_some() {
        State::Exited
    } else {
        State::Dead
    };
    Some(Session { id, dir, state, meta })
}

/// Every session directory under the base directory, sorted by id.
pub fn list() -> Vec<Session> {
    let base = base_dir();
    let mut out: Vec<Session> = match fs::read_dir(&base) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
            .filter_map(|e| inspect(e.path()))
            .collect(),
        Err(_) => Vec::new(),
    };
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

/// Find a session by id or unique prefix.
pub fn find(sel: &str) -> Result<Session, String> {
    let all = list();
    if let Some(s) = all.iter().find(|s| s.id == sel) {
        return Ok(s.clone());
    }
    let matches: Vec<&Session> = all.iter().filter(|s| s.id.starts_with(sel)).collect();
    match matches.len() {
        1 => Ok(matches[0].clone()),
        0 => Err(format!("no session '{sel}'; run 'waldo-term list' to see sessions")),
        _ => Err(format!(
            "'{sel}' matches several sessions: {}",
            matches.iter().map(|s| s.id.as_str()).collect::<Vec<_>>().join(", ")
        )),
    }
}

//! Delivery of messages to the session's destination (DESIGN.md section 5).
//!
//! One thread owns delivery, so a slow receiver can never stall the pty. This
//! milestone implements the `file:` scheme only.

use std::fs::OpenOptions;
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const QUEUE_MESSAGES: usize = 256;
const WILL_RETRY: Duration = Duration::from_secs(5);

#[derive(Clone, Debug)]
pub enum Dest {
    File(PathBuf),
}

impl Dest {
    pub fn parse(uri: &str) -> Result<Dest, String> {
        if let Some(path) = uri.strip_prefix("file:") {
            let path = path.strip_prefix("//").unwrap_or(path);
            let p = PathBuf::from(path);
            if !p.is_absolute() {
                return Err(format!("the destination path must be absolute: {uri}"));
            }
            return Ok(Dest::File(p));
        }
        if uri.starts_with("unix:") || uri.starts_with("http://") {
            return Err(format!("{uri}: only file: destinations are implemented so far; use --dest file:/absolute/path"));
        }
        Err(format!("{uri}: not a destination; use --dest file:/absolute/path"))
    }

    pub fn uri(&self) -> String {
        match self {
            Dest::File(p) => format!("file:{}", p.display()),
        }
    }

    /// Deliver one message. A line is appended with a single write.
    pub fn deliver(&self, line: &str) -> io::Result<()> {
        match self {
            Dest::File(path) => {
                let mut f = OpenOptions::new().create(true).append(true).mode(0o600).open(path)?;
                let mut buf = Vec::with_capacity(line.len() + 1);
                buf.extend_from_slice(line.as_bytes());
                buf.push(b'\n');
                f.write_all(&buf)
            }
        }
    }
}

enum Msg {
    Line(String),
    Will(String),
}

pub struct Delivery {
    tx: SyncSender<Msg>,
    will_result: Receiver<bool>,
    handle: Option<JoinHandle<()>>,
    /// Messages dropped because the queue was full.
    pub dropped: u64,
}

impl Delivery {
    /// Start the delivery thread. Call only after every fork has happened.
    pub fn start(dest: Dest, log: impl Fn(&str) + Send + 'static) -> Delivery {
        let (tx, rx) = mpsc::sync_channel::<Msg>(QUEUE_MESSAGES);
        let (will_tx, will_result) = mpsc::channel::<bool>();
        let handle = thread::spawn(move || {
            for msg in rx {
                match msg {
                    Msg::Line(line) => {
                        if let Err(e) = dest.deliver(&line) {
                            log(&format!("delivery to {} failed: {e}", dest.uri()));
                        }
                    }
                    Msg::Will(line) => {
                        let start = Instant::now();
                        let ok = loop {
                            match dest.deliver(&line) {
                                Ok(()) => break true,
                                Err(e) => {
                                    if start.elapsed() >= WILL_RETRY {
                                        log(&format!("the will could not be delivered to {}: {e}", dest.uri()));
                                        break false;
                                    }
                                    thread::sleep(Duration::from_millis(250));
                                }
                            }
                        };
                        let _ = will_tx.send(ok);
                        return;
                    }
                }
            }
        });
        Delivery { tx, will_result, handle: Some(handle), dropped: 0 }
    }

    /// Queue a message. If the queue is full the message is dropped and
    /// counted.
    #[allow(dead_code)] // used once settle, match and stream messages exist
    pub fn send(&mut self, line: String) {
        match self.tx.try_send(Msg::Line(line)) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => self.dropped += 1,
        }
    }

    /// Queue a message that must not be dropped, waiting for room.
    pub fn send_blocking(&mut self, line: String) {
        let _ = self.tx.send(Msg::Line(line));
    }

    /// Deliver the will as the last message and wait for the outcome.
    /// Returns false if it could not be delivered.
    pub fn deliver_will(mut self, line: String) -> bool {
        if self.tx.send(Msg::Will(line)).is_err() {
            return false;
        }
        let ok = self.will_result.recv_timeout(WILL_RETRY + Duration::from_secs(2)).unwrap_or(false);
        if ok {
            if let Some(h) = self.handle.take() {
                let _ = h.join();
            }
        }
        ok
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_uris() {
        assert!(matches!(Dest::parse("file:/tmp/x").unwrap(), Dest::File(p) if p == std::path::Path::new("/tmp/x")));
        assert!(matches!(Dest::parse("file:///tmp/x").unwrap(), Dest::File(p) if p == std::path::Path::new("/tmp/x")));
        assert!(Dest::parse("file:relative").is_err());
        assert!(Dest::parse("http://localhost/x").unwrap_err().contains("only file:"));
        assert!(Dest::parse("nonsense").is_err());
    }

    #[test]
    fn file_delivery_appends_lines() {
        let path = std::env::temp_dir().join(format!("waldo-dest-test-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let mut d = Delivery::start(Dest::File(path.clone()), |_| {});
        d.send("{\"a\":1}".into());
        d.send_blocking("{\"a\":2}".into());
        assert!(d.deliver_will("{\"a\":3}".into()));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{\"a\":1}\n{\"a\":2}\n{\"a\":3}\n");
        let _ = std::fs::remove_file(&path);
    }
}

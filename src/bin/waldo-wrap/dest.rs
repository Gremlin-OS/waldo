//! Delivery of messages to the session's destination (DESIGN.md sections 4
//! and 5).
//!
//! One thread owns delivery, so a slow receiver can never stall the pty. The
//! host puts messages on a bounded queue; when the queue is full, newer
//! screens replace older ones and the oldest match or stream message is
//! dropped and counted.

use serde_json::{Map, Value};
use std::collections::VecDeque;
use std::fs::OpenOptions;
use std::io::{self, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use waldo::util;

const QUEUE_MESSAGES: usize = 256;
const QUEUE_BYTES: usize = 8 << 20;
/// Stream messages are merged while the result stays under this size.
const MERGE_BYTES: usize = 1 << 20;
const WILL_RETRY: Duration = Duration::from_secs(5);
const IO_TIMEOUT: Duration = Duration::from_secs(5);
/// How long to wait before each further attempt at an `http:` delivery.
const HTTP_RETRIES: [Duration; 2] = [Duration::from_millis(250), Duration::from_secs(1)];

#[derive(Clone, Debug, PartialEq)]
pub enum Dest {
    File(PathBuf),
    Unix(PathBuf),
    Http { host: String, port: u16, path: String },
}

impl Dest {
    pub fn parse(uri: &str) -> Result<Dest, String> {
        let local_path = |rest: &str| -> Result<PathBuf, String> {
            let p = PathBuf::from(rest.strip_prefix("//").unwrap_or(rest));
            if p.is_absolute() {
                Ok(p)
            } else {
                Err(format!("the destination path must be absolute: {uri}"))
            }
        };
        if let Some(rest) = uri.strip_prefix("file:") {
            return local_path(rest).map(Dest::File);
        }
        if let Some(rest) = uri.strip_prefix("unix:") {
            return local_path(rest).map(Dest::Unix);
        }
        if let Some(rest) = uri.strip_prefix("http://") {
            let (authority, path) = match rest.find('/') {
                Some(i) => (&rest[..i], &rest[i..]),
                None => (rest, "/"),
            };
            let bad = || format!("{uri}: an http destination looks like http://HOST:PORT/PATH");
            if authority.is_empty() || authority.contains('@') {
                return Err(bad());
            }
            // An IPv6 address is written in brackets: http://[::1]:8080/x
            let (host, port) = if let Some(v6) = authority.strip_prefix('[') {
                let (h, after) = v6.split_once(']').ok_or_else(bad)?;
                (h, after.strip_prefix(':'))
            } else {
                match authority.rsplit_once(':') {
                    Some((h, p)) => (h, Some(p)),
                    None => (authority, None),
                }
            };
            let port = match port {
                None => 80,
                Some(p) => p.parse::<u16>().ok().filter(|n| *n > 0).ok_or_else(bad)?,
            };
            if host.is_empty() {
                return Err(bad());
            }
            return Ok(Dest::Http { host: host.to_string(), port, path: path.to_string() });
        }
        if uri.starts_with("https://") {
            return Err(format!("{uri}: https destinations are not supported; send to a local http proxy that forwards it"));
        }
        Err(format!("{uri}: not a destination; use file:/PATH, unix:/PATH or http://HOST:PORT/PATH"))
    }

    pub fn uri(&self) -> String {
        match self {
            Dest::File(p) => format!("file:{}", p.display()),
            Dest::Unix(p) => format!("unix:{}", p.display()),
            Dest::Http { path, .. } => format!("http://{}{path}", self.authority()),
        }
    }

    fn authority(&self) -> String {
        match self {
            Dest::Http { host, port, .. } => {
                let host = if host.contains(':') { format!("[{host}]") } else { host.clone() };
                if *port == 80 {
                    host
                } else {
                    format!("{host}:{port}")
                }
            }
            _ => String::new(),
        }
    }

    fn is_http(&self) -> bool {
        matches!(self, Dest::Http { .. })
    }
}

/// A destination and the connection to it, if it keeps one.
pub struct Sink {
    dest: Dest,
    unix: Option<UnixStream>,
}

impl Sink {
    pub fn new(dest: Dest) -> Sink {
        Sink { dest, unix: None }
    }

    /// Deliver one message, given as one line of JSON without a newline.
    pub fn deliver(&mut self, line: &str) -> io::Result<()> {
        match &self.dest {
            Dest::File(path) => {
                // One write per message, so concurrent appenders interleave
                // whole lines.
                let mut f = OpenOptions::new().create(true).append(true).mode(0o600).open(path)?;
                let mut buf = Vec::with_capacity(line.len() + 1);
                buf.extend_from_slice(line.as_bytes());
                buf.push(b'\n');
                f.write_all(&buf)
            }
            Dest::Unix(path) => {
                if self.unix.is_none() {
                    let s = UnixStream::connect(path)?;
                    s.set_write_timeout(Some(IO_TIMEOUT))?;
                    self.unix = Some(s);
                }
                let mut buf = Vec::with_capacity(line.len() + 1);
                buf.extend_from_slice(line.as_bytes());
                buf.push(b'\n');
                let result = self.unix.as_mut().map(|s| s.write_all(&buf)).unwrap_or(Ok(()));
                if result.is_err() {
                    // The next message connects again.
                    self.unix = None;
                }
                result
            }
            Dest::Http { host, port, path } => http_post(host, *port, &self.dest.authority(), path, line),
        }
    }
}

/// POST one message and require a 2xx answer. One connection per message:
/// it is slower than keeping one open, and it cannot go stale.
fn http_post(host: &str, port: u16, authority: &str, path: &str, line: &str) -> io::Result<()> {
    let mut last = io::Error::new(io::ErrorKind::NotFound, format!("{host} has no address"));
    let mut stream = None;
    for addr in (host, port).to_socket_addrs()? {
        match TcpStream::connect_timeout(&addr, IO_TIMEOUT) {
            Ok(s) => {
                stream = Some(s);
                break;
            }
            Err(e) => last = e,
        }
    }
    let Some(mut s) = stream else { return Err(last) };
    s.set_write_timeout(Some(IO_TIMEOUT))?;
    s.set_read_timeout(Some(IO_TIMEOUT))?;
    let head = format!(
        "POST {path} HTTP/1.1\r\nHost: {authority}\r\nUser-Agent: waldo\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        line.len()
    );
    let mut req = head.into_bytes();
    req.extend_from_slice(line.as_bytes());
    s.write_all(&req)?;

    // Only the status line matters.
    let mut got = Vec::new();
    let mut buf = [0u8; 512];
    while !got.contains(&b'\n') && got.len() < 4096 {
        match s.read(&mut buf)? {
            0 => break,
            n => got.extend_from_slice(&buf[..n]),
        }
    }
    let text = String::from_utf8_lossy(&got);
    let status = text.lines().next().unwrap_or("");
    let code = status.strip_prefix("HTTP/").and_then(|r| r.split_whitespace().nth(1)).and_then(|c| c.parse::<u16>().ok());
    match code {
        Some(c) if (200..300).contains(&c) => Ok(()),
        Some(c) => Err(io::Error::other(format!("the receiver answered {c}"))),
        None => Err(io::Error::other("the receiver did not answer with HTTP")),
    }
}

/// The fields every message starts with (section 4).
pub fn envelope(session: &str, seq: u64, trigger: &str, start: Instant) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("v".into(), 1.into());
    m.insert("session".into(), session.into());
    m.insert("seq".into(), seq.into());
    m.insert("trigger".into(), trigger.into());
    m.insert("time".into(), util::rfc3339_now().into());
    m.insert("t_ms".into(), (start.elapsed().as_millis() as u64).into());
    m
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Kind {
    Hello,
    Settle,
    Match,
    Stream,
    Heartbeat,
    Exit,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Kind::Hello => "hello",
            Kind::Settle => "settle",
            Kind::Match => "match",
            Kind::Stream => "stream",
            Kind::Heartbeat => "heartbeat",
            Kind::Exit => "exit",
        }
    }
}

struct Queued {
    kind: Kind,
    seq: u64,
    /// The message's own fields; the envelope is added when it is sent.
    fields: Map<String, Value>,
    time: String,
    t_ms: u64,
    size: usize,
}

fn size_of(fields: &Map<String, Value>) -> usize {
    // The envelope adds about 120 bytes.
    serde_json::to_string(fields).map(|s| s.len()).unwrap_or(0) + 120
}

/// The queue between the host and the delivery thread.
struct Queue {
    items: VecDeque<Queued>,
    bytes: usize,
    next_seq: u64,
    /// Messages dropped since the last one that was sent.
    dropped: u64,
    max_messages: usize,
    max_bytes: usize,
}

impl Queue {
    fn new(first_seq: u64, max_messages: usize, max_bytes: usize) -> Queue {
        Queue { items: VecDeque::new(), bytes: 0, next_seq: first_seq, dropped: 0, max_messages, max_bytes }
    }

    fn full(&self, incoming: usize) -> bool {
        self.items.len() >= self.max_messages || self.bytes + incoming > self.max_bytes
    }

    fn remove(&mut self, i: usize) {
        if let Some(old) = self.items.remove(i) {
            self.bytes -= old.size;
        }
    }

    /// Add a message. A sequence number is used only if the message takes a
    /// new place in the queue.
    fn push(&mut self, kind: Kind, fields: Map<String, Value>, time: String, t_ms: u64) {
        let size = size_of(&fields);
        if self.full(size) {
            match kind {
                // The newest screen replaces a queued one and keeps its place.
                Kind::Settle => {
                    if let Some(old) = self.items.iter_mut().rev().find(|m| m.kind == Kind::Settle) {
                        self.bytes = self.bytes - old.size + size;
                        old.fields = fields;
                        old.time = time;
                        old.t_ms = t_ms;
                        old.size = size;
                        return;
                    }
                }
                // Lines join the stream message they follow.
                Kind::Stream => {
                    if let Some(last) = self.items.back_mut().filter(|m| m.kind == Kind::Stream && m.size + size <= MERGE_BYTES) {
                        let more = fields.get("lines").and_then(|l| l.as_array()).cloned().unwrap_or_default();
                        if let Some(lines) = last.fields.get_mut("lines").and_then(|l| l.as_array_mut()) {
                            lines.extend(more);
                        }
                        let skipped = |f: &Map<String, Value>| f.get("skipped_lines").and_then(|s| s.as_u64()).unwrap_or(0);
                        let total = skipped(&last.fields) + skipped(&fields);
                        if total > 0 {
                            last.fields.insert("skipped_lines".into(), total.into());
                        }
                        let new_size = size_of(&last.fields);
                        self.bytes = self.bytes - last.size + new_size;
                        last.size = new_size;
                        return;
                    }
                }
                // A heartbeat says the host is alive; queued messages say so too.
                Kind::Heartbeat => return,
                _ => {}
            }
            while self.full(size) {
                let victim = self
                    .items
                    .iter()
                    .position(|m| matches!(m.kind, Kind::Match | Kind::Stream))
                    .or_else(|| self.items.iter().position(|m| matches!(m.kind, Kind::Settle | Kind::Heartbeat)));
                match victim {
                    Some(i) => {
                        self.remove(i);
                        self.dropped += 1;
                    }
                    // Only hello and exit are left, and they are never dropped.
                    None => break,
                }
            }
        }
        let seq = self.next_seq;
        self.next_seq += 1;
        self.bytes += size;
        self.items.push_back(Queued { kind, seq, fields, time, t_ms, size });
    }

    fn pop(&mut self) -> Option<Queued> {
        let m = self.items.pop_front()?;
        self.bytes -= m.size;
        Some(m)
    }
}

struct Shared {
    queue: Mutex<Queue>,
    wake: Condvar,
}

pub struct Delivery {
    shared: Arc<Shared>,
    session: String,
    start: Instant,
    will_result: Receiver<bool>,
    handle: Option<JoinHandle<()>>,
}

fn line_of(session: &str, m: &Queued, dropped: u64) -> String {
    let mut body = Map::new();
    body.insert("v".into(), 1.into());
    body.insert("session".into(), session.into());
    body.insert("seq".into(), m.seq.into());
    body.insert("trigger".into(), m.kind.name().into());
    body.insert("time".into(), m.time.clone().into());
    body.insert("t_ms".into(), m.t_ms.into());
    if dropped > 0 {
        body.insert("dropped".into(), dropped.into());
    }
    body.extend(m.fields.clone());
    serde_json::to_string(&Value::Object(body)).unwrap_or_default()
}

impl Delivery {
    /// Start the delivery thread. Call only after every fork has happened.
    /// `first_seq` is the number of messages already delivered directly.
    pub fn start(
        mut sink: Sink,
        session: String,
        start: Instant,
        first_seq: u64,
        heartbeat: Option<Duration>,
        log: impl Fn(&str) + Send + 'static,
    ) -> Delivery {
        let shared = Arc::new(Shared { queue: Mutex::new(Queue::new(first_seq, QUEUE_MESSAGES, QUEUE_BYTES)), wake: Condvar::new() });
        let (will_tx, will_result) = mpsc::channel::<bool>();
        let thread_shared = shared.clone();
        let thread_session = session.clone();
        let handle = thread::spawn(move || {
            let shared = thread_shared;
            let session = thread_session;
            let uri = sink.dest.uri();
            let mut last_sent = Instant::now();
            let mut failing = false;
            loop {
                // Wait for a message, or for the heartbeat to fall due.
                let (msg, dropped) = {
                    let mut q = shared.queue.lock().unwrap_or_else(|e| e.into_inner());
                    loop {
                        if let Some(m) = q.pop() {
                            let dropped = std::mem::take(&mut q.dropped);
                            break (m, dropped);
                        }
                        match heartbeat {
                            None => q = shared.wake.wait(q).unwrap_or_else(|e| e.into_inner()),
                            Some(every) => {
                                let idle = last_sent.elapsed();
                                if idle >= every {
                                    q.push(Kind::Heartbeat, Map::new(), util::rfc3339_now(), start.elapsed().as_millis() as u64);
                                    continue;
                                }
                                q = shared.wake.wait_timeout(q, every - idle).unwrap_or_else(|e| e.into_inner()).0;
                            }
                        }
                    }
                };
                let line = line_of(&session, &msg, dropped);
                last_sent = Instant::now();

                if msg.kind == Kind::Exit {
                    let began = Instant::now();
                    let ok = loop {
                        match sink.deliver(&line) {
                            Ok(()) => break true,
                            Err(e) => {
                                if began.elapsed() >= WILL_RETRY {
                                    log(&format!("the will could not be delivered to {uri}: {e}"));
                                    break false;
                                }
                                thread::sleep(Duration::from_millis(250));
                            }
                        }
                    };
                    let _ = will_tx.send(ok);
                    return;
                }

                let mut result = sink.deliver(&line);
                if sink.dest.is_http() {
                    // At least once: try again before giving the message up.
                    for pause in HTTP_RETRIES {
                        if result.is_ok() {
                            break;
                        }
                        thread::sleep(pause);
                        result = sink.deliver(&line);
                    }
                }
                match result {
                    Ok(()) => {
                        if failing {
                            log(&format!("delivery to {uri} works again"));
                            failing = false;
                        }
                    }
                    Err(e) => {
                        // Say so once, not for every message.
                        if !failing {
                            log(&format!("delivery to {uri} failed, messages are being lost: {e}"));
                            failing = true;
                        }
                        // The loss shows up in the next message that arrives.
                        let mut q = shared.queue.lock().unwrap_or_else(|e| e.into_inner());
                        q.dropped += dropped + 1;
                    }
                }
            }
        });
        Delivery { shared, session, start, will_result, handle: Some(handle) }
    }

    /// Queue a message. It is never blocked on; see the queue rules above.
    pub fn send(&self, kind: Kind, fields: Map<String, Value>) {
        let mut q = self.shared.queue.lock().unwrap_or_else(|e| e.into_inner());
        q.push(kind, fields, util::rfc3339_now(), self.start.elapsed().as_millis() as u64);
        drop(q);
        self.shared.wake.notify_one();
    }

    /// Deliver the will as the last message and wait for the outcome.
    /// Returns whether it arrived, and the will as it was sent, so the
    /// caller can keep it if it did not arrive.
    pub fn deliver_will(mut self, fields: Map<String, Value>) -> (bool, String) {
        let line = {
            let mut q = self.shared.queue.lock().unwrap_or_else(|e| e.into_inner());
            q.push(Kind::Exit, fields, util::rfc3339_now(), self.start.elapsed().as_millis() as u64);
            // What is still queued ahead of it may yet be dropped or fail, so
            // this copy can differ from the delivered one in `dropped` only.
            q.items.back().map(|m| line_of(&self.session, m, q.dropped)).unwrap_or_default()
        };
        self.shared.wake.notify_one();
        // Messages queued ahead of the will are sent first, so allow for them.
        let ok = self.will_result.recv_timeout(WILL_RETRY + IO_TIMEOUT * 2).unwrap_or(false);
        if ok {
            if let Some(h) = self.handle.take() {
                let _ = h.join();
            }
        }
        (ok, line)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::BufRead;
    use std::path::Path;

    fn obj(v: Value) -> Map<String, Value> {
        v.as_object().cloned().unwrap_or_default()
    }

    #[test]
    fn destinations_parse() {
        assert!(matches!(Dest::parse("file:/tmp/x").unwrap(), Dest::File(p) if p == Path::new("/tmp/x")));
        assert!(matches!(Dest::parse("file:///tmp/x").unwrap(), Dest::File(p) if p == Path::new("/tmp/x")));
        assert!(matches!(Dest::parse("unix:/run/x.sock").unwrap(), Dest::Unix(p) if p == Path::new("/run/x.sock")));
        assert!(Dest::parse("file:relative").is_err());
        assert!(Dest::parse("unix:relative").is_err());
        assert!(Dest::parse("https://example.org/x").unwrap_err().contains("proxy"));
        assert!(Dest::parse("ftp://x").is_err());

        let http = |u: &str| Dest::parse(u).unwrap();
        assert_eq!(http("http://localhost:8080/hook"), Dest::Http { host: "localhost".into(), port: 8080, path: "/hook".into() });
        assert_eq!(http("http://example.org"), Dest::Http { host: "example.org".into(), port: 80, path: "/".into() });
        assert_eq!(http("http://[::1]:9000/a?b=c"), Dest::Http { host: "::1".into(), port: 9000, path: "/a?b=c".into() });
        for bad in ["http://", "http://:80/x", "http://host:0/x", "http://host:port/x", "http://user@host/x", "http://[::1/x"] {
            assert!(Dest::parse(bad).is_err(), "{bad}");
        }
        for uri in ["file:/tmp/x", "unix:/run/x.sock", "http://localhost:8080/hook", "http://example.org/", "http://[::1]:9000/a?b=c"] {
            assert_eq!(Dest::parse(uri).unwrap().uri(), uri);
        }
    }

    #[test]
    fn file_delivery_appends_lines() {
        let path = std::env::temp_dir().join(format!("waldo-dest-test-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let mut sink = Sink::new(Dest::File(path.clone()));
        sink.deliver("{\"a\":1}").unwrap();
        sink.deliver("{\"b\":2}").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{\"a\":1}\n{\"b\":2}\n");
        let _ = std::fs::remove_file(&path);
    }

    fn push(q: &mut Queue, kind: Kind, fields: Value) {
        q.push(kind, obj(fields), "t".into(), 0);
    }

    fn kinds(q: &Queue) -> Vec<(Kind, u64)> {
        q.items.iter().map(|m| (m.kind, m.seq)).collect()
    }

    #[test]
    fn a_queue_with_room_keeps_everything_in_order() {
        let mut q = Queue::new(1, 8, 1 << 20);
        push(&mut q, Kind::Settle, json!({"n": 1}));
        push(&mut q, Kind::Match, json!({"n": 2}));
        push(&mut q, Kind::Settle, json!({"n": 3}));
        assert_eq!(kinds(&q), [(Kind::Settle, 1), (Kind::Match, 2), (Kind::Settle, 3)]);
        assert_eq!(q.dropped, 0);
    }

    #[test]
    fn a_full_queue_replaces_the_queued_settle() {
        let mut q = Queue::new(0, 3, 1 << 20);
        push(&mut q, Kind::Hello, json!({}));
        push(&mut q, Kind::Settle, json!({"n": 1}));
        push(&mut q, Kind::Match, json!({"n": 2}));
        push(&mut q, Kind::Settle, json!({"n": 3}));
        // The newer screen took the older one's place and number.
        assert_eq!(kinds(&q), [(Kind::Hello, 0), (Kind::Settle, 1), (Kind::Match, 2)]);
        assert_eq!(q.items[1].fields["n"], 3);
        assert_eq!(q.dropped, 0);
        assert_eq!(q.next_seq, 3);
    }

    #[test]
    fn a_full_queue_merges_consecutive_stream_messages() {
        let mut q = Queue::new(0, 2, 1 << 20);
        push(&mut q, Kind::Hello, json!({}));
        push(&mut q, Kind::Stream, json!({"from_line": 1, "lines": ["a", "b"]}));
        push(&mut q, Kind::Stream, json!({"from_line": 3, "lines": ["c"], "skipped_lines": 4}));
        assert_eq!(kinds(&q), [(Kind::Hello, 0), (Kind::Stream, 1)]);
        assert_eq!(q.items[1].fields["from_line"], 1);
        assert_eq!(q.items[1].fields["lines"], json!(["a", "b", "c"]));
        assert_eq!(q.items[1].fields["skipped_lines"], 4);
        assert_eq!(q.dropped, 0);
    }

    #[test]
    fn a_full_queue_drops_the_oldest_match_or_stream_and_counts_it() {
        let mut q = Queue::new(0, 3, 1 << 20);
        push(&mut q, Kind::Hello, json!({}));
        push(&mut q, Kind::Match, json!({"n": 1}));
        push(&mut q, Kind::Settle, json!({"n": 2}));
        push(&mut q, Kind::Match, json!({"n": 3}));
        assert_eq!(kinds(&q), [(Kind::Hello, 0), (Kind::Settle, 2), (Kind::Match, 3)]);
        assert_eq!(q.dropped, 1);
    }

    #[test]
    fn hello_and_exit_are_never_dropped() {
        let mut q = Queue::new(0, 2, 1 << 20);
        push(&mut q, Kind::Hello, json!({}));
        push(&mut q, Kind::Match, json!({"n": 1}));
        push(&mut q, Kind::Exit, json!({"exit_code": 0}));
        assert_eq!(kinds(&q), [(Kind::Hello, 0), (Kind::Exit, 2)]);
        assert_eq!(q.dropped, 1);
        // Even with nothing left to drop, the will still goes in.
        let mut q = Queue::new(0, 1, 1 << 20);
        push(&mut q, Kind::Hello, json!({}));
        push(&mut q, Kind::Exit, json!({}));
        assert_eq!(kinds(&q), [(Kind::Hello, 0), (Kind::Exit, 1)]);
    }

    #[test]
    fn the_byte_limit_counts_too() {
        let big = "x".repeat(600);
        let mut q = Queue::new(0, 100, 2000);
        push(&mut q, Kind::Match, json!({"t": big}));
        push(&mut q, Kind::Match, json!({"t": big}));
        push(&mut q, Kind::Match, json!({"t": big}));
        assert_eq!(kinds(&q), [(Kind::Match, 1), (Kind::Match, 2)]);
        assert_eq!(q.dropped, 1);
        assert!(q.bytes <= 2000);
    }

    #[test]
    fn a_heartbeat_is_not_queued_behind_a_full_queue() {
        let mut q = Queue::new(0, 1, 1 << 20);
        push(&mut q, Kind::Hello, json!({}));
        push(&mut q, Kind::Heartbeat, json!({}));
        assert_eq!(kinds(&q), [(Kind::Hello, 0)]);
        assert_eq!(q.dropped, 0);
    }

    /// End to end through the thread: order, sequence numbers, and the will
    /// last, over a unix socket.
    #[test]
    fn unix_delivery_streams_lines_and_ends_with_the_will() {
        let path = std::env::temp_dir().join(format!("waldo-dest-sock-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let reader = thread::spawn(move || {
            let (conn, _) = listener.accept().unwrap();
            io::BufReader::new(conn).lines().map(|l| serde_json::from_str::<Value>(&l.unwrap()).unwrap()).collect::<Vec<_>>()
        });
        let d = Delivery::start(Sink::new(Dest::Unix(path.clone())), "s1".into(), Instant::now(), 0, None, |_| {});
        d.send(Kind::Hello, obj(json!({"cols": 80})));
        d.send(Kind::Settle, obj(json!({"screen": {"text": "hi"}})));
        let (ok, line) = d.deliver_will(obj(json!({"exit_code": 0})));
        assert!(ok);
        let got = reader.join().unwrap();
        let _ = std::fs::remove_file(&path);
        let triggers: Vec<&str> = got.iter().map(|m| m["trigger"].as_str().unwrap()).collect();
        assert_eq!(triggers, ["hello", "settle", "exit"]);
        for (i, m) in got.iter().enumerate() {
            assert_eq!(m["seq"], i as u64);
            assert_eq!(m["session"], "s1");
            assert_eq!(m["v"], 1);
        }
        assert_eq!(got[1]["screen"]["text"], "hi");
        assert_eq!(serde_json::from_str::<Value>(&line).unwrap()["seq"], 2);
    }
}

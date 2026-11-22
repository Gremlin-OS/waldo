//! The host's event loop (DESIGN.md sections 1.2, 1.5, 2 and 3).
//!
//! One thread, one epoll: the pty, the child's pidfd, a signalfd, the
//! listening socket and the client connections. Timers are the epoll
//! timeout. Nothing here blocks on the pty or on a client.

use crate::dest::{Delivery, Dest, Kind};
use crate::emu::{Emu, Row, Space, Tracked};
use crate::http::{self, Parsed, Request};
use crate::keys::{self, Tok};
use crate::outbuf::OutBuf;
use crate::record::Recorder;
use crate::sys::{self, Child, Epoll, ExitStatus};
use regex_lite::Regex;
use serde_json::{json, Map, Value};
use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, VecDeque};
use std::fs::File;
use std::hash::Hasher;
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::os::unix::io::{AsRawFd, RawFd};
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use waldo::util;

/// Why an input item was refused: an error code and a message for the caller.
type ItemError = (&'static str, String);

const T_PTY: u64 = 0;
const T_PIDFD: u64 = 1;
const T_SIGNAL: u64 = 2;
const T_LISTEN: u64 = 3;
const T_FIRST_CONN: u64 = 100;

const CHUNK: usize = 4096;
const READ_BATCH: usize = 64 << 10;
const DEFAULT_TIMEOUT_MS: u64 = 10_000;
const MAX_TIMEOUT_MS: u64 = 600_000;
const PROMPT_QUIET_MS: u64 = 40;
const ESC_PAUSE: Duration = Duration::from_millis(30);
/// A key that follows text in one request is held back until the program has
/// shown that it took the text: the screen changed and went still again. A
/// person types, sees the text appear, then presses Enter. Programs that take
/// a burst of characters for a paste would otherwise read the key as part of
/// it, and an Enter becomes a line break instead of a submit. The wait is at
/// least KEY_MIN and, if the screen never reacts (echo off), at most KEY_MAX.
const KEY_MIN: Duration = Duration::from_millis(40);
const KEY_MAX: Duration = Duration::from_millis(300);
const KEY_STILL: Duration = Duration::from_millis(30);
const KEY_POLL: Duration = Duration::from_millis(10);
/// The first input of a session waits for the screen's first settle, but
/// not past this long after the session started. A shell echoes what
/// arrives before its first prompt a second time.
const FIRST_DRAW_MAX: Duration = Duration::from_secs(2);
const HOLD_CAP: Duration = Duration::from_secs(1);
/// How long a resize waits for the program to redraw and go quiet.
const RESIZE_WAIT: Duration = Duration::from_millis(1000);
/// How many broadcasts are kept for clients that have not heard them yet.
const MAX_BROADCASTS: usize = 32;
const MAX_BROADCAST_BYTES: usize = 1000;
/// How many client names are remembered.
const MAX_NAMES: usize = 64;
const BROADCASTS_OFF: &str = "broadcasts are off for this session: it was started with --no-broadcast. Its other clients cannot be reached through it.";
const NAME_RULE: &str = "a name is 1 to 32 characters from a-z, A-Z, 0-9, _ and -";
const DRAIN_QUIET: Duration = Duration::from_millis(100);
const DRAIN_CAP: Duration = Duration::from_secs(2);
const KILL_STEP: Duration = Duration::from_secs(2);
const MAX_FILE_ITEM: u64 = 1 << 20;
/// The longest delay a request may ask for, between keys or as a pause.
const MAX_DELAY_MS: u64 = 10_000;
/// How often a wait for a program inside the session to exit looks again.
const EXIT_POLL: Duration = Duration::from_millis(100);
const ALT_BASE: u64 = 1 << 40;
/// Match triggers look at the visible screen at most this often (section 3.2).
const TRIG_EVERY: Duration = Duration::from_millis(25);
const MAX_TRIGGERS: usize = 32;
/// Longest line a pattern is matched against, in characters.
const MATCH_CHARS: usize = 4096;
/// Stream messages go out at most this often (section 3.4).
const STREAM_EVERY: Duration = Duration::from_millis(250);
/// Lines waiting for the next stream message; beyond this the oldest are skipped.
const STREAM_BUF_BYTES: usize = 512 << 10;

pub const MIN_COLS: u16 = 2;
pub const MAX_COLS: u16 = 500;
pub const MIN_ROWS: u16 = 1;
pub const MAX_ROWS: u16 = 200;

pub struct Config {
    pub id: String,
    pub name: Option<String>,
    pub dir: PathBuf,
    pub socket: PathBuf,
    pub argv: Vec<String>,
    pub cwd: String,
    pub dest: Dest,
    pub dest_is_default: bool,
    pub output_cap: usize,
    pub quiet_ms: u64,
    /// Send a `settle` message each time the screen settles.
    pub on_settle: bool,
    /// Send `stream` messages with output lines as they are committed.
    pub on_stream: bool,
    /// Clients may broadcast to each other (off with --no-broadcast).
    pub broadcasts: bool,
    /// Keep answering read-only requests this long after the program exits.
    pub linger: Option<Duration>,
    pub started_wall: String,
    /// The descriptor that holds the session lock. The host releases it
    /// itself once the will is out, before it answers `exit`.
    pub lock_fd: RawFd,
}

enum QKind {
    Bytes(Vec<u8>),
    Key {
        key: i32,
        mods: u16,
        text: Option<char>,
        is_esc: bool,
    },
    Paste(Vec<u8>),
    /// A signal for the program. Not input, but it takes its turn in the
    /// queue so that it never lands in the middle of a request's input.
    Signal(i32),
    /// A change of terminal size, in the queue for the same reason.
    Resize {
        cols: u16,
        rows: u16,
        why: Resize,
    },
}

/// Who asked for a resize, which decides what size the session goes back to.
#[derive(Clone, Copy, PartialEq)]
enum Resize {
    /// A resize request: the new size is the one to keep.
    Explicit,
    /// A viewer attached with `--resize`: the old size comes back later.
    Viewer,
    /// That viewer left: back to the old size.
    Restore,
}

impl QKind {
    /// Something that acts on the program without being typed.
    fn is_action(&self) -> bool {
        matches!(self, QKind::Signal(_) | QKind::Resize { .. })
    }
}

struct QItem {
    owner: u64,
    kind: QKind,
    off: usize,
    pause_after: bool,
    /// Wait a moment after the previous input before sending this.
    pause_before: bool,
    /// Send this no sooner than this long after the previous input: a delay
    /// the request asked for.
    gap: Duration,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Until {
    Settle,
    Match,
    Change(u64),
    Exit,
    Broadcast,
    /// This many milliseconds after the request began.
    After(u64),
    None,
}

struct MatchInfo {
    text: String,
    groups: Vec<Value>,
    line: u64,
    row: Option<usize>,
    col: usize,
}

enum Reason {
    Settle,
    Match(MatchInfo),
    Change,
    Exit,
    Broadcast,
    After,
    None,
    Timeout,
}

impl Reason {
    fn as_str(&self) -> &'static str {
        match self {
            Reason::Settle => "settle",
            Reason::Match(_) => "match",
            Reason::Change => "change",
            Reason::Exit => "exit",
            Reason::Broadcast => "broadcast",
            Reason::After => "after",
            Reason::None => "none",
            Reason::Timeout => "timeout",
        }
    }
}

struct Wait {
    conn: u64,
    req_id: u64,
    /// The condition waited for: the first one the request named.
    until: Until,
    /// Any others it named. The request returns when one of them all is met.
    also: Vec<Until>,
    /// For a wait for a broadcast: only one from this sender counts.
    from: Option<String>,
    started: Instant,
    deadline: Instant,
    wants_text: bool,
    keep_alive: bool,
    interleaved: bool,
    /// Running out of time is not a failure: the answer is `none`.
    soft: bool,
    pattern: Option<(Regex, String)>,
    /// Matches that were already there when the request began; they do not
    /// satisfy it.
    known: Vec<(u64, String)>,
    hint: Option<String>,
    /// Return the screen with SGR colour codes left in.
    ansi: bool,
    /// For a request that sends input: the foreground process group
    /// before it was sent.
    fg_before: Option<libc::pid_t>,
    /// Its name, if it was a program inside the session and not the
    /// session's own.
    fg_name_before: Option<String>,
    /// `gen` when this request's first input was written. The answer says
    /// whether the screen has changed since.
    gen_at_typing: Option<u64>,
    /// For `until: exit` when a program inside the session had the
    /// foreground as the request began: its process group and name. The
    /// wait is for that program, not for the whole session.
    exit_of: Option<(libc::pid_t, String)>,
    /// Who is asking, when the request said (`as`). A named client is
    /// given the broadcasts it has not heard.
    name: Option<String>,
    /// The newest broadcast when the request began. A wait for a
    /// broadcast from a client without a name means one after this.
    heard_at_start: u64,
}

impl Wait {
    /// Whether the request waits for this kind of condition, among others.
    fn has(&self, kind: Until) -> bool {
        let same = |u: &Until| std::mem::discriminant(u) == std::mem::discriminant(&kind);
        same(&self.until) || self.also.iter().any(same)
    }

    /// Every condition, in the order the request named them.
    fn conditions(&self) -> Vec<Until> {
        std::iter::once(self.until)
            .chain(self.also.iter().copied())
            .collect()
    }
}

/// A line one client said to the others on the session. It never reaches
/// the program.
struct Broadcast {
    n: u64,
    from: Option<String>,
    text: String,
    t_ms: u64,
    /// How it arrived: `attach` from a viewer's stream, `api` otherwise.
    via: &'static str,
}

impl Broadcast {
    fn json(&self) -> Value {
        json!({"n": self.n, "from": self.from, "text": self.text, "t_ms": self.t_ms, "via": self.via})
    }
}

fn valid_name(s: &str) -> bool {
    (1..=32).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// A standing match trigger (section 3.2). Its matches go to the destination.
struct Trigger {
    id: String,
    re: Regex,
    pattern: String,
    once: bool,
    /// How many times it has fired.
    count: u64,
    /// Matches already reported: the line's absolute row and the matched
    /// text. One is forgotten when it is absent at a settle point.
    known: Vec<(u64, String)>,
}

struct Conn {
    fd: RawFd,
    inbuf: Vec<u8>,
    outbuf: Vec<u8>,
    continue_sent: bool,
    busy: bool,
    close_after: bool,
    wants_out: bool,
    attach: Option<Attach>,
}

struct Logical {
    text: String,
    first_row: usize,
    abs: u64,
}

pub struct Host {
    cfg: Config,
    epoll: Epoll,
    master: RawFd,
    child: Child,
    sigfd: RawFd,
    listener: UnixListener,
    emu: Emu,
    log: Option<File>,
    delivery: Option<Delivery>,
    start: Instant,

    gen: u64,
    screen_hash: u64,
    rows: Vec<Row>,
    last_change: Instant,
    hold_since: Option<Instant>,
    prompt_seen: bool,
    /// The screen has shown text, or input has been sent. Before either, a
    /// still screen is a program that has not started drawing, not a
    /// settled one. Output alone does not count: programs set the terminal
    /// up before they draw.
    seen_output: bool,
    /// The screen has shown text.
    drawn: bool,
    /// Input has been written to the program.
    typed: bool,

    outbuf: OutBuf,
    watermark: Option<Tracked>,
    rows_committed: u64,
    abs_base: u64,
    fresh_lines: Vec<(u64, String)>,

    triggers: Vec<Trigger>,
    next_trigger: u64,
    broadcasts: VecDeque<Broadcast>,
    /// The number of the newest broadcast; they count from 1.
    last_broadcast: u64,
    /// For each client name, the newest broadcast it has been given.
    heard: Vec<(String, u64)>,
    /// Lines that left the screen since the triggers last looked.
    trig_fresh: Vec<(u64, String)>,
    /// `gen` when the triggers last looked at the visible screen.
    trig_gen: u64,
    trig_last: Instant,
    /// The screen has changed since it last settled.
    settle_pending: bool,

    /// Output lines committed so far; the number of the newest one.
    lines_committed: u64,
    stream_buf: VecDeque<String>,
    stream_bytes: usize,
    /// The line number of the first line in `stream_buf`.
    stream_from: u64,
    stream_skipped: u64,
    stream_last: Instant,

    replies: Vec<u8>,
    queue: VecDeque<QItem>,
    pause_until: Option<Instant>,
    last_input: Instant,
    /// `gen` when input was last written, to see the program react to it.
    gen_at_input: u64,
    master_out: bool,
    next_req: u64,

    conns: HashMap<u64, Conn>,
    next_conn: u64,
    waits: Vec<Wait>,
    record: Option<Recorder>,
    /// The program has exited and the will is delivered; only read-only
    /// requests are answered (`--linger`).
    lingering: bool,
    linger_stop: bool,

    pty_eof: bool,
    exit_status: Option<ExitStatus>,
    exited_at: Option<Instant>,
    drain_until: Option<Instant>,
    stopping: Option<(Instant, u8)>,
    kill_reason: Option<&'static str>,
    /// The size to go back to when the last client that resized detaches.
    restore_size: Option<(u16, u16)>,
}

fn ms_since(t: Instant) -> u64 {
    t.elapsed().as_millis() as u64
}

fn trimmed(s: &str) -> &str {
    s.trim_end()
}

impl Host {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cfg: Config,
        master: RawFd,
        child: Child,
        sigfd: RawFd,
        listener: UnixListener,
        emu: Emu,
        log: Option<File>,
        delivery: Delivery,
        record: Option<Recorder>,
        start: Instant,
    ) -> std::io::Result<Host> {
        let epoll = Epoll::new()?;
        epoll.add(master, sys::IN, T_PTY)?;
        epoll.add(child.pidfd, sys::IN, T_PIDFD)?;
        epoll.add(sigfd, sys::IN, T_SIGNAL)?;
        listener.set_nonblocking(true)?;
        epoll.add(listener.as_raw_fd(), sys::IN, T_LISTEN)?;
        let watermark = emu.track(Space::Active, 0);
        let now = Instant::now();
        let outbuf = OutBuf::new(cfg.output_cap);
        let mut host = Host {
            cfg,
            epoll,
            master,
            child,
            sigfd,
            listener,
            emu,
            log,
            delivery: Some(delivery),
            start,
            gen: 0,
            screen_hash: 0,
            rows: Vec::new(),
            last_change: now,
            hold_since: None,
            prompt_seen: false,
            seen_output: false,
            drawn: false,
            typed: false,
            outbuf,
            watermark,
            rows_committed: 0,
            abs_base: 0,
            fresh_lines: Vec::new(),
            triggers: Vec::new(),
            next_trigger: 1,
            broadcasts: VecDeque::new(),
            last_broadcast: 0,
            heard: Vec::new(),
            trig_fresh: Vec::new(),
            trig_gen: 0,
            trig_last: now,
            settle_pending: false,
            lines_committed: 0,
            stream_buf: VecDeque::new(),
            stream_bytes: 0,
            stream_from: 0,
            stream_skipped: 0,
            stream_last: now,
            replies: Vec::new(),
            queue: VecDeque::new(),
            pause_until: None,
            last_input: now,
            gen_at_input: 0,
            master_out: false,
            next_req: 1,
            conns: HashMap::new(),
            next_conn: T_FIRST_CONN,
            waits: Vec::new(),
            record,
            lingering: false,
            linger_stop: false,
            pty_eof: false,
            exit_status: None,
            exited_at: None,
            drain_until: None,
            stopping: None,
            kill_reason: None,
            restore_size: None,
        };
        host.refresh();
        Ok(host)
    }

    fn log(&mut self, msg: &str) {
        if let Some(f) = self.log.as_mut() {
            let _ = writeln!(f, "{} {}", util::rfc3339_now(), msg);
        }
    }

    // ---- the loop -------------------------------------------------------

    /// Serve until the child has exited and its output is drained. Returns
    /// the exit status for the host process.
    pub fn run(mut self) -> i32 {
        loop {
            let timeout = self.next_timeout();
            for (token, ev) in self.epoll.wait(timeout) {
                match token {
                    T_PTY => {
                        if ev & (sys::IN | sys::HUP) != 0 {
                            self.read_pty();
                        }
                    }
                    T_PIDFD => self.on_child_exit(),
                    T_SIGNAL => self.on_signal(),
                    T_LISTEN => self.accept(),
                    id => self.on_conn(id, ev),
                }
            }
            self.pump_input();
            self.tick();
            // Triggers first, so a match is on its way to the destination
            // before a request that saw the same screen is answered.
            self.evaluate_triggers(false);
            self.evaluate_waits();
            self.service_attach();
            if self.ready_to_finish() {
                return self.finish();
            }
        }
    }

    /// Milliseconds until something time-based needs attention.
    fn next_timeout(&mut self) -> i32 {
        let now = Instant::now();
        let mut next: Option<Instant> = None;
        let mut consider = |t: Instant| {
            if next.map(|n| t < n).unwrap_or(true) {
                next = Some(t);
            }
        };
        // A timer is set only for something that can then happen, or the
        // loop would spin: waiting for the first output has no time to
        // wait for.
        let settle_timer = self.settle_timer(now);
        for w in &self.waits {
            consider(w.deadline);
            if let Some(t) = settle_timer.filter(|_| w.has(Until::Settle)) {
                consider(t);
            }
            // A program that exits without drawing anything wakes nothing.
            if w.has(Until::Exit) && w.exit_of.is_some() {
                consider(now + EXIT_POLL);
            }
            for cond in w.conditions() {
                if let Until::After(ms) = cond {
                    consider(w.started + Duration::from_millis(ms));
                }
            }
        }
        let held = self.hold_since.filter(|h| now < *h + HOLD_CAP);
        if !self.triggers.is_empty() && self.trig_gen != self.gen {
            consider(
                held.map(|h| h + HOLD_CAP)
                    .unwrap_or(self.trig_last + TRIG_EVERY),
            );
        }
        if self.settle_pending && (self.cfg.on_settle || !self.triggers.is_empty()) {
            if let Some(t) = settle_timer {
                consider(t);
            }
        }
        if !self.stream_buf.is_empty() {
            consider(self.stream_last + STREAM_EVERY);
        }
        if let Some(p) = self.pause_until {
            consider(p);
        }
        if let Some(h) = self.hold_since {
            if !self.waits.is_empty() || self.has_attach() {
                consider(h + HOLD_CAP);
            }
        }
        if let Some(d) = self.drain_until {
            consider(d);
        }
        if let Some(t) = self.attach_due() {
            consider(t);
        }
        if let Some((since, stage)) = self.stopping {
            if stage < 2 {
                consider(since + KILL_STEP * (stage as u32 + 1));
            }
        }
        match next {
            None => -1,
            Some(t) if t <= now => 0,
            // Round up so the loop does not wake a moment early and spin.
            Some(t) => (t.duration_since(now).as_millis() as i64 + 1).min(60_000) as i32,
        }
    }

    fn tick(&mut self) {
        if let Some((since, stage)) = self.stopping {
            if self.exit_status.is_none()
                && stage < 2
                && since.elapsed() >= KILL_STEP * (stage as u32 + 1)
            {
                let sig = if stage == 0 {
                    libc::SIGTERM
                } else {
                    libc::SIGKILL
                };
                self.signal_child(sig);
                self.stopping = Some((since, stage + 1));
            }
        }
    }

    // ---- pty output -----------------------------------------------------

    fn read_pty(&mut self) {
        if self.pty_eof {
            return;
        }
        let mut buf = [0u8; CHUNK];
        let mut total = 0;
        while total < READ_BATCH {
            match sys::read(self.master, &mut buf) {
                Ok(Some(0)) | Err(_) => {
                    // EIO: every slave fd is closed and the buffer is empty.
                    self.pty_eof = true;
                    self.epoll.del(self.master);
                    break;
                }
                Ok(Some(n)) => {
                    total += n;
                    self.record(|r| r.output(&buf[..n]));
                    self.process_chunk(&buf[..n]);
                }
                Ok(None) => break,
            }
        }
        if total > 0 {
            if self.exit_status.is_some() {
                self.drain_until = Some(Instant::now() + DRAIN_QUIET);
            }
            self.record(|r| r.flush());
            self.refresh();
        }
    }

    fn process_chunk(&mut self, data: &[u8]) {
        self.emu.feed(data);
        let replies = self.emu.take_replies();
        if self.exit_status.is_none() {
            self.replies.extend_from_slice(&replies);
        }
        if self.emu.hold() {
            if self.hold_since.is_none() {
                self.hold_since = Some(Instant::now());
            }
        } else {
            self.hold_since = None;
        }
        if self.emu.take_prompt_start() {
            self.prompt_seen = true;
        }
        self.collect();
    }

    /// Write to the recording, if there is one. A recording that cannot be
    /// written is given up, not the session.
    fn record(&mut self, f: impl FnOnce(&mut Recorder) -> std::io::Result<()>) {
        if let Some(r) = self.record.as_mut() {
            if let Err(e) = f(r) {
                self.record = None;
                self.log(&format!("recording stopped: {e}"));
            }
        }
    }

    /// Move rows that have scrolled into history to the output buffer
    /// (section 6.5).
    fn collect(&mut self) {
        if self.emu.alt_screen() {
            return;
        }
        let sb = self.emu.scrollback_rows() as u32;
        let tracked = self
            .watermark
            .as_ref()
            .and_then(|w| self.emu.tracked_row(w));
        let start = match tracked {
            Some(y) => y,
            None => {
                self.commit_line("[... output lost ...]".into());
                self.watermark = self.emu.track(Space::Screen, 0);
                0
            }
        };
        let mut line_start = start;
        let mut acc = String::new();
        let mut y = start;
        while y < sb {
            let Some(row) = self.emu.row(Space::Screen, y, None) else {
                break;
            };
            acc.push_str(&row.text);
            y += 1;
            // A logical line ends at a row that does not wrap. A line that
            // wraps for thousands of rows is cut so this stays bounded.
            if !row.wrap || y - line_start >= 2000 {
                let abs = self.rows_committed + (line_start - start) as u64;
                let line = std::mem::take(&mut acc);
                let line = trimmed(&line).to_string();
                if self.waits.iter().any(|w| w.has(Until::Match)) {
                    self.fresh_lines.push((abs, line.clone()));
                }
                if !self.triggers.is_empty() {
                    self.trig_fresh.push((abs, line.clone()));
                }
                self.commit_line(line);
                line_start = y;
            }
        }
        if line_start > start {
            if let Some(w) = self.watermark.as_ref() {
                self.emu.tracked_set(w, Space::Screen, line_start);
            }
            self.rows_committed += (line_start - start) as u64;
        }
        self.abs_base = self.rows_committed + sb.saturating_sub(line_start) as u64;
    }

    /// Re-read the visible screen and bump `gen` if it looks different.
    fn refresh(&mut self) {
        let mut h = DefaultHasher::new();
        let rows: Vec<Row> = (0..self.emu.rows() as u32)
            .map(|y| {
                self.emu
                    .row(Space::Active, y, Some(&mut h))
                    .unwrap_or_default()
            })
            .collect();
        let (cx, cy) = self.emu.cursor();
        h.write_u16(cx);
        h.write_u16(cy);
        h.write_u8(self.emu.cursor_visible() as u8);
        h.write_u8(self.emu.alt_screen() as u8);
        h.write_u16(self.emu.cols());
        h.write_u16(self.emu.rows());
        h.write(self.emu.title().as_bytes());
        let hash = h.finish();
        if hash != self.screen_hash || self.gen == 0 {
            self.screen_hash = hash;
            self.gen += 1;
            self.last_change = Instant::now();
            self.settle_pending = true;
        }
        if !self.drawn && rows.iter().any(|r| !r.text.trim().is_empty()) {
            self.drawn = true;
            self.seen_output = true;
        }
        self.rows = rows;
    }

    // ---- reading the screen ----------------------------------------------

    /// The screen object. With `ansi`, rows keep their SGR colour and
    /// attribute codes (section 6.2).
    fn screen_json(&self, ansi: bool) -> Value {
        let styled: Vec<String> = if ansi {
            (0..self.emu.rows() as u32)
                .map(|y| {
                    self.emu
                        .row_ansi(Space::Active, y, 0, self.emu.cols())
                        .unwrap_or_default()
                })
                .collect()
        } else {
            Vec::new()
        };
        let mut lines: Vec<&str> = if ansi {
            styled.iter().map(|s| s.as_str()).collect()
        } else {
            self.rows.iter().map(|r| trimmed(&r.text)).collect()
        };
        while lines.last().map(|l| l.is_empty()).unwrap_or(false) {
            lines.pop();
        }
        let (cx, cy) = self.emu.cursor();
        json!({
            "gen": self.gen,
            "cols": self.emu.cols(),
            "rows": self.emu.rows(),
            "cursor": {"row": cy + 1, "col": cx + 1, "visible": self.emu.cursor_visible()},
            "alt_screen": self.emu.alt_screen(),
            "title": self.emu.title(),
            "text": lines.join("\n"),
        })
    }

    /// The visible screen as logical lines: rows joined where they wrapped.
    /// The cursor's line is padded to the cursor so a prompt that ends in a
    /// space the program never drew still ends in one.
    fn visible_lines(&self) -> Vec<Logical> {
        let base = if self.emu.alt_screen() {
            ALT_BASE
        } else {
            self.abs_base
        };
        let (cx, cy) = self.emu.cursor();
        let mut out = Vec::new();
        let mut acc = String::new();
        let mut first = 0usize;
        for (i, row) in self.rows.iter().enumerate() {
            if acc.is_empty() {
                first = i;
            }
            let mut text = row.text.clone();
            if i == cy as usize {
                let have = text.chars().count();
                if have < cx as usize {
                    text.extend(std::iter::repeat_n(' ', cx as usize - have));
                }
            }
            acc.push_str(&text);
            if !row.wrap || i + 1 == self.rows.len() {
                out.push(Logical {
                    text: std::mem::take(&mut acc),
                    first_row: first,
                    abs: base + first as u64,
                });
            }
        }
        out
    }

    /// The primary screen as output lines, without trailing blank lines.
    /// Empty while a full-screen program is on the alternate screen.
    fn primary_lines(&self) -> Vec<String> {
        if self.emu.alt_screen() {
            return Vec::new();
        }
        let mut lines: Vec<String> = self
            .visible_lines()
            .into_iter()
            .map(|l| trimmed(&l.text).to_string())
            .collect();
        while lines.last().map(|l| l.is_empty()).unwrap_or(false) {
            lines.pop();
        }
        lines
    }

    // ---- input -----------------------------------------------------------

    fn pump_input(&mut self) {
        if self.pty_eof || self.exit_status.is_some() {
            self.queue.clear();
            self.replies.clear();
            self.pause_until = None;
            return;
        }
        let mut blocked = false;
        loop {
            if !self.replies.is_empty() {
                match sys::write(self.master, &self.replies) {
                    Ok(Some(n)) => {
                        self.replies.drain(..n);
                        continue;
                    }
                    Ok(None) => {
                        blocked = true;
                        break;
                    }
                    Err(_) => {
                        self.replies.clear();
                        break;
                    }
                }
            }
            // A signal or resize at the front has waited its turn. It also
            // waits for the program to take in what was typed before it: a
            // signal travels faster than input, and would otherwise overtake
            // the last keys of the request ahead of it.
            if self
                .queue
                .front()
                .map(|q| q.kind.is_action())
                .unwrap_or(false)
            {
                if !self.input_absorbed() {
                    self.pause_until = Some(Instant::now() + KEY_POLL);
                    break;
                }
                self.pause_until = None;
                if let Some(item) = self.queue.pop_front() {
                    self.apply_action(item);
                }
                continue;
            }
            if let Some(p) = self.pause_until {
                if Instant::now() < p {
                    break;
                }
                self.pause_until = None;
            }
            let absorbed = self.input_absorbed();
            let Some(front) = self.queue.front_mut() else {
                break;
            };
            if !self.typed {
                let ready = self.drawn
                    && self.last_change.elapsed() >= Duration::from_millis(self.cfg.quiet_ms);
                if !ready && Instant::now() < self.start + FIRST_DRAW_MAX {
                    self.pause_until = Some(Instant::now() + KEY_POLL);
                    break;
                }
            }
            if !front.gap.is_zero() {
                if self.typed && self.last_input.elapsed() < front.gap {
                    self.pause_until = Some(self.last_input + front.gap);
                    break;
                }
                front.gap = Duration::ZERO;
            }
            if front.pause_before {
                if !absorbed {
                    self.pause_until = Some(Instant::now() + KEY_POLL);
                    break;
                }
                front.pause_before = false;
            }
            // Keys and pastes are encoded when they reach the front, so they
            // follow the modes the program has set by then.
            match &front.kind {
                QKind::Bytes(_) => {}
                QKind::Key {
                    key,
                    mods,
                    text,
                    is_esc,
                } => {
                    let (key, mods, text, is_esc) = (*key, *mods, *text, *is_esc);
                    let mut bytes = self.emu.encode_key(key, mods, text);
                    if bytes.is_empty() {
                        bytes = keys::legacy(mods, text);
                    }
                    front.pause_after = is_esc && self.emu.kitty_keyboard_flags() == 0;
                    front.kind = QKind::Bytes(bytes);
                }
                QKind::Paste(data) => {
                    let bytes = self.emu.encode_paste(data);
                    front.kind = QKind::Bytes(bytes);
                }
                QKind::Signal(_) | QKind::Resize { .. } => unreachable!(),
            }
            let QKind::Bytes(bytes) = &front.kind else {
                unreachable!()
            };
            if front.off < bytes.len() {
                match sys::write(self.master, &bytes[front.off..]) {
                    Ok(Some(n)) => {
                        front.off += n;
                        let owner = front.owner;
                        self.last_input = Instant::now();
                        self.gen_at_input = self.gen;
                        self.seen_output = true;
                        self.typed = true;
                        self.prompt_seen = false;
                        let gen = self.gen;
                        for w in self.waits.iter_mut() {
                            if w.req_id != owner {
                                w.interleaved = true;
                            } else if w.gen_at_typing.is_none() {
                                w.gen_at_typing = Some(gen);
                            }
                        }
                    }
                    Ok(None) => {
                        blocked = true;
                        break;
                    }
                    Err(_) => {
                        self.queue.clear();
                        break;
                    }
                }
            }
            let done = match self.queue.front() {
                Some(QItem {
                    kind: QKind::Bytes(b),
                    off,
                    ..
                }) => *off >= b.len(),
                _ => false,
            };
            if done {
                if let Some(item) = self.queue.pop_front() {
                    if item.pause_after {
                        self.pause_until = Some(Instant::now() + ESC_PAUSE);
                    }
                }
            }
        }
        if blocked {
            // The program is not reading its input, so the queue is stuck
            // and may stay so. Signals and resizes do not wait behind it:
            // an interrupt is most needed exactly then.
            let mut rest = VecDeque::with_capacity(self.queue.len());
            for item in std::mem::take(&mut self.queue) {
                if item.kind.is_action() {
                    self.apply_action(item);
                } else {
                    rest.push_back(item);
                }
            }
            rest.append(&mut self.queue);
            self.queue = rest;
        }
        if blocked != self.master_out {
            self.master_out = blocked;
            let ev = if blocked { sys::IN | sys::OUT } else { sys::IN };
            let _ = self.epoll.modify(self.master, ev, T_PTY);
        }
    }

    /// Whether the program has taken in the input written so far: nothing
    /// has been typed, or the screen has reacted to it and gone still, or
    /// it has had long enough (a program that echoes nothing).
    fn input_absorbed(&self) -> bool {
        if !self.typed {
            return true;
        }
        let since = self.last_input.elapsed();
        let reacted = self.gen != self.gen_at_input && self.last_change.elapsed() >= KEY_STILL;
        since >= KEY_MAX || (since >= KEY_MIN && reacted)
    }

    /// Carry out a queued signal or resize.
    fn apply_action(&mut self, item: QItem) {
        match item.kind {
            QKind::Signal(sig) => self.signal_child(sig),
            QKind::Resize { cols, rows, why } => {
                let before = (self.emu.cols(), self.emu.rows());
                if why == Resize::Explicit || before != (cols, rows) {
                    if let Err(e) = self.do_resize(cols, rows) {
                        self.log(&format!("resize to {cols}x{rows} failed: {e}"));
                    } else if why == Resize::Viewer {
                        self.restore_size.get_or_insert(before);
                    }
                }
                if why == Resize::Explicit {
                    // An explicit resize is the new size to keep.
                    self.restore_size = None;
                }
                // The wait for the redraw starts now, not when it was asked.
                let now = Instant::now();
                for w in self
                    .waits
                    .iter_mut()
                    .filter(|w| w.req_id == item.owner && w.soft)
                {
                    w.deadline = now + RESIZE_WAIT;
                }
            }
            _ => return,
        }
        for w in self.waits.iter_mut() {
            if w.req_id != item.owner {
                w.interleaved = true;
            }
        }
    }

    /// Whether a request's signal or resize is still waiting its turn.
    fn action_pending(&self, owner: u64) -> bool {
        self.queue
            .iter()
            .any(|q| q.owner == owner && q.kind.is_action())
    }

    fn input_idle(&self) -> bool {
        self.queue.is_empty() && self.replies.is_empty() && self.pause_until.is_none()
    }

    // ---- settle and waits -------------------------------------------------

    fn quiet(&self) -> Duration {
        Duration::from_millis(if self.prompt_seen {
            PROMPT_QUIET_MS.min(self.cfg.quiet_ms)
        } else {
            self.cfg.quiet_ms
        })
    }

    fn settle_at(&self) -> Instant {
        self.last_change.max(self.last_input) + self.quiet()
    }

    fn held(&self, now: Instant) -> bool {
        self.hold_since
            .map(|h| now.duration_since(h) < HOLD_CAP)
            .unwrap_or(false)
    }

    fn settled(&self, now: Instant) -> bool {
        self.seen_output && self.input_idle() && !self.held(now) && now >= self.settle_at()
    }

    /// When the screen will count as settled if nothing else happens. None
    /// while that waits on something other than time: the first output, or
    /// input still to be written.
    fn settle_timer(&self, now: Instant) -> Option<Instant> {
        if !self.seen_output || !self.input_idle() {
            return None;
        }
        let at = self.settle_at();
        match self.hold_since.map(|h| h + HOLD_CAP) {
            Some(cap) if now < cap => Some(at.max(cap)),
            _ => Some(at),
        }
    }

    // ---- triggers, settle and stream messages (sections 3 and 4) ---------

    fn send(&self, kind: Kind, fields: Map<String, Value>) {
        if let Some(d) = self.delivery.as_ref() {
            d.send(kind, fields);
        }
    }

    /// A line has left the screen for good: it joins the output, and the
    /// stream if that is on.
    fn commit_line(&mut self, line: String) {
        self.lines_committed += 1;
        if self.cfg.on_stream {
            self.stream_push(line.clone(), self.lines_committed);
        }
        self.outbuf.push(line);
    }

    fn stream_push(&mut self, line: String, number: u64) {
        if self.stream_buf.is_empty() {
            self.stream_from = number;
            // After a quiet spell, wait a moment for the lines that follow
            // the first, so they travel together.
            let now = Instant::now();
            if self.stream_last + STREAM_EVERY < now {
                self.stream_last = now;
            }
        }
        self.stream_bytes += line.len() + 1;
        self.stream_buf.push_back(line);
        while self.stream_bytes > STREAM_BUF_BYTES && self.stream_buf.len() > 1 {
            if let Some(old) = self.stream_buf.pop_front() {
                self.stream_bytes -= old.len() + 1;
                self.stream_from += 1;
                self.stream_skipped += 1;
            }
        }
    }

    fn flush_stream(&mut self, now: Instant) {
        if self.stream_buf.is_empty() {
            return;
        }
        let lines: Vec<String> = self.stream_buf.drain(..).collect();
        let mut fields = Map::new();
        fields.insert("from_line".into(), self.stream_from.into());
        fields.insert("lines".into(), lines.into());
        if self.stream_skipped > 0 {
            fields.insert("skipped_lines".into(), self.stream_skipped.into());
        }
        self.stream_bytes = 0;
        self.stream_skipped = 0;
        self.stream_last = now;
        self.send(Kind::Stream, fields);
    }

    /// Run the match triggers, and send the settle and stream messages that
    /// are due. `last` is the final call, after the child has exited: it
    /// looks at the screen regardless of the 25 ms limit.
    fn evaluate_triggers(&mut self, last: bool) {
        let now = Instant::now();
        let at_settle = self.settle_pending && self.settled(now);
        if self.triggers.is_empty() {
            self.trig_fresh.clear();
        } else {
            let due = self.trig_gen != self.gen && now >= self.trig_last + TRIG_EVERY;
            let look = last || (!self.held(now) && (at_settle || due));
            let lines = if look {
                self.visible_lines()
            } else {
                Vec::new()
            };
            let fresh = std::mem::take(&mut self.trig_fresh);
            let committed = self.rows_committed;
            let mut triggers = std::mem::take(&mut self.triggers);
            let mut fired: Vec<(usize, MatchInfo)> = Vec::new();
            for (i, t) in triggers.iter_mut().enumerate() {
                // Lines that scrolled off, each seen once here. One that
                // matched while it was on screen is already known.
                for (abs, line) in &fresh {
                    if let Some(m) = line_match(&t.re, *abs, line) {
                        if !t.known.iter().any(|k| k.0 == m.line && k.1 == m.text) {
                            fired.push((i, m));
                        }
                    }
                }
                // Rows that are now output cannot be on screen again.
                t.known.retain(|k| k.0 >= ALT_BASE || k.0 >= committed);
                if look {
                    let found = self.matches_now(&t.re, &lines);
                    if at_settle {
                        t.known
                            .retain(|k| found.iter().any(|f| f.line == k.0 && f.text == k.1));
                    }
                    for f in found {
                        if !t.known.iter().any(|k| k.0 == f.line && k.1 == f.text) {
                            t.known.push((f.line, f.text.clone()));
                            fired.push((i, f));
                        }
                    }
                }
            }
            if look {
                self.trig_gen = self.gen;
                self.trig_last = now;
            }
            let screen = if fired.is_empty() {
                Value::Null
            } else {
                self.screen_json(false)
            };
            let mut spent: Vec<usize> = Vec::new();
            for (i, m) in fired {
                let t = &mut triggers[i];
                if t.once && t.count > 0 {
                    continue;
                }
                t.count += 1;
                let mut mj = json!({
                    "id": t.id,
                    "pattern": t.pattern,
                    "text": m.text,
                    "groups": m.groups,
                    "count": t.count,
                    "line": m.line,
                    "on_screen": m.row.is_some(),
                });
                if let Some(row) = m.row {
                    mj["row"] = row.into();
                    mj["col"] = m.col.into();
                }
                if t.once {
                    spent.push(i);
                }
                let mut fields = Map::new();
                fields.insert("match".into(), mj);
                fields.insert("screen".into(), screen.clone());
                self.send(Kind::Match, fields);
            }
            for i in spent.into_iter().rev() {
                triggers.remove(i);
            }
            self.triggers = triggers;
        }

        if at_settle {
            self.settle_pending = false;
            if self.cfg.on_settle {
                let mut fields = Map::new();
                fields.insert("screen".into(), self.screen_json(false));
                self.send(Kind::Settle, fields);
            }
        }
        if last || now >= self.stream_last + STREAM_EVERY {
            self.flush_stream(now);
        }
    }

    fn trigger_json(t: &Trigger) -> Value {
        json!({"id": t.id, "pattern": t.pattern, "mode": if t.once { "once" } else { "edge" }, "count": t.count})
    }

    fn triggers_reply(&mut self, id: u64, req: &Request, extra: Option<(&str, Value)>) {
        let mut v = json!({
            "v": 1,
            "session": self.cfg.id,
            "dest": self.cfg.dest.uri(),
            "triggers": self.triggers.iter().map(Host::trigger_json).collect::<Vec<_>>(),
        });
        if let Some((k, x)) = extra {
            v[k] = x;
        }
        self.reply(id, 200, &v, false, req.keep_alive);
    }

    /// The broadcasts a request has not been given: for a named client,
    /// those after the last it heard, leaving out its own; otherwise those
    /// made since the request began.
    fn unheard(&self, w: &Wait) -> Vec<Value> {
        let after = match w.name.as_ref() {
            Some(name) => self
                .heard
                .iter()
                .find(|(n, _)| n == name)
                .map(|h| h.1)
                .unwrap_or(0),
            None => w.heard_at_start,
        };
        self.broadcasts
            .iter()
            .filter(|b| b.n > after && (w.name.is_none() || b.from != w.name))
            .map(Broadcast::json)
            .collect()
    }

    /// Whether there is a broadcast that satisfies a wait for one: one the
    /// request has not been given and, if it named a sender, from them.
    fn spoken_to(&self, w: &Wait) -> bool {
        self.unheard(w).iter().any(|b| {
            w.from.is_none() || b.get("from").and_then(|f| f.as_str()) == w.from.as_deref()
        })
    }

    fn handle_broadcast_list(&mut self, id: u64, req: &Request) {
        if !self.cfg.broadcasts {
            return self.error(id, req, 409, "broadcasts_off", BROADCASTS_OFF);
        }
        let since = req
            .query_get("since")
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0);
        let v = json!({
            "v": 1,
            "session": self.cfg.id,
            "last": self.last_broadcast,
            "broadcasts": self.broadcasts.iter().filter(|b| b.n > since).map(Broadcast::json).collect::<Vec<_>>(),
        });
        self.reply(id, 200, &v, false, req.keep_alive);
    }

    fn handle_broadcast(&mut self, id: u64, req: &Request) {
        if !self.cfg.broadcasts {
            return self.error(id, req, 409, "broadcasts_off", BROADCASTS_OFF);
        }
        let Some(body) = self.body_json(id, req) else {
            return;
        };
        let named = req
            .query_get("as")
            .map(|s| Value::from(s.to_string()))
            .or_else(|| body.get("as").cloned());
        let from = match named {
            None | Some(Value::Null) => None,
            Some(v) => match v.as_str().filter(|s| valid_name(s)) {
                Some(s) => Some(s.to_string()),
                None => return self.error(id, req, 400, "bad_name", NAME_RULE),
            },
        };
        let Some(text) = body
            .get("text")
            .and_then(|t| t.as_str())
            .map(|t| t.trim())
            .filter(|t| !t.is_empty())
        else {
            return self.error(
                id,
                req,
                400,
                "missing_text",
                "the body needs the text to broadcast, such as {\"text\": \"the shell is yours\", \"as\": \"ann\"}",
            );
        };
        if text.len() > MAX_BROADCAST_BYTES || text.chars().any(|c| c.is_control()) {
            return self.error(
                id,
                req,
                400,
                "bad_text",
                &format!("a broadcast is one line of at most {MAX_BROADCAST_BYTES} bytes, with no control characters"),
            );
        }
        let text = text.to_string();
        let said = self.post_broadcast(from, text, "api");
        let v = json!({"v": 1, "session": self.cfg.id, "broadcast": said});
        self.reply(id, 200, &v, false, req.keep_alive);
    }

    /// Keep a broadcast, send it to the destination, and queue it for
    /// every attached viewer.
    fn post_broadcast(&mut self, from: Option<String>, text: String, via: &'static str) -> Value {
        self.last_broadcast += 1;
        let b = Broadcast {
            n: self.last_broadcast,
            from,
            text,
            t_ms: ms_since(self.start),
            via,
        };
        let said = b.json();
        if self.broadcasts.len() >= MAX_BROADCASTS {
            self.broadcasts.pop_front();
        }
        self.broadcasts.push_back(b);
        let mut fields = Map::new();
        fields.insert("broadcast".into(), said.clone());
        self.send(Kind::Broadcast, fields);
        for c in self.conns.values_mut() {
            if let Some(a) = c.attach.as_mut() {
                a.said.push(said.clone());
                // A frame is due even if the screen has not changed.
                a.key = None;
            }
        }
        said
    }

    fn handle_trigger_add(&mut self, id: u64, req: &Request) {
        let Some(body) = self.body_json(id, req) else {
            return;
        };
        let Some(pattern) = body.get("pattern").and_then(|p| p.as_str()) else {
            return self.error(
                id,
                req,
                400,
                "missing_pattern",
                "the body needs a pattern, such as {\"pattern\": \"[Pp]assword: $\"}: a regular expression matched against one line",
            );
        };
        let re = match regex_lite::RegexBuilder::new(pattern)
            .size_limit(1 << 20)
            .build()
        {
            Ok(re) => re,
            Err(e) => {
                return self.error(
                    id,
                    req,
                    400,
                    "bad_pattern",
                    &format!("the pattern is not a valid regular expression: {e}"),
                )
            }
        };
        let once = match body.get("mode").and_then(|m| m.as_str()) {
            None | Some("edge") => false,
            Some("once") => true,
            Some(other) => {
                return self.error(
                    id,
                    req,
                    400,
                    "unknown_mode",
                    &format!("unknown mode '{other}'; use edge or once"),
                )
            }
        };
        let name = match body.get("id") {
            None | Some(Value::Null) => loop {
                let candidate = format!("t{}", self.next_trigger);
                self.next_trigger += 1;
                if !self.triggers.iter().any(|t| t.id == candidate) {
                    break candidate;
                }
            },
            Some(v) => match v.as_str().filter(|s| {
                (1..=32).contains(&s.len())
                    && s.bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
            }) {
                Some(s) => s.to_string(),
                None => {
                    return self.error(
                        id,
                        req,
                        400,
                        "bad_id",
                        "a trigger id is 1 to 32 characters from a-z, A-Z, 0-9, _ and -",
                    );
                }
            },
        };
        if self.triggers.iter().any(|t| t.id == name) {
            return self.error(
                id,
                req,
                409,
                "trigger_exists",
                &format!("there is already a trigger '{name}'; remove it with 'waldo-term match rm {name}', or pick another id"),
            );
        }
        if self.triggers.len() >= MAX_TRIGGERS {
            return self.error(
                id,
                req,
                409,
                "too_many_triggers",
                &format!("a session takes at most {MAX_TRIGGERS} triggers; remove one with 'waldo-term match rm ID'"),
            );
        }
        let t = Trigger {
            id: name,
            re,
            pattern: pattern.to_string(),
            once,
            count: 0,
            known: Vec::new(),
        };
        let added = Host::trigger_json(&t);
        self.triggers.push(t);
        // Look at the screen for it straight away: text that is already
        // there is a match.
        self.trig_gen = 0;
        self.triggers_reply(id, req, Some(("added", added)));
    }

    fn handle_trigger_remove(&mut self, id: u64, req: &Request, name: &str) {
        let Some(i) = self.triggers.iter().position(|t| t.id == name) else {
            let have: Vec<&str> = self.triggers.iter().map(|t| t.id.as_str()).collect();
            let list = if have.is_empty() {
                "there are none".to_string()
            } else {
                format!("there are: {}", have.join(", "))
            };
            return self.error(
                id,
                req,
                404,
                "no_such_trigger",
                &format!("no trigger '{name}'; {list}"),
            );
        };
        let removed = Host::trigger_json(&self.triggers.remove(i));
        self.triggers_reply(id, req, Some(("removed", removed)));
    }

    /// Matches of a pattern on the visible screen.
    fn matches_now(&self, re: &Regex, lines: &[Logical]) -> Vec<MatchInfo> {
        let cols = self.emu.cols().max(1) as usize;
        let mut out = Vec::new();
        for l in lines {
            let subject: String = l.text.chars().take(4096).collect();
            if let Some(caps) = re.captures(&subject) {
                let m = caps.get(0).unwrap();
                let col0 = subject[..m.start()].chars().count();
                out.push(MatchInfo {
                    text: m.as_str().to_string(),
                    groups: (1..caps.len())
                        .map(|i| {
                            caps.get(i)
                                .map(|g| Value::from(g.as_str()))
                                .unwrap_or(Value::Null)
                        })
                        .collect(),
                    line: l.abs,
                    row: Some(l.first_row + col0 / cols + 1),
                    col: col0 % cols + 1,
                });
            }
        }
        out
    }

    fn check_match(&self, w: &mut Wait, lines: &[Logical]) -> Option<MatchInfo> {
        let (re, _) = w.pattern.as_ref()?;
        // Lines that scrolled off since the last look.
        for (abs, line) in &self.fresh_lines {
            if let Some(caps) = re.captures(line) {
                let m = caps.get(0).unwrap();
                let key = (*abs, m.as_str().to_string());
                if !w.known.contains(&key) {
                    return Some(MatchInfo {
                        text: key.1,
                        groups: (1..caps.len())
                            .map(|i| {
                                caps.get(i)
                                    .map(|g| Value::from(g.as_str()))
                                    .unwrap_or(Value::Null)
                            })
                            .collect(),
                        line: *abs,
                        row: None,
                        col: line[..m.start()].chars().count() + 1,
                    });
                }
            }
        }
        let found = self.matches_now(re, lines);
        // A match that was there at the start stops counting as known once it
        // is gone, so the same text appearing again later is a new match.
        w.known
            .retain(|k| found.iter().any(|f| f.line == k.0 && f.text == k.1));
        found
            .into_iter()
            .find(|f| !w.known.contains(&(f.line, f.text.clone())))
    }

    fn evaluate_waits(&mut self) {
        if self.waits.is_empty() {
            self.fresh_lines.clear();
            return;
        }
        let now = Instant::now();
        let settled = self.settled(now);
        let lines = if self.waits.iter().any(|w| w.has(Until::Match)) {
            self.visible_lines()
        } else {
            Vec::new()
        };
        let mut waits = std::mem::take(&mut self.waits);
        let mut keep = Vec::with_capacity(waits.len());
        for mut w in waits.drain(..) {
            // A request is not answered while input of its own is still to
            // be typed. Otherwise a pattern that matches the echo of its
            // text would answer it before its Enter had been sent, and
            // that Enter would land in someone else's request.
            let typing = w.until != Until::None && owner_has_input(&self.queue, w.req_id);
            // The first condition that is met answers the request, in the
            // order the request named them.
            let mut reason = None;
            for cond in w.conditions() {
                reason = match cond {
                    _ if typing => None,
                    // A signal or resize is answered once it has happened.
                    Until::None if !self.action_pending(w.req_id) => Some(Reason::None),
                    Until::Settle if settled => Some(Reason::Settle),
                    Until::Change(g) if self.gen != g => Some(Reason::Change),
                    Until::Broadcast if self.spoken_to(&w) => Some(Reason::Broadcast),
                    Until::After(ms) if ms_since(w.started) >= ms => Some(Reason::After),
                    // The program inside the session that was being typed
                    // into is gone, and what replaced it has finished
                    // drawing.
                    Until::Exit
                        if settled
                            && w.exit_of
                                .as_ref()
                                .map(|(pg, _)| !sys::group_exists(*pg))
                                .unwrap_or(false) =>
                    {
                        Some(Reason::Exit)
                    }
                    Until::Match => self.check_match(&mut w, &lines).map(Reason::Match),
                    _ => None,
                };
                if reason.is_some() {
                    break;
                }
            }
            let reason = reason.or_else(|| {
                (now >= w.deadline).then_some(if w.soft && !self.action_pending(w.req_id) {
                    Reason::None
                } else {
                    Reason::Timeout
                })
            });
            match reason {
                Some(r) => self.respond_wait(w, r),
                None => keep.push(w),
            }
        }
        // Responding can start the next request on a connection, which may
        // add waits; keep those too.
        keep.append(&mut self.waits);
        self.waits = keep;
        self.fresh_lines.clear();
    }

    fn child_json(&self) -> Value {
        if self.exit_status.is_some() {
            return json!({"state": "exited", "pid": self.child.pid});
        }
        let fg = sys::foreground_pgrp(self.master).and_then(sys::process_name);
        json!({"state": "running", "pid": self.child.pid, "foreground": fg})
    }

    /// The fields that describe how the child ended (PROTOCOL.md section 8.6).
    fn exit_fields(&self, output: Value) -> Map<String, Value> {
        let st = self.exit_status.unwrap_or(ExitStatus {
            code: None,
            signal: None,
            core_dumped: false,
        });
        let mut m = Map::new();
        m.insert(
            "exit_code".into(),
            st.code.map(Value::from).unwrap_or(Value::Null),
        );
        m.insert(
            "signal".into(),
            st.signal
                .map(|s| Value::from(sys::signal_name(s)))
                .unwrap_or(Value::Null),
        );
        m.insert("core_dumped".into(), st.core_dumped.into());
        m.insert("killed_by_waldo".into(), self.kill_reason.is_some().into());
        m.insert(
            "kill_reason".into(),
            self.kill_reason.map(Value::from).unwrap_or(Value::Null),
        );
        m.insert("spawn_error".into(), Value::Null);
        let end = self.exited_at.unwrap_or_else(Instant::now);
        m.insert(
            "duration_ms".into(),
            (end.duration_since(self.start).as_millis() as u64).into(),
        );
        m.insert("output".into(), output);
        m
    }

    fn respond_wait(&mut self, w: Wait, reason: Reason) {
        let mut resp = Map::new();
        resp.insert("v".into(), 1.into());
        resp.insert("session".into(), self.cfg.id.clone().into());
        resp.insert("reason".into(), reason.as_str().into());
        resp.insert("waited_ms".into(), ms_since(w.started).into());
        resp.insert("interleaved".into(), w.interleaved.into());
        if let Some(before) = w.gen_at_typing {
            // Only for a request that typed: did anything on screen change
            // since its first input was written?
            resp.insert("changed".into(), (self.gen != before).into());
        }
        resp.insert("child".into(), self.child_json());
        resp.insert("screen".into(), self.screen_json(w.ansi));
        // What others on the session have said that this client has not
        // been given yet. Only a client that names itself is given them,
        // and a wait for a broadcast.
        if w.name.is_some() || w.has(Until::Broadcast) {
            let said = self.unheard(&w);
            if !said.is_empty() {
                resp.insert("broadcasts".into(), Value::Array(said));
            }
            if let Some(name) = w.name.as_ref() {
                let newest = self.last_broadcast;
                match self.heard.iter_mut().find(|(n, _)| n == name) {
                    Some(h) => h.1 = newest,
                    None => {
                        if self.heard.len() >= MAX_NAMES {
                            self.heard.remove(0);
                        }
                        self.heard.push((name.clone(), newest));
                    }
                }
            }
        }

        let mut hint = w.hint.clone();
        match &reason {
            Reason::Match(m) => {
                let mut mj = json!({
                    "id": Value::Null,
                    "pattern": w.pattern.as_ref().map(|p| p.1.clone()),
                    "text": m.text,
                    "groups": m.groups,
                    "count": 1,
                    "line": m.line,
                    "on_screen": m.row.is_some(),
                });
                if let Some(row) = m.row {
                    mj["row"] = row.into();
                    mj["col"] = m.col.into();
                }
                resp.insert("match".into(), mj);
            }
            // The session's own program has exited. When it was a program
            // inside the session that exited, there is nothing to report
            // about it beyond the reason.
            Reason::Exit if self.exit_status.is_some() => {
                let tail = self.outbuf.render(&self.primary_lines(), Some(40));
                resp.insert("exit".into(), Value::Object(self.exit_fields(tail)));
            }
            Reason::Settle => {
                // Only for the request whose input started the program. Once
                // it is the thing being driven, the hint would be on every
                // screen.
                if hint.is_none() && w.fg_before.is_some() {
                    let fg = sys::foreground_pgrp(self.master);
                    if let Some(pg) =
                        fg.filter(|pg| *pg != self.child.pid && Some(*pg) != w.fg_before)
                    {
                        let name = sys::process_name(pg).unwrap_or_else(|| "a command".into());
                        hint = Some(format!(
                            "'{name}' is still running; settle only means the screen went quiet. To wait for it to end, see 'waldo-term help shell'."
                        ));
                    } else if let Some(was) = w
                        .fg_name_before
                        .as_ref()
                        .filter(|_| fg == Some(self.child.pid))
                    {
                        // The mirror case: the program the input was meant
                        // for has left the foreground during the request.
                        let now = sys::process_name(self.child.pid)
                            .unwrap_or_else(|| "the session's program".into());
                        hint = Some(format!(
                            "'{was}' is no longer in the foreground; input now goes to '{now}'."
                        ));
                    }
                }
            }
            Reason::Timeout if hint.is_none() => {
                hint = match w.until {
                    Until::Broadcast => Some("nobody else on the session broadcast anything in that time.".into()),
                    Until::Settle if !self.seen_output => {
                        Some("the program has not drawn anything yet; settle waits for the first text on screen.".into())
                    }
                    Until::Settle if self.last_change.elapsed() < Duration::from_secs(2) => {
                        Some("the screen never went quiet; wait for specific text with --until match.".into())
                    }
                    Until::Match if !w.known.is_empty() => Some(
                        "the pattern matches text that was on screen before the input was sent, and nothing new matched it; a request that sends input waits for a new match. To accept text that is already there, use 'waldo-term wait --until match'.".into(),
                    ),
                    Until::Match => Some("the pattern did not appear; it is matched against one line at a time. Check the screen above for what is there.".into()),
                    Until::Exit => {
                        let fg = sys::foreground_pgrp(self.master).and_then(sys::process_name).unwrap_or_else(|| "the program".into());
                        Some(match w.exit_of.as_ref() {
                            Some((_, name)) => format!("'{name}' has not exited. --until exit waits for the program that had the foreground when the request began."),
                            None => format!(
                                "the session has not ended: '{fg}' is in the foreground. --until exit waits for the program you are typing into, and that was the session's own ({}); to wait for a command started from it, wait for its output or a marker (waldo-term help shell).",
                                self.cfg.argv.first().map(|s| s.as_str()).unwrap_or("?")
                            ),
                        })
                    }
                    _ => None,
                };
            }
            _ => {}
        }
        if let Some(h) = hint {
            resp.insert("hint".into(), h.into());
        }
        let value = Value::Object(resp);
        self.reply(w.conn, 200, &value, w.wants_text, w.keep_alive);
    }

    // ---- the child and signals -------------------------------------------

    fn signal_child(&mut self, sig: i32) {
        // The foreground job has its own process group when the child is a
        // shell with job control, so signal both.
        unsafe { libc::kill(-self.child.pid, sig) };
        if let Some(pg) = sys::foreground_pgrp(self.master).filter(|pg| *pg != self.child.pid) {
            unsafe { libc::kill(-pg, sig) };
        }
    }

    fn stop(&mut self, reason: &'static str) {
        if self.stopping.is_some() || self.exit_status.is_some() {
            return;
        }
        self.log(&format!("stopping the child ({reason})"));
        self.kill_reason = Some(reason);
        self.stopping = Some((Instant::now(), 0));
        self.signal_child(libc::SIGHUP);
    }

    fn on_signal(&mut self) {
        while let Some(sig) = sys::read_signal(self.sigfd) {
            self.log(&format!("received {}", sys::signal_name(sig)));
            self.stop("host_signal");
        }
    }

    fn on_child_exit(&mut self) {
        if self.exit_status.is_some() {
            return;
        }
        if let Some(st) = sys::reap(self.child.pid) {
            self.epoll.del(self.child.pidfd);
            self.exit_status = Some(st);
            let now = Instant::now();
            self.exited_at = Some(now);
            self.drain_until = Some(now + DRAIN_QUIET);
            // Read what is already buffered before deciding the pty is quiet.
            self.read_pty();
        }
    }

    fn ready_to_finish(&self) -> bool {
        let Some(exited) = self.exited_at else {
            return false;
        };
        let now = Instant::now();
        self.pty_eof
            || self.drain_until.map(|d| now >= d).unwrap_or(true)
            || now.duration_since(exited) >= DRAIN_CAP
    }

    /// The child is gone and its output is drained: send the will, answer
    /// whoever is waiting, clean up.
    fn finish(mut self) -> i32 {
        self.refresh();
        self.collect();
        let st = self.exit_status.unwrap_or(ExitStatus {
            code: Some(1),
            signal: None,
            core_dumped: false,
        });
        self.log(&format!(
            "child exited: code={:?} signal={:?}",
            st.code, st.signal
        ));

        // Whatever the last output matched, and the end of the stream: the
        // lines still on screen never scroll off, so they are sent now.
        if self.cfg.on_stream {
            let mut number = self.lines_committed;
            for line in self.primary_lines() {
                number += 1;
                self.stream_push(line, number);
            }
        }
        self.evaluate_triggers(true);

        let output = self.outbuf.render(&self.primary_lines(), None);
        let mut will = self.exit_fields(output);
        will.insert("screen".into(), self.screen_json(false));

        // The will goes out first, so a client that sees `exit` can read it.
        let (delivered, will_line) = match self.delivery.take() {
            Some(d) => d.deliver_will(will),
            None => (false, String::new()),
        };
        if !delivered {
            self.log("writing the will to will.json");
            let _ = std::fs::write(self.cfg.dir.join("will.json"), format!("{will_line}\n"));
        }

        self.end_attach();
        self.record(|r| r.flush());
        self.record = None;
        if let Some(time) = self.cfg.linger {
            // A lingering session still counts as running, so those waiting
            // are answered now and the lock is kept until it is over.
            for w in std::mem::take(&mut self.waits) {
                self.respond_wait(w, Reason::Exit);
            }
            self.flush_all(Duration::from_secs(1));
            self.linger(time);
        }

        let _ = std::fs::remove_file(&self.cfg.socket);
        if delivered && !self.cfg.dest_is_default {
            // Nothing in the directory is needed any more.
            for f in ["meta.json", "log", "lock"] {
                let _ = std::fs::remove_file(self.cfg.dir.join(f));
            }
            let _ = std::fs::remove_dir(&self.cfg.dir);
        }
        // The session is over once the lock is free, and whoever is told
        // `exit` may act on that at once (`gc`, a new session of the same
        // name). So the lock goes first and the answers after it.
        unsafe { libc::flock(self.cfg.lock_fd, libc::LOCK_UN) };
        for w in std::mem::take(&mut self.waits) {
            self.respond_wait(w, Reason::Exit);
        }
        self.flush_all(Duration::from_secs(1));
        match (st.code, st.signal) {
            (Some(c), _) => c,
            (None, Some(s)) => 128 + s,
            _ => 1,
        }
    }

    /// `--linger`: the program is gone and the will is delivered. Keep the
    /// socket open for a while so the last screen and the output can still
    /// be read. A stop request or a signal to the host ends it early.
    fn linger(&mut self, time: Duration) {
        self.lingering = true;
        self.log(&format!("lingering for {} s", time.as_secs()));
        if !self.pty_eof {
            // A grandchild may still hold the pty; its output is no longer read.
            self.epoll.del(self.master);
        }
        let end = Instant::now() + time;
        while !self.linger_stop {
            let now = Instant::now();
            if now >= end {
                break;
            }
            let ms = (end.duration_since(now).as_millis() as i64 + 1).min(60_000) as i32;
            for (token, ev) in self.epoll.wait(ms) {
                match token {
                    T_PTY | T_PIDFD => {}
                    T_SIGNAL => {
                        while sys::read_signal(self.sigfd).is_some() {}
                        self.linger_stop = true;
                    }
                    T_LISTEN => self.accept(),
                    id => self.on_conn(id, ev),
                }
            }
            // Nothing is left to wait for: every request is answered at once.
            for w in std::mem::take(&mut self.waits) {
                self.respond_wait(w, Reason::Exit);
            }
        }
        self.flush_all(Duration::from_secs(1));
    }

    // ---- connections -------------------------------------------------------

    fn accept(&mut self) {
        loop {
            let fd = unsafe {
                libc::accept4(
                    self.listener.as_raw_fd(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
                )
            };
            if fd < 0 {
                return;
            }
            if sys::peer_uid(fd) != Some(unsafe { libc::getuid() }) {
                self.log("refused a connection from another user");
                sys::close(fd);
                continue;
            }
            let id = self.next_conn;
            self.next_conn += 1;
            if self.epoll.add(fd, sys::IN, id).is_err() {
                sys::close(fd);
                continue;
            }
            self.conns.insert(
                id,
                Conn {
                    fd,
                    inbuf: Vec::new(),
                    outbuf: Vec::new(),
                    continue_sent: false,
                    busy: false,
                    close_after: false,
                    wants_out: false,
                    attach: None,
                },
            );
        }
    }

    fn drop_conn(&mut self, id: u64) {
        let mut resized = false;
        if let Some(c) = self.conns.remove(&id) {
            self.epoll.del(c.fd);
            sys::close(c.fd);
            resized = c.attach.map(|a| a.resized).unwrap_or(false);
        }
        // The session goes back to its own size when the last client that
        // resized it detaches.
        if resized
            && !self
                .conns
                .values()
                .any(|c| c.attach.as_ref().map(|a| a.resized).unwrap_or(false))
        {
            if let Some((cols, rows)) = self.restore_size.take() {
                if self.exit_status.is_none() {
                    let owner = self.next_req;
                    self.next_req += 1;
                    let kind = QKind::Resize {
                        cols,
                        rows,
                        why: Resize::Restore,
                    };
                    self.queue.push_back(QItem {
                        owner,
                        kind,
                        off: 0,
                        pause_after: false,
                        pause_before: false,
                        gap: Duration::ZERO,
                    });
                }
            }
        }
        // A client that went away no longer waits. Input it queued is still
        // sent.
        self.waits.retain(|w| w.conn != id);
    }

    fn on_conn(&mut self, id: u64, ev: u32) {
        if !self.conns.contains_key(&id) {
            return;
        }
        if ev & sys::OUT != 0 {
            self.flush_conn(id);
        }
        if ev & (sys::IN | sys::HUP) != 0 {
            let mut buf = [0u8; 16384];
            loop {
                let Some(c) = self.conns.get_mut(&id) else {
                    return;
                };
                match sys::read(c.fd, &mut buf) {
                    Ok(Some(0)) | Err(_) => {
                        self.drop_conn(id);
                        return;
                    }
                    Ok(Some(n)) => {
                        c.inbuf.extend_from_slice(&buf[..n]);
                        if c.inbuf.len() > http::MAX_BODY + (128 << 10) {
                            self.drop_conn(id);
                            return;
                        }
                    }
                    Ok(None) => break,
                }
            }
            self.process_conn(id);
        }
    }

    /// Handle as many complete requests as the connection has buffered, one
    /// at a time.
    fn process_conn(&mut self, id: u64) {
        if self
            .conns
            .get(&id)
            .map(|c| c.attach.is_some())
            .unwrap_or(false)
        {
            return self.attach_input(id);
        }
        loop {
            let parsed = {
                let Some(c) = self.conns.get_mut(&id) else {
                    return;
                };
                if c.busy || c.close_after || c.inbuf.is_empty() {
                    return;
                }
                http::parse(&c.inbuf, c.continue_sent)
            };
            match parsed {
                Parsed::Incomplete => return,
                Parsed::WantsContinue => {
                    if let Some(c) = self.conns.get_mut(&id) {
                        c.continue_sent = true;
                        c.outbuf.extend_from_slice(b"HTTP/1.1 100 Continue\r\n\r\n");
                    }
                    self.flush_conn(id);
                    return;
                }
                Parsed::Bad(status, code, text) => {
                    self.reply(id, status, &http::error_value(code, &text), false, false);
                    return;
                }
                Parsed::Ready(req, used) => {
                    if let Some(c) = self.conns.get_mut(&id) {
                        c.inbuf.drain(..used);
                        c.continue_sent = false;
                        c.busy = true;
                    }
                    self.handle(id, req);
                }
            }
        }
    }

    fn reply(&mut self, id: u64, status: u16, value: &Value, wants_text: bool, keep_alive: bool) {
        let bytes = if wants_text && status == 200 {
            let text = if value.get("reason").is_some() {
                waldo::render::response(value)
            } else if let Some(t) = value
                .get("output")
                .and_then(|o| o.get("text"))
                .and_then(|t| t.as_str())
            {
                format!("{t}\n")
            } else {
                format!(
                    "{}\n",
                    serde_json::to_string_pretty(value).unwrap_or_default()
                )
            };
            http::response(
                200,
                "text/plain; charset=utf-8",
                text.as_bytes(),
                keep_alive,
            )
        } else {
            http::json_response(status, value, keep_alive)
        };
        let Some(c) = self.conns.get_mut(&id) else {
            return;
        };
        c.outbuf.extend_from_slice(&bytes);
        c.busy = false;
        if !keep_alive {
            c.close_after = true;
        }
        self.flush_conn(id);
        self.process_conn(id);
    }

    fn error(&mut self, id: u64, req: &Request, status: u16, code: &str, text: &str) {
        self.reply(
            id,
            status,
            &http::error_value(code, text),
            false,
            req.keep_alive,
        );
    }

    fn flush_conn(&mut self, id: u64) {
        let Some(c) = self.conns.get_mut(&id) else {
            return;
        };
        while !c.outbuf.is_empty() {
            match sys::write(c.fd, &c.outbuf) {
                Ok(Some(n)) => {
                    c.outbuf.drain(..n);
                }
                Ok(None) => break,
                Err(_) => {
                    self.drop_conn(id);
                    return;
                }
            }
        }
        let wants_out = !c.outbuf.is_empty();
        if wants_out != c.wants_out {
            c.wants_out = wants_out;
            let ev = if wants_out {
                sys::IN | sys::OUT
            } else {
                sys::IN
            };
            let _ = self.epoll.modify(c.fd, ev, id);
        }
        if !wants_out && c.close_after {
            self.drop_conn(id);
        }
    }

    /// Before exiting: give pending responses a moment to leave.
    fn flush_all(&mut self, limit: Duration) {
        let end = Instant::now() + limit;
        loop {
            let ids: Vec<u64> = self
                .conns
                .iter()
                .filter(|(_, c)| !c.outbuf.is_empty())
                .map(|(id, _)| *id)
                .collect();
            if ids.is_empty() || Instant::now() >= end {
                return;
            }
            for id in ids {
                self.flush_conn(id);
            }
            if self.conns.values().any(|c| !c.outbuf.is_empty()) {
                self.epoll.wait(20);
            }
        }
    }

    // ---- requests ----------------------------------------------------------

    fn handle(&mut self, id: u64, req: Request) {
        let known = [
            ("GET", "/v1/status"),
            ("GET", "/v1/screen"),
            ("POST", "/v1/input"),
            ("POST", "/v1/wait"),
            ("GET", "/v1/output"),
            ("POST", "/v1/resize"),
            ("POST", "/v1/signal"),
            ("GET", "/v1/attach"),
            ("GET", "/v1/triggers"),
            ("POST", "/v1/triggers"),
            ("DELETE", "/v1/triggers/ID"),
            ("GET", "/v1/broadcasts"),
            ("POST", "/v1/broadcasts"),
        ];
        if self.lingering {
            // A wait types nothing, so it counts as reading.
            let read_only = (req.method == "GET"
                && [
                    "/v1/status",
                    "/v1/screen",
                    "/v1/output",
                    "/v1/triggers",
                    "/v1/broadcasts",
                ]
                .contains(&req.path.as_str()))
                || (req.method == "POST" && req.path == "/v1/wait");
            if req.method == "POST" && req.path == "/v1/signal" {
                // Whatever was asked, there is no program to signal; the
                // caller wants the session gone.
                self.linger_stop = true;
                return self.handle_wait(id, &req, Until::None, Some(json!({})), None);
            }
            if !read_only {
                return self.error(
                    id,
                    &req,
                    409,
                    "exited",
                    "the program has exited; the session only answers status, screen, wait, output and triggers until it closes. 'waldo-term kill' closes it now",
                );
            }
        }
        match (req.method.as_str(), req.path.as_str()) {
            ("GET", "/v1/status") => self.handle_status(id, &req),
            ("GET", "/v1/screen") => self.handle_wait(id, &req, Until::None, None, None),
            ("POST", "/v1/wait") => self.handle_wait(id, &req, Until::Settle, None, None),
            ("POST", "/v1/input") => self.handle_input(id, &req),
            ("GET", "/v1/output") => self.handle_output(id, &req),
            ("POST", "/v1/resize") => self.handle_resize(id, &req),
            ("POST", "/v1/signal") => self.handle_signal(id, &req),
            ("GET", "/v1/attach") => self.handle_attach(id, &req),
            ("GET", "/v1/triggers") => self.triggers_reply(id, &req, None),
            ("POST", "/v1/triggers") => self.handle_trigger_add(id, &req),
            ("GET", "/v1/broadcasts") => self.handle_broadcast_list(id, &req),
            ("POST", "/v1/broadcasts") => self.handle_broadcast(id, &req),
            ("DELETE", p) if p.starts_with("/v1/triggers/") => {
                let name = p["/v1/triggers/".len()..].to_string();
                self.handle_trigger_remove(id, &req, &name);
            }
            (m, p) if known.iter().any(|(_, kp)| *kp == p) => {
                let right: Vec<&str> = known
                    .iter()
                    .filter(|(_, kp)| *kp == p)
                    .map(|(km, _)| *km)
                    .collect();
                self.error(
                    id,
                    &req,
                    405,
                    "method_not_allowed",
                    &format!("{p} takes {}, not {m}", right.join(" or ")),
                );
            }
            (m, p) if p.starts_with("/v1/triggers/") => {
                self.error(
                    id,
                    &req,
                    405,
                    "method_not_allowed",
                    &format!("{p} takes DELETE, not {m}; GET /v1/triggers lists them"),
                );
            }
            (_, p) => {
                let list: Vec<String> = known.iter().map(|(m, p)| format!("{m} {p}")).collect();
                self.error(
                    id,
                    &req,
                    404,
                    "not_found",
                    &format!("no such endpoint {p}; endpoints: {}", list.join(", ")),
                );
            }
        }
    }

    fn body_json(&mut self, id: u64, req: &Request) -> Option<Value> {
        if req.body.is_empty() {
            return Some(json!({}));
        }
        match serde_json::from_slice::<Value>(&req.body) {
            Ok(v) if v.is_object() => Some(v),
            Ok(_) => {
                self.error(
                    id,
                    req,
                    400,
                    "bad_json",
                    "the request body must be a JSON object",
                );
                None
            }
            Err(e) => {
                self.error(
                    id,
                    req,
                    400,
                    "bad_json",
                    &format!("the request body is not valid JSON: {e}"),
                );
                None
            }
        }
    }

    fn handle_status(&mut self, id: u64, req: &Request) {
        let v = json!({
            "v": 1,
            "session": self.cfg.id,
            "name": self.cfg.name,
            "argv": self.cfg.argv,
            "cwd": self.cfg.cwd,
            "socket": self.cfg.socket.display().to_string(),
            "dest": self.cfg.dest.uri(),
            "host_pid": std::process::id(),
            "started": self.cfg.started_wall,
            "uptime_ms": ms_since(self.start),
            "cols": self.emu.cols(),
            "rows": self.emu.rows(),
            "gen": self.gen,
            "child": self.child_json(),
            "broadcasts": self.cfg.broadcasts,
            "messages": {
                "settle": self.cfg.on_settle,
                "stream": self.cfg.on_stream,
                "triggers": self.triggers.len(),
            },
        });
        self.reply(id, 200, &v, req.wants_text, req.keep_alive);
    }

    /// Register a wait for a request. `body` is the parsed JSON body, if the
    /// request had one. `owner` is the id of the input this request queued.
    fn handle_wait(
        &mut self,
        id: u64,
        req: &Request,
        default: Until,
        body: Option<Value>,
        owner: Option<(u64, Option<String>)>,
    ) {
        let body = match body {
            Some(b) => b,
            None if req.method == "POST" && (req.json_body || !req.body.is_empty()) => {
                match self.body_json(id, req) {
                    Some(b) => b,
                    None => return,
                }
            }
            None => json!({}),
        };
        let wait_obj = body.get("wait").cloned().unwrap_or(Value::Null);
        let field = |name: &str| -> Option<Value> {
            req.query_get(name)
                .map(|s| Value::from(s.to_string()))
                .or_else(|| wait_obj.get(name).cloned())
                .or_else(|| body.get(name).cloned())
        };
        let as_u64 = |v: &Value| {
            v.as_u64()
                .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
        };

        // One condition, or several: a list, or names with commas between.
        // The request returns when any one of them is met.
        let named: Vec<String> = match field("until") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(a)) => a
                .iter()
                .map(|v| v.as_str().unwrap_or("").to_string())
                .collect(),
            Some(v) => v
                .as_str()
                .unwrap_or("")
                .split(',')
                .map(|s| s.trim().to_string())
                .collect(),
        };
        let after_ms = match field("after_ms") {
            None | Some(Value::Null) => None,
            Some(v) => match as_u64(&v).filter(|n| *n <= MAX_TIMEOUT_MS) {
                Some(n) => Some(n),
                None => {
                    return self.error(
                        id,
                        req,
                        400,
                        "bad_after",
                        &format!(
                        "after_ms must be a whole number of milliseconds, at most {MAX_TIMEOUT_MS}"
                    ),
                    )
                }
            },
        };
        let mut conds: Vec<Until> = Vec::new();
        for name in &named {
            let cond = match name.as_str() {
                "settle" => Until::Settle,
                "match" => Until::Match,
                "exit" => Until::Exit,
                "none" => Until::None,
                "broadcast" if !self.cfg.broadcasts => {
                    return self.error(id, req, 409, "broadcasts_off", BROADCASTS_OFF);
                }
                "broadcast" => Until::Broadcast,
                "after" => match after_ms {
                    Some(ms) => Until::After(ms),
                    None => return self.error(
                        id,
                        req,
                        400,
                        "bad_after",
                        "until=after needs after_ms: how many milliseconds to wait (--after MS)",
                    ),
                },
                "change" => match field("since_gen").as_ref().and_then(as_u64) {
                    Some(g) => Until::Change(g),
                    None => Until::Change(self.gen),
                },
                other => {
                    return self.error(
                        id,
                        req,
                        400,
                        "unknown_until",
                        &format!(
                            "unknown wait condition '{other}'; use settle, match, change, exit, broadcast, after or none"
                        ),
                    );
                }
            };
            if !conds.contains(&cond) {
                conds.push(cond);
            }
        }
        if conds.is_empty() {
            conds.push(default);
        }
        if conds.len() > 1 && conds.contains(&Until::None) {
            return self.error(
                id,
                req,
                400,
                "unknown_until",
                "'none' answers at once, so it cannot be combined with another wait condition",
            );
        }
        let until = conds[0];
        let from = match field("from") {
            None | Some(Value::Null) => None,
            Some(v) => match v.as_str().filter(|s| valid_name(s)) {
                Some(s) => Some(s.to_string()),
                None => return self.error(id, req, 400, "bad_name", NAME_RULE),
            },
        };
        let ansi = match field("style")
            .and_then(|v| v.as_str().map(|s| s.to_string()))
            .as_deref()
        {
            None | Some("plain") => false,
            Some("ansi") => true,
            Some(other) => {
                return self.error(
                    id,
                    req,
                    400,
                    "unknown_style",
                    &format!("unknown style '{other}'; use plain or ansi"),
                );
            }
        };
        let name = match field("as") {
            None | Some(Value::Null) => None,
            Some(v) => match v.as_str().filter(|s| valid_name(s)) {
                Some(s) => Some(s.to_string()),
                None => return self.error(id, req, 400, "bad_name", NAME_RULE),
            },
        };
        // A wait for a length of time is not cut short by the usual
        // deadline; one the request sets itself still applies.
        let default_timeout =
            match after_ms.filter(|_| conds.iter().any(|c| matches!(c, Until::After(_)))) {
                Some(ms) => DEFAULT_TIMEOUT_MS.max(ms + 1000).min(MAX_TIMEOUT_MS + 1000),
                None => DEFAULT_TIMEOUT_MS,
            };
        let timeout_ms = match field("timeout_ms").or_else(|| field("timeout")) {
            None => default_timeout,
            Some(v) => match as_u64(&v) {
                Some(n) => n.min(MAX_TIMEOUT_MS),
                None => {
                    return self.error(
                        id,
                        req,
                        400,
                        "bad_timeout",
                        "timeout_ms must be a whole number of milliseconds",
                    )
                }
            },
        };
        let mut pattern = None;
        if conds.contains(&Until::Match) {
            let Some(p) = field("pattern").and_then(|v| v.as_str().map(|s| s.to_string())) else {
                return self.error(
                    id,
                    req,
                    400,
                    "missing_pattern",
                    "until=match needs a pattern: a regular expression matched against one line of the screen (--pattern RE)",
                );
            };
            match regex_lite::RegexBuilder::new(&p)
                .size_limit(1 << 20)
                .build()
            {
                Ok(re) => pattern = Some((re, p)),
                Err(e) => {
                    return self.error(
                        id,
                        req,
                        400,
                        "bad_pattern",
                        &format!("the pattern is not a valid regular expression: {e}"),
                    )
                }
            }
        }

        let now = Instant::now();
        let sends_input = owner.is_some();
        let (req_id, hint) = match owner {
            Some((rid, hint)) => (rid, hint),
            None => {
                let rid = self.next_req;
                self.next_req += 1;
                (rid, None)
            }
        };
        let mut w = Wait {
            conn: id,
            req_id,
            until,
            started: now,
            deadline: now + Duration::from_millis(timeout_ms),
            wants_text: req.wants_text,
            keep_alive: req.keep_alive,
            interleaved: false,
            soft: false,
            pattern,
            known: Vec::new(),
            hint,
            ansi,
            fg_before: None,
            fg_name_before: None,
            gen_at_typing: None,
            exit_of: None,
            name,
            heard_at_start: self.last_broadcast,
            also: conds[1..].to_vec(),
            from,
        };
        if w.has(Until::Exit) {
            w.exit_of = sys::foreground_pgrp(self.master)
                .filter(|pg| *pg != self.child.pid)
                .map(|pg| {
                    (
                        pg,
                        sys::process_name(pg).unwrap_or_else(|| "the program".into()),
                    )
                });
        }
        if sends_input {
            w.fg_before = sys::foreground_pgrp(self.master);
            w.fg_name_before = w
                .fg_before
                .filter(|pg| *pg != self.child.pid)
                .and_then(sys::process_name);
        }
        // With input, text that is already on screen does not count: the
        // caller is waiting for what its input causes.
        if owner_has_input(&self.queue, req_id) {
            if let Some((re, _)) = w.pattern.as_ref() {
                w.known = self
                    .matches_now(re, &self.visible_lines())
                    .into_iter()
                    .map(|m| (m.line, m.text))
                    .collect();
            }
        }
        if self.exit_status.is_some() && until != Until::None {
            // The child is already gone; the answer comes when the pty is
            // drained.
            w.until = Until::Exit;
            w.also.clear();
        }
        self.waits.push(w);
    }

    fn handle_input(&mut self, id: u64, req: &Request) {
        let req_id = self.next_req;
        self.next_req += 1;
        // Each item, with the pause the request put in front of it.
        let mut items: Vec<(QKind, Duration)> = Vec::new();
        let mut pause = Duration::ZERO;
        let mut hint: Option<String> = None;
        let body: Value;
        let delay_of = |v: Option<u64>| {
            v.filter(|ms| *ms <= MAX_DELAY_MS)
                .map(Duration::from_millis)
        };
        let bad_delay =
            format!("key_delay_ms must be a whole number of milliseconds, at most {MAX_DELAY_MS}");
        let mut key_delay = Duration::ZERO;
        if let Some(q) = req.query_get("key_delay_ms") {
            match delay_of(q.parse().ok()) {
                Some(d) => key_delay = d,
                None => return self.error(id, req, 400, "bad_delay", &bad_delay),
            }
        }

        if req.json_body || req.body.first() == Some(&b'{') {
            body = match self.body_json(id, req) {
                Some(b) => b,
                None => return,
            };
            let Some(list) = body.get("input").and_then(|i| i.as_array()) else {
                return self.error(
                    id,
                    req,
                    400,
                    "missing_input",
                    "the body needs an \"input\" list, such as {\"input\": [{\"text\": \"ls\"}, {\"keys\": \"<Enter>\"}]}",
                );
            };
            if let Some(v) = body.get("key_delay_ms") {
                match delay_of(
                    v.as_u64()
                        .or_else(|| v.as_str().and_then(|s| s.parse().ok())),
                ) {
                    Some(d) => key_delay = d,
                    None => return self.error(id, req, 400, "bad_delay", &bad_delay),
                }
            }
            for (n, item) in list.iter().enumerate() {
                // A pause is not typed: it delays whatever comes after it.
                if let Some(v) = item
                    .as_object()
                    .filter(|o| o.len() == 1)
                    .and_then(|o| o.get("pause"))
                {
                    match delay_of(
                        v.as_u64()
                            .or_else(|| v.as_str().and_then(|s| s.parse().ok())),
                    ) {
                        Some(d) => pause += d,
                        None => {
                            let text = format!("input item {n}: the value of 'pause' must be a whole number of milliseconds, at most {MAX_DELAY_MS}");
                            return self.error(id, req, 400, "bad_item", &text);
                        }
                    }
                    continue;
                }
                match self.read_item(item, !key_delay.is_zero()) {
                    Ok((kinds, h)) => {
                        for kind in kinds {
                            items.push((kind, std::mem::take(&mut pause)));
                        }
                        hint = hint.or(h);
                    }
                    Err((code, text)) => {
                        return self.error(id, req, 400, code, &format!("input item {n}: {text}"));
                    }
                }
            }
        } else {
            // For curl: a body that is not JSON is one `keys` item.
            body = json!({});
            let text = String::from_utf8_lossy(&req.body).into_owned();
            match parse_keys(&text, !key_delay.is_zero()) {
                Ok(kinds) => items.extend(kinds.into_iter().map(|k| (k, Duration::ZERO))),
                Err(e) => return self.error(id, req, 400, "unknown_key", &e),
            }
        }

        let mut after_text = false;
        let mut first = true;
        for (kind, before) in items {
            let is_key = matches!(kind, QKind::Key { .. });
            // The key delay goes between this request's own items.
            let gap = before + if first { Duration::ZERO } else { key_delay };
            self.queue.push_back(QItem {
                owner: req_id,
                kind,
                off: 0,
                pause_after: false,
                pause_before: is_key && after_text,
                gap,
            });
            after_text = !is_key;
            first = false;
        }
        if !pause.is_zero() {
            // A pause at the end holds back whatever is typed next.
            self.queue.push_back(QItem {
                owner: req_id,
                kind: QKind::Bytes(Vec::new()),
                off: 0,
                pause_after: false,
                pause_before: false,
                gap: pause,
            });
        }
        self.handle_wait(id, req, Until::Settle, Some(body), Some((req_id, hint)));
        if !self.waits.iter().any(|w| w.req_id == req_id) {
            // The wait part of the request was refused, so nothing is sent.
            self.queue.retain(|q| q.owner != req_id);
        }
    }

    /// Turn one input item into what goes in the queue. Files are read here,
    /// before anything is sent.
    fn read_item(
        &self,
        item: &Value,
        each_key: bool,
    ) -> Result<(Vec<QKind>, Option<String>), ItemError> {
        let obj = item.as_object().filter(|o| o.len() == 1).ok_or((
            "bad_item",
            "each item is an object with exactly one of: text, keys, paste, text_file, keys_file, paste_file, pause".to_string(),
        ))?;
        let (kind, value) = obj.iter().next().unwrap();
        let s = value.as_str().ok_or((
            "bad_item",
            format!("the value of '{kind}' must be a string"),
        ))?;
        let content: String = if let Some(base) = kind.strip_suffix("_file") {
            if !["text", "keys", "paste"].contains(&base) {
                return Err(("bad_item", format!("unknown item kind '{kind}'; use text, keys, paste, text_file, keys_file or paste_file")));
            }
            read_file_item(s).map_err(|e| ("file_unreadable", e))?
        } else {
            s.to_string()
        };
        match kind.trim_end_matches("_file") {
            "text" => {
                let hint = keys::looks_like_keys(&content)
                    .map(|k| format!("'{k}' was sent as literal text; use --keys (a 'keys' item) for key names."));
                Ok((vec![QKind::Bytes(text_bytes(&content))], hint))
            }
            "keys" => Ok((parse_keys(&content, each_key).map_err(|e| ("unknown_key", e))?, None)),
            "paste" => Ok((vec![QKind::Paste(content.into_bytes())], None)),
            other => Err(("bad_item", format!("unknown item kind '{other}'; use text, keys, paste, text_file, keys_file or paste_file"))),
        }
    }

    fn handle_output(&mut self, id: u64, req: &Request) {
        let tail = match req.query_get("tail") {
            None => None,
            Some(t) => match t.parse::<usize>() {
                Ok(n) => Some(n),
                Err(_) => {
                    return self.error(
                        id,
                        req,
                        400,
                        "bad_tail",
                        "tail must be a number of lines, such as ?tail=50",
                    )
                }
            },
        };
        let v = json!({
            "v": 1,
            "session": self.cfg.id,
            "output": self.outbuf.render(&self.primary_lines(), tail),
        });
        self.reply(id, 200, &v, req.wants_text, req.keep_alive);
    }

    fn handle_resize(&mut self, id: u64, req: &Request) {
        let Some(body) = self.body_json(id, req) else {
            return;
        };
        let dim = |k: &str| body.get(k).and_then(|v| v.as_u64());
        let (Some(cols), Some(rows)) = (dim("cols"), dim("rows")) else {
            return self.error(
                id,
                req,
                400,
                "bad_size",
                "the body needs cols and rows, such as {\"cols\": 100, \"rows\": 30}",
            );
        };
        if !(MIN_COLS as u64..=MAX_COLS as u64).contains(&cols)
            || !(MIN_ROWS as u64..=MAX_ROWS as u64).contains(&rows)
        {
            return self.error(
                id,
                req,
                400,
                "bad_size",
                &format!("the size must be between {MIN_COLS}x{MIN_ROWS} and {MAX_COLS}x{MAX_ROWS} (cols x rows)"),
            );
        }
        // It takes its turn behind input already queued, so that it never
        // lands in the middle of another request's typing.
        let req_id = self.next_req;
        self.next_req += 1;
        let kind = QKind::Resize {
            cols: cols as u16,
            rows: rows as u16,
            why: Resize::Explicit,
        };
        self.queue.push_back(QItem {
            owner: req_id,
            kind,
            off: 0,
            pause_after: false,
            pause_before: false,
            gap: Duration::ZERO,
        });
        // The program redraws for the new size, so the answer waits for
        // that. A program that has drawn nothing has nothing to redraw.
        // It does not wait long: a screen that never goes quiet is no reason
        // for a resize to fail.
        // A request that names its own wait gets that one as it is.
        let until = if self.seen_output {
            Until::Settle
        } else {
            Until::None
        };
        let own_wait = body.get("wait").map(|w| json!({"wait": w}));
        let chosen = own_wait.is_some();
        self.handle_wait(
            id,
            req,
            until,
            Some(own_wait.unwrap_or_else(|| json!({}))),
            Some((req_id, None)),
        );
        match self.waits.iter_mut().find(|w| w.req_id == req_id) {
            // The short deadline is set when the resize happens.
            Some(w) if !chosen => w.soft = true,
            Some(_) => {}
            // The wait was refused, so nothing is resized.
            None => self.queue.retain(|q| q.owner != req_id),
        }
    }

    fn handle_signal(&mut self, id: u64, req: &Request) {
        let Some(body) = self.body_json(id, req) else {
            return;
        };
        if body.get("stop").and_then(|s| s.as_bool()) == Some(true) {
            self.stop("request");
        } else if let Some(name) = body.get("signal").and_then(|s| s.as_str()) {
            let Some(sig) = sys::signal_number(name) else {
                return self.error(
                    id,
                    req,
                    400,
                    "unknown_signal",
                    &format!(
                        "unknown signal '{name}'; use a name such as SIGINT, SIGTERM or SIGKILL"
                    ),
                );
            };
            // Like a resize, a signal takes its turn behind queued input.
            let req_id = self.next_req;
            self.next_req += 1;
            self.queue.push_back(QItem {
                owner: req_id,
                kind: QKind::Signal(sig),
                off: 0,
                pause_after: false,
                pause_before: false,
                gap: Duration::ZERO,
            });
            return self.handle_wait(id, req, Until::None, Some(json!({})), Some((req_id, None)));
        } else {
            return self.error(
                id,
                req,
                400,
                "bad_signal",
                "the body needs {\"signal\": \"SIGINT\"} or {\"stop\": true}",
            );
        }
        self.handle_wait(id, req, Until::None, Some(json!({})), None);
    }
}

fn owner_has_input(queue: &VecDeque<QItem>, owner: u64) -> bool {
    queue.iter().any(|q| q.owner == owner)
}

/// Literal text as a keyboard would send it: Enter is a carriage return.
fn text_bytes(text: &str) -> Vec<u8> {
    text.replace("\r\n", "\r").replace('\n', "\r").into_bytes()
}

/// Key notation as queue items. With `each_key`, literal characters between
/// key names are one item each, so that a delay can go between them.
fn parse_keys(notation: &str, each_key: bool) -> Result<Vec<QKind>, String> {
    let mut out = Vec::new();
    for t in keys::parse(notation)? {
        match t {
            Tok::Text(s) if each_key => {
                out.extend(s.chars().map(|c| QKind::Bytes(text_bytes(&c.to_string()))))
            }
            Tok::Text(s) => out.push(QKind::Bytes(text_bytes(&s))),
            Tok::Key {
                key,
                mods,
                text,
                is_esc,
            } => out.push(QKind::Key {
                key,
                mods,
                text,
                is_esc,
            }),
        }
    }
    Ok(out)
}

fn read_file_item(path: &str) -> Result<String, String> {
    if !path.starts_with('/') {
        return Err(format!(
            "{path}: the path must be absolute, because the host reads the file"
        ));
    }
    let md = std::fs::metadata(path).map_err(|e| format!("{path}: {e}"))?;
    if !md.is_file() {
        return Err(format!("{path}: not a regular file"));
    }
    if md.size() > MAX_FILE_ITEM {
        return Err(format!(
            "{path}: larger than 1 MiB; split it into several requests"
        ));
    }
    let bytes = std::fs::read(path).map_err(|e| format!("{path}: {e}"))?;
    String::from_utf8(bytes).map_err(|_| format!("{path}: not UTF-8 text"))
}

/// The fields every message to the destination starts with (section 4).
/// A pattern against one line that has left the screen.
fn line_match(re: &Regex, abs: u64, line: &str) -> Option<MatchInfo> {
    let subject: String = line.chars().take(MATCH_CHARS).collect();
    let caps = re.captures(&subject)?;
    let m = caps.get(0)?;
    Some(MatchInfo {
        text: m.as_str().to_string(),
        groups: (1..caps.len())
            .map(|i| {
                caps.get(i)
                    .map(|g| Value::from(g.as_str()))
                    .unwrap_or(Value::Null)
            })
            .collect(),
        line: abs,
        row: None,
        col: subject[..m.start()].chars().count() + 1,
    })
}

// ---- attach (DESIGN.md section 7) -----------------------------------------
//
// An attached client gets the screen re-rendered from emulator state, never
// the program's own bytes. After the HTTP upgrade both directions carry one
// JSON object per line.

const FRAME_INTERVAL: Duration = Duration::from_millis(16);
const ATTACH_BACKLOG: usize = 256 << 10;

struct Attach {
    view_cols: u16,
    view_rows: u16,
    /// A hash of each viewport row as last sent.
    sent: Vec<u64>,
    sent_meta: u64,
    /// The host state the last frame was drawn from.
    key: Option<(u64, usize)>,
    last_sent: Instant,
    /// This client asked the session to take its size.
    resized: bool,
    /// Broadcasts to go out with this viewer's next frame.
    said: Vec<Value>,
}

fn hash_str(s: &str) -> u64 {
    let mut h = DefaultHasher::new();
    h.write(s.as_bytes());
    h.finish()
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

impl Host {
    fn attach_key(&self) -> (u64, usize) {
        (self.gen, self.waits.len())
    }

    fn has_attach(&self) -> bool {
        self.conns.values().any(|c| c.attach.is_some())
    }

    /// When the next frame for a waiting attach client may go out.
    fn attach_due(&self) -> Option<Instant> {
        let key = self.attach_key();
        self.conns
            .values()
            .filter_map(|c| c.attach.as_ref())
            .filter(|a| a.key != Some(key))
            .map(|a| a.last_sent + FRAME_INTERVAL)
            .min()
    }

    fn do_resize(&mut self, cols: u16, rows: u16) -> Result<(), String> {
        self.emu.resize(cols, rows)?;
        let _ = sys::set_winsize(self.master, cols, rows);
        self.record(|r| r.resize(cols, rows));
        self.hold_since = None;
        self.collect();
        self.refresh();
        self.last_change = Instant::now();
        // A resize reflows lines and renumbers rows. What is on screen now
        // is recorded, not reported again (section 3.2).
        if !self.triggers.is_empty() {
            let lines = self.visible_lines();
            let mut triggers = std::mem::take(&mut self.triggers);
            for t in triggers.iter_mut() {
                t.known = self
                    .matches_now(&t.re, &lines)
                    .into_iter()
                    .map(|m| (m.line, m.text))
                    .collect();
            }
            self.triggers = triggers;
            self.trig_gen = self.gen;
        }
        Ok(())
    }

    fn handle_attach(&mut self, id: u64, req: &Request) {
        if req.upgrade.as_deref() != Some("waldo-attach") {
            return self.error(
                id,
                req,
                400,
                "upgrade_required",
                "attach is a stream: send 'Upgrade: waldo-attach', or use 'waldo-term attach'. To read the screen once, GET /v1/screen",
            );
        }
        let dim = |name: &str, default: u16| -> u16 {
            req.query_get(name)
                .and_then(|v| v.parse::<u16>().ok())
                .filter(|n| *n >= 1)
                .unwrap_or(default)
                .min(1000)
        };
        let attach = Attach {
            view_cols: dim("cols", self.emu.cols()),
            view_rows: dim("rows", self.emu.rows()),
            sent: Vec::new(),
            sent_meta: 0,
            key: None,
            last_sent: Instant::now() - FRAME_INTERVAL,
            resized: false,
            said: Vec::new(),
        };
        let Some(c) = self.conns.get_mut(&id) else {
            return;
        };
        c.outbuf.extend_from_slice(b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: waldo-attach\r\nConnection: Upgrade\r\n\r\n");
        c.attach = Some(attach);
        self.flush_conn(id);
        self.attach_input(id);
    }

    /// Handle the lines an attached client has sent.
    fn attach_input(&mut self, id: u64) {
        loop {
            let line = {
                let Some(c) = self.conns.get_mut(&id) else {
                    return;
                };
                let Some(nl) = c.inbuf.iter().position(|b| *b == b'\n') else {
                    if c.inbuf.len() > (1 << 20) {
                        self.drop_conn(id);
                    }
                    return;
                };
                c.inbuf.drain(..=nl).collect::<Vec<u8>>()
            };
            let Ok(msg) = serde_json::from_slice::<Value>(&line) else {
                continue;
            };
            let size = |v: &Value| -> Option<(u16, u16)> {
                Some((
                    v.get("cols")?.as_u64()? as u16,
                    v.get("rows")?.as_u64()? as u16,
                ))
            };
            if let Some((cols, rows)) = msg.get("viewport").and_then(size) {
                if let Some(a) = self.conns.get_mut(&id).and_then(|c| c.attach.as_mut()) {
                    a.view_cols = cols.clamp(1, 1000);
                    a.view_rows = rows.clamp(1, 1000);
                    // The client cleared its terminal; send everything again.
                    a.sent.clear();
                    a.key = None;
                }
            }
            if let Some((cols, rows)) = msg.get("resize").and_then(size) {
                let (cols, rows) = (
                    cols.clamp(MIN_COLS, MAX_COLS),
                    rows.clamp(MIN_ROWS, MAX_ROWS),
                );
                let owner = self.next_req;
                self.next_req += 1;
                let kind = QKind::Resize {
                    cols,
                    rows,
                    why: Resize::Viewer,
                };
                self.queue.push_back(QItem {
                    owner,
                    kind,
                    off: 0,
                    pause_after: false,
                    pause_before: false,
                    gap: Duration::ZERO,
                });
                if let Some(a) = self.conns.get_mut(&id).and_then(|c| c.attach.as_mut()) {
                    a.resized = true;
                }
            }
            if let Some(bytes) = msg.get("keys_hex").and_then(|k| k.as_str()).and_then(unhex) {
                if !bytes.is_empty() {
                    // A person's typing is its own piece of input: requests
                    // in flight learn that someone else typed.
                    let owner = self.next_req;
                    self.next_req += 1;
                    self.queue.push_back(QItem {
                        owner,
                        kind: QKind::Bytes(bytes),
                        off: 0,
                        pause_after: false,
                        pause_before: false,
                        gap: Duration::ZERO,
                    });
                }
            }
            if let Some(b) = msg.get("broadcast").filter(|_| self.cfg.broadcasts) {
                // The same rules as POST /v1/broadcasts. There is nowhere
                // to send an error on this stream, so one that breaks them
                // is dropped; the viewer is expected to check first.
                let from = match b.get("as") {
                    None | Some(Value::Null) => Some(None),
                    Some(v) => v
                        .as_str()
                        .filter(|s| valid_name(s))
                        .map(|s| Some(s.to_string())),
                };
                let text = b
                    .get("text")
                    .and_then(|t| t.as_str())
                    .map(|t| t.trim())
                    .filter(|t| {
                        !t.is_empty()
                            && t.len() <= MAX_BROADCAST_BYTES
                            && !t.chars().any(|c| c.is_control())
                    })
                    .map(|t| t.to_string());
                if let (Some(from), Some(text)) = (from, text) {
                    self.post_broadcast(from, text, "attach");
                }
            }
            if msg.get("detach").and_then(|d| d.as_bool()) == Some(true) {
                self.drop_conn(id);
                return;
            }
        }
    }

    /// Send a frame to each attached client whose view is out of date.
    fn service_attach(&mut self) {
        if !self.has_attach() {
            return;
        }
        let now = Instant::now();
        if self
            .hold_since
            .map(|h| now.duration_since(h) < HOLD_CAP)
            .unwrap_or(false)
        {
            // The program asked for its updates to be shown together.
            return;
        }
        let key = self.attach_key();
        let ids: Vec<u64> = self
            .conns
            .iter()
            .filter(|(_, c)| {
                c.attach
                    .as_ref()
                    .map(|a| {
                        a.key != Some(key)
                            && now >= a.last_sent + FRAME_INTERVAL
                            && c.outbuf.len() < ATTACH_BACKLOG
                    })
                    .unwrap_or(false)
            })
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            let Some(a) = self.conns.get(&id).and_then(|c| c.attach.as_ref()) else {
                continue;
            };
            let cols = a.view_cols.min(self.emu.cols());
            let rows = a.view_rows.min(self.emu.rows());
            let (cx, cy) = self.emu.cursor();
            // A smaller viewer sees the part of the screen with the cursor.
            let off_y = if cy >= rows { cy - rows + 1 } else { 0 };
            let off_x = if cx >= cols { cx - cols + 1 } else { 0 };
            let mut lines: Vec<Value> = Vec::new();
            let mut hashes: Vec<u64> = Vec::with_capacity(rows as usize);
            for r in 0..rows {
                let text = self
                    .emu
                    .row_ansi(Space::Active, (r + off_y) as u32, off_x, cols)
                    .unwrap_or_default();
                let h = hash_str(&text);
                if a.sent.get(r as usize) != Some(&h) {
                    lines.push(json!([r, text]));
                }
                hashes.push(h);
            }
            let meta = json!({
                "session": self.cfg.id,
                "cols": self.emu.cols(),
                "rows": self.emu.rows(),
                "view": {"cols": cols, "rows": rows, "x": off_x, "y": off_y},
                "cursor": {"row": cy - off_y, "col": cx - off_x, "visible": self.emu.cursor_visible()},
                "title": self.emu.title(),
                "alt_screen": self.emu.alt_screen(),
                "busy": self.waits.len(),
                "broadcasting": self.cfg.broadcasts,
                "modes": {
                    "app_cursor": self.emu.dec_mode(1),
                    "app_keypad": self.emu.dec_mode(66),
                    "bracketed_paste": self.emu.dec_mode(2004),
                },
            });
            let meta_hash = hash_str(&meta.to_string());
            let changed = !lines.is_empty() || meta_hash != a.sent_meta || !a.said.is_empty();
            let mut frame = meta;
            frame["lines"] = Value::Array(lines);
            let Some(c) = self.conns.get_mut(&id) else {
                continue;
            };
            if let Some(a) = c.attach.as_mut() {
                a.key = Some(key);
                if !changed {
                    continue;
                }
                a.sent = hashes;
                a.sent_meta = meta_hash;
                a.last_sent = now;
                if !a.said.is_empty() {
                    frame["broadcasts"] = Value::Array(std::mem::take(&mut a.said));
                }
            }
            c.outbuf.extend_from_slice(frame.to_string().as_bytes());
            c.outbuf.push(b'\n');
            self.flush_conn(id);
        }
    }

    /// Tell attached clients the session is over.
    fn end_attach(&mut self) {
        let ids: Vec<u64> = self
            .conns
            .iter()
            .filter(|(_, c)| c.attach.is_some())
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            if let Some(c) = self.conns.get_mut(&id) {
                c.outbuf.extend_from_slice(b"{\"ended\":true}\n");
            }
            self.flush_conn(id);
        }
    }
}

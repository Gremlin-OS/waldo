//! The host's event loop (DESIGN.md sections 1.2, 1.5, 2 and 3).
//!
//! One thread, one epoll: the pty, the child's pidfd, a signalfd, the
//! listening socket and the client connections. Timers are the epoll
//! timeout. Nothing here blocks on the pty or on a client.

use crate::dest::{Delivery, Dest};
use crate::emu::{Emu, Row, Space, Tracked};
use crate::http::{self, Parsed, Request};
use crate::keys::{self, Tok};
use crate::outbuf::{cap_lines, OutBuf};
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

const T_PTY: u64 = 0;
const T_PIDFD: u64 = 1;
const T_SIGNAL: u64 = 2;
const T_LISTEN: u64 = 3;
const T_FIRST_CONN: u64 = 100;

const CHUNK: usize = 4096;
const READ_BATCH: usize = 64 << 10;
const DEFAULT_TIMEOUT_MS: u64 = 10_000;
const RUN_TIMEOUT_MS: u64 = 120_000;
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
const HOLD_CAP: Duration = Duration::from_secs(1);
const DRAIN_QUIET: Duration = Duration::from_millis(100);
const DRAIN_CAP: Duration = Duration::from_secs(2);
const KILL_STEP: Duration = Duration::from_secs(2);
const MAX_FILE_ITEM: u64 = 1 << 20;
const ALT_BASE: u64 = 1 << 40;
/// The private OSC number `run` uses for its end-of-command mark.
const SENTINEL_PREFIX: &str = "7770;waldo;";

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
    pub started_wall: String,
}

enum QKind {
    Bytes(Vec<u8>),
    Key { key: i32, mods: u16, text: Option<char>, is_esc: bool },
    Paste(Vec<u8>),
}

struct QItem {
    owner: u64,
    kind: QKind,
    off: usize,
    pause_after: bool,
    /// Wait a moment after the previous input before sending this.
    pause_before: bool,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Until {
    Settle,
    Match,
    Change(u64),
    Exit,
    None,
    Done,
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
    None,
    Timeout,
    Done,
}

impl Reason {
    fn as_str(&self) -> &'static str {
        match self {
            Reason::Settle => "settle",
            Reason::Match(_) => "match",
            Reason::Change => "change",
            Reason::Exit => "exit",
            Reason::None => "none",
            Reason::Timeout => "timeout",
            Reason::Done => "done",
        }
    }
}

struct Wait {
    conn: u64,
    req_id: u64,
    until: Until,
    started: Instant,
    deadline: Instant,
    wants_text: bool,
    keep_alive: bool,
    interleaved: bool,
    pattern: Option<(Regex, String)>,
    /// Matches that were already there when the request began; they do not
    /// satisfy it.
    known: Vec<(u64, String)>,
    hint: Option<String>,
    /// Return the screen with SGR colour codes left in.
    ansi: bool,
}

struct RunResult {
    code: i64,
    output: Value,
    ms: u64,
}

struct Run {
    nonce: String,
    /// The end of the typed line, as the shell echoes it.
    echo_tail: String,
    tracked: Option<Tracked>,
    started: Instant,
    finished: Option<RunResult>,
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
    seq: u64,
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

    outbuf: OutBuf,
    watermark: Option<Tracked>,
    rows_committed: u64,
    abs_base: u64,
    fresh_lines: Vec<(u64, String)>,

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
    run: Option<Run>,

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
            seq: 1,
            start,
            gen: 0,
            screen_hash: 0,
            rows: Vec::new(),
            last_change: now,
            hold_since: None,
            prompt_seen: false,
            seen_output: false,
            outbuf,
            watermark,
            rows_committed: 0,
            abs_base: 0,
            fresh_lines: Vec::new(),
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
            run: None,
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
        let settle_at = self.settle_at();
        for w in &self.waits {
            consider(w.deadline);
            if matches!(w.until, Until::Settle | Until::Done) {
                consider(settle_at);
            }
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
            if self.exit_status.is_none() && stage < 2 && since.elapsed() >= KILL_STEP * (stage as u32 + 1) {
                let sig = if stage == 0 { libc::SIGTERM } else { libc::SIGKILL };
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
                    self.process_chunk(&buf[..n]);
                }
                Ok(None) => break,
            }
        }
        if total > 0 {
            if self.exit_status.is_some() {
                self.drain_until = Some(Instant::now() + DRAIN_QUIET);
            }
            self.refresh();
        }
    }

    fn process_chunk(&mut self, data: &[u8]) {
        let mut off = 0;
        while off < data.len() {
            let (n, stopped) = self.emu.feed_until_osc_end(&data[off..]);
            off += n.max(1);
            if stopped {
                self.handle_unknown_osc();
            }
        }
        self.handle_unknown_osc();
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

    fn handle_unknown_osc(&mut self) {
        for osc in self.emu.take_unknown_osc() {
            let Ok(s) = std::str::from_utf8(&osc) else { continue };
            let Some(rest) = s.strip_prefix(SENTINEL_PREFIX) else { continue };
            let Some((code, nonce)) = rest.split_once(';') else { continue };
            let is_current = matches!(&self.run, Some(r) if r.finished.is_none() && r.nonce == nonce);
            if !is_current {
                continue;
            }
            let output = self.run_output();
            if let Some(run) = self.run.as_mut() {
                run.finished = Some(RunResult {
                    code: code.trim().parse().unwrap_or(-1),
                    output,
                    ms: ms_since(run.started),
                });
                run.tracked = None;
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
        let tracked = self.watermark.as_ref().and_then(|w| self.emu.tracked_row(w));
        let start = match tracked {
            Some(y) => y,
            None => {
                self.outbuf.push("[... output lost ...]".into());
                self.watermark = self.emu.track(Space::Screen, 0);
                0
            }
        };
        let mut line_start = start;
        let mut acc = String::new();
        let mut y = start;
        while y < sb {
            let Some(row) = self.emu.row(Space::Screen, y, None) else { break };
            acc.push_str(&row.text);
            y += 1;
            // A logical line ends at a row that does not wrap. A line that
            // wraps for thousands of rows is cut so this stays bounded.
            if !row.wrap || y - line_start >= 2000 {
                let abs = self.rows_committed + (line_start - start) as u64;
                let line = std::mem::take(&mut acc);
                let line = trimmed(&line).to_string();
                if self.waits.iter().any(|w| w.until == Until::Match) {
                    self.fresh_lines.push((abs, line.clone()));
                }
                self.outbuf.push(line);
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
            .map(|y| self.emu.row(Space::Active, y, Some(&mut h)).unwrap_or_default())
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
        }
        if !self.seen_output && rows.iter().any(|r| !r.text.trim().is_empty()) {
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
                .map(|y| self.emu.row_ansi(Space::Active, y, 0, self.emu.cols()).unwrap_or_default())
                .collect()
        } else {
            Vec::new()
        };
        let mut lines: Vec<&str> =
            if ansi { styled.iter().map(|s| s.as_str()).collect() } else { self.rows.iter().map(|r| trimmed(&r.text)).collect() };
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
        let base = if self.emu.alt_screen() { ALT_BASE } else { self.abs_base };
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
                    text.extend(std::iter::repeat(' ').take(cx as usize - have));
                }
            }
            acc.push_str(&text);
            if !row.wrap || i + 1 == self.rows.len() {
                out.push(Logical { text: std::mem::take(&mut acc), first_row: first, abs: base + first as u64 });
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
        let mut lines: Vec<String> = self.visible_lines().into_iter().map(|l| trimmed(&l.text).to_string()).collect();
        while lines.last().map(|l| l.is_empty()).unwrap_or(false) {
            lines.pop();
        }
        lines
    }

    /// What the command started by `run` has printed so far.
    fn run_output(&self) -> Value {
        let Some(run) = self.run.as_ref() else { return Value::Null };
        if self.emu.alt_screen() {
            return cap_lines(Vec::new(), self.cfg.output_cap);
        }
        let sb = self.emu.scrollback_rows() as u32;
        let (_, cy) = self.emu.cursor();
        let end = sb + cy as u32;
        let (start, lost) = match run.tracked.as_ref().and_then(|t| self.emu.tracked_row(t)) {
            Some(y) => (y, false),
            None => (0, true),
        };
        let mut lines: Vec<String> = Vec::new();
        let mut acc = String::new();
        let mut y = start;
        while y <= end {
            let Some(row) = self.emu.row(Space::Screen, y, None) else { break };
            acc.push_str(&row.text);
            if !row.wrap || y == end {
                lines.push(trimmed(&std::mem::take(&mut acc)).to_string());
            }
            y += 1;
        }
        // The first lines are the echo of what was typed, ending with the
        // line that carries the mark's arguments as typed. The terminal may
        // echo the line once itself and the shell again when it redraws, so
        // take the last such line.
        let head = lines.len().min(200);
        let echo_end = lines[..head]
            .iter()
            .rposition(|l| l.contains(&run.echo_tail))
            .or_else(|| lines[..head].iter().position(|l| l.contains(&run.nonce)));
        if let Some(i) = echo_end {
            lines.drain(..=i);
        }
        while lines.last().map(|l| l.is_empty()).unwrap_or(false) {
            lines.pop();
        }
        if lost {
            lines.insert(0, "[... output lost ...]".into());
        }
        cap_lines(lines, self.cfg.output_cap)
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
            if let Some(p) = self.pause_until {
                if Instant::now() < p {
                    break;
                }
                self.pause_until = None;
            }
            let Some(front) = self.queue.front_mut() else { break };
            if front.pause_before {
                let since = self.last_input.elapsed();
                let reacted = self.gen != self.gen_at_input && self.last_change.elapsed() >= KEY_STILL;
                if since < KEY_MAX && !(since >= KEY_MIN && reacted) {
                    self.pause_until = Some(Instant::now() + KEY_POLL);
                    break;
                }
                front.pause_before = false;
            }
            // Keys and pastes are encoded when they reach the front, so they
            // follow the modes the program has set by then.
            match &front.kind {
                QKind::Bytes(_) => {}
                QKind::Key { key, mods, text, is_esc } => {
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
            }
            let QKind::Bytes(bytes) = &front.kind else { unreachable!() };
            if front.off < bytes.len() {
                match sys::write(self.master, &bytes[front.off..]) {
                    Ok(Some(n)) => {
                        front.off += n;
                        let owner = front.owner;
                        self.last_input = Instant::now();
                        self.gen_at_input = self.gen;
                        self.seen_output = true;
                        self.prompt_seen = false;
                        for w in self.waits.iter_mut() {
                            if w.req_id != owner {
                                w.interleaved = true;
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
                Some(QItem { kind: QKind::Bytes(b), off, .. }) => *off >= b.len(),
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
        if blocked != self.master_out {
            self.master_out = blocked;
            let ev = if blocked { sys::IN | sys::OUT } else { sys::IN };
            let _ = self.epoll.modify(self.master, ev, T_PTY);
        }
    }

    fn input_idle(&self) -> bool {
        self.queue.is_empty() && self.replies.is_empty() && self.pause_until.is_none()
    }

    // ---- settle and waits -------------------------------------------------

    fn quiet(&self) -> Duration {
        Duration::from_millis(if self.prompt_seen { PROMPT_QUIET_MS.min(self.cfg.quiet_ms) } else { self.cfg.quiet_ms })
    }

    fn settle_at(&self) -> Instant {
        self.last_change.max(self.last_input) + self.quiet()
    }

    fn settled(&self, now: Instant) -> bool {
        let held = self.hold_since.map(|h| now.duration_since(h) < HOLD_CAP).unwrap_or(false);
        self.seen_output && self.input_idle() && !held && now >= self.settle_at()
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
                        .map(|i| caps.get(i).map(|g| Value::from(g.as_str())).unwrap_or(Value::Null))
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
                            .map(|i| caps.get(i).map(|g| Value::from(g.as_str())).unwrap_or(Value::Null))
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
        w.known.retain(|k| found.iter().any(|f| f.line == k.0 && f.text == k.1));
        found.into_iter().find(|f| !w.known.contains(&(f.line, f.text.clone())))
    }

    fn evaluate_waits(&mut self) {
        if self.waits.is_empty() {
            self.fresh_lines.clear();
            return;
        }
        let now = Instant::now();
        let settled = self.settled(now);
        let lines = if self.waits.iter().any(|w| w.until == Until::Match) { self.visible_lines() } else { Vec::new() };
        let run_finished = matches!(&self.run, Some(r) if r.finished.is_some());
        let mut waits = std::mem::take(&mut self.waits);
        let mut keep = Vec::with_capacity(waits.len());
        for mut w in waits.drain(..) {
            let reason = match w.until {
                Until::None => Some(Reason::None),
                Until::Settle if settled => Some(Reason::Settle),
                Until::Change(g) if self.gen != g => Some(Reason::Change),
                Until::Done if run_finished && settled => Some(Reason::Done),
                Until::Match => self.check_match(&mut w, &lines).map(Reason::Match),
                _ => None,
            };
            let reason = reason.or_else(|| (now >= w.deadline).then_some(Reason::Timeout));
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

    /// The fields that describe how the child ended (section 4.5).
    fn exit_fields(&self, output: Value) -> Map<String, Value> {
        let st = self.exit_status.unwrap_or(ExitStatus { code: None, signal: None, core_dumped: false });
        let mut m = Map::new();
        m.insert("exit_code".into(), st.code.map(Value::from).unwrap_or(Value::Null));
        m.insert("signal".into(), st.signal.map(|s| Value::from(sys::signal_name(s))).unwrap_or(Value::Null));
        m.insert("core_dumped".into(), st.core_dumped.into());
        m.insert("killed_by_waldo".into(), self.kill_reason.is_some().into());
        m.insert("kill_reason".into(), self.kill_reason.map(Value::from).unwrap_or(Value::Null));
        m.insert("spawn_error".into(), Value::Null);
        let end = self.exited_at.unwrap_or_else(Instant::now);
        m.insert("duration_ms".into(), (end.duration_since(self.start).as_millis() as u64).into());
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
        resp.insert("child".into(), self.child_json());
        resp.insert("screen".into(), self.screen_json(w.ansi));

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
            Reason::Exit => {
                let tail = self.outbuf.render(&self.primary_lines(), Some(40));
                resp.insert("exit".into(), Value::Object(self.exit_fields(tail)));
            }
            Reason::Settle => {
                if hint.is_none() {
                    let fg = sys::foreground_pgrp(self.master);
                    if let Some(pg) = fg.filter(|pg| *pg != self.child.pid) {
                        let name = sys::process_name(pg).unwrap_or_else(|| "a command".into());
                        hint = Some(format!(
                            "'{name}' is still running; settle only means the screen went quiet. Use --until match, or waldo-term run."
                        ));
                    }
                }
            }
            Reason::Timeout => {
                if hint.is_none() {
                    hint = match w.until {
                        Until::Settle if !self.seen_output => {
                            Some("the program has not drawn anything yet; settle waits for the first text on screen.".into())
                        }
                        Until::Settle if self.last_change.elapsed() < Duration::from_secs(2) => {
                            Some("the screen never went quiet; wait for specific text with --until match.".into())
                        }
                        Until::Done => Some(
                            "the command is still running; wait for it with 'waldo-term wait --until done', or interrupt it with 'waldo-term send --keys <C-c>'.".into(),
                        ),
                        Until::Match => Some("the pattern did not appear; it is matched against one line at a time. Check the screen above for what is there.".into()),
                        _ => None,
                    };
                }
            }
            _ => {}
        }
        if w.until == Until::Done {
            match (&reason, self.run.as_ref().and_then(|r| r.finished.as_ref())) {
                (Reason::Done, Some(fin)) => {
                    resp.insert("command_exit_code".into(), fin.code.into());
                    resp.insert("command_ms".into(), fin.ms.into());
                    resp.insert("output".into(), fin.output.clone());
                }
                _ => {
                    let ms = self.run.as_ref().map(|r| ms_since(r.started)).unwrap_or(0);
                    resp.insert("command_ms".into(), ms.into());
                    resp.insert("output".into(), self.run_output());
                }
            }
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
        let Some(exited) = self.exited_at else { return false };
        let now = Instant::now();
        self.pty_eof || self.drain_until.map(|d| now >= d).unwrap_or(true) || now.duration_since(exited) >= DRAIN_CAP
    }

    /// The child is gone and its output is drained: send the will, answer
    /// whoever is waiting, clean up.
    fn finish(mut self) -> i32 {
        self.refresh();
        self.collect();
        let st = self.exit_status.unwrap_or(ExitStatus { code: Some(1), signal: None, core_dumped: false });
        self.log(&format!("child exited: code={:?} signal={:?}", st.code, st.signal));

        let output = self.outbuf.render(&self.primary_lines(), None);
        let mut will = envelope(&self.cfg.id, self.seq, "exit", self.start);
        let dropped = self.delivery.as_ref().map(|d| d.dropped).unwrap_or(0);
        if dropped > 0 {
            will.insert("dropped".into(), dropped.into());
        }
        will.extend(self.exit_fields(output));
        will.insert("screen".into(), self.screen_json(false));
        let will_line = serde_json::to_string(&Value::Object(will)).unwrap_or_default();

        // The will goes out first, so a client that sees `exit` can read it.
        let delivered = match self.delivery.take() {
            Some(d) => d.deliver_will(will_line.clone()),
            None => false,
        };
        if !delivered {
            self.log("writing the will to will.json");
            let _ = std::fs::write(self.cfg.dir.join("will.json"), format!("{will_line}\n"));
        }

        for w in std::mem::take(&mut self.waits) {
            self.respond_wait(w, Reason::Exit);
        }
        self.end_attach();
        self.flush_all(Duration::from_secs(1));

        let _ = std::fs::remove_file(&self.cfg.socket);
        if delivered && !self.cfg.dest_is_default {
            // Nothing in the directory is needed any more.
            for f in ["meta.json", "log", "lock"] {
                let _ = std::fs::remove_file(self.cfg.dir.join(f));
            }
            let _ = std::fs::remove_dir(&self.cfg.dir);
        }
        match (st.code, st.signal) {
            (Some(c), _) => c,
            (None, Some(s)) => 128 + s,
            _ => 1,
        }
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
        if resized && !self.conns.values().any(|c| c.attach.as_ref().map(|a| a.resized).unwrap_or(false)) {
            if let Some((cols, rows)) = self.restore_size.take() {
                if self.exit_status.is_none() {
                    let _ = self.do_resize(cols, rows);
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
                let Some(c) = self.conns.get_mut(&id) else { return };
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
        if self.conns.get(&id).map(|c| c.attach.is_some()).unwrap_or(false) {
            return self.attach_input(id);
        }
        loop {
            let parsed = {
                let Some(c) = self.conns.get_mut(&id) else { return };
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
            } else if let Some(t) = value.get("output").and_then(|o| o.get("text")).and_then(|t| t.as_str()) {
                format!("{t}\n")
            } else {
                format!("{}\n", serde_json::to_string_pretty(value).unwrap_or_default())
            };
            http::response(200, "text/plain; charset=utf-8", text.as_bytes(), keep_alive)
        } else {
            http::json_response(status, value, keep_alive)
        };
        let Some(c) = self.conns.get_mut(&id) else { return };
        c.outbuf.extend_from_slice(&bytes);
        c.busy = false;
        if !keep_alive {
            c.close_after = true;
        }
        self.flush_conn(id);
        self.process_conn(id);
    }

    fn error(&mut self, id: u64, req: &Request, status: u16, code: &str, text: &str) {
        self.reply(id, status, &http::error_value(code, text), false, req.keep_alive);
    }

    fn flush_conn(&mut self, id: u64) {
        let Some(c) = self.conns.get_mut(&id) else { return };
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
            let ev = if wants_out { sys::IN | sys::OUT } else { sys::IN };
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
            let ids: Vec<u64> = self.conns.iter().filter(|(_, c)| !c.outbuf.is_empty()).map(|(id, _)| *id).collect();
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
            ("POST", "/v1/run"),
            ("GET", "/v1/output"),
            ("POST", "/v1/resize"),
            ("POST", "/v1/signal"),
            ("GET", "/v1/attach"),
        ];
        match (req.method.as_str(), req.path.as_str()) {
            ("GET", "/v1/status") => self.handle_status(id, &req),
            ("GET", "/v1/screen") => self.handle_wait(id, &req, Until::None, None, None),
            ("POST", "/v1/wait") => self.handle_wait(id, &req, Until::Settle, None, None),
            ("POST", "/v1/input") => self.handle_input(id, &req),
            ("POST", "/v1/run") => self.handle_run(id, &req),
            ("GET", "/v1/output") => self.handle_output(id, &req),
            ("POST", "/v1/resize") => self.handle_resize(id, &req),
            ("POST", "/v1/signal") => self.handle_signal(id, &req),
            ("GET", "/v1/attach") => self.handle_attach(id, &req),
            (_, p) if p.starts_with("/v1/triggers") => self.error(
                id,
                &req,
                501,
                "not_implemented",
                "triggers are not implemented yet; to wait for text use POST /v1/wait with until=match",
            ),
            (m, p) if known.iter().any(|(_, kp)| *kp == p) => {
                let right = known.iter().find(|(_, kp)| *kp == p).map(|(km, _)| *km).unwrap_or("GET");
                self.error(id, &req, 405, "method_not_allowed", &format!("{p} takes {right}, not {m}"));
            }
            (_, p) => {
                let list: Vec<String> = known.iter().map(|(m, p)| format!("{m} {p}")).collect();
                self.error(id, &req, 404, "not_found", &format!("no such endpoint {p}; endpoints: {}", list.join(", ")));
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
                self.error(id, req, 400, "bad_json", "the request body must be a JSON object");
                None
            }
            Err(e) => {
                self.error(id, req, 400, "bad_json", &format!("the request body is not valid JSON: {e}"));
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
            "run_in_progress": matches!(&self.run, Some(r) if r.finished.is_none()),
        });
        self.reply(id, 200, &v, req.wants_text, req.keep_alive);
    }

    /// Register a wait for a request. `body` is the parsed JSON body, if the
    /// request had one. `owner` is the id of the input this request queued.
    fn handle_wait(&mut self, id: u64, req: &Request, default: Until, body: Option<Value>, owner: Option<(u64, Option<String>)>) {
        let body = match body {
            Some(b) => b,
            None if req.method == "POST" && (req.json_body || !req.body.is_empty()) => match self.body_json(id, req) {
                Some(b) => b,
                None => return,
            },
            None => json!({}),
        };
        let wait_obj = body.get("wait").cloned().unwrap_or(Value::Null);
        let field = |name: &str| -> Option<Value> {
            req.query_get(name)
                .map(|s| Value::from(s.to_string()))
                .or_else(|| wait_obj.get(name).cloned())
                .or_else(|| body.get(name).cloned())
        };
        let as_u64 = |v: &Value| v.as_u64().or_else(|| v.as_str().and_then(|s| s.parse().ok()));

        let until_name = field("until").and_then(|v| v.as_str().map(|s| s.to_string()));
        let until = match until_name.as_deref() {
            None => default,
            Some("settle") => Until::Settle,
            Some("match") => Until::Match,
            Some("exit") => Until::Exit,
            Some("none") => Until::None,
            Some("done") => Until::Done,
            Some("change") => match field("since_gen").as_ref().and_then(as_u64) {
                Some(g) => Until::Change(g),
                None => Until::Change(self.gen),
            },
            Some(other) => {
                return self.error(
                    id,
                    req,
                    400,
                    "unknown_until",
                    &format!("unknown wait condition '{other}'; use settle, match, change, exit, none or done"),
                );
            }
        };
        let ansi = match field("style").and_then(|v| v.as_str().map(|s| s.to_string())).as_deref() {
            None | Some("plain") => false,
            Some("ansi") => true,
            Some(other) => {
                return self.error(id, req, 400, "unknown_style", &format!("unknown style '{other}'; use plain or ansi"));
            }
        };
        let default_timeout = if until == Until::Done { RUN_TIMEOUT_MS } else { DEFAULT_TIMEOUT_MS };
        let timeout_ms = match field("timeout_ms").or_else(|| field("timeout")) {
            None => default_timeout,
            Some(v) => match as_u64(&v) {
                Some(n) => n.min(MAX_TIMEOUT_MS),
                None => return self.error(id, req, 400, "bad_timeout", "timeout_ms must be a whole number of milliseconds"),
            },
        };
        let mut pattern = None;
        if until == Until::Match {
            let Some(p) = field("pattern").and_then(|v| v.as_str().map(|s| s.to_string())) else {
                return self.error(
                    id,
                    req,
                    400,
                    "missing_pattern",
                    "until=match needs a pattern: a regular expression matched against one line of the screen (--pattern RE)",
                );
            };
            match regex_lite::RegexBuilder::new(&p).size_limit(1 << 20).build() {
                Ok(re) => pattern = Some((re, p)),
                Err(e) => return self.error(id, req, 400, "bad_pattern", &format!("the pattern is not a valid regular expression: {e}")),
            }
        }
        if until == Until::Done && self.run.is_none() {
            return self.error(
                id,
                req,
                400,
                "no_run",
                "until=done waits for a command started with run, and none has been started; use 'waldo-term run COMMAND'",
            );
        }

        let now = Instant::now();
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
            pattern,
            known: Vec::new(),
            hint,
            ansi,
        };
        // With input, text that is already on screen does not count: the
        // caller is waiting for what its input causes.
        if owner_has_input(&self.queue, req_id) {
            if let Some((re, _)) = w.pattern.as_ref() {
                w.known = self.matches_now(re, &self.visible_lines()).into_iter().map(|m| (m.line, m.text)).collect();
            }
        }
        if self.exit_status.is_some() && until != Until::None {
            // The child is already gone; the answer comes when the pty is
            // drained.
            w.until = Until::Exit;
        }
        self.waits.push(w);
    }

    fn handle_input(&mut self, id: u64, req: &Request) {
        let req_id = self.next_req;
        self.next_req += 1;
        let mut items: Vec<QKind> = Vec::new();
        let mut hint: Option<String> = None;
        let body: Value;

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
            for (n, item) in list.iter().enumerate() {
                match self.read_item(item) {
                    Ok((mut kinds, h)) => {
                        items.append(&mut kinds);
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
            match parse_keys(&text) {
                Ok(mut kinds) => items.append(&mut kinds),
                Err(e) => return self.error(id, req, 400, "unknown_key", &e),
            }
        }

        let mut after_text = false;
        for kind in items {
            let is_key = matches!(kind, QKind::Key { .. });
            self.queue.push_back(QItem { owner: req_id, kind, off: 0, pause_after: false, pause_before: is_key && after_text });
            after_text = !is_key;
        }
        self.handle_wait(id, req, Until::Settle, Some(body), Some((req_id, hint)));
        if !self.waits.iter().any(|w| w.req_id == req_id) {
            // The wait part of the request was refused, so nothing is sent.
            self.queue.retain(|q| q.owner != req_id);
        }
    }

    /// Turn one input item into what goes in the queue. Files are read here,
    /// before anything is sent.
    fn read_item(&self, item: &Value) -> Result<(Vec<QKind>, Option<String>), (&'static str, String)> {
        let obj = item.as_object().filter(|o| o.len() == 1).ok_or((
            "bad_item",
            "each item is an object with exactly one of: text, keys, paste, text_file, keys_file, paste_file".to_string(),
        ))?;
        let (kind, value) = obj.iter().next().unwrap();
        let s = value.as_str().ok_or(("bad_item", format!("the value of '{kind}' must be a string")))?;
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
            "keys" => Ok((parse_keys(&content).map_err(|e| ("unknown_key", e))?, None)),
            "paste" => Ok((vec![QKind::Paste(content.into_bytes())], None)),
            other => Err(("bad_item", format!("unknown item kind '{other}'; use text, keys, paste, text_file, keys_file or paste_file"))),
        }
    }

    fn handle_run(&mut self, id: u64, req: &Request) {
        let Some(body) = self.body_json(id, req) else { return };
        let Some(command) = body.get("command").and_then(|c| c.as_str()).filter(|c| !c.trim().is_empty()) else {
            return self.error(id, req, 400, "missing_command", "the body needs a \"command\", such as {\"command\": \"make test\"}");
        };
        if matches!(&self.run, Some(r) if r.finished.is_none()) {
            return self.error(
                id,
                req,
                409,
                "run_in_progress",
                "a command started by run is still going; wait for it with 'waldo-term wait --until done', or interrupt it with 'waldo-term send --keys <C-c>'",
            );
        }
        let shell = body.get("shell").and_then(|s| s.as_str()).unwrap_or("sh");
        let nonce = util::random_id(10);
        // The command is wrapped in a group so that a trailing `&` or comment
        // cannot swallow the mark that follows it. The mark is a private OSC
        // sequence, which draws nothing; the emulator hands it to the host.
        let (open, close, status) = match shell {
            "sh" | "bash" | "zsh" => ("{ ", "}", "\"$?\""),
            "fish" => ("begin; ", "end", "$status"),
            other => {
                return self.error(id, req, 400, "unknown_shell", &format!("unknown shell '{other}'; use sh (also for bash and zsh) or fish"));
            }
        };
        let echo_tail = format!("{status} {nonce}");
        let line = format!("{open}{command}\n{close}; printf '\\033]{SENTINEL_PREFIX}%s;%s\\007' {echo_tail}");
        let (_, cy) = self.emu.cursor();
        let tracked = if self.emu.alt_screen() { None } else { self.emu.track(Space::Active, cy as u32) };
        self.run = Some(Run { nonce, echo_tail, tracked, started: Instant::now(), finished: None });

        let req_id = self.next_req;
        self.next_req += 1;
        // As a paste where the shell supports it, so tabs and newlines in the
        // command are taken literally; otherwise typed.
        let kind = if self.emu.dec_mode(2004) { QKind::Paste(line.into_bytes()) } else { QKind::Bytes(text_bytes(&line)) };
        self.queue.push_back(QItem { owner: req_id, kind, off: 0, pause_after: false, pause_before: false });
        self.queue.push_back(QItem { owner: req_id, kind: QKind::Bytes(b"\r".to_vec()), off: 0, pause_after: false, pause_before: false });

        let mut wait_body = json!({"wait": {"until": "done"}});
        if let Some(t) = body.get("timeout_ms") {
            wait_body["timeout_ms"] = t.clone();
        }
        self.handle_wait(id, req, Until::Done, Some(wait_body), Some((req_id, None)));
    }

    fn handle_output(&mut self, id: u64, req: &Request) {
        let tail = match req.query_get("tail") {
            None => None,
            Some(t) => match t.parse::<usize>() {
                Ok(n) => Some(n),
                Err(_) => return self.error(id, req, 400, "bad_tail", "tail must be a number of lines, such as ?tail=50"),
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
        let Some(body) = self.body_json(id, req) else { return };
        let dim = |k: &str| body.get(k).and_then(|v| v.as_u64());
        let (Some(cols), Some(rows)) = (dim("cols"), dim("rows")) else {
            return self.error(id, req, 400, "bad_size", "the body needs cols and rows, such as {\"cols\": 100, \"rows\": 30}");
        };
        if !(MIN_COLS as u64..=MAX_COLS as u64).contains(&cols) || !(MIN_ROWS as u64..=MAX_ROWS as u64).contains(&rows) {
            return self.error(
                id,
                req,
                400,
                "bad_size",
                &format!("the size must be between {MIN_COLS}x{MIN_ROWS} and {MAX_COLS}x{MAX_ROWS} (cols x rows)"),
            );
        }
        if let Err(e) = self.do_resize(cols as u16, rows as u16) {
            return self.error(id, req, 500, "resize_failed", &e);
        }
        // An explicit resize is the new size to keep.
        self.restore_size = None;
        self.handle_wait(id, req, Until::None, Some(json!({})), None);
    }

    fn handle_signal(&mut self, id: u64, req: &Request) {
        let Some(body) = self.body_json(id, req) else { return };
        if body.get("stop").and_then(|s| s.as_bool()) == Some(true) {
            self.stop("request");
        } else if let Some(name) = body.get("signal").and_then(|s| s.as_str()) {
            let Some(sig) = sys::signal_number(name) else {
                return self.error(id, req, 400, "unknown_signal", &format!("unknown signal '{name}'; use a name such as SIGINT, SIGTERM or SIGKILL"));
            };
            self.signal_child(sig);
        } else {
            return self.error(id, req, 400, "bad_signal", "the body needs {\"signal\": \"SIGINT\"} or {\"stop\": true}");
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

fn parse_keys(notation: &str) -> Result<Vec<QKind>, String> {
    Ok(keys::parse(notation)?
        .into_iter()
        .map(|t| match t {
            Tok::Text(s) => QKind::Bytes(text_bytes(&s)),
            Tok::Key { key, mods, text, is_esc } => QKind::Key { key, mods, text, is_esc },
        })
        .collect())
}

fn read_file_item(path: &str) -> Result<String, String> {
    if !path.starts_with('/') {
        return Err(format!("{path}: the path must be absolute, because the host reads the file"));
    }
    let md = std::fs::metadata(path).map_err(|e| format!("{path}: {e}"))?;
    if !md.is_file() {
        return Err(format!("{path}: not a regular file"));
    }
    if md.size() > MAX_FILE_ITEM {
        return Err(format!("{path}: larger than 1 MiB; split it into several requests"));
    }
    let bytes = std::fs::read(path).map_err(|e| format!("{path}: {e}"))?;
    String::from_utf8(bytes).map_err(|_| format!("{path}: not UTF-8 text"))
}

/// The fields every message to the destination starts with (section 4).
pub fn envelope(session: &str, seq: u64, trigger: &str, start: Instant) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("v".into(), 1.into());
    m.insert("session".into(), session.into());
    m.insert("seq".into(), seq.into());
    m.insert("trigger".into(), trigger.into());
    m.insert("time".into(), util::rfc3339_now().into());
    m.insert("t_ms".into(), ms_since(start).into());
    m
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
}

fn hash_str(s: &str) -> u64 {
    let mut h = DefaultHasher::new();
    h.write(s.as_bytes());
    h.finish()
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok()).collect()
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
        self.hold_since = None;
        self.collect();
        self.refresh();
        self.last_change = Instant::now();
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
            req.query_get(name).and_then(|v| v.parse::<u16>().ok()).filter(|n| *n >= 1).unwrap_or(default).min(1000)
        };
        let attach = Attach {
            view_cols: dim("cols", self.emu.cols()),
            view_rows: dim("rows", self.emu.rows()),
            sent: Vec::new(),
            sent_meta: 0,
            key: None,
            last_sent: Instant::now() - FRAME_INTERVAL,
            resized: false,
        };
        let Some(c) = self.conns.get_mut(&id) else { return };
        c.outbuf.extend_from_slice(b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: waldo-attach\r\nConnection: Upgrade\r\n\r\n");
        c.attach = Some(attach);
        self.flush_conn(id);
        self.attach_input(id);
    }

    /// Handle the lines an attached client has sent.
    fn attach_input(&mut self, id: u64) {
        loop {
            let line = {
                let Some(c) = self.conns.get_mut(&id) else { return };
                let Some(nl) = c.inbuf.iter().position(|b| *b == b'\n') else {
                    if c.inbuf.len() > (1 << 20) {
                        self.drop_conn(id);
                    }
                    return;
                };
                c.inbuf.drain(..=nl).collect::<Vec<u8>>()
            };
            let Ok(msg) = serde_json::from_slice::<Value>(&line) else { continue };
            let size = |v: &Value| -> Option<(u16, u16)> {
                Some((v.get("cols")?.as_u64()? as u16, v.get("rows")?.as_u64()? as u16))
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
                let (cols, rows) = (cols.clamp(MIN_COLS, MAX_COLS), rows.clamp(MIN_ROWS, MAX_ROWS));
                if (cols, rows) != (self.emu.cols(), self.emu.rows()) {
                    let before = (self.emu.cols(), self.emu.rows());
                    if self.do_resize(cols, rows).is_ok() {
                        self.restore_size.get_or_insert(before);
                    }
                }
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
                    self.queue.push_back(QItem { owner, kind: QKind::Bytes(bytes), off: 0, pause_after: false, pause_before: false });
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
        if self.hold_since.map(|h| now.duration_since(h) < HOLD_CAP).unwrap_or(false) {
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
                    .map(|a| a.key != Some(key) && now >= a.last_sent + FRAME_INTERVAL && c.outbuf.len() < ATTACH_BACKLOG)
                    .unwrap_or(false)
            })
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            let Some(a) = self.conns.get(&id).and_then(|c| c.attach.as_ref()) else { continue };
            let cols = a.view_cols.min(self.emu.cols());
            let rows = a.view_rows.min(self.emu.rows());
            let (cx, cy) = self.emu.cursor();
            // A smaller viewer sees the part of the screen with the cursor.
            let off_y = if cy >= rows { cy - rows + 1 } else { 0 };
            let off_x = if cx >= cols { cx - cols + 1 } else { 0 };
            let mut lines: Vec<Value> = Vec::new();
            let mut hashes: Vec<u64> = Vec::with_capacity(rows as usize);
            for r in 0..rows {
                let text = self.emu.row_ansi(Space::Active, (r + off_y) as u32, off_x, cols).unwrap_or_default();
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
                "modes": {
                    "app_cursor": self.emu.dec_mode(1),
                    "app_keypad": self.emu.dec_mode(66),
                    "bracketed_paste": self.emu.dec_mode(2004),
                },
            });
            let meta_hash = hash_str(&meta.to_string());
            let changed = !lines.is_empty() || meta_hash != a.sent_meta;
            let mut frame = meta;
            frame["lines"] = Value::Array(lines);
            let Some(c) = self.conns.get_mut(&id) else { continue };
            if let Some(a) = c.attach.as_mut() {
                a.key = Some(key);
                if !changed {
                    continue;
                }
                a.sent = hashes;
                a.sent_meta = meta_hash;
                a.last_sent = now;
            }
            c.outbuf.extend_from_slice(frame.to_string().as_bytes());
            c.outbuf.push(b'\n');
            self.flush_conn(id);
        }
    }

    /// Tell attached clients the session is over.
    fn end_attach(&mut self) {
        let ids: Vec<u64> = self.conns.iter().filter(|(_, c)| c.attach.is_some()).map(|(id, _)| *id).collect();
        for id in ids {
            if let Some(c) = self.conns.get_mut(&id) {
                c.outbuf.extend_from_slice(b"{\"ended\":true}\n");
            }
            self.flush_conn(id);
        }
    }
}

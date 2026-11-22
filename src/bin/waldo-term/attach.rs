//! `waldo-term attach`: watch a session live, and optionally type into it
//! (DESIGN.md section 7).
//!
//! The host sends the screen as rows of styled text; this draws them inside
//! the person's own terminal with a status bar on the last line. It never
//! passes the program's own bytes through, and it never resizes the session
//! unless asked to.

use serde_json::{json, Value};
use std::io::{Read, Write};
use std::os::unix::io::AsRawFd;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

/// Ctrl-], the one key attach keeps for itself: it switches between
/// viewing and interacting. Whoever has the keyboard gets every other key.
/// While interacting that is the program. While viewing it is this viewer,
/// so Ctrl-C leaves, as it would leave any program.
const MODE_KEY: u8 = 0x1d;
const LEAVE_KEY: u8 = 0x03; // Ctrl-C
/// How long something another client said stays in the status bar.
const NOTICE_TIME: Duration = Duration::from_secs(12);
/// The most a broadcast may hold (PROTOCOL.md section 4.10).
const SAY_MAX: usize = 1000;

pub enum Outcome {
    Detached,
    Ended,
    Lost(String),
}

pub struct Options {
    pub drive: bool,
    pub resize: bool,
    /// The name this person's broadcasts carry.
    pub name: Option<String>,
}

pub fn term_size() -> (u16, u16) {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    if unsafe { libc::ioctl(1, libc::TIOCGWINSZ, &mut ws) } == 0 && ws.ws_col > 0 && ws.ws_row > 0 {
        (ws.ws_col, ws.ws_row)
    } else {
        (80, 24)
    }
}

pub fn is_terminal() -> bool {
    unsafe { libc::isatty(0) == 1 && libc::isatty(1) == 1 }
}

/// Puts the terminal in raw mode on the alternate screen, and puts it back
/// when dropped.
struct RawTerminal {
    saved: libc::termios,
}

impl RawTerminal {
    fn enter() -> Option<RawTerminal> {
        unsafe {
            let mut saved: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(0, &mut saved) != 0 {
                return None;
            }
            let mut raw = saved;
            libc::cfmakeraw(&mut raw);
            if libc::tcsetattr(0, libc::TCSANOW, &raw) != 0 {
                return None;
            }
            put(b"\x1b[?1049h\x1b[2J\x1b[H");
            Some(RawTerminal { saved })
        }
    }
}

impl Drop for RawTerminal {
    fn drop(&mut self) {
        // Undo every mode a frame may have set, then leave the alternate
        // screen.
        put(b"\x1b[0m\x1b[?1l\x1b>\x1b[?2004l\x1b[?25h\x1b[?1049l");
        unsafe { libc::tcsetattr(0, libc::TCSANOW, &self.saved) };
    }
}

fn put(bytes: &[u8]) {
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(bytes);
    let _ = out.flush();
}

fn send(stream: &mut UnixStream, msg: &Value) -> bool {
    let mut line = msg.to_string();
    line.push('\n');
    stream.write_all(line.as_bytes()).is_ok()
}

#[derive(Default)]
struct View {
    drive: bool,
    /// Rows drawn by the last frame, to clear what a smaller frame leaves.
    rows_drawn: u64,
    app_cursor: bool,
    app_keypad: bool,
    bracketed_paste: bool,
    last: Option<Value>,
    /// The line being written to say to the other clients, while the
    /// one-line editor is open.
    compose: Option<Vec<u8>>,
    /// What another client last said, and when it arrived.
    notice: Option<(String, Instant)>,
    /// This person's own name, to tell their broadcasts from others'.
    name: Option<String>,
}

/// Cut plain text to a number of characters.
fn fit(text: &str, width: usize) -> String {
    let mut s: String = text.chars().take(width).collect();
    let have = s.chars().count();
    s.extend(std::iter::repeat_n(' ', width.saturating_sub(have)));
    s
}

impl View {
    fn status_bar(&self, frame: &Value, term: (u16, u16)) -> String {
        let n = |v: &Value, k: &str| v.get(k).and_then(|x| x.as_u64()).unwrap_or(0);
        let bar = |text: &str| {
            format!(
                "\x1b[{};1H\x1b[0;7m{}\x1b[0m",
                term.1,
                fit(text, term.0 as usize)
            )
        };
        // The editor takes the whole bar, and so does what someone said.
        if let Some(line) = self.compose.as_ref() {
            return bar(&format!(" say: {}", String::from_utf8_lossy(line)));
        }
        if let Some((said, _)) = self.notice.as_ref() {
            let how = if self.drive { "Ctrl-] then s" } else { "s" };
            return bar(&format!(" {said}   |  {how} to answer"));
        }
        let view = frame.get("view").cloned().unwrap_or(Value::Null);
        // The most important things first: a narrow terminal cuts the rest.
        let mut first = format!(
            " waldo {}  {}x{}",
            frame.get("session").and_then(|s| s.as_str()).unwrap_or("?"),
            n(frame, "cols"),
            n(frame, "rows")
        );
        if n(&view, "cols") < n(frame, "cols") || n(&view, "rows") < n(frame, "rows") {
            first.push_str(&format!(
                " (showing {}x{})",
                n(&view, "cols"),
                n(&view, "rows")
            ));
        }
        let mut parts = vec![first];
        // What is happening comes before the reminders of the keys.
        if n(frame, "busy") > 0 {
            parts.push("agent request in flight".into());
        }
        if self.drive {
            parts.push("TYPING (Ctrl-] to stop)".into());
        } else {
            parts.push("view only".into());
            parts.push("Ctrl-] type".into());
            if Self::can_say(frame) {
                parts.push("s say".into());
            }
            parts.push("Ctrl-C leave".into());
        }
        bar(&parts.join("  |  "))
    }

    /// Whether the session lets its clients broadcast. A host that does
    /// not say is one from before the option existed, where they could.
    fn can_say(frame: &Value) -> bool {
        frame.get("broadcasting").and_then(|b| b.as_bool()) != Some(false)
    }

    /// Draw the status bar again from the last frame, without its rows.
    fn redraw(&mut self, term: (u16, u16)) {
        if let Some(frame) = self.last.clone() {
            let mut again = frame;
            again["lines"] = json!([]);
            again.as_object_mut().map(|o| o.remove("broadcasts"));
            put(self.render(&again, term).as_bytes());
        }
    }

    /// Turn a frame into what to write to the terminal.
    fn render(&mut self, frame: &Value, term: (u16, u16)) -> String {
        let mut out = String::from("\x1b[?25l");
        let n = |v: &Value, k: &str| v.get(k).and_then(|x| x.as_u64()).unwrap_or(0);
        // What other clients said rides on a frame.
        let said: Vec<String> = frame
            .get("broadcasts")
            .and_then(|b| b.as_array())
            .into_iter()
            .flatten()
            .map(|b| {
                let text = b.get("text").and_then(|t| t.as_str()).unwrap_or("");
                match b.get("from").and_then(|f| f.as_str()) {
                    Some(from) if Some(from) == self.name.as_deref() => {
                        format!("you said: {text}")
                    }
                    Some(from) => format!("{from}: {text}"),
                    None => format!("someone: {text}"),
                }
            })
            .collect();
        if !said.is_empty() {
            self.notice = Some((said.join("  /  "), Instant::now()));
        }
        // Mirror the modes that change what the keyboard sends, so that what
        // a person types here is encoded the way the program expects.
        if let Some(m) = frame.get("modes") {
            let flag = |k: &str| m.get(k).and_then(|b| b.as_bool()).unwrap_or(false);
            for (now, was, on, off) in [
                (
                    flag("app_cursor"),
                    &mut self.app_cursor,
                    "\x1b[?1h",
                    "\x1b[?1l",
                ),
                (flag("app_keypad"), &mut self.app_keypad, "\x1b=", "\x1b>"),
                (
                    flag("bracketed_paste"),
                    &mut self.bracketed_paste,
                    "\x1b[?2004h",
                    "\x1b[?2004l",
                ),
            ] {
                if now != *was {
                    out.push_str(if now { on } else { off });
                    *was = now;
                }
            }
        }
        if let Some(lines) = frame.get("lines").and_then(|l| l.as_array()) {
            for l in lines {
                let (Some(row), Some(text)) = (
                    l.get(0).and_then(|r| r.as_u64()),
                    l.get(1).and_then(|t| t.as_str()),
                ) else {
                    continue;
                };
                if row + 1 < term.1 as u64 {
                    out.push_str(&format!("\x1b[{};1H\x1b[0m{}\x1b[0m\x1b[K", row + 1, text));
                }
            }
        }
        let rows = frame.get("view").map(|v| n(v, "rows")).unwrap_or(0);
        for r in rows..self.rows_drawn {
            if r + 1 < term.1 as u64 {
                out.push_str(&format!("\x1b[{};1H\x1b[0m\x1b[K", r + 1));
            }
        }
        self.rows_drawn = rows;
        out.push_str(&self.status_bar(frame, term));
        if let Some(c) = frame.get("cursor") {
            if c.get("visible").and_then(|v| v.as_bool()) == Some(true)
                && n(c, "row") + 1 < term.1 as u64
            {
                out.push_str(&format!(
                    "\x1b[{};{}H\x1b[?25h",
                    n(c, "row") + 1,
                    n(c, "col") + 1
                ));
            }
        }
        // While the editor is open the cursor is in the bar, after the text.
        if let Some(line) = self.compose.as_ref() {
            let col = 7 + String::from_utf8_lossy(line).chars().count();
            out.push_str(&format!(
                "\x1b[{};{}H\x1b[?25h",
                term.1,
                col.min(term.0 as usize)
            ));
        }
        out
    }
}

/// Run an attach session over a connected socket. `leftover` is anything
/// that arrived after the upgrade response.
pub fn run(mut stream: UnixStream, leftover: Vec<u8>, opts: Options) -> Outcome {
    let mut term = term_size();
    let content = |t: (u16, u16)| json!({"cols": t.0, "rows": t.1.saturating_sub(1).max(1)});
    if opts.resize {
        send(&mut stream, &json!({"resize": content(term)}));
    }
    let Some(_raw) = RawTerminal::enter() else {
        return Outcome::Lost("cannot put the terminal in raw mode".into());
    };
    let mut view = View {
        drive: opts.drive,
        name: opts.name.clone(),
        ..Default::default()
    };
    let mut inbuf = leftover;
    let sock = stream.as_raw_fd();
    loop {
        // Frames already received.
        while let Some(nl) = inbuf.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = inbuf.drain(..=nl).collect();
            let Ok(frame) = serde_json::from_slice::<Value>(&line) else {
                continue;
            };
            if frame.get("ended").and_then(|e| e.as_bool()) == Some(true) {
                return Outcome::Ended;
            }
            put(view.render(&frame, term).as_bytes());
            view.last = Some(frame);
        }

        let mut fds = [
            libc::pollfd {
                fd: 0,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: sock,
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let n = unsafe { libc::poll(fds.as_mut_ptr(), 2, 200) };
        if n < 0 && std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
            return Outcome::Lost("poll failed".into());
        }

        // What someone said has been shown for long enough.
        if view
            .notice
            .as_ref()
            .map(|(_, at)| at.elapsed() >= NOTICE_TIME)
            .unwrap_or(false)
        {
            view.notice = None;
            view.redraw(term);
        }

        // The window may have changed size.
        let now = term_size();
        if now != term {
            term = now;
            put(b"\x1b[0m\x1b[2J");
            view.rows_drawn = 0;
            send(&mut stream, &json!({"viewport": content(term)}));
            if opts.resize {
                send(&mut stream, &json!({"resize": content(term)}));
            }
        }

        if fds[0].revents != 0 {
            let mut buf = [0u8; 4096];
            let got = unsafe { libc::read(0, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
            if got <= 0 {
                return Outcome::Detached;
            }
            let mut typed: Vec<u8> = Vec::new();
            let chunk = &buf[..got as usize];
            for (i, &b) in chunk.iter().enumerate() {
                // The one-line editor has the keyboard while it is open.
                if let Some(mut line) = view.compose.take() {
                    match b {
                        // Enter says it.
                        b'\r' | b'\n' => {
                            let text = String::from_utf8_lossy(&line).trim().to_string();
                            if !text.is_empty() {
                                let mut said = json!({"text": text});
                                if let Some(name) = opts.name.as_ref() {
                                    said["as"] = name.clone().into();
                                }
                                if !send(&mut stream, &json!({"broadcast": said})) {
                                    return Outcome::Ended;
                                }
                            }
                        }
                        // Escape by itself, or Ctrl-C, gives up. An escape
                        // with more after it is an arrow or function key;
                        // the rest of what was read belongs to it.
                        0x1b if i + 1 < chunk.len() => {
                            view.compose = Some(line);
                            break;
                        }
                        0x1b | 0x03 => {}
                        0x7f | 0x08 => {
                            // Remove one character, which may be several bytes.
                            while let Some(last) = line.pop() {
                                if last & 0xc0 != 0x80 {
                                    break;
                                }
                            }
                            view.compose = Some(line);
                        }
                        0x15 => view.compose = Some(Vec::new()),
                        _ if b >= 0x20 && line.len() < SAY_MAX => {
                            line.push(b);
                            view.compose = Some(line);
                        }
                        _ => view.compose = Some(line),
                    }
                    view.redraw(term);
                    continue;
                }
                match b {
                    // Typing: everything is the program's but the one key.
                    MODE_KEY => {
                        view.drive = !view.drive;
                        view.redraw(term);
                    }
                    _ if view.drive => typed.push(b),
                    // Viewing: the keyboard is this viewer's.
                    LEAVE_KEY => {
                        if !typed.is_empty() {
                            let hex: String = typed.iter().map(|b| format!("{b:02x}")).collect();
                            let _ = send(&mut stream, &json!({"keys_hex": hex}));
                        }
                        let _ = send(&mut stream, &json!({"detach": true}));
                        return Outcome::Detached;
                    }
                    b's' if view.last.as_ref().map(View::can_say).unwrap_or(true) => {
                        view.compose = Some(Vec::new());
                        view.notice = None;
                        view.redraw(term);
                    }
                    _ => {}
                }
            }
            if !typed.is_empty() {
                let hex: String = typed.iter().map(|b| format!("{b:02x}")).collect();
                if !send(&mut stream, &json!({"keys_hex": hex})) {
                    return Outcome::Ended;
                }
            }
        }

        if fds[1].revents != 0 {
            let mut buf = [0u8; 65536];
            match stream.read(&mut buf) {
                Ok(0) | Err(_) => return Outcome::Ended,
                Ok(got) => inbuf.extend_from_slice(&buf[..got]),
            }
        }
    }
}

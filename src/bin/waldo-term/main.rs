//! waldo-term: the command-line client for waldo-wrap sessions.
//!
//! It adds argument parsing, text formatting and help to the control API
//! (DESIGN.md section 2.4). Everything else happens in the host.

mod attach;
mod help;

use serde_json::{json, Map, Value};
use std::fs::File;
use std::io::{Read, Write};
use std::os::unix::io::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use waldo::session::{self, Session, State};
use waldo::{render, util};

const EXIT_OK: i32 = 0;
const EXIT_OTHER: i32 = 1;
const EXIT_USAGE: i32 = 2;
const EXIT_CHILD_EXITED: i32 = 3;
const EXIT_NO_SESSION: i32 = 4;
const EXIT_RUN_FAILED: i32 = 5;
const EXIT_TIMEOUT: i32 = 124;

fn die(code: i32, msg: &str) -> ! {
    eprintln!("waldo-term: {msg}");
    std::process::exit(code);
}

fn usage(msg: &str) -> ! {
    die(EXIT_USAGE, &format!("{msg}\nRun 'waldo-term help' for a guide, or 'waldo-term help commands' for every option."));
}

// ---- arguments -------------------------------------------------------------

#[derive(Default)]
struct Opts {
    session: Option<String>,
    until: Option<String>,
    pattern: Option<String>,
    timeout_ms: Option<u64>,
    json: bool,
    items: Vec<Value>,
    tail: Option<u64>,
    shell: Option<String>,
    stop: bool,
    all: bool,
    older_than: Option<i64>,
    style: Option<String>,
    drive: bool,
    resize: bool,
    rest: Vec<String>,
}

fn read_stdin() -> String {
    let mut s = String::new();
    if std::io::stdin().read_to_string(&mut s).is_err() {
        die(EXIT_OTHER, "cannot read stdin as UTF-8 text");
    }
    s
}

fn parse_ms(v: &str) -> Option<u64> {
    if let Some(s) = v.strip_suffix("ms") {
        return s.parse().ok();
    }
    if let Some(s) = v.strip_suffix('s') {
        return s.parse::<f64>().ok().filter(|f| *f >= 0.0).map(|f| (f * 1000.0) as u64);
    }
    v.parse().ok()
}

/// Parse a subcommand's arguments. `allowed` lists the options it takes;
/// `positional` says whether it takes bare words.
fn parse_opts(cmd: &str, args: &[String], allowed: &[&str], positional: bool) -> Opts {
    let mut o = Opts::default();
    let mut stdin_used = false;
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        i += 1;
        if o.rest.is_empty() && arg.starts_with('-') && arg != "-" && arg != "--" {
            if arg == "-h" || arg == "--help" {
                print!("{}", help::COMMANDS);
                std::process::exit(0);
            }
            let name = if arg == "-s" { "--session" } else { arg };
            if !allowed.contains(&name) {
                usage(&format!("'{cmd}' does not take {arg}"));
            }
            let mut value = || -> String {
                let v = args.get(i).cloned().unwrap_or_else(|| usage(&format!("{arg} needs a value")));
                i += 1;
                v
            };
            match name {
                "--session" => o.session = Some(value()),
                "--until" => o.until = Some(value()),
                "--pattern" => o.pattern = Some(value()),
                "--timeout" => {
                    let v = value();
                    o.timeout_ms = Some(parse_ms(&v).unwrap_or_else(|| usage("--timeout takes milliseconds, such as 5000, or seconds as 5s")));
                }
                "--json" => o.json = true,
                "--stop" => o.stop = true,
                "--drive" => o.drive = true,
                "--resize" => o.resize = true,
                "--style" => o.style = Some(value()),
                "--all" => o.all = true,
                "--older-than" => {
                    let v = value();
                    o.older_than = Some(parse_ms(&format!("{v}s")).map(|ms| (ms / 1000) as i64).unwrap_or_else(|| usage("--older-than takes seconds")));
                }
                "--tail" => o.tail = Some(value().parse().unwrap_or_else(|_| usage("--tail takes a number of lines"))),
                "--shell" => o.shell = Some(value()),
                "--text" | "--keys" | "--paste" => {
                    let mut v = value();
                    if v == "-" {
                        if stdin_used {
                            usage("only one item can read from stdin");
                        }
                        stdin_used = true;
                        v = read_stdin();
                    }
                    let mut m = Map::new();
                    m.insert(name[2..].to_string(), v.into());
                    o.items.push(Value::Object(m));
                }
                "--text-file" | "--keys-file" | "--paste-file" => {
                    let v = value();
                    // The host reads the file, so it needs the full path.
                    let abs = std::path::absolute(&v).unwrap_or_else(|_| PathBuf::from(&v));
                    let mut m = Map::new();
                    m.insert(name[2..].replace('-', "_"), abs.display().to_string().into());
                    o.items.push(Value::Object(m));
                }
                _ => usage(&format!("'{cmd}' does not take {arg}")),
            }
        } else if arg == "--" && o.rest.is_empty() {
            o.rest.extend(args[i..].iter().cloned());
            break;
        } else {
            if !positional {
                let hint = if cmd == "send" { "; say what it is with --text or --keys" } else { "" };
                usage(&format!("'{cmd}' does not take the argument '{arg}'{hint}"));
            }
            o.rest.push(arg.to_string());
        }
    }
    o
}

// ---- finding the session -----------------------------------------------------

enum Target {
    Live { id: String, socket: PathBuf, dir: Option<PathBuf> },
    Ended(Session),
}

fn target(sel: Option<&str>) -> Target {
    match sel {
        Some(s) if s.contains('/') => {
            let socket = PathBuf::from(s);
            let dir = socket.parent().map(|p| p.to_path_buf());
            let id = dir.as_ref().and_then(|d| d.file_name()).map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            Target::Live { id, socket, dir }
        }
        Some(s) => match session::find(s) {
            Ok(sess) if sess.state == State::Live => Target::Live { id: sess.id.clone(), socket: sess.socket(), dir: Some(sess.dir) },
            Ok(sess) => Target::Ended(sess),
            Err(e) => die(EXIT_NO_SESSION, &e),
        },
        None => {
            let live: Vec<Session> = session::list().into_iter().filter(|s| s.state == State::Live).collect();
            match live.len() {
                1 => Target::Live { id: live[0].id.clone(), socket: live[0].socket(), dir: Some(live[0].dir.clone()) },
                0 => die(EXIT_NO_SESSION, "no live session; start one with: waldo-wrap --detach -- PROGRAM"),
                _ => die(
                    EXIT_USAGE,
                    &format!("several sessions are live; pick one with -s: {}", live.iter().map(|s| s.id.as_str()).collect::<Vec<_>>().join(", ")),
                ),
            }
        }
    }
}

/// The session has ended: print its will if there is one. Never returns.
fn ended(sess: &Session, as_json: bool, ok_status: i32) -> ! {
    match session::read_will(&sess.dir, sess.meta.as_ref()) {
        Some(will) => {
            if as_json {
                println!("{will}");
            } else {
                print!("{}", render::will(&will));
            }
            std::process::exit(ok_status);
        }
        None => die(
            EXIT_NO_SESSION,
            &format!(
                "session '{}' is dead: its host was killed and left no will. Remove it with 'waldo-term gc --all' and start again.",
                sess.id
            ),
        ),
    }
}

// ---- talking to the host -------------------------------------------------------

fn connect(socket: &Path) -> std::io::Result<UnixStream> {
    if socket.as_os_str().len() < 100 {
        return UnixStream::connect(socket);
    }
    // Too long for a socket address: go through a directory fd.
    let dir = socket.parent().unwrap_or(Path::new("/"));
    let name = socket.file_name().unwrap_or_default().to_string_lossy().into_owned();
    let d = File::open(dir)?;
    UnixStream::connect(format!("/proc/self/fd/{}/{}", d.as_raw_fd(), name))
}

fn http(stream: &mut UnixStream, method: &str, path: &str, body: Option<&Value>) -> Result<(u16, Value), String> {
    let body_bytes = body.map(|b| serde_json::to_vec(b).unwrap_or_default()).unwrap_or_default();
    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: waldo\r\nConnection: close\r\nAccept: application/json\r\n");
    if body.is_some() {
        req.push_str(&format!("Content-Type: application/json\r\nContent-Length: {}\r\n", body_bytes.len()));
    }
    req.push_str("\r\n");
    stream.write_all(req.as_bytes()).map_err(|e| format!("cannot send the request: {e}"))?;
    stream.write_all(&body_bytes).map_err(|e| format!("cannot send the request: {e}"))?;
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).map_err(|e| format!("no response from the session: {e}"))?;
    if raw.is_empty() {
        return Err("closed".into());
    }
    let mut headers = [httparse::EMPTY_HEADER; 32];
    let mut resp = httparse::Response::new(&mut headers);
    let head = match resp.parse(&raw) {
        Ok(httparse::Status::Complete(n)) => n,
        _ => return Err("the session sent a response that is not HTTP".into()),
    };
    let status = resp.code.unwrap_or(0);
    let value: Value = serde_json::from_slice(&raw[head..]).map_err(|e| format!("the session sent a response that is not JSON: {e}"))?;
    Ok((status, value))
}

/// Send one request to a session. If the session has ended, or ends before
/// answering, its will is printed instead and this does not return.
fn call(sel: Option<&str>, method: &str, path: &str, body: Option<&Value>, as_json: bool, ended_status: i32) -> (String, u16, Value) {
    let (id, socket, dir) = match target(sel) {
        Target::Live { id, socket, dir } => (id, socket, dir),
        Target::Ended(sess) => ended(&sess, as_json, ended_status),
    };
    let gone = |why: &str| -> ! {
        // The host may be on its way out: it releases its lock as it exits.
        if let Some(d) = dir.as_ref() {
            let end = Instant::now() + Duration::from_secs(3);
            while session::lock_is_held(d) && Instant::now() < end {
                std::thread::sleep(Duration::from_millis(20));
            }
            if !session::lock_is_held(d) {
                if let Ok(sess) = session::find(&id) {
                    ended(&sess, as_json, ended_status);
                }
            }
        }
        die(EXIT_NO_SESSION, &format!("cannot reach session '{id}' at {}: {why}", socket.display()));
    };
    let mut stream = match connect(&socket) {
        Ok(s) => s,
        Err(e) => gone(&e.to_string()),
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(660)));
    match http(&mut stream, method, path, body) {
        Ok((status, value)) => (id, status, value),
        Err(e) if e == "closed" => gone("it closed the connection"),
        Err(e) => die(EXIT_OTHER, &e),
    }
}

fn check_status(status: u16, value: &Value) {
    if status == 200 {
        return;
    }
    let text = value.get("text").and_then(|t| t.as_str()).unwrap_or("the session refused the request");
    let code = value.get("error").and_then(|t| t.as_str()).unwrap_or("error");
    die(if (400..500).contains(&status) && status != 409 { EXIT_USAGE } else { EXIT_OTHER }, &format!("{text} ({code})"));
}

/// Print a screen response and exit with the status its reason calls for.
fn finish(value: &Value, as_json: bool, wanted_exit: bool) -> ! {
    if as_json {
        println!("{value}");
    } else {
        print!("{}", render::response(value));
    }
    let code = match value.get("reason").and_then(|r| r.as_str()).unwrap_or("") {
        "timeout" => EXIT_TIMEOUT,
        "exit" if !wanted_exit => EXIT_CHILD_EXITED,
        "done" => match value.get("command_exit_code").and_then(|c| c.as_i64()) {
            Some(0) => EXIT_OK,
            _ => EXIT_RUN_FAILED,
        },
        _ => EXIT_OK,
    };
    std::process::exit(code);
}

fn wait_object(o: &Opts) -> Map<String, Value> {
    let mut wait = Map::new();
    if let Some(s) = &o.style {
        wait.insert("style".into(), s.clone().into());
    }
    if let Some(u) = &o.until {
        wait.insert("until".into(), u.clone().into());
    }
    if let Some(p) = &o.pattern {
        wait.insert("pattern".into(), p.clone().into());
        if o.until.is_none() {
            wait.insert("until".into(), "match".into());
        }
    }
    wait
}

fn wanted_exit(o: &Opts) -> bool {
    o.until.as_deref() == Some("exit")
}

fn encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

// ---- commands ------------------------------------------------------------------

const WAIT_OPTS: [&str; 6] = ["--session", "--until", "--pattern", "--timeout", "--json", "--style"];

fn cmd_send(args: &[String]) -> ! {
    let mut allowed = WAIT_OPTS.to_vec();
    allowed.extend(["--text", "--keys", "--paste", "--text-file", "--keys-file", "--paste-file"]);
    let o = parse_opts("send", args, &allowed, false);
    if o.items.is_empty() {
        usage("'send' needs something to send: --text S, --keys S, --paste S, or a --*-file PATH");
    }
    let mut body = json!({"input": o.items, "wait": wait_object(&o)});
    if let Some(t) = o.timeout_ms {
        body["timeout_ms"] = t.into();
    }
    let (_, status, value) = call(o.session.as_deref(), "POST", "/v1/input", Some(&body), o.json, EXIT_CHILD_EXITED);
    check_status(status, &value);
    finish(&value, o.json, wanted_exit(&o));
}

fn cmd_screen(args: &[String]) -> ! {
    let o = parse_opts("screen", args, &WAIT_OPTS, false);
    let mut q: Vec<String> = Vec::new();
    for (k, v) in wait_object(&o) {
        q.push(format!("{k}={}", encode(v.as_str().unwrap_or(""))));
    }
    if let Some(t) = o.timeout_ms {
        q.push(format!("timeout_ms={t}"));
    }
    let path = if q.is_empty() { "/v1/screen".to_string() } else { format!("/v1/screen?{}", q.join("&")) };
    let (_, status, value) = call(o.session.as_deref(), "GET", &path, None, o.json, EXIT_CHILD_EXITED);
    check_status(status, &value);
    finish(&value, o.json, wanted_exit(&o));
}

fn cmd_wait(args: &[String]) -> ! {
    let o = parse_opts("wait", args, &WAIT_OPTS, false);
    let mut body = json!({"wait": wait_object(&o)});
    if let Some(t) = o.timeout_ms {
        body["timeout_ms"] = t.into();
    }
    // Waiting for the end of a session that has already ended is a success.
    let ended_status = if wanted_exit(&o) { EXIT_OK } else { EXIT_CHILD_EXITED };
    let (_, status, value) = call(o.session.as_deref(), "POST", "/v1/wait", Some(&body), o.json, ended_status);
    check_status(status, &value);
    finish(&value, o.json, wanted_exit(&o));
}

fn cmd_run(args: &[String]) -> ! {
    let o = parse_opts("run", args, &["--session", "--timeout", "--shell", "--json"], true);
    if o.rest.is_empty() {
        usage("'run' needs a command, such as: waldo-term run 'make test'");
    }
    let mut body = json!({"command": o.rest.join(" ")});
    if let Some(t) = o.timeout_ms {
        body["timeout_ms"] = t.into();
    }
    if let Some(s) = &o.shell {
        body["shell"] = s.clone().into();
    }
    let (_, status, value) = call(o.session.as_deref(), "POST", "/v1/run", Some(&body), o.json, EXIT_CHILD_EXITED);
    check_status(status, &value);
    finish(&value, o.json, false);
}

fn cmd_output(args: &[String]) -> ! {
    let o = parse_opts("output", args, &["--session", "--tail", "--json"], false);
    let path = match o.tail {
        Some(n) => format!("/v1/output?tail={n}"),
        None => "/v1/output".to_string(),
    };
    // Reading the output of a session that has ended is a success.
    let (_, status, value) = call(o.session.as_deref(), "GET", &path, None, o.json, EXIT_OK);
    check_status(status, &value);
    if o.json {
        println!("{value}");
    } else {
        let text = value.get("output").and_then(|x| x.get("text")).and_then(|t| t.as_str()).unwrap_or("");
        if !text.is_empty() {
            println!("{text}");
        }
    }
    std::process::exit(EXIT_OK);
}

fn cmd_resize(args: &[String]) -> ! {
    let o = parse_opts("resize", args, &["--session", "--json"], true);
    let size = o.rest.first().and_then(|s| s.split_once(['x', 'X'])).and_then(|(c, r)| Some((c.parse::<u64>().ok()?, r.parse::<u64>().ok()?)));
    let Some((cols, rows)) = size else { usage("'resize' takes COLSxROWS, such as: waldo-term resize 100x30") };
    let body = json!({"cols": cols, "rows": rows});
    let (_, status, value) = call(o.session.as_deref(), "POST", "/v1/resize", Some(&body), o.json, EXIT_CHILD_EXITED);
    check_status(status, &value);
    finish(&value, o.json, false);
}

fn cmd_signal(args: &[String]) -> ! {
    let o = parse_opts("signal", args, &["--session", "--stop", "--json"], true);
    let body = if o.stop {
        json!({"stop": true})
    } else if let Some(name) = o.rest.first() {
        json!({"signal": name})
    } else {
        usage("'signal' takes a signal name such as SIGINT, or --stop to end the session")
    };
    let (_, status, value) = call(o.session.as_deref(), "POST", "/v1/signal", Some(&body), o.json, EXIT_CHILD_EXITED);
    check_status(status, &value);
    finish(&value, o.json, false);
}

fn cmd_attach(args: &[String]) -> ! {
    let o = parse_opts("attach", args, &["--session", "--drive", "--resize"], false);
    if !attach::is_terminal() {
        die(EXIT_USAGE, "'attach' draws on a terminal and this is not one; from a script, read the screen with 'waldo-term screen'");
    }
    let (id, socket, dir) = match target(o.session.as_deref()) {
        Target::Live { id, socket, dir } => (id, socket, dir),
        Target::Ended(sess) => ended(&sess, false, EXIT_CHILD_EXITED),
    };
    let mut stream = connect(&socket).unwrap_or_else(|e| die(EXIT_NO_SESSION, &format!("cannot reach session '{id}': {e}")));
    // The size goes in the request so the first frame already fits. The last
    // line of the terminal is the status bar.
    let (cols, rows) = attach::term_size();
    let request = format!(
        "GET /v1/attach?cols={cols}&rows={} HTTP/1.1\r\nHost: waldo\r\nUpgrade: waldo-attach\r\nConnection: Upgrade\r\n\r\n",
        rows.saturating_sub(1).max(1)
    );
    if stream.write_all(request.as_bytes()).is_err() {
        die(EXIT_OTHER, "cannot send the attach request");
    }
    // Read the response head; whatever follows it is already the stream.
    let mut head: Vec<u8> = Vec::new();
    let split = loop {
        if let Some(i) = head.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
        let mut buf = [0u8; 4096];
        match stream.read(&mut buf) {
            Ok(n) if n > 0 => head.extend_from_slice(&buf[..n]),
            _ => die(EXIT_OTHER, "the session closed the connection before answering"),
        }
    };
    if !head.starts_with(b"HTTP/1.1 101") {
        let body: Value = serde_json::from_slice(&head[split..]).unwrap_or(Value::Null);
        let text = body.get("text").and_then(|t| t.as_str()).unwrap_or("the session refused to attach");
        die(EXIT_OTHER, text);
    }
    let leftover = head[split..].to_vec();
    match attach::run(stream, leftover, attach::Options { drive: o.drive, resize: o.resize }) {
        attach::Outcome::Detached => {
            println!("detached from '{id}'; it is still running");
            std::process::exit(EXIT_OK);
        }
        attach::Outcome::Lost(e) => die(EXIT_OTHER, &e),
        attach::Outcome::Ended => {
            if let Some(d) = dir.as_ref() {
                let end = Instant::now() + Duration::from_secs(3);
                while session::lock_is_held(d) && Instant::now() < end {
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
            match session::find(&id) {
                Ok(sess) => ended(&sess, false, EXIT_CHILD_EXITED),
                Err(_) => die(EXIT_CHILD_EXITED, &format!("session '{id}' has ended")),
            }
        }
    }
}

/// End a session: ask the host to stop its program (hang up, then terminate,
/// then kill), wait until it has, and say how it ended.
fn cmd_kill(args: &[String]) -> ! {
    let o = parse_opts("kill", args, &["--session"], true);
    let sel = o.session.clone().or_else(|| o.rest.first().cloned());
    let (id, socket, dir) = match target(sel.as_deref()) {
        Target::Live { id, socket, dir } => (id, socket, dir),
        Target::Ended(sess) => {
            println!("session '{}' had already ended", sess.id);
            std::process::exit(EXIT_OK);
        }
    };
    match connect(&socket) {
        Ok(mut stream) => {
            let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
            match http(&mut stream, "POST", "/v1/signal", Some(&json!({"stop": true}))) {
                Ok((status, value)) => check_status(status, &value),
                // The host may exit before it answers; that is what was asked for.
                Err(e) if e == "closed" => {}
                Err(e) => die(EXIT_OTHER, &e),
            }
        }
        Err(e) => die(EXIT_NO_SESSION, &format!("cannot reach session '{id}' at {}: {e}", socket.display())),
    }
    let Some(dir) = dir else {
        println!("asked '{id}' to stop");
        std::process::exit(EXIT_OK);
    };
    // The host escalates over a few seconds if the program ignores the
    // hangup, and releases its lock when it is gone.
    let end = Instant::now() + Duration::from_secs(10);
    while session::lock_is_held(&dir) {
        if Instant::now() >= end {
            die(EXIT_OTHER, &format!("session '{id}' has not stopped after 10 seconds; its host is process {}", host_pid(&dir)));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let will = session::find(&id).ok().and_then(|s| session::read_will(&s.dir, s.meta.as_ref()));
    match will {
        // One line: how it ended. The output is still there for `output`.
        Some(w) => println!("{}", render::will(&w).lines().next().unwrap_or("")),
        None => println!("session '{id}' stopped"),
    }
    std::process::exit(EXIT_OK);
}

fn host_pid(dir: &Path) -> String {
    std::fs::read(dir.join("meta.json"))
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .and_then(|m| m.get("host_pid").and_then(|p| p.as_i64()))
        .map(|p| p.to_string())
        .unwrap_or_else(|| "unknown".into())
}

fn age(secs: i64) -> String {
    match secs {
        s if s < 120 => format!("{s}s"),
        s if s < 7200 => format!("{}m", s / 60),
        s if s < 172_800 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86_400),
    }
}

fn cmd_list(args: &[String]) -> ! {
    let o = parse_opts("list", args, &["--json"], false);
    let sessions = session::list();
    if o.json {
        let list: Vec<Value> = sessions
            .iter()
            .map(|s| json!({"session": s.id, "state": s.state.as_str(), "socket": s.socket().display().to_string(), "meta": s.meta}))
            .collect();
        println!("{}", Value::Array(list));
        std::process::exit(EXIT_OK);
    }
    if sessions.is_empty() {
        println!("no sessions. Start one with: waldo-wrap --detach -- PROGRAM");
        std::process::exit(EXIT_OK);
    }
    let width = sessions.iter().map(|s| s.id.len()).max().unwrap_or(2).max(2);
    println!("{:width$}  {:11}  {:>4}  COMMAND", "ID", "STATE", "AGE");
    for s in &sessions {
        let argv = s
            .meta
            .as_ref()
            .and_then(|m| m.get("argv"))
            .and_then(|a| a.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(" "))
            .unwrap_or_else(|| "?".into());
        println!("{:width$}  {:11}  {:>4}  {}", s.id, s.state.as_str(), age(s.age_secs()), argv);
    }
    std::process::exit(EXIT_OK);
}

fn cmd_gc(args: &[String]) -> ! {
    let o = parse_opts("gc", args, &["--all", "--older-than"], false);
    let threshold = if o.all { 0 } else { o.older_than.unwrap_or(3600) };
    let mut removed = 0;
    for s in session::list() {
        if s.state == State::Live || s.age_secs() < threshold {
            continue;
        }
        // Only what waldo put there; anything else stops the rmdir.
        for f in ["sock", "meta.json", "will.json", "events.ndjson", "log", "lock"] {
            let _ = std::fs::remove_file(s.dir.join(f));
        }
        if std::fs::remove_dir(&s.dir).is_ok() {
            removed += 1;
        }
    }
    println!("removed {removed} ended session{}", if removed == 1 { "" } else { "s" });
    std::process::exit(EXIT_OK);
}

fn cmd_help(args: &[String]) -> ! {
    match args.first().map(|s| s.as_str()) {
        None => print!("{}", help::GUIDE),
        Some(t) => match help::topic(t) {
            Some(text) => print!("{text}"),
            None => usage(&format!("no help topic '{t}'; topics: keys, wait, run, messages, exit-status, commands")),
        },
    }
    std::process::exit(EXIT_OK);
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(cmd) = args.first() else {
        print!("{}", help::GUIDE);
        std::process::exit(EXIT_USAGE);
    };
    let rest = &args[1..];
    match cmd.as_str() {
        "help" | "-h" | "--help" => cmd_help(rest),
        "--version" => {
            println!("waldo-term {}", env!("CARGO_PKG_VERSION"));
            let _ = util::unix_now();
        }
        "list" | "ls" => cmd_list(rest),
        "send" => cmd_send(rest),
        "screen" => cmd_screen(rest),
        "wait" => cmd_wait(rest),
        "run" => cmd_run(rest),
        "output" => cmd_output(rest),
        "resize" => cmd_resize(rest),
        "signal" => cmd_signal(rest),
        "kill" => cmd_kill(rest),
        "gc" => cmd_gc(rest),
        "attach" => cmd_attach(rest),
        "screencap" | "match" => die(EXIT_OTHER, &format!("'{cmd}' is not implemented yet")),
        other => usage(&format!("unknown command '{other}'")),
    }
}

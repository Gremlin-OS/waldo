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
const EXIT_TIMEOUT: i32 = 124;

fn die(code: i32, msg: &str) -> ! {
    eprintln!("waldo-term: {msg}");
    std::process::exit(code);
}

/// The most a --text-file or --keys-file may hold. A request body is capped
/// at 1 MiB by the host.
const MAX_FILE: u64 = 1 << 20;

fn usage(msg: &str) -> ! {
    die(EXIT_USAGE, &format!("{msg}\nRun 'waldo-term help' for a guide, or 'waldo-term help commands' for every option."));
}

// ---- arguments -------------------------------------------------------------

#[derive(Default)]
struct Opts {
    session: Option<String>,
    /// What to wait for: the wait parameters `until_match` and so on, as
    /// the --until-* options gave them.
    conds: Map<String, Value>,
    timeout_ms: Option<u64>,
    key_delay_ms: Option<u64>,
    json: bool,
    items: Vec<Value>,
    tail: Option<u64>,
    all: bool,
    older_than: Option<i64>,
    style: Option<String>,
    drive: bool,
    resize: bool,
    id: Option<String>,
    once: bool,
    /// Who this client is to the others on the session (--as, or WALDO_AS).
    name: Option<String>,
    rest: Vec<String>,
}

/// The name to give the session: --as, or else the environment's.
fn client_name(o: &Opts) -> Option<String> {
    o.name
        .clone()
        .or_else(|| std::env::var("WALDO_AS").ok().filter(|s| !s.is_empty()))
}

/// What a file or stdin holds, as input: without its one final newline, LF
/// or CRLF. That newline ends the last line; it is not something to type.
/// Only one is taken, since a blank last line was put there on purpose.
fn without_final_newline(mut s: String) -> String {
    if s.ends_with('\n') {
        s.pop();
        if s.ends_with('\r') {
            s.pop();
        }
    }
    s
}

fn read_stdin() -> String {
    let mut s = String::new();
    if std::io::stdin().read_to_string(&mut s).is_err() {
        die(EXIT_OTHER, "cannot read stdin as UTF-8 text");
    }
    without_final_newline(s)
}

/// The content of a --text-file or --keys-file. Nothing has been sent yet
/// when this fails, so a file that cannot be read sends nothing.
fn read_file(path: &str) -> String {
    let fail = |why: &str| -> ! { usage(&format!("cannot read {path}: {why}")) };
    let md = std::fs::metadata(path).unwrap_or_else(|e| fail(&e.to_string()));
    if !md.is_file() {
        fail("not a regular file");
    }
    if md.len() > MAX_FILE {
        fail("larger than 1 MiB; send it in several requests");
    }
    let bytes = std::fs::read(path).unwrap_or_else(|e| fail(&e.to_string()));
    without_final_newline(String::from_utf8(bytes).unwrap_or_else(|_| fail("not UTF-8 text")))
}

fn parse_ms(v: &str) -> Option<u64> {
    if let Some(s) = v.strip_suffix("ms") {
        return s.parse().ok();
    }
    if let Some(s) = v.strip_suffix('s') {
        return s
            .parse::<f64>()
            .ok()
            .filter(|f| *f >= 0.0)
            .map(|f| (f * 1000.0) as u64);
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
        // An option may come after a bare word, as in `resize 100x30
        // --until-elapsed 0`. Not for broadcast: what follows its first
        // word is the rest of what is said.
        let may_be_option = o.rest.is_empty() || cmd != "broadcast";
        if may_be_option && arg.starts_with('-') && arg != "-" && arg != "--" {
            if arg == "-h" || arg == "--help" {
                print!("{}", help::COMMANDS);
                std::process::exit(0);
            }
            if arg == "--until" || arg == "--pattern" {
                usage("each wait condition is an option of its own: --until-match RE, --until-settle, --until-change, --until-exit, --until-broadcast, --until-elapsed MS");
            }
            if arg == "--until-none" {
                usage("--until-none is gone; to not wait, use: --until-elapsed 0");
            }
            if arg == "--stop" {
                usage("to end a session, use: waldo-term kill ID. 'signal' sends a signal and nothing else");
            }
            let name = if arg == "-s" { "--session" } else { arg };
            if !allowed.contains(&name) {
                usage(&format!("'{cmd}' does not take {arg}"));
            }
            let mut value = || -> String {
                let v = args
                    .get(i)
                    .cloned()
                    .unwrap_or_else(|| usage(&format!("{arg} needs a value")));
                i += 1;
                v
            };
            match name {
                "--session" => o.session = Some(value()),
                // Several may be given: the wait ends when any one is met.
                "--until-match" | "--until-match-new" => {
                    let re = value();
                    match o
                        .conds
                        .entry(name[2..].replace('-', "_"))
                        .or_insert_with(|| Value::Array(Vec::new()))
                    {
                        Value::Array(list) => list.push(re.into()),
                        _ => unreachable!(),
                    }
                }
                "--until-settle" | "--until-change" | "--until-exit" => {
                    o.conds.insert(name[2..].replace('-', "_"), true.into());
                }
                "--until-broadcast" => {
                    o.conds
                        .entry("until_broadcast")
                        .or_insert(Value::Bool(true));
                }
                "--until-broadcast-from" => {
                    o.conds.insert("until_broadcast".into(), value().into());
                }
                "--until-elapsed" => {
                    let v = value();
                    let ms = parse_ms(&v).unwrap_or_else(|| {
                        usage("--until-elapsed takes milliseconds, such as 5000, or seconds as 5s")
                    });
                    o.conds.insert("until_elapsed".into(), ms.into());
                }
                "--as" => o.name = Some(value()),
                "--timeout" => {
                    let v = value();
                    o.timeout_ms = Some(parse_ms(&v).unwrap_or_else(|| {
                        usage("--timeout takes milliseconds, such as 5000, or seconds as 5s")
                    }));
                }
                "--key-delay" => {
                    let v = value();
                    o.key_delay_ms =
                        Some(parse_ms(&v).unwrap_or_else(|| {
                            usage("--key-delay takes milliseconds, such as 200")
                        }));
                }
                "--pause" => {
                    let v = value();
                    let ms = parse_ms(&v)
                        .unwrap_or_else(|| usage("--pause takes milliseconds, such as 500"));
                    o.items.push(json!({"pause": ms}));
                }
                "--json" => o.json = true,
                "--drive" => o.drive = true,
                "--resize" => o.resize = true,
                "--once" => o.once = true,
                "--id" => o.id = Some(value()),
                "--style" => o.style = Some(value()),
                "--all" => o.all = true,
                "--older-than" => {
                    let v = value();
                    o.older_than = Some(
                        parse_ms(&format!("{v}s"))
                            .map(|ms| (ms / 1000) as i64)
                            .unwrap_or_else(|| usage("--older-than takes seconds")),
                    );
                }
                "--tail" => {
                    o.tail = Some(
                        value()
                            .parse()
                            .unwrap_or_else(|_| usage("--tail takes a number of lines")),
                    )
                }
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
                "--text-file" | "--keys-file" => {
                    let content = read_file(&value());
                    let mut m = Map::new();
                    m.insert(name[2..name.len() - 5].to_string(), content.into());
                    o.items.push(Value::Object(m));
                }
                "--paste-file" => usage(
                    "there is no --paste-file. Type the file with --text-file PATH, or paste it from stdin with --paste - < PATH",
                ),
                _ => usage(&format!("'{cmd}' does not take {arg}")),
            }
        } else if arg == "--" && o.rest.is_empty() {
            o.rest.extend(args[i..].iter().cloned());
            break;
        } else {
            if !positional {
                let hint = if cmd == "send" {
                    "; say what it is with --text or --keys"
                } else {
                    ""
                };
                usage(&format!("'{cmd}' does not take the argument '{arg}'{hint}"));
            }
            o.rest.push(arg.to_string());
        }
    }
    o
}

/// For a command that takes one bare word: a second is refused, where it
/// used to be dropped without a word.
fn at_most_one(cmd: &str, rest: &[String]) {
    if let Some(extra) = rest.get(1) {
        usage(&format!(
            "'{cmd}' takes one argument, '{}'; it does not take '{extra}' as well",
            rest[0]
        ));
    }
}

// ---- finding the session -----------------------------------------------------

enum Target {
    Live {
        id: String,
        socket: PathBuf,
        dir: Option<PathBuf>,
    },
    Ended(Session),
}

fn target(sel: Option<&str>) -> Target {
    match sel {
        Some(s) if s.contains('/') => {
            let socket = PathBuf::from(s);
            let dir = socket.parent().map(|p| p.to_path_buf());
            let id = dir
                .as_ref()
                .and_then(|d| d.file_name())
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            Target::Live { id, socket, dir }
        }
        Some(s) => match session::find(s) {
            Ok(sess) if sess.state == State::Live => Target::Live {
                id: sess.id.clone(),
                socket: sess.socket(),
                dir: Some(sess.dir),
            },
            Ok(sess) => Target::Ended(sess),
            Err(e) => die(EXIT_NO_SESSION, &e),
        },
        None => {
            let live: Vec<Session> = session::list()
                .into_iter()
                .filter(|s| s.state == State::Live)
                .collect();
            match live.len() {
                1 => Target::Live {
                    id: live[0].id.clone(),
                    socket: live[0].socket(),
                    dir: Some(live[0].dir.clone()),
                },
                0 => die(
                    EXIT_NO_SESSION,
                    "no live session; start one with: waldo-wrap --detach -- PROGRAM",
                ),
                _ => die(
                    EXIT_USAGE,
                    &format!(
                        "several sessions are live; pick one with -s: {}",
                        live.iter()
                            .map(|s| s.id.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
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
    let name = socket
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    let d = File::open(dir)?;
    UnixStream::connect(format!("/proc/self/fd/{}/{}", d.as_raw_fd(), name))
}

fn http(
    stream: &mut UnixStream,
    method: &str,
    path: &str,
    body: Option<&Value>,
) -> Result<(u16, Value), String> {
    let body_bytes = body
        .map(|b| serde_json::to_vec(b).unwrap_or_default())
        .unwrap_or_default();
    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: waldo\r\nConnection: close\r\nAccept: application/json\r\n");
    if body.is_some() {
        req.push_str(&format!(
            "Content-Type: application/json\r\nContent-Length: {}\r\n",
            body_bytes.len()
        ));
    }
    req.push_str("\r\n");
    // A host that exits with this request unread resets the connection.
    // That is the session ending, the same as a connection closed with no
    // answer.
    let hung_up = |e: &std::io::Error| {
        matches!(
            e.kind(),
            std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::BrokenPipe
        )
    };
    let sent = stream
        .write_all(req.as_bytes())
        .and_then(|_| stream.write_all(&body_bytes));
    match sent {
        Err(e) if hung_up(&e) => return Err("closed".into()),
        Err(e) => return Err(format!("cannot send the request: {e}")),
        Ok(()) => {}
    }
    let mut raw = Vec::new();
    match stream.read_to_end(&mut raw) {
        Err(e) if hung_up(&e) && raw.is_empty() => return Err("closed".into()),
        Err(e) => return Err(format!("no response from the session: {e}")),
        Ok(_) => {}
    }
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
    let value: Value = serde_json::from_slice(&raw[head..])
        .map_err(|e| format!("the session sent a response that is not JSON: {e}"))?;
    Ok((status, value))
}

/// Send one request to a session. If the session has ended, or ends before
/// answering, its will is printed instead and this does not return.
fn call(
    sel: Option<&str>,
    method: &str,
    path: &str,
    body: Option<&Value>,
    as_json: bool,
    ended_status: i32,
) -> (String, u16, Value) {
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
        die(
            EXIT_NO_SESSION,
            &format!("cannot reach session '{id}' at {}: {why}", socket.display()),
        );
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
    let text = value
        .get("text")
        .and_then(|t| t.as_str())
        .unwrap_or("the session refused the request");
    let code = value
        .get("error")
        .and_then(|t| t.as_str())
        .unwrap_or("error");
    die(
        if (400..500).contains(&status) && status != 409 {
            EXIT_USAGE
        } else {
            EXIT_OTHER
        },
        &format!("{text} ({code})"),
    );
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
        _ => EXIT_OK,
    };
    std::process::exit(code);
}

fn wait_object(o: &Opts) -> Map<String, Value> {
    let mut wait = Map::new();
    if let Some(s) = &o.style {
        wait.insert("style".into(), s.clone().into());
    }
    for (k, v) in &o.conds {
        wait.insert(k.clone(), v.clone());
    }
    if let Some(name) = client_name(o) {
        wait.insert("as".into(), name.into());
    }
    wait
}

fn wanted_exit(o: &Opts) -> bool {
    o.conds.contains_key("until_exit")
}

fn encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

// ---- commands ------------------------------------------------------------------

const WAIT_OPTS: [&str; 13] = [
    "--session",
    "--as",
    "--until-match",
    "--until-match-new",
    "--until-settle",
    "--until-change",
    "--until-exit",
    "--until-broadcast",
    "--until-broadcast-from",
    "--until-elapsed",
    "--timeout",
    "--json",
    "--style",
];

fn cmd_send(args: &[String]) -> ! {
    let mut allowed = WAIT_OPTS.to_vec();
    allowed.extend([
        "--text",
        "--keys",
        "--paste",
        "--text-file",
        "--keys-file",
        "--paste-file",
        "--pause",
        "--key-delay",
    ]);
    let o = parse_opts("send", args, &allowed, false);
    if o.items.is_empty() {
        usage("'send' needs something to send: --text S, --keys S, --paste S, or a --*-file PATH.");
    }
    let mut body = json!({"input": o.items, "wait": wait_object(&o)});
    if let Some(t) = o.timeout_ms {
        body["timeout_ms"] = t.into();
    }
    if let Some(d) = o.key_delay_ms {
        body["key_delay_ms"] = d.into();
    }
    let (_, status, value) = call(
        o.session.as_deref(),
        "POST",
        "/v1/input",
        Some(&body),
        o.json,
        EXIT_CHILD_EXITED,
    );
    check_status(status, &value);
    finish(&value, o.json, wanted_exit(&o));
}

fn cmd_screen(args: &[String]) -> ! {
    let o = parse_opts("screen", args, &WAIT_OPTS, false);
    let mut q: Vec<String> = Vec::new();
    for (k, v) in wait_object(&o) {
        // Several patterns are the same name several times.
        let each = match v {
            Value::Array(list) => list,
            one => vec![one],
        };
        for v in each {
            let value = v
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| v.to_string());
            q.push(format!("{k}={}", encode(&value)));
        }
    }
    if let Some(t) = o.timeout_ms {
        q.push(format!("timeout_ms={t}"));
    }
    let path = if q.is_empty() {
        "/v1/screen".to_string()
    } else {
        format!("/v1/screen?{}", q.join("&"))
    };
    let (_, status, value) = call(
        o.session.as_deref(),
        "GET",
        &path,
        None,
        o.json,
        EXIT_CHILD_EXITED,
    );
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
    let ended_status = if wanted_exit(&o) {
        EXIT_OK
    } else {
        EXIT_CHILD_EXITED
    };
    let (_, status, value) = call(
        o.session.as_deref(),
        "POST",
        "/v1/wait",
        Some(&body),
        o.json,
        ended_status,
    );
    check_status(status, &value);
    finish(&value, o.json, wanted_exit(&o));
}

/// Say a line to everyone else on the session. It is not typed into the
/// program.
fn cmd_broadcast(args: &[String]) -> ! {
    let o = parse_opts("broadcast", args, &["--session", "--as", "--json"], true);
    if o.rest.is_empty() {
        usage("'broadcast' takes the text to say, such as: waldo-term broadcast --as ann 'the shell is yours'");
    }
    let mut body = json!({"text": o.rest.join(" ")});
    if let Some(name) = client_name(&o) {
        body["as"] = name.into();
    }
    let (id, status, value) = call(
        o.session.as_deref(),
        "POST",
        "/v1/broadcasts",
        Some(&body),
        o.json,
        EXIT_CHILD_EXITED,
    );
    check_status(status, &value);
    if o.json {
        println!("{value}");
    } else {
        let n = value["broadcast"]["n"].as_u64().unwrap_or(0);
        println!("[{id} broadcast sent n={n}]");
    }
    std::process::exit(EXIT_OK);
}

/// What the session is: its program, where it runs, how to reach it, and
/// which of the optional things are on.
fn cmd_status(args: &[String]) -> ! {
    let o = parse_opts("status", args, &["--session", "--json"], false);
    let (id, status, v) = call(
        o.session.as_deref(),
        "GET",
        "/v1/status",
        None,
        o.json,
        EXIT_CHILD_EXITED,
    );
    check_status(status, &v);
    if o.json {
        println!("{v}");
        std::process::exit(EXIT_OK);
    }
    let text = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
    let num = |k: &str| v.get(k).and_then(|x| x.as_u64()).unwrap_or(0);
    let on = |x: Option<&Value>| {
        if x.and_then(|b| b.as_bool()) == Some(true) {
            "on"
        } else {
            "off"
        }
    };
    let child = v.get("child").cloned().unwrap_or(Value::Null);
    let state = child.get("state").and_then(|s| s.as_str()).unwrap_or("?");
    let mut words = vec![id, state.to_string()];
    if let Some(fg) = child.get("foreground").and_then(|f| f.as_str()) {
        words.push(format!("fg={fg}"));
    }
    words.push(format!("{}x{}", num("cols"), num("rows")));
    words.push(format!("gen={}", num("gen")));
    words.push(format!("up={}", age((num("uptime_ms") / 1000) as i64)));
    println!("[{}]", words.join(" "));
    let argv: Vec<String> = v
        .get("argv")
        .and_then(|a| a.as_array())
        .into_iter()
        .flatten()
        .map(|a| a.as_str().unwrap_or("").to_string())
        .collect();
    let messages = v.get("messages").cloned().unwrap_or(Value::Null);
    println!("program     {}", argv.join(" "));
    println!("cwd         {}", text("cwd"));
    println!(
        "pid         {} (host {})",
        child.get("pid").and_then(|p| p.as_u64()).unwrap_or(0),
        num("host_pid")
    );
    println!("started     {}", text("started"));
    println!("socket      {}", text("socket"));
    println!("dest        {}", text("dest"));
    println!(
        "messages    settle {}, stream {}, {} match trigger(s)",
        on(messages.get("settle")),
        on(messages.get("stream")),
        messages
            .get("triggers")
            .and_then(|n| n.as_u64())
            .unwrap_or(0)
    );
    println!("broadcasts  {}", on(v.get("broadcasts")));
    std::process::exit(EXIT_OK);
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
        let text = value
            .get("output")
            .and_then(|x| x.get("text"))
            .and_then(|t| t.as_str())
            .unwrap_or("");
        if !text.is_empty() {
            println!("{text}");
        }
    }
    std::process::exit(EXIT_OK);
}

fn cmd_resize(args: &[String]) -> ! {
    let mut allowed = WAIT_OPTS.to_vec();
    allowed.retain(|a| a.starts_with("--until-") || *a == "--session" || *a == "--json");
    let o = parse_opts("resize", args, &allowed, true);
    at_most_one("resize", &o.rest);
    let size = o
        .rest
        .first()
        .and_then(|s| s.split_once(['x', 'X']))
        .and_then(|(c, r)| Some((c.parse::<u64>().ok()?, r.parse::<u64>().ok()?)));
    let Some((cols, rows)) = size else {
        usage("'resize' takes COLSxROWS, such as: waldo-term resize 100x30")
    };
    let mut body = json!({"cols": cols, "rows": rows});
    if !o.conds.is_empty() {
        body["wait"] = Value::Object(wait_object(&o));
    }
    let (_, status, value) = call(
        o.session.as_deref(),
        "POST",
        "/v1/resize",
        Some(&body),
        o.json,
        EXIT_CHILD_EXITED,
    );
    check_status(status, &value);
    finish(&value, o.json, false);
}

fn cmd_signal(args: &[String]) -> ! {
    let o = parse_opts("signal", args, &["--session", "--json"], true);
    at_most_one("signal", &o.rest);
    let Some(name) = o.rest.first() else {
        usage("'signal' takes a signal name such as SIGINT. To end the session, use: waldo-term kill ID")
    };
    let body = json!({"signal": name});
    let (_, status, value) = call(
        o.session.as_deref(),
        "POST",
        "/v1/signal",
        Some(&body),
        o.json,
        EXIT_CHILD_EXITED,
    );
    check_status(status, &value);
    finish(&value, o.json, false);
}

fn cmd_match(args: &[String]) -> ! {
    let how = "'match' takes add PATTERN, rm ID or list, such as: waldo-term match add --id pw 'assword: $'";
    let Some(sub) = args.first() else { usage(how) };
    let rest = &args[1..];
    let (o, method, path, body) = match sub.as_str() {
        "add" => {
            let o = parse_opts(
                "match add",
                rest,
                &["--session", "--id", "--once", "--json"],
                true,
            );
            at_most_one("match add", &o.rest);
            let Some(pattern) = o.rest.first() else {
                usage("'match add' takes a pattern: a regular expression matched against one line")
            };
            let mut body =
                json!({"pattern": pattern, "mode": if o.once { "once" } else { "edge" }});
            if let Some(id) = o.id.as_ref() {
                body["id"] = id.clone().into();
            }
            (o, "POST", "/v1/triggers".to_string(), Some(body))
        }
        "rm" | "remove" => {
            let o = parse_opts("match rm", rest, &["--session", "--json"], true);
            at_most_one("match rm", &o.rest);
            let Some(id) = o.rest.first() else {
                usage("'match rm' takes the id of a trigger; 'waldo-term match list' shows them")
            };
            let path = format!("/v1/triggers/{}", encode(id));
            (o, "DELETE", path, None)
        }
        "list" | "ls" => (
            parse_opts("match list", rest, &["--session", "--json"], false),
            "GET",
            "/v1/triggers".to_string(),
            None,
        ),
        _ => usage(how),
    };
    let (_, status, value) = call(
        o.session.as_deref(),
        method,
        &path,
        body.as_ref(),
        o.json,
        EXIT_CHILD_EXITED,
    );
    check_status(status, &value);
    if o.json {
        println!("{value}");
        std::process::exit(EXIT_OK);
    }
    let field = |t: &Value, k: &str| {
        t.get(k)
            .map(|v| {
                v.as_str()
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| v.to_string())
            })
            .unwrap_or_default()
    };
    let dest = field(&value, "dest");
    if let Some(t) = value.get("added") {
        println!(
            "added trigger {} ({}): {}",
            field(t, "id"),
            field(t, "mode"),
            field(t, "pattern")
        );
        println!("its matches go to {dest}");
    } else if let Some(t) = value.get("removed") {
        println!(
            "removed trigger {} after {} matches",
            field(t, "id"),
            field(t, "count")
        );
    } else {
        let list = value
            .get("triggers")
            .and_then(|t| t.as_array())
            .cloned()
            .unwrap_or_default();
        if list.is_empty() {
            println!("no triggers; add one with: waldo-term match add PATTERN");
        }
        for t in &list {
            println!(
                "{}  {}  matches={}  {}",
                field(t, "id"),
                field(t, "mode"),
                field(t, "count"),
                field(t, "pattern")
            );
        }
        if !list.is_empty() {
            println!("matches go to {dest}");
        }
    }
    std::process::exit(EXIT_OK);
}

fn cmd_attach(args: &[String]) -> ! {
    let o = parse_opts(
        "attach",
        args,
        &["--session", "--drive", "--resize", "--as"],
        false,
    );
    // What this person says to the other clients carries a name: the one
    // given, or else the login name.
    let allowed = |s: &String| {
        (1..=32).contains(&s.len())
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    };
    let name = match client_name(&o) {
        Some(n) if allowed(&n) => Some(n),
        Some(_) => usage("--as takes a name of 1 to 32 characters from a-z, A-Z, 0-9, _ and -"),
        None => std::env::var("USER").ok().filter(allowed),
    };
    if !attach::is_terminal() {
        die(EXIT_USAGE, "'attach' draws on a terminal and this is not one; from a script, read the screen with 'waldo-term screen'");
    }
    let (id, socket, dir) = match target(o.session.as_deref()) {
        Target::Live { id, socket, dir } => (id, socket, dir),
        Target::Ended(sess) => ended(&sess, false, EXIT_CHILD_EXITED),
    };
    let mut stream = connect(&socket).unwrap_or_else(|e| {
        die(
            EXIT_NO_SESSION,
            &format!("cannot reach session '{id}': {e}"),
        )
    });
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
            _ => die(
                EXIT_OTHER,
                "the session closed the connection before answering",
            ),
        }
    };
    if !head.starts_with(b"HTTP/1.1 101") {
        let body: Value = serde_json::from_slice(&head[split..]).unwrap_or(Value::Null);
        let text = body
            .get("text")
            .and_then(|t| t.as_str())
            .unwrap_or("the session refused to attach");
        die(EXIT_OTHER, text);
    }
    let leftover = head[split..].to_vec();
    match attach::run(
        stream,
        leftover,
        attach::Options {
            drive: o.drive,
            resize: o.resize,
            name,
        },
    ) {
        attach::Outcome::Detached => {
            println!("left '{id}'; it is still running");
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
    at_most_one("kill", &o.rest);
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
            match http(
                &mut stream,
                "POST",
                "/v1/signal",
                Some(&json!({"stop": true})),
            ) {
                Ok((status, value)) => check_status(status, &value),
                // The host may exit before it answers; that is what was asked for.
                Err(e) if e == "closed" => {}
                Err(e) => die(EXIT_OTHER, &e),
            }
        }
        Err(e) => die(
            EXIT_NO_SESSION,
            &format!("cannot reach session '{id}' at {}: {e}", socket.display()),
        ),
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
            die(
                EXIT_OTHER,
                &format!(
                    "session '{id}' has not stopped after 10 seconds; its host is process {}",
                    host_pid(&dir)
                ),
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let will = session::find(&id)
        .ok()
        .and_then(|s| session::read_will(&s.dir, s.meta.as_ref()));
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
    let width = sessions
        .iter()
        .map(|s| s.id.len())
        .max()
        .unwrap_or(2)
        .max(2);
    println!("{:width$}  {:11}  {:>4}  COMMAND", "ID", "STATE", "AGE");
    for s in &sessions {
        let argv = s
            .meta
            .as_ref()
            .and_then(|m| m.get("argv"))
            .and_then(|a| a.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str())
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .unwrap_or_else(|| "?".into());
        println!(
            "{:width$}  {:11}  {:>4}  {}",
            s.id,
            s.state.as_str(),
            age(s.age_secs()),
            argv
        );
    }
    std::process::exit(EXIT_OK);
}

fn cmd_gc(args: &[String]) -> ! {
    let o = parse_opts("gc", args, &["--all", "--older-than"], false);
    let threshold = if o.all {
        0
    } else {
        o.older_than.unwrap_or(3600)
    };
    let mut removed = 0;
    let mut live = 0;
    for s in session::list() {
        if s.state == State::Live {
            live += 1;
            continue;
        }
        if s.age_secs() < threshold {
            continue;
        }
        // Only what waldo put there; anything else stops the rmdir.
        for f in [
            "sock",
            "meta.json",
            "will.json",
            "events.ndjson",
            "log",
            "lock",
        ] {
            let _ = std::fs::remove_file(s.dir.join(f));
        }
        if std::fs::remove_dir(&s.dir).is_ok() {
            removed += 1;
        }
    }
    let running = match live {
        0 => String::new(),
        // A session asked to stop is still running until its program is gone.
        n => format!("; {n} still running (end one with: waldo-term kill ID)"),
    };
    println!(
        "removed {removed} ended session{}{running}",
        if removed == 1 { "" } else { "s" }
    );
    std::process::exit(EXIT_OK);
}

fn cmd_help(args: &[String]) -> ! {
    match args.first().map(|s| s.as_str()) {
        None => print!("{}", help::all()),
        Some(t) => match help::topic(t) {
            Some(text) => print!("{text}"),
            None => usage(&format!("no help topic '{t}'; topics: tips, keys, wait, shell, sharing, messages, exit-status, commands")),
        },
    }
    std::process::exit(EXIT_OK);
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(cmd) = args.first() else {
        print!("{}", help::all());
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
        "status" => cmd_status(rest),
        "broadcast" | "say" => cmd_broadcast(rest),
        "run" => die(EXIT_USAGE, "there is no 'run' command; to run a shell command and learn how it ended, see: waldo-term help shell"),
        "output" => cmd_output(rest),
        "resize" => cmd_resize(rest),
        "signal" => cmd_signal(rest),
        "kill" => cmd_kill(rest),
        "gc" => cmd_gc(rest),
        "attach" => cmd_attach(rest),
        "match" => cmd_match(rest),
        "screencap" => die(EXIT_OTHER, &format!("'{cmd}' is not implemented yet")),
        other => usage(&format!("unknown command '{other}'")),
    }
}

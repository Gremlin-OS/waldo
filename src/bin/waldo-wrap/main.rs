//! waldo-wrap: host one program in a pty and serve a control API for it.
//! See DESIGN.md; the startup sequence here is section 1.1.

mod dest;
mod emu;
mod host;
mod http;
mod keys;
mod outbuf;
mod sys;

use dest::{Delivery, Dest};
use serde_json::{json, Value};
use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use waldo::{session, util};

const HELP: &str = "\
waldo-wrap: run a program in a terminal that is driven by messages.

usage: waldo-wrap [options] -- PROGRAM [ARGS...]

It starts PROGRAM in a pty, prints where to reach it, and serves a control
API on a unix socket until PROGRAM exits. Use waldo-term to talk to it.

options:
  --detach           return once the session is ready; the host keeps running
  --name NAME        session id to use (a-z 0-9 _ -); default is six random characters
  --dest URI         where messages go; default file:<session dir>/events.ndjson
                     The last message is the will: how PROGRAM ended and its output.
  --no-probe         do not require the destination to accept a message at startup
  --size COLSxROWS   terminal size, default 120x30
  --output-cap BYTES how much rendered output to keep for the will, default 65536
  --quiet-ms MS      how long the screen must be still to count as settled, default 200
  --json             print the session's coordinates as one JSON object
  -h, --help         this text
  --version          print the version

On success it prints exactly this, and nothing else, on stdout:
  session=k3f9q2
  socket=/run/user/1000/waldo/k3f9q2/sock
  dest=file:/run/user/1000/waldo/k3f9q2/events.ndjson

Without --detach it then waits, and exits with PROGRAM's exit status.

examples:
  waldo-wrap --detach --name sh -- bash
  waldo-wrap --detach -- vim notes.txt
  waldo-wrap --dest file:/tmp/build.ndjson -- make test

Run 'waldo-term help' for how to drive the session.
";

struct Args {
    name: Option<String>,
    dest: Option<String>,
    no_probe: bool,
    detach: bool,
    json: bool,
    cols: u16,
    rows: u16,
    output_cap: usize,
    quiet_ms: u64,
    program: Vec<String>,
}

fn usage_error(msg: &str) -> ! {
    eprintln!("waldo-wrap: {msg}\nusage: waldo-wrap [options] -- PROGRAM [ARGS...]   (waldo-wrap --help for details)");
    std::process::exit(2);
}

fn parse_args() -> Args {
    let mut a = Args {
        name: None,
        dest: None,
        no_probe: false,
        detach: false,
        json: false,
        cols: 120,
        rows: 30,
        output_cap: 64 << 10,
        quiet_ms: 200,
        program: Vec::new(),
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut value = |name: &str| -> String {
            it.next().unwrap_or_else(|| usage_error(&format!("{name} needs a value")))
        };
        match arg.as_str() {
            "-h" | "--help" => {
                print!("{HELP}");
                std::process::exit(0);
            }
            "--version" => {
                println!("waldo-wrap {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            "--detach" => a.detach = true,
            "--no-probe" => a.no_probe = true,
            "--json" => a.json = true,
            "--name" => {
                let n = value("--name");
                if !session::valid_name(&n) {
                    usage_error("--name takes 1 to 32 characters from a-z, 0-9, _ and -, starting with a letter or digit");
                }
                a.name = Some(n);
            }
            "--dest" => a.dest = Some(value("--dest")),
            "--size" => {
                let v = value("--size");
                let parsed = v.split_once(['x', 'X']).and_then(|(c, r)| Some((c.parse::<u16>().ok()?, r.parse::<u16>().ok()?)));
                match parsed {
                    Some((c, r))
                        if (host::MIN_COLS..=host::MAX_COLS).contains(&c) && (host::MIN_ROWS..=host::MAX_ROWS).contains(&r) =>
                    {
                        a.cols = c;
                        a.rows = r;
                    }
                    _ => usage_error(&format!(
                        "--size takes COLSxROWS between {}x{} and {}x{}, such as 100x30",
                        host::MIN_COLS,
                        host::MIN_ROWS,
                        host::MAX_COLS,
                        host::MAX_ROWS
                    )),
                }
            }
            "--output-cap" => {
                a.output_cap = value("--output-cap")
                    .parse()
                    .ok()
                    .filter(|n| *n >= 1024)
                    .unwrap_or_else(|| usage_error("--output-cap takes a number of bytes, at least 1024"));
            }
            "--quiet-ms" => {
                a.quiet_ms = value("--quiet-ms")
                    .parse()
                    .ok()
                    .filter(|n| (10..=10_000).contains(n))
                    .unwrap_or_else(|| usage_error("--quiet-ms takes milliseconds between 10 and 10000"));
            }
            "--" => {
                a.program = it.by_ref().collect();
                break;
            }
            s if s.starts_with('-') => usage_error(&format!("unknown option {s}")),
            _ => {
                a.program = std::iter::once(arg).chain(it.by_ref()).collect();
                break;
            }
        }
    }
    if a.program.is_empty() {
        usage_error("no program given");
    }
    a
}

/// Find the program now, so a missing one fails before a session exists.
fn resolve_program(name: &str) -> Result<PathBuf, String> {
    let runnable = |p: &Path| {
        p.is_file() && CString::new(p.as_os_str().as_bytes()).map(|c| unsafe { libc::access(c.as_ptr(), libc::X_OK) } == 0).unwrap_or(false)
    };
    if name.contains('/') {
        let p = PathBuf::from(name);
        return if runnable(&p) {
            Ok(p)
        } else if p.exists() {
            Err(format!("{name}: not an executable file"))
        } else {
            Err(format!("{name}: no such file"))
        };
    }
    let path = std::env::var_os("PATH").unwrap_or_default();
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if runnable(&candidate) {
            return Ok(candidate);
        }
    }
    Err(format!("{name}: command not found on PATH"))
}

/// How the outcome of startup is reported: on our own stdout and stderr, or
/// through a pipe to the process that the user started (`--detach`).
enum Report {
    Direct,
    Pipe(File),
}

impl Report {
    fn fail(&mut self, msg: &str) -> ! {
        match self {
            Report::Direct => eprintln!("waldo-wrap: {msg}"),
            Report::Pipe(f) => {
                let _ = f.write_all(format!("E{msg}").as_bytes());
            }
        }
        std::process::exit(1);
    }

    fn ready(self, text: &str) {
        match self {
            Report::Direct => {
                print!("{text}");
                let _ = std::io::stdout().flush();
            }
            Report::Pipe(mut f) => {
                let _ = f.write_all(format!("K{text}").as_bytes());
            }
        }
    }
}

/// Fork for `--detach`. The original process waits for the verdict and
/// exits; the new one becomes the host. Must run before any thread exists.
fn detach() -> Report {
    let mut fds = [0 as RawFd; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        eprintln!("waldo-wrap: cannot create a pipe: {}", std::io::Error::last_os_error());
        std::process::exit(1);
    }
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        eprintln!("waldo-wrap: cannot fork: {}", std::io::Error::last_os_error());
        std::process::exit(1);
    }
    if pid > 0 {
        sys::close(fds[1]);
        let mut verdict = Vec::new();
        let _ = unsafe { File::from_raw_fd(fds[0]) }.read_to_end(&mut verdict);
        let text = String::from_utf8_lossy(&verdict);
        match text.chars().next() {
            Some('K') => {
                print!("{}", &text[1..]);
                let _ = std::io::stdout().flush();
                std::process::exit(0);
            }
            Some('E') => {
                eprintln!("waldo-wrap: {}", &text[1..]);
                std::process::exit(1);
            }
            _ => {
                eprintln!("waldo-wrap: the host died during startup");
                std::process::exit(1);
            }
        }
    }
    sys::close(fds[0]);
    unsafe {
        libc::setsid();
        let null = libc::open(c"/dev/null".as_ptr(), libc::O_RDWR);
        if null >= 0 {
            for fd in 0..3 {
                libc::dup2(null, fd);
            }
            if null > 2 {
                libc::close(null);
            }
        }
    }
    Report::Pipe(unsafe { File::from_raw_fd(fds[1]) })
}

struct SessionDir {
    id: String,
    dir: PathBuf,
    _lock: File,
}

fn try_lock(dir: &Path) -> Result<File, String> {
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(dir.join("lock"))
        .map_err(|e| format!("cannot create {}: {e}", dir.join("lock").display()))?;
    // Someone listing sessions may hold the lock for an instant; retry.
    for _ in 0..10 {
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(lock);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    Err("held".into())
}

fn create_session_dir(name: Option<&str>) -> Result<SessionDir, String> {
    let base = session::ensure_base_dir()?;
    let mkdir = |dir: &Path| fs::DirBuilder::new().mode(0o700).create(dir);
    if let Some(name) = name {
        let dir = base.join(name);
        match mkdir(&dir) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(format!("cannot create {}: {e}", dir.display())),
        }
        let lock = try_lock(&dir).map_err(|e| {
            if e == "held" {
                format!("a session named '{name}' is already running; pick another --name, or end it with: waldo-term signal -s {name} --stop")
            } else {
                e
            }
        })?;
        // Leftovers from an earlier session of the same name.
        for f in ["sock", "meta.json", "will.json", "events.ndjson", "log"] {
            let _ = fs::remove_file(dir.join(f));
        }
        return Ok(SessionDir { id: name.to_string(), dir, _lock: lock });
    }
    for _ in 0..20 {
        let id = util::random_id(6);
        let dir = base.join(&id);
        match mkdir(&dir) {
            Ok(()) => {
                let lock = try_lock(&dir)?;
                return Ok(SessionDir { id, dir, _lock: lock });
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("cannot create {}: {e}", dir.display())),
        }
    }
    Err("could not find an unused session id".into())
}

/// Bind the control socket. A path too long for a socket address is bound
/// through a directory fd instead.
fn bind_socket(dir: &Path) -> Result<UnixListener, String> {
    let path = dir.join("sock");
    let listener = if path.as_os_str().len() < 100 {
        UnixListener::bind(&path)
    } else {
        let d = File::open(dir).map_err(|e| format!("cannot open {}: {e}", dir.display()))?;
        UnixListener::bind(format!("/proc/self/fd/{}/sock", d.as_raw_fd()))
    }
    .map_err(|e| format!("cannot bind {}: {e}", path.display()))?;
    let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));
    Ok(listener)
}

fn remove_session_dir(dir: &Path) {
    for f in ["sock", "meta.json", "will.json", "events.ndjson", "log", "lock"] {
        let _ = fs::remove_file(dir.join(f));
    }
    let _ = fs::remove_dir(dir);
}

static LOG_PATH: OnceLock<PathBuf> = OnceLock::new();

fn child_env(id: &str, socket: &Path) -> Vec<CString> {
    let ours: [(&str, String); 4] = [
        ("TERM", "xterm-256color".into()),
        ("COLORTERM", "truecolor".into()),
        ("WALDO_SESSION", id.into()),
        ("WALDO_SOCKET", socket.display().to_string()),
    ];
    let mut env: Vec<CString> = std::env::vars_os()
        .filter(|(k, _)| !ours.iter().any(|(name, _)| k.as_bytes() == name.as_bytes()))
        .filter_map(|(k, v)| {
            let mut kv = k.into_vec();
            kv.push(b'=');
            kv.extend_from_slice(v.as_bytes());
            CString::new(kv).ok()
        })
        .collect();
    for (k, v) in ours {
        if let Ok(c) = CString::new(format!("{k}={v}")) {
            env.push(c);
        }
    }
    env
}

fn main() {
    let args = parse_args();
    // Step 1: resolve the program.
    let program = resolve_program(&args.program[0]).unwrap_or_else(|e| {
        eprintln!("waldo-wrap: {e}");
        std::process::exit(127);
    });
    let dest_arg = args.dest.as_deref().map(|d| {
        Dest::parse(d).unwrap_or_else(|e| {
            eprintln!("waldo-wrap: {e}");
            std::process::exit(2);
        })
    });

    // Step 2: detach, before any thread exists.
    let mut report = if args.detach { detach() } else { Report::Direct };
    let start = Instant::now();
    let started_wall = util::rfc3339_now();

    // Step 3: the session directory, the lock, the socket.
    let sd = create_session_dir(args.name.as_deref()).unwrap_or_else(|e| report.fail(&e));
    let listener = bind_socket(&sd.dir).unwrap_or_else(|e| {
        remove_session_dir(&sd.dir);
        report.fail(&e)
    });
    let socket = sd.dir.join("sock");
    let log_path = sd.dir.join("log");
    let _ = LOG_PATH.set(log_path.clone());
    std::panic::set_hook(Box::new(|info| {
        if let Some(p) = LOG_PATH.get() {
            if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(p) {
                let _ = writeln!(f, "{} panic: {info}", util::rfc3339_now());
            }
        }
        eprintln!("waldo-wrap: internal error: {info}");
    }));
    let log = OpenOptions::new().create(true).append(true).mode(0o600).open(&log_path).ok();

    let dest_is_default = dest_arg.is_none();
    let dest = dest_arg.unwrap_or_else(|| Dest::File(sd.dir.join("events.ndjson")));
    let cwd = std::env::current_dir().map(|p| p.display().to_string()).unwrap_or_default();

    // Step 4: the hello message, which is also the destination probe.
    let mut hello = host::envelope(&sd.id, 0, "hello", start);
    hello.insert("name".into(), args.name.clone().map(Value::from).unwrap_or(Value::Null));
    hello.insert("argv".into(), json!(args.program));
    hello.insert("cwd".into(), cwd.clone().into());
    hello.insert("socket".into(), socket.display().to_string().into());
    hello.insert("cols".into(), args.cols.into());
    hello.insert("rows".into(), args.rows.into());
    hello.insert("host_pid".into(), std::process::id().into());
    let hello_line = serde_json::to_string(&Value::Object(hello)).unwrap_or_default();
    if !args.no_probe {
        if let Err(e) = dest.deliver(&hello_line) {
            remove_session_dir(&sd.dir);
            report.fail(&format!(
                "cannot deliver to {}: {e}; fix the destination, or pass --no-probe to start without checking it",
                dest.uri()
            ));
        }
    }

    // Step 5: the pty and the emulator.
    let pty = sys::open_pty(args.cols, args.rows).unwrap_or_else(|e| {
        remove_session_dir(&sd.dir);
        report.fail(&format!("cannot open a pty: {e}"))
    });
    let emu = emu::Emu::new(args.cols, args.rows, 4 << 20).unwrap_or_else(|e| {
        remove_session_dir(&sd.dir);
        report.fail(&e)
    });
    let sigfd = sys::signal_fd().unwrap_or_else(|e| {
        remove_session_dir(&sd.dir);
        report.fail(&format!("cannot set up signal handling: {e}"))
    });

    // Step 6: the child.
    let program_c = CString::new(program.as_os_str().as_bytes()).unwrap_or_default();
    let argv: Vec<CString> = args.program.iter().filter_map(|a| CString::new(a.as_bytes()).ok()).collect();
    let envp = child_env(&sd.id, &socket);
    let child = match sys::spawn(&program_c, &argv, &envp, &pty.slave_path) {
        Ok(c) => c,
        Err(e) => {
            let text = format!("cannot run {}: {e}", program.display());
            let mut exit = host::envelope(&sd.id, 1, "exit", start);
            exit.insert("exit_code".into(), Value::Null);
            exit.insert("signal".into(), Value::Null);
            exit.insert("core_dumped".into(), false.into());
            exit.insert("killed_by_waldo".into(), false.into());
            exit.insert("kill_reason".into(), Value::Null);
            exit.insert("spawn_error".into(), text.clone().into());
            exit.insert("duration_ms".into(), 0.into());
            let _ = dest.deliver(&serde_json::to_string(&Value::Object(exit)).unwrap_or_default());
            remove_session_dir(&sd.dir);
            report.fail(&text)
        }
    };

    // Every fork is done; the delivery thread can start.
    let log_for_thread = log.as_ref().and_then(|f| f.try_clone().ok());
    let mut delivery = Delivery::start(dest.clone(), move |msg| {
        if let Some(mut f) = log_for_thread.as_ref() {
            let _ = writeln!(f, "{} {msg}", util::rfc3339_now());
        }
    });
    if args.no_probe {
        delivery.send_blocking(hello_line);
    }

    // Step 7: meta.json, then the coordinates.
    let meta = json!({
        "id": sd.id,
        "name": args.name,
        "argv": args.program,
        "cwd": cwd,
        "child_pid": child.pid,
        "host_pid": std::process::id(),
        "started": started_wall,
        "cols": args.cols,
        "rows": args.rows,
        "socket": socket.display().to_string(),
        "dest": dest.uri(),
    });
    let _ = fs::write(sd.dir.join("meta.json"), format!("{}\n", serde_json::to_string_pretty(&meta).unwrap_or_default()));

    let coordinates = if args.json {
        format!("{}\n", json!({"session": sd.id, "socket": socket.display().to_string(), "dest": dest.uri()}))
    } else {
        format!("session={}\nsocket={}\ndest={}\n", sd.id, socket.display(), dest.uri())
    };

    let cfg = host::Config {
        id: sd.id.clone(),
        name: args.name.clone(),
        dir: sd.dir.clone(),
        socket,
        argv: args.program.clone(),
        cwd,
        dest,
        dest_is_default,
        output_cap: args.output_cap,
        quiet_ms: args.quiet_ms,
        started_wall,
    };
    let host = match host::Host::new(cfg, pty.master, child, sigfd, listener, emu, log, delivery, start) {
        Ok(h) => h,
        Err(e) => {
            remove_session_dir(&sd.dir);
            report.fail(&format!("cannot start the event loop: {e}"))
        }
    };
    report.ready(&coordinates);

    let code = host.run();
    // `sd` holds the lock until here, so the session counts as alive until
    // the will is delivered.
    drop(sd);
    std::process::exit(code);
}

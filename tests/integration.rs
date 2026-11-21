//! Integration tests: real programs in real ptys, driven through the two
//! binaries. The programs (bash, vim, coreutils) come from nixpkgs through
//! the flake.

use serde_json::Value;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

const WRAP: &str = env!("CARGO_BIN_EXE_waldo-wrap");
const TERM: &str = env!("CARGO_BIN_EXE_waldo-term");

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// One test's world: its own session base directory and scratch space.
struct T {
    base: PathBuf,
    work: PathBuf,
}

struct Ran {
    code: i32,
    out: String,
    err: String,
}

impl T {
    fn new() -> T {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let root = std::env::temp_dir().join(format!("wt{}-{}", std::process::id(), n));
        let _ = std::fs::remove_dir_all(&root);
        let base = root.join("s");
        let work = root.join("w");
        std::fs::create_dir_all(&base).unwrap();
        std::fs::create_dir_all(&work).unwrap();
        std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o700)).unwrap();
        T { base, work }
    }

    fn command(&self, bin: &str) -> Command {
        let mut c = Command::new(bin);
        c.env("WALDO_DIR", &self.base)
            .env("HOME", &self.work)
            .env("PS1", "$ ")
            .env_remove("PROMPT_COMMAND")
            .env_remove("XDG_RUNTIME_DIR")
            .current_dir(&self.work)
            .stdin(Stdio::null());
        c
    }

    fn finish(out: Output) -> Ran {
        Ran {
            code: out.status.code().unwrap_or(-1),
            out: String::from_utf8_lossy(&out.stdout).into_owned(),
            err: String::from_utf8_lossy(&out.stderr).into_owned(),
        }
    }

    fn wrap(&self, args: &[&str]) -> Ran {
        T::finish(self.command(WRAP).args(args).output().unwrap())
    }

    fn term(&self, args: &[&str]) -> Ran {
        T::finish(self.command(TERM).args(args).output().unwrap())
    }

    /// Start a detached session and return what waldo-wrap printed.
    fn start(&self, name: &str, program: &[&str]) -> Ran {
        let mut args = vec!["--detach", "--name", name, "--"];
        args.extend_from_slice(program);
        let r = self.wrap(&args);
        assert_eq!(r.code, 0, "waldo-wrap failed: {}", r.err);
        r
    }

    /// Start an interactive bash with a plain `$ ` prompt and wait for it.
    fn bash(&self, name: &str) {
        self.start(name, &["bash", "--norc", "--noprofile"]);
        let r = self.term(&["wait", "-s", name, "--until", "match", "--pattern", r"^\$ $"]);
        assert_eq!(r.code, 0, "bash did not show its prompt:\n{}\n{}", r.out, r.err);
    }

    /// Run a shell command in a session the way `waldo-term help shell` says:
    /// a marker after it, and a wait for the marker. Returns the command's
    /// exit status as `code` and what it printed as `out`.
    fn sh(&self, name: &str, cmd: &str) -> Ran {
        let mark = format!("rc{}", COUNTER.fetch_add(1, Ordering::SeqCst));
        let typed = format!("{cmd}; echo \"{mark}=$?\"");
        let pattern = format!("^{mark}=[0-9]+$");
        let r = self.term(&["send", "-s", name, "--text", &typed, "--keys", "<Enter>", "--until", "match", "--pattern", &pattern, "--timeout", "30000"]);
        assert_eq!(r.code, 0, "the command did not finish: {cmd}\n{}\n{}", r.out, r.err);
        let all = self.term(&["output", "-s", name]).out;
        let lines: Vec<&str> = all.lines().collect();
        let end = lines.iter().rposition(|l| l.starts_with(&format!("{mark}="))).unwrap_or_else(|| panic!("no marker in the output:\n{all}"));
        let echo = format!("echo \"{mark}=$?\"");
        let start = lines[..end].iter().rposition(|l| l.contains(&echo)).map(|i| i + 1).unwrap_or(0);
        let code = lines[end][mark.len() + 1..].parse().unwrap();
        Ran { code, out: lines[start..end].join("\n"), err: String::new() }
    }

    fn session_dir(&self, name: &str) -> PathBuf {
        self.base.join(name)
    }

    /// The will of an ended session that used the default destination.
    fn will(&self, name: &str) -> Value {
        let path = self.session_dir(name).join("events.ndjson");
        let end = Instant::now() + Duration::from_secs(10);
        loop {
            let text = std::fs::read_to_string(&path).unwrap_or_default();
            if let Some(last) = text.lines().last() {
                let v: Value = serde_json::from_str(last).unwrap();
                if v["trigger"] == "exit" {
                    return v;
                }
            }
            assert!(Instant::now() < end, "no will in {}", path.display());
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn json(&self, args: &[&str]) -> (i32, Value) {
        let mut a = args.to_vec();
        a.insert(1, "--json");
        let r = self.term(&a);
        let v = serde_json::from_str(&r.out).unwrap_or_else(|e| panic!("not JSON ({e}): {} / {}", r.out, r.err));
        (r.code, v)
    }
}

impl Drop for T {
    fn drop(&mut self) {
        // End whatever is still running, then remove the directories.
        if let Ok(rd) = std::fs::read_dir(&self.base) {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                let _ = self.command(TERM).args(["signal", "-s", &name, "KILL"]).output();
            }
        }
        std::thread::sleep(Duration::from_millis(50));
        if let Some(root) = self.base.parent() {
            let _ = std::fs::remove_dir_all(root);
        }
    }
}

fn status_line(out: &str) -> &str {
    out.lines().next().unwrap_or("")
}

// ---- sessions and startup ------------------------------------------------------

#[test]
fn detach_returns_promptly_with_coordinates() {
    let t = T::new();
    let began = Instant::now();
    let r = t.start("d1", &["sleep", "30"]);
    assert!(began.elapsed() < Duration::from_secs(2), "took {:?}", began.elapsed());
    let lines: Vec<&str> = r.out.lines().collect();
    assert_eq!(lines.len(), 3, "{}", r.out);
    assert_eq!(lines[0], "session=d1");
    let socket = lines[1].strip_prefix("socket=").unwrap();
    let dest = lines[2].strip_prefix("dest=file:").unwrap();
    assert!(std::path::Path::new(socket).exists());
    assert_eq!(dest, t.session_dir("d1").join("events.ndjson").to_str().unwrap());

    // The socket works at once.
    let s = t.term(&["screen", "-s", socket]);
    assert_eq!(s.code, 0, "{}", s.err);
    assert!(status_line(&s.out).contains("running"), "{}", s.out);

    // hello is the first message.
    let events = std::fs::read_to_string(dest).unwrap();
    let hello: Value = serde_json::from_str(events.lines().next().unwrap()).unwrap();
    assert_eq!(hello["trigger"], "hello");
    assert_eq!(hello["seq"], 0);
    assert_eq!(hello["argv"], serde_json::json!(["sleep", "30"]));

    let list = t.term(&["list"]);
    assert!(list.out.contains("d1") && list.out.contains("live"), "{}", list.out);
}

#[test]
fn json_coordinates() {
    let t = T::new();
    let r = t.wrap(&["--detach", "--json", "--", "sleep", "30"]);
    assert_eq!(r.code, 0, "{}", r.err);
    let v: Value = serde_json::from_str(&r.out).unwrap();
    let id = v["session"].as_str().unwrap();
    assert_eq!(id.len(), 6);
    assert!(v["socket"].as_str().unwrap().ends_with(&format!("{id}/sock")));
    assert!(v["dest"].as_str().unwrap().starts_with("file:/"));
}

#[test]
fn missing_program_fails_without_a_session() {
    let t = T::new();
    let r = t.wrap(&["--detach", "--", "no-such-program-here"]);
    assert_eq!(r.code, 127);
    assert_eq!(r.out, "");
    assert!(r.err.contains("command not found"), "{}", r.err);
    assert_eq!(std::fs::read_dir(&t.base).unwrap().count(), 0);
}

#[test]
fn exec_failure_reports_the_errno_and_no_socket() {
    let t = T::new();
    // Executable, but not something the kernel can run.
    let bad = t.work.join("bad");
    std::fs::write(&bad, "#!/no/such/interpreter\n").unwrap();
    std::fs::set_permissions(&bad, std::fs::Permissions::from_mode(0o755)).unwrap();
    let dest = t.work.join("events");
    let r = t.wrap(&["--detach", "--dest", &format!("file:{}", dest.display()), "--", bad.to_str().unwrap()]);
    assert_eq!(r.code, 1);
    assert_eq!(r.out, "", "no coordinates may be printed");
    assert!(r.err.contains("cannot run"), "{}", r.err);
    // The destination was probed, so it hears how the session ended.
    let events = std::fs::read_to_string(&dest).unwrap();
    let last: Value = serde_json::from_str(events.lines().last().unwrap()).unwrap();
    assert_eq!(last["trigger"], "exit");
    assert!(last["spawn_error"].as_str().unwrap().contains("cannot run"));
    assert_eq!(std::fs::read_dir(&t.base).unwrap().count(), 0);
}

#[test]
fn unreachable_destination_refuses_to_start() {
    let t = T::new();
    let r = t.wrap(&["--detach", "--dest", "file:/no/such/dir/events", "--", "sleep", "30"]);
    assert_eq!(r.code, 1);
    assert_eq!(r.out, "");
    assert!(r.err.contains("--no-probe"), "{}", r.err);
    assert_eq!(std::fs::read_dir(&t.base).unwrap().count(), 0);

    // With --no-probe it starts anyway.
    let r = t.wrap(&["--detach", "--no-probe", "--name", "np", "--dest", "file:/no/such/dir/events", "--", "sleep", "30"]);
    assert_eq!(r.code, 0, "{}", r.err);
    assert!(r.out.contains("session=np"));
}

#[test]
fn a_live_name_cannot_be_reused() {
    let t = T::new();
    t.start("dup", &["sleep", "30"]);
    let r = t.wrap(&["--detach", "--name", "dup", "--", "sleep", "30"]);
    assert_eq!(r.code, 1);
    assert!(r.err.contains("already running"), "{}", r.err);
}

#[test]
fn foreground_mode_exits_with_the_childs_status() {
    let t = T::new();
    let r = t.wrap(&["--name", "fg", "--", "sh", "-c", "echo out; exit 7"]);
    assert_eq!(r.code, 7);
    assert!(r.out.starts_with("session=fg\n"), "{}", r.out);
    let will = t.will("fg");
    assert_eq!(will["exit_code"], 7);
    assert_eq!(will["signal"], Value::Null);
    assert_eq!(will["output"]["text"], "out");
}

#[test]
fn explicit_destination_removes_the_session_directory() {
    let t = T::new();
    let dest = t.work.join("ev.ndjson");
    let r = t.wrap(&["--name", "ex", "--dest", &format!("file:{}", dest.display()), "--", "echo", "bye"]);
    assert_eq!(r.code, 0, "{}", r.err);
    assert!(!t.session_dir("ex").exists(), "the session directory should be gone");
    let events = std::fs::read_to_string(&dest).unwrap();
    let lines: Vec<Value> = events.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0]["trigger"], "hello");
    assert_eq!(lines[1]["trigger"], "exit");
    assert_eq!(lines[1]["seq"], 1);
    assert_eq!(lines[1]["output"]["text"], "bye");
    // The session is unknown now.
    assert_eq!(t.term(&["screen", "-s", "ex"]).code, 4);
}

// ---- vim -------------------------------------------------------------------------

#[test]
fn vim_edit_write_quit() {
    let t = T::new();
    let file = t.work.join("main.c");
    t.start("vi", &["vim", "--clean", "-i", "NONE", file.to_str().unwrap()]);

    let r = t.term(&["wait", "-s", "vi", "--until", "match", "--pattern", r"main\.c"]);
    assert_eq!(r.code, 0, "{}\n{}", r.out, r.err);
    assert!(status_line(&r.out).contains(" alt "), "vim should be on the alternate screen: {}", r.out);

    // Text with `<` in it goes in --text, untouched.
    let r = t.term(&["send", "-s", "vi", "--keys", "i", "--text", "#include <stdio.h>", "--keys", "<Enter>", "--text", "int x = 1 << 3;"]);
    assert_eq!(r.code, 0, "{}", r.err);
    let mut lines = r.out.lines();
    let status = lines.next().unwrap();
    assert!(status.contains("cursor=2,16"), "{status}");
    assert_eq!(lines.next().unwrap(), "#include <stdio.h>");
    assert_eq!(lines.next().unwrap(), "int x = 1 << 3;");
    assert!(r.out.contains("-- INSERT --"), "{}", r.out);

    let r = t.term(&["send", "-s", "vi", "--keys", "<Esc>:w<Enter>", "--until", "match", "--pattern", "written"]);
    assert_eq!(r.code, 0, "{}\n{}", r.out, r.err);
    assert!(r.out.contains("2L, 35B written"), "{}", r.out);
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "#include <stdio.h>\nint x = 1 << 3;\n");

    let r = t.term(&["send", "-s", "vi", "--keys", ":q<Enter>", "--until", "exit"]);
    assert_eq!(r.code, 0, "{}\n{}", r.out, r.err);
    let will = t.will("vi");
    assert_eq!(will["exit_code"], 0);
    assert_eq!(will["killed_by_waldo"], false);
}

#[test]
fn unknown_key_sends_nothing() {
    let t = T::new();
    t.bash("uk");
    let before = t.term(&["screen", "-s", "uk"]);
    let r = t.term(&["send", "-s", "uk", "--text", "echo should-not-appear", "--keys", "<Entr>"]);
    assert_eq!(r.code, 2);
    assert!(r.err.contains("<Entr>") && r.err.contains("unknown_key") && r.err.contains("<lt>"), "{}", r.err);
    std::thread::sleep(Duration::from_millis(300));
    let after = t.term(&["screen", "-s", "uk"]);
    assert_eq!(before.out, after.out);
}

#[test]
fn key_names_in_text_get_a_hint() {
    let t = T::new();
    t.bash("kh");
    let r = t.term(&["send", "-s", "kh", "--text", "echo hi<Enter>"]);
    assert_eq!(r.code, 0);
    assert!(r.out.contains("hint: '<Enter>' was sent as literal text; use --keys"), "{}", r.out);
}

// ---- run ---------------------------------------------------------------------------

#[test]
fn settle_during_a_silent_job_carries_a_hint() {
    let t = T::new();
    t.bash("h1");
    let r = t.term(&["send", "-s", "h1", "--text", "sleep 3", "--keys", "<Enter>"]);
    assert_eq!(r.code, 0);
    assert!(status_line(&r.out).contains("settle") && status_line(&r.out).contains("fg=sleep"), "{}", r.out);
    assert!(
        r.out.contains("hint: 'sleep' is still running; settle only means the screen went quiet. To wait for it to end, see 'waldo-term help shell'."),
        "{}",
        r.out
    );
    // Only the request that started it is told. After that the program is
    // the thing being driven, and its screens carry no hint.
    let r = t.term(&["send", "-s", "h1", "--text", "x"]);
    assert!(status_line(&r.out).contains("settle") && status_line(&r.out).contains("fg=sleep"), "{}", r.out);
    assert!(!r.out.contains("hint:"), "{}", r.out);
    let r = t.term(&["wait", "-s", "h1", "--until", "settle"]);
    assert!(status_line(&r.out).contains("fg=sleep"), "{}", r.out);
    assert!(!r.out.contains("hint:"), "{}", r.out);
    // At the prompt there is no hint.
    let r = t.term(&["wait", "-s", "h1", "--until", "match", "--pattern", r"\$ x$", "--timeout", "5000"]);
    assert_eq!(r.code, 0, "{}", r.out);
    let r = t.term(&["wait", "-s", "h1", "--until", "settle"]);
    assert!(!r.out.contains("hint:"), "{}", r.out);
    assert!(status_line(&r.out).contains("fg=bash"), "{}", r.out);
}

// ---- matching -----------------------------------------------------------------------

#[test]
fn bash_command_then_match_prompt_then_output() {
    let t = T::new();
    t.bash("m1");
    // The prompt that is already on screen does not satisfy the wait; the
    // one drawn after the command does.
    let (code, v) = t.json(&["send", "-s", "m1", "--text", "sleep 0.6; echo marker-$((6*7))", "--keys", "<Enter>", "--until", "match", "--pattern", r"^\$ $"]);
    assert_eq!(code, 0);
    assert_eq!(v["reason"], "match");
    assert!(v["waited_ms"].as_u64().unwrap() >= 600, "{}", v["waited_ms"]);
    assert_eq!(v["match"]["text"], "$ ");
    assert_eq!(v["match"]["on_screen"], true);
    assert_eq!(v["match"]["row"], 3);
    assert_eq!(v["match"]["col"], 1);
    assert_eq!(v["interleaved"], false);
    assert_eq!(v["screen"]["text"], "$ sleep 0.6; echo marker-$((6*7))\nmarker-42\n$");
    assert_eq!(v["screen"]["cursor"]["row"], 3);
    assert_eq!(v["screen"]["cursor"]["col"], 3);

    // Capture groups.
    let (_, v) = t.json(&["send", "-s", "m1", "--text", "echo version=1.2.3", "--keys", "<Enter>", "--until", "match", "--pattern", r"^version=(\d+)\.(\d+)"]);
    assert_eq!(v["match"]["groups"], serde_json::json!(["1", "2"]));

    let r = t.term(&["output", "-s", "m1"]);
    assert_eq!(r.code, 0);
    assert!(r.out.contains("marker-42\n") && r.out.contains("version=1.2.3\n"), "{}", r.out);
    let r = t.term(&["output", "-s", "m1", "--tail", "2"]);
    assert_eq!(r.out, "version=1.2.3\n$\n");
}

#[test]
fn prompt_with_echo_off_and_no_newline() {
    let t = T::new();
    t.bash("pw");
    let r = t.term(&["send", "-s", "pw", "--text", "read -s -p 'Password: ' pw; echo; echo got=$pw", "--keys", "<Enter>", "--until", "match", "--pattern", "assword: $"]);
    assert_eq!(r.code, 0, "{}\n{}", r.out, r.err);
    assert!(status_line(&r.out).contains("cursor=2,11"), "{}", r.out);
    // The secret comes from stdin, not the command line, and is not echoed.
    let mut child = t.command(TERM).args(["send", "-s", "pw", "--text", "-", "--until", "match", "--pattern", "^got="]).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
    child.stdin.take().unwrap().write_all(b"hunter2\n").unwrap();
    let out = child.wait_with_output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(0), "{text}");
    assert!(text.contains("got=hunter2"), "{text}");
    assert_eq!(text.matches("hunter2").count(), 1, "the password was echoed: {text}");
}

#[test]
fn match_sees_lines_that_scroll_past() {
    let t = T::new();
    t.bash("sc");
    // The needle is long gone from the screen by the time the output stops.
    let (code, v) = t.json(&["send", "-s", "sc", "--text", "seq 1 2000 | sed 's/^777$/needle-777/'", "--keys", "<Enter>", "--until", "match", "--pattern", "^needle-(\\d+)$"]);
    assert_eq!(code, 0);
    assert_eq!(v["reason"], "match");
    assert_eq!(v["match"]["text"], "needle-777");
}

#[test]
fn bad_pattern_is_a_usage_error() {
    let t = T::new();
    t.bash("bp");
    let r = t.term(&["wait", "-s", "bp", "--until", "match", "--pattern", "(unclosed"]);
    assert_eq!(r.code, 2);
    assert!(r.err.contains("bad_pattern"), "{}", r.err);
    let r = t.term(&["wait", "-s", "bp", "--until", "match"]);
    assert_eq!(r.code, 2);
    assert!(r.err.contains("missing_pattern"), "{}", r.err);
    let r = t.term(&["wait", "-s", "bp", "--until", "sometime"]);
    assert_eq!(r.code, 2);
    assert!(r.err.contains("unknown_until"), "{}", r.err);
}

// ---- file items and paste -------------------------------------------------------------

#[test]
fn file_items() {
    let t = T::new();
    t.bash("fi");
    let script = t.work.join("cmd.txt");
    std::fs::write(&script, "echo from-file <<< x\n").unwrap();
    // A relative path is made absolute by waldo-term; the final newline is Enter.
    let r = t.term(&["send", "-s", "fi", "--text-file", "cmd.txt", "--until", "match", "--pattern", "^from-file$"]);
    assert_eq!(r.code, 0, "{}\n{}", r.out, r.err);

    let keys = t.work.join("keys.txt");
    std::fs::write(&keys, "echo keyed<Enter>").unwrap();
    let r = t.term(&["send", "-s", "fi", "--keys-file", keys.to_str().unwrap(), "--until", "match", "--pattern", "^keyed$"]);
    assert_eq!(r.code, 0, "{}\n{}", r.out, r.err);

    // An unreadable file sends nothing, including the items before it.
    let before = t.term(&["screen", "-s", "fi"]);
    let r = t.term(&["send", "-s", "fi", "--text", "echo nope", "--text-file", "/no/such/file"]);
    assert_eq!(r.code, 2);
    assert!(r.err.contains("file_unreadable"), "{}", r.err);
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(before.out, t.term(&["screen", "-s", "fi"]).out);
}

#[test]
fn paste_is_one_piece() {
    let t = T::new();
    let file = t.work.join("p.txt");
    t.start("pa", &["vim", "--clean", "-i", "NONE", file.to_str().unwrap()]);
    assert_eq!(t.term(&["wait", "-s", "pa", "--until", "match", "--pattern", r"p\.txt"]).code, 0);
    // In insert mode with autoindent, typed lines would gain indentation; a
    // bracketed paste arrives as written.
    let body = "if (x) {\n    y();\n}";
    let r = t.term(&["send", "-s", "pa", "--keys", ":set autoindent<Enter>i", "--paste", body, "--keys", "<Esc>:wq<Enter>", "--until", "exit"]);
    assert_eq!(r.code, 0, "{}\n{}", r.out, r.err);
    assert_eq!(std::fs::read_to_string(&file).unwrap(), format!("{body}\n"));
}

// ---- the will ----------------------------------------------------------------------------

#[test]
fn long_output_is_capped_head_and_tail() {
    let t = T::new();
    let r = t.wrap(&["--name", "lo", "--output-cap", "4096", "--", "sh", "-c", "seq -f 'line %05g' 1 5000; exit 2"]);
    assert_eq!(r.code, 2);
    let will = t.will("lo");
    assert_eq!(will["exit_code"], 2);
    let out = &will["output"];
    let text = out["text"].as_str().unwrap();
    assert!(text.len() <= 4096 + 64, "{}", text.len());
    assert!(text.starts_with("line 00001\nline 00002\n"), "{}", &text[..60]);
    assert!(text.ends_with("line 04999\nline 05000"), "{}", &text[text.len() - 60..]);
    assert_eq!(out["lines_total"], 5000);
    assert_eq!(out["truncated"], true);
    let omitted = out["omitted_lines"].as_u64().unwrap();
    assert!(text.contains(&format!("[... {omitted} lines omitted ...]")));
    assert_eq!(text.lines().count() as u64 - 1 + omitted, 5000);
    // The screen shows the end.
    assert!(will["screen"]["text"].as_str().unwrap().ends_with("line 05000"));

    // After the end, waldo-term reads the will.
    let r = t.term(&["screen", "-s", "lo"]);
    assert_eq!(r.code, 3);
    assert!(status_line(&r.out).starts_with("[lo exit code=2 "), "{}", r.out);
    assert!(r.out.ends_with("line 05000\n"));
    let r = t.term(&["output", "-s", "lo"]);
    assert_eq!(r.code, 0);
    let l = t.term(&["list"]);
    assert!(l.out.contains("exited"), "{}", l.out);
}

#[test]
fn wrapped_lines_are_joined_and_progress_bars_collapse() {
    let t = T::new();
    let long = "x".repeat(200);
    let script = format!("printf '10%%\\r50%%\\r100%%\\n'; echo {long}; echo end");
    let r = t.wrap(&["--name", "wr", "--size", "80x24", "--", "sh", "-c", &script]);
    assert_eq!(r.code, 0);
    let will = t.will("wr");
    assert_eq!(will["output"]["text"].as_str().unwrap(), format!("100%\n{long}\nend"));
    // On the screen the long line occupies three rows.
    assert_eq!(will["screen"]["text"].as_str().unwrap().lines().count(), 5);
}

#[test]
fn killed_by_signal_and_core_dump() {
    let t = T::new();
    let r = t.wrap(&["--name", "k1", "--", "sh", "-c", "echo before; kill -KILL $$"]);
    assert_eq!(r.code, 128 + 9);
    let will = t.will("k1");
    assert_eq!(will["exit_code"], Value::Null);
    assert_eq!(will["signal"], "SIGKILL");
    assert_eq!(will["core_dumped"], false);
    assert_eq!(will["killed_by_waldo"], false);
    assert_eq!(will["output"]["text"], "before");

    let r = t.wrap(&["--name", "k2", "--", "sh", "-c", "ulimit -c unlimited 2>/dev/null; kill -SEGV $$"]);
    assert_eq!(r.code, 128 + 11);
    let will = t.will("k2");
    assert_eq!(will["signal"], "SIGSEGV");
    // Whether a core is written depends on the machine's limits and core
    // pattern; the flag has to be present and a boolean either way.
    assert!(will["core_dumped"].is_boolean());
    let r = t.term(&["screen", "-s", "k2"]);
    assert_eq!(r.code, 3);
    assert!(status_line(&r.out).contains("signal=SIGSEGV"), "{}", r.out);
}

#[test]
fn background_process_holding_the_pty_does_not_block_the_will() {
    let t = T::new();
    let began = Instant::now();
    // The child exits at once; its grandchild keeps the terminal open.
    let r = t.wrap(&["--name", "bg", "--", "sh", "-c", "(sleep 20 &) ; echo parent-done"]);
    assert_eq!(r.code, 0);
    assert!(began.elapsed() < Duration::from_secs(5), "waited for the grandchild: {:?}", began.elapsed());
    let will = t.will("bg");
    assert_eq!(will["exit_code"], 0);
    assert_eq!(will["output"]["text"], "parent-done");
}

#[test]
fn child_exit_ends_a_wait_early() {
    let t = T::new();
    t.start("ce", &["sh", "-c", "sleep 0.5; echo bye; exit 4"]);
    let began = Instant::now();
    let (code, v) = t.json(&["wait", "-s", "ce", "--until", "match", "--pattern", "never-appears", "--timeout", "20000"]);
    assert_eq!(code, 3);
    assert!(began.elapsed() < Duration::from_secs(5));
    assert_eq!(v["reason"], "exit");
    assert_eq!(v["child"]["state"], "exited");
    assert_eq!(v["exit"]["exit_code"], 4);
    assert_eq!(v["exit"]["output"]["text"], "bye");
    // By the time the answer arrives the will has been delivered.
    assert_eq!(t.will("ce")["exit_code"], 4);
}

// ---- timeouts ------------------------------------------------------------------------------

#[test]
fn a_screen_that_never_settles_times_out() {
    let t = T::new();
    t.start("ns", &["sh", "-c", "i=0; while true; do i=$((i+1)); printf '\\r%s' $i; sleep 0.05; done"]);
    let began = Instant::now();
    let r = t.term(&["wait", "-s", "ns", "--until", "settle", "--timeout", "700"]);
    assert_eq!(r.code, 124, "{}\n{}", r.out, r.err);
    assert!(began.elapsed() >= Duration::from_millis(700));
    assert!(status_line(&r.out).starts_with("[ns timeout "), "{}", r.out);
    assert!(r.out.contains("hint: the screen never went quiet; wait for specific text with --until match."), "{}", r.out);
    // The screen is still returned.
    assert!(r.out.lines().nth(1).unwrap().parse::<u32>().is_ok(), "{}", r.out);
}

// ---- stopping and dying ----------------------------------------------------------------------

#[test]
fn stop_kills_the_child_and_says_so() {
    let t = T::new();
    t.start("st", &["sleep", "60"]);
    let r = t.term(&["signal", "-s", "st", "--stop"]);
    assert_eq!(r.code, 0, "{}", r.err);
    let will = t.will("st");
    assert_eq!(will["killed_by_waldo"], true);
    assert_eq!(will["kill_reason"], "request");
    assert_eq!(will["signal"], "SIGHUP");
}

#[test]
fn sigterm_to_the_host_kills_the_child_and_leaves_a_will() {
    let t = T::new();
    t.start("ht", &["sleep", "60"]);
    let meta: Value = serde_json::from_str(&std::fs::read_to_string(t.session_dir("ht").join("meta.json")).unwrap()).unwrap();
    let host = meta["host_pid"].as_i64().unwrap() as i32;
    let child = meta["child_pid"].as_i64().unwrap() as i32;
    unsafe { libc::kill(host, libc::SIGTERM) };
    let will = t.will("ht");
    assert_eq!(will["killed_by_waldo"], true);
    assert_eq!(will["kill_reason"], "host_signal");
    assert_eq!(unsafe { libc::kill(child, 0) }, -1, "the child should be gone");
}

#[test]
fn sigkill_of_the_host_is_reported_as_dead() {
    let t = T::new();
    t.start("hk", &["sleep", "60"]);
    let meta: Value = serde_json::from_str(&std::fs::read_to_string(t.session_dir("hk").join("meta.json")).unwrap()).unwrap();
    let host = meta["host_pid"].as_i64().unwrap() as i32;
    let child = meta["child_pid"].as_i64().unwrap() as i32;
    assert!(waldo::session::lock_is_held(&t.session_dir("hk")));
    unsafe { libc::kill(host, libc::SIGKILL) };
    let end = Instant::now() + Duration::from_secs(5);
    while waldo::session::lock_is_held(&t.session_dir("hk")) {
        assert!(Instant::now() < end, "the lock was not released");
        std::thread::sleep(Duration::from_millis(10));
    }
    let l = t.term(&["list"]);
    assert!(l.out.lines().any(|line| line.starts_with("hk") && line.contains("dead")), "{}", l.out);
    let r = t.term(&["screen", "-s", "hk"]);
    assert_eq!(r.code, 4);
    assert!(r.err.contains("left no will"), "{}", r.err);
    // The kernel hangs up the child when the pty master closes.
    let end = Instant::now() + Duration::from_secs(5);
    while unsafe { libc::kill(child, 0) } == 0 {
        assert!(Instant::now() < end, "the child survived its host");
        std::thread::sleep(Duration::from_millis(10));
    }
    // gc removes it, and the name can be used again.
    assert!(t.term(&["gc", "--all"]).out.contains("removed 1"));
    assert!(!t.session_dir("hk").exists());
}

// ---- resize ------------------------------------------------------------------------------------

#[test]
fn resize_reaches_the_program() {
    let t = T::new();
    t.bash("rs");
    let r = t.term(&["resize", "-s", "rs", "132x40"]);
    assert_eq!(r.code, 0, "{}", r.err);
    assert!(status_line(&r.out).contains("132x40"), "{}", r.out);
    let r = t.sh("rs", "stty size");
    assert_eq!(r.out, "40 132", "{}", r.out);
    let r = t.term(&["resize", "-s", "rs", "0x10"]);
    assert_eq!(r.code, 2);
    assert!(r.err.contains("bad_size"), "{}", r.err);
}

#[test]
fn resize_during_output_soak() {
    let t = T::new();
    t.start("so", &["sh", "-c", "i=0; while [ $i -lt 100000 ]; do i=$((i+1)); echo \"row $i abcdefghijklmnopqrstuvwxyz0123456789abcdefghijklmnopqrstuvwxyz0123456789 end\"; done; echo soak-done; sleep 60"]);
    let sizes = ["40x10", "132x50", "20x5", "80x24", "7x3", "200x60", "33x17", "80x24"];
    let mut n = 0;
    let end = Instant::now() + Duration::from_secs(60);
    loop {
        // Without waiting for a redraw: the point is many resizes mid-output.
        let r = t.term(&["resize", "-s", "so", "--until", "none", sizes[n % sizes.len()]]);
        assert_eq!(r.code, 0, "resize {n} failed: {}\n{}", r.out, r.err);
        n += 1;
        if r.out.contains("soak-done") && sizes[(n - 1) % sizes.len()] == "80x24" {
            break;
        }
        assert!(Instant::now() < end, "the output never finished");
    }
    assert!(n >= 8, "only {n} resizes happened during the output");
    // Still alive and coherent after all that.
    let r = t.term(&["wait", "-s", "so", "--until", "settle"]);
    assert_eq!(r.code, 0, "{}", r.err);
    let o = t.term(&["output", "-s", "so", "--tail", "2"]);
    assert_eq!(o.out, "row 100000 abcdefghijklmnopqrstuvwxyz0123456789abcdefghijklmnopqrstuvwxyz0123456789 end\nsoak-done\n", "{}", o.out);
}

// ---- the API without waldo-term ---------------------------------------------------------------------

fn raw_http(socket: &str, request: &str) -> String {
    let mut s = UnixStream::connect(socket).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    s.write_all(request.as_bytes()).unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).unwrap();
    out
}

#[test]
fn plain_http_as_curl_would_send_it() {
    let t = T::new();
    t.bash("ht");
    let socket = t.session_dir("ht").join("sock");
    let socket = socket.to_str().unwrap();

    // A body that is not JSON is one keys item; waits can be query parameters.
    let body = "echo via-http<Enter>";
    let resp = raw_http(
        socket,
        &format!(
            "POST /v1/input?until=match&pattern=%5Evia-http%24&format=text HTTP/1.1\r\nHost: waldo\r\nContent-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        ),
    );
    assert!(resp.starts_with("HTTP/1.1 200 OK\r\n"), "{resp}");
    assert!(resp.contains("Content-Type: text/plain"), "{resp}");
    let text = resp.split("\r\n\r\n").nth(1).unwrap();
    assert!(text.starts_with("[ht match "), "{text}");
    assert!(text.contains("\nvia-http\n"), "{text}");

    // JSON by default; two requests on one connection.
    let mut s = UnixStream::connect(socket).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    s.write_all(b"GET /v1/status HTTP/1.1\r\nHost: waldo\r\n\r\nGET /v1/screen HTTP/1.1\r\nHost: waldo\r\nConnection: close\r\n\r\n").unwrap();
    let mut both = String::new();
    s.read_to_string(&mut both).unwrap();
    assert_eq!(both.matches("HTTP/1.1 200 OK").count(), 2, "{both}");
    assert!(both.contains("\"gen\":") && both.contains("\"reason\":\"none\""), "{both}");

    // Errors are a code and a sentence.
    let resp = raw_http(socket, "GET /v1/nothing HTTP/1.1\r\nHost: waldo\r\nConnection: close\r\n\r\n");
    assert!(resp.starts_with("HTTP/1.1 404 "), "{resp}");
    assert!(resp.contains("\"error\":\"not_found\""), "{resp}");
    let resp = raw_http(socket, "POST /v1/input HTTP/1.1\r\nHost: waldo\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n");
    assert!(resp.starts_with("HTTP/1.1 411 "), "{resp}");
    let resp = raw_http(socket, "POST /v1/triggers HTTP/1.1\r\nHost: waldo\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}");
    assert!(resp.starts_with("HTTP/1.1 400 "), "{resp}");
    assert!(resp.contains("\"error\":\"missing_pattern\""), "{resp}");
}

#[test]
fn a_session_under_a_very_long_path() {
    let t = T::new();
    let deep = t.work.join("a".repeat(60)).join("b".repeat(60));
    std::fs::create_dir_all(&deep).unwrap();
    std::fs::set_permissions(&deep, std::fs::Permissions::from_mode(0o700)).unwrap();
    let out = t.command(WRAP).env("WALDO_DIR", &deep).args(["--detach", "--name", "lp", "--", "sleep", "30"]).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let r = T::finish(t.command(TERM).env("WALDO_DIR", &deep).args(["screen", "-s", "lp"]).output().unwrap());
    assert_eq!(r.code, 0, "{}", r.err);
    let _ = t.command(TERM).env("WALDO_DIR", &deep).args(["signal", "-s", "lp", "KILL"]).output();
}

// ---- help ---------------------------------------------------------------------------------------------

#[test]
fn every_help_example_runs_as_written() {
    let t = T::new();
    let _ = std::fs::remove_file("/tmp/waldo-demo.c");
    let bin_dir = std::path::Path::new(WRAP).parent().unwrap();
    let path = format!("{}:{}", bin_dir.display(), std::env::var("PATH").unwrap_or_default());
    let mut ran = 0;
    for topic in ["", "keys", "wait", "shell", "messages", "commands", "exit-status"] {
        let args: Vec<&str> = if topic.is_empty() { vec!["help"] } else { vec!["help", topic] };
        let help = t.term(&args);
        assert_eq!(help.code, 0, "help {topic}: {}", help.err);
        for line in help.out.lines() {
            let Some(cmd) = line.strip_prefix("  $ ") else { continue };
            let out = t.command("sh").env("PATH", &path).args(["-c", cmd]).output().unwrap();
            assert!(
                out.status.success(),
                "example from 'help {topic}' failed with {:?}: {cmd}\nstdout: {}\nstderr: {}",
                out.status.code(),
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            ran += 1;
        }
    }
    assert!(ran >= 20, "only {ran} examples were found");
    // The guide's vim example really wrote the file.
    assert_eq!(std::fs::read_to_string("/tmp/waldo-demo.c").unwrap(), "#include <stdio.h>\n");
    let _ = std::fs::remove_file("/tmp/waldo-demo.c");
    // The guide is short enough to read in one go.
    assert!(t.term(&["help"]).out.lines().count() <= 45);
}

#[test]
fn usage_errors() {
    let t = T::new();
    let r = t.term(&["frobnicate"]);
    assert_eq!(r.code, 2);
    assert!(r.err.contains("waldo-term help"), "{}", r.err);
    let r = t.term(&["screen"]);
    assert_eq!(r.code, 4);
    assert!(r.err.contains("waldo-wrap --detach"), "{}", r.err);
    let r = t.term(&["screen", "-s", "nope"]);
    assert_eq!(r.code, 4);
    t.bash("ue");
    let r = t.term(&["send", "-s", "ue", "ls"]);
    assert_eq!(r.code, 2);
    assert!(r.err.contains("--text or --keys"), "{}", r.err);
    let r = t.wrap(&[]);
    assert_eq!(r.code, 2);
    assert_eq!(t.wrap(&["--help"]).code, 0);
}

#[test]
fn settle_waits_for_the_first_output() {
    let t = T::new();
    // A program that is slow to draw: a blank screen is not a settled one.
    t.start("fo", &["sh", "-c", "sleep 1; echo up; sleep 30"]);
    let began = Instant::now();
    let r = t.term(&["screen", "-s", "fo", "--until", "settle"]);
    assert_eq!(r.code, 0, "{}\n{}", r.out, r.err);
    assert!(began.elapsed() >= Duration::from_millis(900), "returned after {:?}", began.elapsed());
    assert_eq!(r.out.lines().nth(1), Some("up"), "{}", r.out);

    // A program that never prints runs to the deadline, and says why.
    t.start("fs", &["sleep", "30"]);
    let r = t.term(&["wait", "-s", "fs", "--until", "settle", "--timeout", "500"]);
    assert_eq!(r.code, 124, "{}", r.out);
    assert!(r.out.contains("hint: the program has not drawn anything yet"), "{}", r.out);
}

// ---- styled screens ------------------------------------------------------------------------------------

#[test]
fn styled_screen_keeps_colour_codes() {
    let t = T::new();
    t.bash("sy");
    let r = t.sh("sy", r"printf '\033[7mSELECTED\033[0m plain \033[1;32mgreen\033[0m\n'");
    assert_eq!(r.code, 0, "{}", r.out);
    // Plain by default: no escape characters at all.
    let plain = t.term(&["screen", "-s", "sy"]);
    assert!(plain.out.contains("SELECTED plain green"), "{}", plain.out);
    assert!(!plain.out.contains('\x1b'), "{:?}", plain.out);
    // On request, SGR codes mark what is highlighted.
    let styled = t.term(&["screen", "-s", "sy", "--style", "ansi"]);
    assert_eq!(styled.code, 0, "{}", styled.err);
    assert!(styled.out.contains("\x1b[0;7mSELECTED\x1b[0m plain \x1b[0;1;32mgreen\x1b[0m"), "{:?}", styled.out);
    // The same through JSON, and with a wait.
    let (_, v) = t.json(&["send", "-s", "sy", "--text", r"printf '\033[44mbar\033[0m\n'", "--keys", "<Enter>", "--style", "ansi", "--until", "match", "--pattern", "^bar$"]);
    assert!(v["screen"]["text"].as_str().unwrap().contains("\u{1b}[0;44mbar\u{1b}[0m"), "{}", v["screen"]["text"]);
    let r = t.term(&["screen", "-s", "sy", "--style", "html"]);
    assert_eq!(r.code, 2);
    assert!(r.err.contains("unknown_style"), "{}", r.err);
}

// ---- attach ----------------------------------------------------------------------------------------------

impl T {
    /// Run `waldo-term attach` to session `target` inside another session
    /// named `viewer`, so it has a terminal to draw on.
    fn attach(&self, viewer: &str, size: &str, target: &str, extra: &[&str]) {
        let mut args = vec!["--detach", "--name", viewer, "--size", size, "--", TERM, "attach", "-s", target];
        args.extend_from_slice(extra);
        let r = self.wrap(&args);
        assert_eq!(r.code, 0, "{}", r.err);
        let r = self.term(&["wait", "-s", viewer, "--until", "match", "--pattern", &format!("waldo {target} ")]);
        assert_eq!(r.code, 0, "the viewer never drew its status bar:\n{}\n{}", r.out, r.err);
    }
}

#[test]
fn attach_shows_the_session_and_is_view_only() {
    let t = T::new();
    let r = t.wrap(&["--detach", "--name", "a", "--size", "60x10", "--", "bash", "--norc", "--noprofile"]);
    assert_eq!(r.code, 0, "{}", r.err);
    assert_eq!(t.term(&["wait", "-s", "a", "--until", "match", "--pattern", r"^\$ $"]).code, 0);

    t.attach("v", "100x20", "a", &[]);
    let v = t.term(&["screen", "-s", "v"]);
    assert!(v.out.lines().nth(1) == Some("$"), "{}", v.out);
    assert!(v.out.contains("waldo a  60x10") && v.out.contains("view only") && v.out.contains("Ctrl-] detach"), "{}", v.out);

    // What happens in the session shows up in the viewer.
    assert_eq!(t.sh("a", "echo seen-by-viewer").code, 0);
    let v = t.term(&["wait", "-s", "v", "--until", "match", "--pattern", "^seen-by-viewer$"]);
    assert_eq!(v.code, 0, "{}", v.out);

    // View only: typing in the viewer does not reach the session.
    let before = t.term(&["screen", "-s", "a"]).out;
    t.term(&["send", "-s", "v", "--text", "echo nope", "--keys", "<Enter>"]);
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(before, t.term(&["screen", "-s", "a"]).out);

    // Ctrl-\ switches to typing.
    let v = t.term(&["send", "-s", "v", "--keys", r"<C-\>", "--until", "match", "--pattern", "TYPING"]);
    assert_eq!(v.code, 0, "{}", v.out);
    t.term(&["send", "-s", "v", "--text", "echo typed-in-viewer", "--keys", "<Enter>"]);
    let a = t.term(&["wait", "-s", "a", "--until", "match", "--pattern", "^typed-in-viewer$"]);
    assert_eq!(a.code, 0, "{}", a.out);

    // Ctrl-] detaches; the session carries on.
    let v = t.term(&["send", "-s", "v", "--keys", "<C-]>", "--until", "exit"]);
    assert_eq!(v.code, 0, "{}\n{}", v.out, v.err);
    let will = t.will("v");
    assert_eq!(will["exit_code"], 0);
    assert!(will["output"]["text"].as_str().unwrap().contains("detached from 'a'; it is still running"), "{}", will["output"]["text"]);
    assert!(t.term(&["list"]).out.lines().any(|l| l.starts_with("a ") && l.contains("live")));
}

#[test]
fn attach_shows_colour_and_reports_agent_requests() {
    let t = T::new();
    let r = t.wrap(&["--detach", "--name", "a", "--size", "60x10", "--", "bash", "--norc", "--noprofile"]);
    assert_eq!(r.code, 0, "{}", r.err);
    assert_eq!(t.term(&["wait", "-s", "a", "--until", "match", "--pattern", r"^\$ $"]).code, 0);
    t.attach("v", "100x20", "a", &[]);
    assert_eq!(t.sh("a", r"printf '\033[1;31mRED\033[0m\n'").code, 0);
    assert_eq!(t.term(&["wait", "-s", "v", "--until", "match", "--pattern", "^RED$"]).code, 0);
    // The viewer's terminal received the colour, not just the letters.
    let v = t.term(&["screen", "-s", "v", "--style", "ansi"]);
    assert!(v.out.contains("\x1b[0;1;31mRED\x1b[0m"), "{:?}", v.out);

    // While an agent's request is waiting, the status bar says so.
    let mut waiting = t.command(TERM).args(["wait", "-s", "a", "--until", "match", "--pattern", "never-shown", "--timeout", "3000"]).stdout(Stdio::null()).spawn().unwrap();
    let v = t.term(&["wait", "-s", "v", "--until", "match", "--pattern", "agent request in flight", "--timeout", "2500"]);
    assert_eq!(v.code, 0, "{}", v.out);
    let _ = waiting.wait();
}

#[test]
fn attach_to_a_larger_session_is_cropped_and_never_resizes_it() {
    let t = T::new();
    t.bash("a"); // 120x30
    t.attach("v", "40x10", "a", &[]);
    let v = t.term(&["screen", "-s", "v"]);
    assert!(v.out.contains("(showing 40x9)"), "{}", v.out);
    assert!(status_line(&t.term(&["screen", "-s", "a"]).out).contains("120x30"));
    // The cropped view follows the cursor: fill the session's screen and the
    // newest line is still visible to the viewer.
    assert_eq!(t.sh("a", "seq 1 40; echo bottom-line").code, 0);
    let v = t.term(&["wait", "-s", "v", "--until", "match", "--pattern", "^bottom-line$"]);
    assert_eq!(v.code, 0, "{}", v.out);
}

#[test]
fn attach_resize_takes_the_viewers_size_and_gives_it_back() {
    let t = T::new();
    let r = t.wrap(&["--detach", "--name", "a", "--size", "60x10", "--", "bash", "--norc", "--noprofile"]);
    assert_eq!(r.code, 0, "{}", r.err);
    assert_eq!(t.term(&["wait", "-s", "a", "--until", "match", "--pattern", r"^\$ $"]).code, 0);
    t.attach("v", "100x20", "a", &["--resize"]);
    let v = t.term(&["wait", "-s", "v", "--until", "match", "--pattern", "waldo a  100x19"]);
    assert_eq!(v.code, 0, "{}", v.out);
    assert_eq!(t.sh("a", "stty size").out, "19 100");
    assert_eq!(t.term(&["send", "-s", "v", "--keys", "<C-]>", "--until", "exit"]).code, 0);
    let end = Instant::now() + Duration::from_secs(5);
    while !status_line(&t.term(&["screen", "-s", "a"]).out).contains("60x10") {
        assert!(Instant::now() < end, "the session kept the viewer's size");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn attach_ends_with_the_session() {
    let t = T::new();
    t.start("a", &["sh", "-c", "echo started; sleep 1.5; echo last-words; exit 3"]);
    t.attach("v", "100x20", "a", &[]);
    let v = t.term(&["wait", "-s", "v", "--until", "exit", "--timeout", "8000"]);
    assert_eq!(v.code, 0, "{}\n{}", v.out, v.err);
    let will = t.will("v");
    // The viewer exits the way any command does when the program has ended.
    assert_eq!(will["exit_code"], 3);
    let text = will["output"]["text"].as_str().unwrap();
    assert!(text.contains("[a exit code=3 ") && text.contains("last-words"), "{text}");
}

#[test]
fn attach_needs_a_terminal_and_an_upgrade() {
    let t = T::new();
    t.bash("a");
    let r = t.term(&["attach", "-s", "a"]);
    assert_eq!(r.code, 2);
    assert!(r.err.contains("waldo-term screen"), "{}", r.err);
    let socket = t.session_dir("a").join("sock");
    let resp = raw_http(socket.to_str().unwrap(), "GET /v1/attach HTTP/1.1\r\nHost: waldo\r\nConnection: close\r\n\r\n");
    assert!(resp.starts_with("HTTP/1.1 400 ") && resp.contains("upgrade_required"), "{resp}");

    // The stream itself: one JSON object per line.
    let mut s = UnixStream::connect(&socket).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    s.write_all(b"GET /v1/attach?cols=20&rows=5 HTTP/1.1\r\nHost: waldo\r\nUpgrade: waldo-attach\r\nConnection: Upgrade\r\n\r\n").unwrap();
    let mut got = String::new();
    let mut buf = [0u8; 4096];
    while !got.contains("\"lines\"") || !got.ends_with('\n') {
        let n = s.read(&mut buf).unwrap();
        assert!(n > 0, "{got}");
        got.push_str(&String::from_utf8_lossy(&buf[..n]));
    }
    assert!(got.starts_with("HTTP/1.1 101 "), "{got}");
    let frame: Value = serde_json::from_str(got.split("\r\n\r\n").nth(1).unwrap().lines().next().unwrap()).unwrap();
    assert_eq!(frame["session"], "a");
    assert_eq!(frame["view"]["cols"], 20);
    assert_eq!(frame["view"]["rows"], 5);
    assert_eq!(frame["lines"][0], serde_json::json!([0, "$"]));
    assert_eq!(frame["cursor"], serde_json::json!({"row": 0, "col": 2, "visible": true}));
}

#[test]
fn a_key_after_text_arrives_separately() {
    let t = T::new();
    // A program that detects pastes by timing. It reads some input, is busy
    // with it for a while, and only then draws. Input that arrived while it
    // was busy belongs to the same burst: an Enter in it is part of a paste,
    // not a keypress. Each `dd` is one read; `read -t 0` asks whether more
    // is waiting.
    let program = "stty raw -echo; printf 'ready\\r\\n'; \
        first=$(dd bs=100 count=1 2>/dev/null); sleep 0.12; \
        if read -t 0; then kind=burst; else kind=alone; fi; \
        printf 'got %s\\r\\n' \"$first\"; \
        second=$(dd bs=100 count=1 2>/dev/null | od -An -c); \
        printf 'then %s %s\\r\\n' \"$kind\" \"$second\"; stty sane";
    t.start("kt", &["bash", "--norc", "--noprofile", "-c", program]);
    assert_eq!(t.term(&["wait", "-s", "kt", "--until", "match", "--pattern", "^ready$"]).code, 0);
    let r = t.term(&["send", "-s", "kt", "--text", "hello", "--keys", "<Enter>", "--until", "exit"]);
    assert_eq!(r.code, 0, "{}\n{}", r.out, r.err);
    let will = t.will("kt");
    let out: Vec<String> = will["output"]["text"].as_str().unwrap().lines().map(|l| l.split_whitespace().collect::<Vec<_>>().join(" ")).collect();
    // The Enter came after the program had drawn what it made of the text.
    assert_eq!(out, vec!["ready", "got hello", "then alone \\r"], "{out:?}");
}

#[test]
fn kill_ends_a_session_and_waits() {
    let t = T::new();
    t.start("kl", &["sleep", "60"]);
    let r = t.term(&["kill", "kl"]);
    assert_eq!(r.code, 0, "{}", r.err);
    assert!(r.out.starts_with("[kl exit signal=SIGHUP killed_by_waldo "), "{}", r.out);
    assert_eq!(r.out.lines().count(), 1);
    // It has ended by the time kill returns.
    assert!(t.term(&["list"]).out.lines().any(|l| l.starts_with("kl") && l.contains("exited")));
    // Killing it again is not an error.
    let r = t.term(&["kill", "-s", "kl"]);
    assert_eq!(r.code, 0);
    assert!(r.out.contains("had already ended"), "{}", r.out);
    assert_eq!(t.term(&["kill", "nope"]).code, 4);

    // A program that ignores the hangup is terminated, then killed.
    t.start("st", &["sh", "-c", "trap '' HUP TERM; echo armed; while true; do sleep 1; done"]);
    assert_eq!(t.term(&["wait", "-s", "st", "--until", "match", "--pattern", "^armed$"]).code, 0);
    let began = Instant::now();
    let r = t.term(&["kill", "st"]);
    assert_eq!(r.code, 0, "{}", r.err);
    assert!(r.out.contains("signal=SIGKILL killed_by_waldo"), "{}", r.out);
    assert!(began.elapsed() >= Duration::from_secs(3) && began.elapsed() < Duration::from_secs(9), "{:?}", began.elapsed());
}

// ---- messages: triggers, settle, stream, destinations ------------------------------

fn read_events(path: &std::path::Path) -> Vec<Value> {
    std::fs::read_to_string(path).unwrap_or_default().lines().filter_map(|l| serde_json::from_str(l).ok()).collect()
}

fn of_kind(events: &[Value], kind: &str) -> Vec<Value> {
    events.iter().filter(|e| e["trigger"] == kind).cloned().collect()
}

/// Poll until `enough` is satisfied by the events in the file; messages are
/// written by the delivery thread a moment after they are queued.
fn events_when(path: &std::path::Path, enough: impl Fn(&[Value]) -> bool) -> Vec<Value> {
    let end = Instant::now() + Duration::from_secs(10);
    loop {
        let events = read_events(path);
        if enough(&events) {
            return events;
        }
        assert!(Instant::now() < end, "the expected messages never arrived in {}: {events:?}", path.display());
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Sequence numbers count up from 0 with nothing missing.
fn assert_unbroken(events: &[Value]) {
    for (i, e) in events.iter().enumerate() {
        assert_eq!(e["seq"], i as u64, "{e}");
        assert!(e.get("dropped").is_none(), "{e}");
    }
}

#[test]
fn a_match_trigger_fires_once_per_appearance() {
    let t = T::new();
    t.bash("mt");
    let events = t.session_dir("mt").join("events.ndjson");

    let r = t.term(&["match", "add", "-s", "mt", "--id", "fin", r"^finished (\d+)"]);
    assert_eq!(r.code, 0, "{}", r.err);
    assert!(r.out.contains("added trigger fin (edge)"), "{}", r.out);
    assert!(r.out.contains("events.ndjson"), "{}", r.out);

    assert_eq!(t.sh("mt", "echo finished 7").code, 0);
    let got = of_kind(&events_when(&events, |e| !of_kind(e, "match").is_empty()), "match");
    let m = &got[0]["match"];
    assert_eq!(m["id"], "fin");
    assert_eq!(m["text"], "finished 7");
    assert_eq!(m["groups"], serde_json::json!(["7"]));
    assert_eq!(m["count"], 1);
    assert_eq!(m["on_screen"], true);
    assert_eq!(m["col"], 1);
    assert!(got[0]["screen"]["text"].as_str().unwrap().contains("finished 7"));

    // The line stays on screen through more commands and settles: no repeat.
    assert_eq!(t.sh("mt", "true").code, 0);
    assert_eq!(t.sh("mt", "echo other").code, 0);
    // A second appearance is a second match.
    assert_eq!(t.sh("mt", "echo finished 8").code, 0);
    let all = events_when(&events, |e| of_kind(e, "match").len() >= 2);
    let got = of_kind(&all, "match");
    assert_eq!(got.len(), 2, "{got:?}");
    assert_eq!(got[1]["match"]["text"], "finished 8");
    assert_eq!(got[1]["match"]["count"], 2);
    assert_unbroken(&all);

    let r = t.term(&["match", "list", "-s", "mt"]);
    assert!(r.out.contains("fin  edge  matches=2"), "{}", r.out);
    let listed: Value = serde_json::from_str(&t.term(&["match", "list", "-s", "mt", "--json"]).out).unwrap();
    assert_eq!(listed["triggers"][0]["pattern"], r"^finished (\d+)");

    // Removed, it is silent. Scrolling the old matches away sends nothing either.
    let r = t.term(&["match", "rm", "-s", "mt", "fin"]);
    assert_eq!(r.code, 0, "{}", r.err);
    assert!(r.out.contains("removed trigger fin after 2 matches"), "{}", r.out);
    assert_eq!(t.sh("mt", "echo finished 9; seq 1 100").code, 0);
    assert!(t.term(&["match", "list", "-s", "mt"]).out.contains("no triggers"));
    t.term(&["kill", "mt"]);
    assert_eq!(of_kind(&read_events(&events), "match").len(), 2);
}

#[test]
fn matches_that_scroll_away_are_not_reported_twice() {
    let t = T::new();
    t.bash("ms");
    let events = t.session_dir("ms").join("events.ndjson");
    assert_eq!(t.term(&["match", "add", "-s", "ms", "--id", "x", "^mark$"]).code, 0);
    // Seen on screen first, then pushed off by 200 lines.
    assert_eq!(t.sh("ms", "echo mark").code, 0);
    events_when(&events, |e| of_kind(e, "match").len() == 1);
    assert_eq!(t.sh("ms", "seq 1 200").code, 0);
    // Never rests on screen: line 77 of 5000 goes straight past.
    assert_eq!(t.term(&["match", "add", "-s", "ms", "--id", "y", "^n77$"]).code, 0);
    assert_eq!(t.sh("ms", "for i in $(seq 1 5000); do echo n$i; done").code, 0);
    let all = events_when(&events, |e| of_kind(e, "match").len() >= 2);
    t.term(&["kill", "ms"]);
    let got = of_kind(&read_events(&events), "match");
    assert_eq!(got.len(), 2, "{got:?}");
    assert_eq!(got[1]["match"]["id"], "y");
    assert_eq!(got[1]["match"]["text"], "n77");
    assert_unbroken(&all);
}

#[test]
fn a_once_trigger_removes_itself_and_text_on_screen_counts() {
    let t = T::new();
    t.bash("mo");
    let events = t.session_dir("mo").join("events.ndjson");
    assert_eq!(t.sh("mo", "echo already here").code, 0);
    // Text that is on screen when the trigger is added is a match.
    let r = t.term(&["match", "add", "-s", "mo", "--once", "already"]);
    assert_eq!(r.code, 0, "{}", r.err);
    assert!(r.out.contains("added trigger t1 (once)"), "{}", r.out);
    events_when(&events, |e| of_kind(e, "match").len() == 1);
    assert!(t.term(&["match", "list", "-s", "mo"]).out.contains("no triggers"));
    assert_eq!(t.sh("mo", "echo already again").code, 0);
    t.term(&["kill", "mo"]);
    let got = of_kind(&read_events(&events), "match");
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(got[0]["match"]["id"], "t1");
}

#[test]
fn trigger_requests_that_are_refused() {
    let t = T::new();
    t.bash("me");
    let r = t.term(&["match", "add", "-s", "me", "(unclosed"]);
    assert_eq!(r.code, 2);
    assert!(r.err.contains("not a valid regular expression"), "{}", r.err);
    assert_eq!(t.term(&["match", "add", "-s", "me", "--id", "a", "x"]).code, 0);
    let r = t.term(&["match", "add", "-s", "me", "--id", "a", "y"]);
    assert_eq!(r.code, 1);
    assert!(r.err.contains("already a trigger 'a'"), "{}", r.err);
    let r = t.term(&["match", "add", "-s", "me", "--id", "no spaces", "y"]);
    assert_eq!(r.code, 2);
    let r = t.term(&["match", "rm", "-s", "me", "nope"]);
    assert_eq!(r.code, 2);
    assert!(r.err.contains("no trigger 'nope'; there are: a"), "{}", r.err);
    assert_eq!(t.term(&["match"]).code, 2);
    assert_eq!(t.term(&["match", "add", "-s", "me"]).code, 2);
    // At most 32.
    for i in 0..31 {
        assert_eq!(t.term(&["match", "add", "-s", "me", &format!("p{i}")]).code, 0);
    }
    let r = t.term(&["match", "add", "-s", "me", "one too many"]);
    assert_eq!(r.code, 1);
    assert!(r.err.contains("at most 32"), "{}", r.err);

    let socket = t.session_dir("me").join("sock");
    let resp = raw_http(socket.to_str().unwrap(), "PUT /v1/triggers HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n");
    assert!(resp.starts_with("HTTP/1.1 405"), "{resp}");
    assert!(resp.contains("GET or POST"), "{resp}");
    let resp = raw_http(socket.to_str().unwrap(), "GET /v1/triggers/a HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n");
    assert!(resp.starts_with("HTTP/1.1 405"), "{resp}");
}

#[test]
fn settle_and_stream_messages() {
    let t = T::new();
    let dest = t.work.join("ev.ndjson");
    let r = t.wrap(&[
        "--name",
        "ss",
        "--on",
        "settle,stream",
        "--dest",
        &format!("file:{}", dest.display()),
        "--",
        "sh",
        "-c",
        "seq 1 100; sleep 1; echo tail; sleep 1",
    ]);
    assert_eq!(r.code, 0, "{}", r.err);
    let events = read_events(&dest);
    assert_unbroken(&events);
    assert_eq!(events[0]["trigger"], "hello");
    assert_eq!(events.last().unwrap()["trigger"], "exit");

    // The stream is every output line, in order, numbered from 1.
    let streams = of_kind(&events, "stream");
    assert!(!streams.is_empty());
    let mut expect_from = 1;
    let mut lines: Vec<String> = Vec::new();
    for s in &streams {
        assert_eq!(s["from_line"], expect_from, "{s}");
        let batch = s["lines"].as_array().unwrap();
        expect_from += batch.len() as u64;
        lines.extend(batch.iter().map(|l| l.as_str().unwrap().to_string()));
    }
    let mut want: Vec<String> = (1..=100).map(|n| n.to_string()).collect();
    want.push("tail".into());
    assert_eq!(lines, want);
    assert_eq!(events.last().unwrap()["output"]["lines_total"], 101);

    // The screen settled twice: after the numbers, and after `tail`.
    let settles = of_kind(&events, "settle");
    assert_eq!(settles.len(), 2, "{settles:?}");
    assert!(settles[0]["screen"]["text"].as_str().unwrap().ends_with("\n100"), "{}", settles[0]);
    assert!(settles[1]["screen"]["text"].as_str().unwrap().ends_with("\ntail"), "{}", settles[1]);
    assert!(settles[1]["screen"]["gen"].as_u64() > settles[0]["screen"]["gen"].as_u64());

    // Without --on, neither is sent.
    let quiet = t.work.join("quiet.ndjson");
    let r = t.wrap(&["--name", "sq", "--dest", &format!("file:{}", quiet.display()), "--", "sh", "-c", "seq 1 100; sleep 0.5"]);
    assert_eq!(r.code, 0, "{}", r.err);
    let kinds: Vec<String> = read_events(&quiet).iter().map(|e| e["trigger"].as_str().unwrap().to_string()).collect();
    assert_eq!(kinds, ["hello", "exit"]);

    let r = t.wrap(&["--on", "matches", "--", "true"]);
    assert_eq!(r.code, 2);
    assert!(r.err.contains("settle, stream"), "{}", r.err);
}

#[test]
fn unix_destination_streams_lines() {
    let t = T::new();
    let path = t.work.join("dest.sock");
    let uri = format!("unix:{}", path.display());

    // Nothing is listening: the probe refuses.
    let r = t.wrap(&["--detach", "--dest", &uri, "--", "sleep", "30"]);
    assert_eq!(r.code, 1);
    assert!(r.err.contains("cannot deliver to unix:"), "{}", r.err);

    let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
    let reader = std::thread::spawn(move || {
        let (mut conn, _) = listener.accept().unwrap();
        let mut text = String::new();
        conn.read_to_string(&mut text).unwrap();
        text
    });
    let r = t.wrap(&["--name", "ux", "--on", "stream", "--dest", &uri, "--", "sh", "-c", "echo one; echo two"]);
    assert_eq!(r.code, 0, "{}", r.err);
    assert!(r.out.contains(&format!("dest={uri}")), "{}", r.out);
    // One connection for the whole session; it ends after the will.
    let text = reader.join().unwrap();
    let events: Vec<Value> = text.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert_unbroken(&events);
    let kinds: Vec<&str> = events.iter().map(|e| e["trigger"].as_str().unwrap()).collect();
    assert_eq!(kinds.first(), Some(&"hello"));
    assert_eq!(kinds.last(), Some(&"exit"));
    assert!(kinds.contains(&"stream"), "{kinds:?}");
    assert_eq!(events.last().unwrap()["output"]["text"], "one\ntwo");
    assert!(!t.session_dir("ux").exists());
}

/// A receiver for `http:` destinations: answers every request with `status`
/// and records the request line, the content type and the body.
type Received = std::sync::Arc<std::sync::Mutex<Vec<(String, String, Value)>>>;

fn http_receiver(status: u16) -> (u16, Received) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let record = seen.clone();
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut conn) = conn else { continue };
            let mut raw = Vec::new();
            let mut buf = [0u8; 4096];
            let (head_end, length) = loop {
                let n = conn.read(&mut buf).unwrap_or(0);
                raw.extend_from_slice(&buf[..n]);
                let text = String::from_utf8_lossy(&raw).into_owned();
                if let Some(end) = text.find("\r\n\r\n") {
                    let length = text[..end]
                        .lines()
                        .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap()))
                        .unwrap_or(0);
                    break (end + 4, length);
                }
                if n == 0 {
                    break (raw.len(), 0);
                }
            };
            while raw.len() < head_end + length {
                let n = conn.read(&mut buf).unwrap_or(0);
                if n == 0 {
                    break;
                }
                raw.extend_from_slice(&buf[..n]);
            }
            let head = String::from_utf8_lossy(&raw[..head_end]).into_owned();
            let request_line = head.lines().next().unwrap_or("").to_string();
            let content_type = head
                .lines()
                .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-type:").map(|v| v.trim().to_string()))
                .unwrap_or_default();
            let body: Value = serde_json::from_slice(&raw[head_end..]).unwrap_or(Value::Null);
            record.lock().unwrap().push((request_line, content_type, body));
            let _ = conn.write_all(format!("HTTP/1.1 {status} X\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").as_bytes());
        }
    });
    (port, seen)
}

#[test]
fn http_destination_posts_each_message_and_heartbeats() {
    let t = T::new();
    let (port, seen) = http_receiver(204);
    let uri = format!("http://127.0.0.1:{port}/hook?x=1");
    let r = t.wrap(&["--name", "hx", "--heartbeat", "1", "--dest", &uri, "--", "sh", "-c", "echo hi; sleep 2.6"]);
    assert_eq!(r.code, 0, "{}", r.err);
    assert!(r.out.contains(&format!("dest={uri}")), "{}", r.out);
    let seen = seen.lock().unwrap().clone();
    for (line, ctype, _) in &seen {
        assert_eq!(line, "POST /hook?x=1 HTTP/1.1");
        assert_eq!(ctype, "application/json");
    }
    let events: Vec<Value> = seen.iter().map(|s| s.2.clone()).collect();
    assert_unbroken(&events);
    let kinds: Vec<&str> = events.iter().map(|e| e["trigger"].as_str().unwrap()).collect();
    assert_eq!(kinds.first(), Some(&"hello"));
    assert_eq!(kinds.last(), Some(&"exit"));
    // 2.6 s of silence with a 1 s heartbeat: two of them.
    assert_eq!(kinds.iter().filter(|k| **k == "heartbeat").count(), 2, "{kinds:?}");
    assert_eq!(events.last().unwrap()["output"]["text"], "hi");
    assert!(!t.session_dir("hx").exists());
}

#[test]
fn http_destination_that_refuses_stops_the_session_from_starting() {
    let t = T::new();
    let (port, seen) = http_receiver(500);
    let r = t.wrap(&["--detach", "--dest", &format!("http://127.0.0.1:{port}/hook"), "--", "sleep", "30"]);
    assert_eq!(r.code, 1);
    assert!(r.err.contains("the receiver answered 500"), "{}", r.err);
    assert_eq!(seen.lock().unwrap().len(), 1);
    assert_eq!(std::fs::read_dir(&t.base).unwrap().count(), 0);

    let r = t.wrap(&["--dest", "https://example.org/hook", "--", "true"]);
    assert_eq!(r.code, 2);
    assert!(r.err.contains("proxy"), "{}", r.err);
}

/// The host sleeps while nothing happens: a wait on a command that prints
/// nothing costs no CPU.
#[test]
fn the_host_is_idle_while_a_command_is_quiet() {
    let t = T::new();
    t.bash("idle");
    let meta: Value = serde_json::from_str(&std::fs::read_to_string(t.session_dir("idle").join("meta.json")).unwrap()).unwrap();
    let pid = meta["host_pid"].as_u64().unwrap();
    let ticks = || -> u64 {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
        // Fields after the command name, which may contain spaces.
        let rest: Vec<&str> = stat.rsplit_once(')').unwrap().1.split_whitespace().collect();
        rest[11].parse::<u64>().unwrap() + rest[12].parse::<u64>().unwrap()
    };
    assert_eq!(t.term(&["match", "add", "-s", "idle", "never-printed"]).code, 0);
    let before = ticks();
    assert_eq!(t.sh("idle", "sleep 2").code, 0);
    let used = ticks() - before;
    // Ticks are 10 ms. Spinning would use about 200.
    assert!(used < 30, "the host used {used} ticks of CPU during a quiet 2 s command");
}

/// Section 3.2: a match is forgotten only when it is absent at a settle
/// point. A program that clears and repaints the same text without pausing
/// reports it once; after the screen has rested without it, it is new.
#[test]
fn a_repaint_between_settles_does_not_fire_again() {
    let t = T::new();
    let script = "read x; printf '\\033[?1049h'; \
                  for i in 1 2 3 4 5; do printf '\\033[2J'; sleep 0.05; printf '\\033[Hmark'; sleep 0.05; done; \
                  sleep 0.7; printf '\\033[2J'; sleep 0.7; printf '\\033[Hmark'; sleep 0.7";
    t.start("rp", &["sh", "-c", script]);
    assert_eq!(t.term(&["match", "add", "-s", "rp", "--id", "m", "^mark$"]).code, 0);
    let r = t.term(&["send", "-s", "rp", "--keys", "<Enter>", "--until", "exit", "--timeout", "20000"]);
    assert_eq!(r.code, 0, "{}\n{}", r.out, r.err);
    let got = of_kind(&read_events(&t.session_dir("rp").join("events.ndjson")), "match");
    let counts: Vec<u64> = got.iter().map(|m| m["match"]["count"].as_u64().unwrap()).collect();
    assert_eq!(counts, [1, 2], "{got:?}");
}

// ---- record, linger, and run by the shell's own marks ------------------------------

#[test]
fn record_writes_an_asciicast() {
    let t = T::new();
    let cast = t.work.join("session.cast");
    let r = t.wrap(&["--name", "rec", "--size", "90x20", "--record", cast.to_str().unwrap(), "--", "sh", "-c", "printf 'h\\303\\251llo\\n'; sleep 0.2"]);
    assert_eq!(r.code, 0, "{}", r.err);
    let lines: Vec<Value> = std::fs::read_to_string(&cast).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert_eq!(lines[0]["version"], 2);
    assert_eq!(lines[0]["width"], 90);
    assert_eq!(lines[0]["height"], 20);
    assert!(lines[0]["timestamp"].as_i64().unwrap() > 1_700_000_000);
    let mut text = String::new();
    let mut last = 0.0;
    for e in &lines[1..] {
        assert_eq!(e[1], "o");
        let at = e[0].as_f64().unwrap();
        assert!(at >= last);
        last = at;
        text.push_str(e[2].as_str().unwrap());
    }
    // Raw output: the pty's line ending, not the rendered text.
    assert_eq!(text, "héllo\r\n");

    let r = t.wrap(&["--detach", "--record", "/no/such/dir/x.cast", "--", "sleep", "30"]);
    assert_eq!(r.code, 1);
    assert!(r.err.contains("cannot write the recording"), "{}", r.err);
    assert_eq!(std::fs::read_dir(&t.base).unwrap().filter(|e| e.as_ref().unwrap().file_name() != "rec").count(), 0);
}

#[test]
fn linger_keeps_the_last_screen_readable() {
    let t = T::new();
    let r = t.wrap(&["--detach", "--name", "lg", "--linger", "60", "--", "sh", "-c", "echo bye; exit 4"]);
    assert_eq!(r.code, 0, "{}", r.err);
    // The will is delivered when the program exits, not when the linger ends.
    assert_eq!(t.will("lg")["exit_code"], 4);

    let (code, v) = t.json(&["screen", "-s", "lg"]);
    assert_eq!(code, 3);
    assert_eq!(v["reason"], "exit");
    assert_eq!(v["screen"]["text"], "bye");
    assert_eq!(v["child"]["state"], "exited");
    assert_eq!(v["exit"]["exit_code"], 4);
    let r = t.term(&["output", "-s", "lg"]);
    assert_eq!(r.code, 0, "{}", r.err);
    assert_eq!(r.out, "bye\n");

    // Nothing can be sent any more, and the refusal says why.
    let r = t.term(&["send", "-s", "lg", "--text", "x"]);
    assert_eq!(r.code, 1, "{}", r.out);
    assert!(r.err.contains("the program has exited"), "{}", r.err);

    // kill ends the linger at once.
    let began = Instant::now();
    let r = t.term(&["kill", "lg"]);
    assert_eq!(r.code, 0, "{}", r.err);
    assert!(began.elapsed() < Duration::from_secs(5), "{:?}", began.elapsed());
    assert!(r.out.starts_with("[lg exit code=4"), "{}", r.out);
    assert!(t.term(&["list"]).out.lines().any(|l| l.starts_with("lg") && l.contains("exited")));
}

#[test]
fn linger_ends_by_itself() {
    let t = T::new();
    let began = Instant::now();
    let r = t.wrap(&["--name", "l1", "--linger", "1", "--", "true"]);
    assert_eq!(r.code, 0, "{}", r.err);
    let took = began.elapsed();
    assert!(took >= Duration::from_secs(1) && took < Duration::from_secs(4), "{took:?}");
}

// ---- found by using it ---------------------------------------------------------------

#[test]
fn an_exit_answer_means_the_lock_is_free() {
    let t = T::new();
    for i in 0..5 {
        let name = format!("lk{i}");
        t.start(&name, &["sleep", "60"]);
        let dir = t.session_dir(&name);
        let mut s = UnixStream::connect(dir.join("sock")).unwrap();
        let body = r#"{"wait":{"until":"exit"},"timeout_ms":10000}"#;
        write!(s, "POST /v1/wait HTTP/1.1\r\nHost: waldo\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}", body.len()).unwrap();
        assert_eq!(t.term(&["signal", "-s", &name, "--stop"]).code, 0);
        // The first bytes of the answer, not the end of the connection: the
        // connection also closes when the host is gone.
        let mut buf = [0u8; 4096];
        let n = s.read(&mut buf).unwrap();
        let held = waldo::session::lock_is_held(&dir);
        assert!(String::from_utf8_lossy(&buf[..n]).starts_with("HTTP/1.1 200"), "{}", String::from_utf8_lossy(&buf[..n]));
        assert!(!held, "the host still held its lock when it answered exit (round {i})");
        let r = t.term(&["gc", "--all"]);
        assert_eq!(r.out.trim(), "removed 1 ended session", "{}", r.out);
    }
}

#[test]
fn gc_says_what_is_still_running() {
    let t = T::new();
    t.start("gr", &["sleep", "60"]);
    let r = t.term(&["gc", "--all"]);
    assert_eq!(r.out.trim(), "removed 0 ended sessions; 1 still running (end one with: waldo-term kill ID)");
    assert_eq!(t.term(&["kill", "gr"]).code, 0);
    assert_eq!(t.term(&["gc", "--all"]).out.trim(), "removed 1 ended session");
}

#[test]
fn the_first_input_waits_for_the_first_prompt() {
    let t = T::new();
    // No wait for the prompt: the input arrives while bash is starting.
    // Typed before the prompt, the line would be there twice: once echoed
    // by the terminal and once by the shell.
    t.start("fs", &["bash", "--norc", "--noprofile"]);
    let r = t.term(&["send", "-s", "fs", "--text", "echo sent", "--keys", "<Enter>"]);
    assert_eq!(r.code, 0, "{}\n{}", r.out, r.err);
    assert_eq!(r.out.lines().skip(1).collect::<Vec<_>>(), vec!["$ echo sent", "sent", "$"], "{}", r.out);
}

// ---- found by the soak runs (soak/README.md) ----------------------------------------

#[test]
fn resize_waits_for_the_program_to_redraw() {
    let t = T::new();
    // Redraws a moment after the size changes, as a full-screen program does.
    let program = "trap 'sleep 0.1; echo \"now $(stty size)\"' WINCH; echo ready; while true; do sleep 0.05; done";
    t.start("rw", &["bash", "--norc", "--noprofile", "-c", program]);
    assert_eq!(t.term(&["wait", "-s", "rw", "--until", "match", "--pattern", "^ready$"]).code, 0);
    let r = t.term(&["resize", "-s", "rw", "100x20"]);
    assert_eq!(r.code, 0, "{}", r.err);
    assert!(status_line(&r.out).starts_with("[rw settle ") && status_line(&r.out).contains("100x20"), "{}", r.out);
    assert!(r.out.contains("now 20 100"), "{}", r.out);

    // A screen that never goes quiet does not make a resize fail or take long.
    t.start("rb", &["sh", "-c", "i=0; while true; do i=$((i+1)); printf '\\r%s' $i; sleep 0.05; done"]);
    assert_eq!(t.term(&["wait", "-s", "rb", "--until", "change"]).code, 0);
    let began = Instant::now();
    let r = t.term(&["resize", "-s", "rb", "90x20"]);
    assert_eq!(r.code, 0, "{}\n{}", r.out, r.err);
    assert!(status_line(&r.out).starts_with("[rb none ") && status_line(&r.out).contains("90x20"), "{}", r.out);
    assert!(began.elapsed() < Duration::from_millis(1800), "{:?}", began.elapsed());
}

#[test]
fn timeout_hints_say_why() {
    let t = T::new();
    t.bash("th");
    // With input, text that was already there does not count, and the hint
    // says so.
    let r = t.term(&["send", "-s", "th", "--text", "x", "--until", "match", "--pattern", r"^\$ ", "--timeout", "400"]);
    assert_eq!(r.code, 124, "{}", r.out);
    assert!(r.out.contains("hint: the pattern matches text that was on screen before the input was sent"), "{}", r.out);
    // Without input the same text is a match.
    let r = t.term(&["wait", "-s", "th", "--until", "match", "--pattern", r"^\$ ", "--timeout", "400"]);
    assert_eq!(r.code, 0, "{}", r.out);
    // Waiting for the end of a session that is still at its shell.
    let r = t.term(&["wait", "-s", "th", "--until", "exit", "--timeout", "300"]);
    assert_eq!(r.code, 124, "{}", r.out);
    assert!(r.out.contains("hint: the session has not ended: 'bash' is in the foreground."), "{}", r.out);
}

#[test]
fn a_program_that_leaves_the_foreground_is_named() {
    let t = T::new();
    t.bash("lf");
    let r = t.term(&["send", "-s", "lf", "--text", "cat", "--keys", "<Enter>"]);
    assert!(r.out.contains("hint: 'cat' is still running"), "{}", r.out);
    // Typing into it carries no hint.
    let r = t.term(&["send", "-s", "lf", "--text", "abc", "--keys", "<Enter>"]);
    assert!(!r.out.contains("hint:"), "{}", r.out);
    // The input that ends it does: what follows goes to the shell.
    let r = t.term(&["send", "-s", "lf", "--keys", "<C-d>"]);
    assert_eq!(r.code, 0, "{}", r.out);
    assert!(status_line(&r.out).contains("fg=bash"), "{}", r.out);
    assert!(r.out.contains("hint: 'cat' is no longer in the foreground; input now goes to 'bash'."), "{}", r.out);
    // At the shell there is nothing to say.
    let r = t.term(&["send", "-s", "lf", "--text", "true", "--keys", "<Enter>"]);
    assert!(!r.out.contains("hint:"), "{}", r.out);
}

// ---- several clients on one session --------------------------------------------------

/// Four clients each send `per_client` commands to one session at once.
fn concurrent_sends(per_client: usize) {
    let t = T::new();
    t.bash("cs");
    let workers: Vec<_> = ["a", "b", "c", "d"]
        .into_iter()
        .map(|p| {
            let cmds: Vec<Command> = (1..=per_client)
                .map(|i| {
                    let mut c = t.command(TERM);
                    c.args(["send", "-s", "cs", "--json", "--text", &format!("echo {p}-{i}-end"), "--keys", "<Enter>"]);
                    c
                })
                .collect();
            std::thread::spawn(move || cmds.into_iter().map(|mut c| T::finish(c.output().unwrap())).collect::<Vec<_>>())
        })
        .collect();
    let mut interleaved = 0;
    for w in workers {
        for r in w.join().unwrap() {
            assert_eq!(r.code, 0, "{}", r.err);
            let v: Value = serde_json::from_str(&r.out).unwrap();
            if v["interleaved"] == true {
                interleaved += 1;
            }
        }
    }
    assert_eq!(t.term(&["wait", "-s", "cs", "--until", "settle"]).code, 0);
    let out = t.term(&["output", "-s", "cs"]).out;
    // Each request's text and its Enter stay together: no line is mixed.
    let mut typed: Vec<&str> = out.lines().filter(|l| l.starts_with("$ echo ")).collect();
    let mut printed: Vec<&str> = out.lines().filter(|l| l.ends_with("-end") && !l.starts_with('$')).collect();
    typed.sort_unstable();
    printed.sort_unstable();
    let mut want: Vec<String> = ["a", "b", "c", "d"].iter().flat_map(|p| (1..=per_client).map(move |i| format!("{p}-{i}-end"))).collect();
    want.sort_unstable();
    assert_eq!(printed, want, "{out}");
    assert_eq!(typed, want.iter().map(|w| format!("$ echo {w}")).collect::<Vec<_>>(), "{out}");
    // Requests that overlapped were told so.
    assert!(interleaved > 0, "no request was marked interleaved");
}

#[test]
fn concurrent_sends_are_typed_whole() {
    concurrent_sends(8);
}

/// The full-size hammer from the soak runs (soak/README.md). Not part of the
/// normal run: `cargo test --test integration -- --ignored soak`.
#[test]
#[ignore = "soak test; run with --ignored"]
fn soak_hammer_one_session() {
    concurrent_sends(50);
}

#[test]
fn a_signal_waits_for_the_request_being_typed() {
    let t = T::new();
    t.bash("sq");
    // One request that types a command line in pieces. Each key after text
    // waits for the shell to react, so the request takes a while.
    let typist = {
        let mut c = t.command(TERM);
        c.args(["send", "-s", "sq", "--json", "--text", "echo "]);
        for part in ["one-", "two-", "three-", "four-", "five-"] {
            c.args(["--text", part, "--keys", "<Home><End>"]);
        }
        c.args(["--text", "six", "--keys", "<Enter>"]);
        std::thread::spawn(move || T::finish(c.output().unwrap()))
    };
    // An interrupt from another client arrives in the middle of it. In the
    // middle of the line it would throw away what had been typed so far.
    std::thread::sleep(Duration::from_millis(120));
    let began = Instant::now();
    let r = t.term(&["signal", "-s", "sq", "SIGINT"]);
    assert_eq!(r.code, 0, "{}", r.err);
    let waited = began.elapsed();
    let typed = typist.join().unwrap();
    assert_eq!(typed.code, 0, "{}", typed.err);
    let v: Value = serde_json::from_str(&typed.out).unwrap();
    assert_eq!(t.term(&["wait", "-s", "sq", "--until", "settle"]).code, 0);
    let out = t.term(&["output", "-s", "sq"]).out;
    assert!(out.lines().any(|l| l == "one-two-three-four-five-six"), "the line was broken:\n{out}");
    // The signal was held until the typing was done, and the typist is told
    // that someone else acted during its request.
    assert!(waited >= Duration::from_millis(100), "the signal did not wait: {waited:?}");
    assert_eq!(v["interleaved"], true, "{}", typed.out);
}

#[test]
fn a_signal_does_not_wait_for_a_program_that_is_not_reading() {
    let t = T::new();
    t.start("nr", &["sh", "-c", "stty raw -echo; echo ready; sleep 60"]);
    assert_eq!(t.term(&["wait", "-s", "nr", "--until", "match", "--pattern", "^ready"]).code, 0);
    // Far more input than the terminal will hold for a program that never
    // reads: the queue stops.
    let big = t.work.join("big.txt");
    std::fs::write(&big, "x".repeat(1000 << 10)).unwrap();
    let r = t.term(&["send", "-s", "nr", "--text-file", big.to_str().unwrap(), "--until", "none"]);
    assert_eq!(r.code, 0, "{}", r.err);
    std::thread::sleep(Duration::from_millis(300));
    let began = Instant::now();
    assert_eq!(t.term(&["signal", "-s", "nr", "SIGTERM"]).code, 0);
    let r = t.term(&["wait", "-s", "nr", "--until", "exit", "--timeout", "5000"]);
    assert_eq!(r.code, 0, "{}\n{}", r.out, r.err);
    assert!(began.elapsed() < Duration::from_secs(4), "{:?}", began.elapsed());
    assert_eq!(t.will("nr")["signal"], "SIGTERM");
}

// ---- waldo inside waldo ---------------------------------------------------------------

#[test]
fn typing_reaches_the_bottom_of_two_attaches() {
    let t = T::new();
    for (name, size) in [("b1", "120x32"), ("b2", "100x26"), ("b3", "80x20")] {
        let r = t.wrap(&["--detach", "--name", name, "--size", size, "--", "bash", "--norc", "--noprofile"]);
        assert_eq!(r.code, 0, "{}", r.err);
        assert_eq!(t.term(&["wait", "-s", name, "--until", "match", "--pattern", r"^\$ $"]).code, 0);
    }
    // b2 attaches to b3, and b1 to b2, both with typing on.
    let r = t.term(&["send", "-s", "b2", "--text", &format!("{TERM} attach -s b3 --drive"), "--keys", "<Enter>", "--until", "match", "--pattern", "waldo b3 "]);
    assert_eq!(r.code, 0, "{}\n{}", r.out, r.err);
    let r = t.term(&["send", "-s", "b1", "--text", &format!("{TERM} attach -s b2 --drive"), "--keys", "<Enter>", "--until", "match", "--pattern", "waldo b2 "]);
    assert_eq!(r.code, 0, "{}\n{}", r.out, r.err);

    // Typed at the top, it runs at the bottom, and the top shows it through
    // both layers with a status bar for each.
    let r = t.term(&["send", "-s", "b1", "--text", "echo through-two", "--keys", "<Enter>", "--until", "match", "--pattern", "^through-two$"]);
    assert_eq!(r.code, 0, "{}\n{}", r.out, r.err);
    assert!(r.out.contains("waldo b3  80x20") && r.out.contains("waldo b2  100x26"), "{}", r.out);
    let bottom = t.term(&["screen", "-s", "b3"]).out;
    assert!(bottom.contains("$ echo through-two\nthrough-two\n"), "{bottom}");

    // The bottom session ends: the middle one is back at its shell, and the
    // top is still attached to the middle.
    assert_eq!(t.term(&["kill", "b3"]).code, 0);
    let r = t.term(&["wait", "-s", "b2", "--until", "match", "--pattern", r"^\$ $", "--timeout", "5000"]);
    assert_eq!(r.code, 0, "{}\n{}", r.out, r.err);
    assert!(status_line(&r.out).contains("fg=bash"), "{}", r.out);
    let top = t.term(&["wait", "-s", "b1", "--until", "settle"]).out;
    assert!(status_line(&top).contains("fg=waldo-term") && top.contains("waldo b2  100x26"), "{top}");
    assert!(!top.contains("waldo b3 "), "{top}");

    // Detach at the top: its own shell again.
    let r = t.term(&["send", "-s", "b1", "--keys", "<C-]>", "--until", "match", "--pattern", "^detached from 'b2'"]);
    assert_eq!(r.code, 0, "{}\n{}", r.out, r.err);
}

// ---- shell commands: a marker, not a special command --------------------------------

#[test]
fn a_marker_tells_when_a_command_ended_and_how() {
    let t = T::new();
    t.bash("mk");
    let r = t.sh("mk", "echo one; echo two");
    assert_eq!((r.code, r.out.as_str()), (0, "one\ntwo"));
    let r = t.sh("mk", "echo oops >&2; exit_with() { return 3; }; exit_with");
    assert_eq!((r.code, r.out.as_str()), (3, "oops"));

    // Silent for much longer than the settle window: the wait is for the
    // marker, not for a quiet screen.
    let began = Instant::now();
    let r = t.sh("mk", "sleep 1.2; echo late");
    assert!(began.elapsed() >= Duration::from_millis(1200));
    assert_eq!((r.code, r.out.as_str()), (0, "late"));

    // Several lines in one send.
    let r = t.sh("mk", "for i in 1 2 3; do\n  echo n$i\ndone");
    assert_eq!((r.code, r.out.as_str()), (0, "n1\nn2\nn3"));

    // More output than the screen holds: `output` has it all.
    let r = t.sh("mk", "seq 1 300");
    assert_eq!(r.out.lines().count(), 300);
    assert!(r.out.starts_with("1\n2\n") && r.out.ends_with("299\n300"), "{}", r.out);

    // A command that is interrupted never prints its marker. The wait runs
    // out, and the shell is usable again.
    let r = t.term(&["send", "-s", "mk", "--text", "sleep 30; echo \"never=$?\"", "--keys", "<Enter>", "--until", "match", "--pattern", "^never=", "--timeout", "500"]);
    assert_eq!(r.code, 124, "{}\n{}", r.out, r.err);
    assert!(status_line(&r.out).contains("fg=sleep"), "{}", r.out);
    assert_eq!(t.term(&["send", "-s", "mk", "--keys", "<C-c>"]).code, 0);
    assert_eq!(t.sh("mk", "echo after").out, "after");
}

// ---- waldo inside waldo ---------------------------------------------------------------

#[test]
fn waldo_inside_waldo_three_deep() {
    let t = T::new();
    t.bash("n1");
    // Only n1 is addressed from outside. It starts n2, and n2 starts n3.
    let r = t.sh("n1", &format!("{WRAP} --detach --name n2 --size 100x28 -- bash --norc --noprofile"));
    assert_eq!(r.code, 0, "{}", r.out);
    assert_eq!(r.out.lines().next(), Some("session=n2"), "{}", r.out);
    let start3 = format!(
        "{TERM} send -s n2 --text '{WRAP} --detach --name n3 --size 80x20 -- bash --norc --noprofile' --keys '<Enter>' --until match --pattern '^session=n3$'"
    );
    let r = t.sh("n1", &start3);
    assert_eq!(r.code, 0, "{}", r.out);
    assert!(r.out.lines().next().unwrap_or("").starts_with("[n2 match ") && r.out.contains(" 100x28 "), "{}", r.out);

    // A command at the bottom, typed through both layers: each layer prints
    // the screen of the one below, with its status line.
    let deep = format!(
        "{TERM} send -s n2 --text \"{TERM} send -s n3 --text 'echo deep-\\$WALDO_SESSION' --keys '<Enter>' --until match --pattern '^deep-n3'\" --keys '<Enter>' --until match --pattern '^deep-n3'"
    );
    let r = t.sh("n1", &deep);
    assert_eq!(r.code, 0, "{}", r.out);
    assert!(r.out.lines().next().unwrap_or("").starts_with("[n2 match "), "{}", r.out);
    assert!(r.out.lines().any(|l| l.starts_with("[n3 match ") && l.contains(" 80x20 ")), "{}", r.out);
    assert!(r.out.lines().any(|l| l == "deep-n3"), "{}", r.out);
    assert!(t.term(&["screen", "-s", "n3"]).out.lines().any(|l| l == "deep-n3"));

    // Two sessions driven at once from the third: each gets its own line.
    let fan = format!(
        "for s in n2 n3; do {TERM} send -s $s --text \"echo hello-from-$s\" --keys '<Enter>' --until match --pattern \"^hello-from-$s\" > /dev/null & done; wait"
    );
    assert_eq!(t.sh("n1", &fan).code, 0);
    for s in ["n2", "n3"] {
        let want = format!("hello-from-{s}");
        assert!(t.term(&["screen", "-s", s]).out.lines().any(|l| l == want), "{s} did not get its line");
    }
}

// ---- PROTOCOL.md agrees with the host ----------------------------------------------

const PROTOCOL: &str = include_str!("../PROTOCOL.md");

/// The codes in the error table of PROTOCOL.md section 9.
fn documented_errors() -> Vec<String> {
    let section = PROTOCOL.split("\n## 9. Errors\n").nth(1).unwrap().split("\n## 10.").next().unwrap();
    section
        .lines()
        .filter(|l| l.starts_with("| 4"))
        .map(|l| l.split('`').nth(1).unwrap_or_else(|| panic!("no code in: {l}")).to_string())
        .collect()
}

#[test]
fn protocol_examples_are_json() {
    let mut blocks = 0;
    let mut rest = PROTOCOL;
    while let Some(at) = rest.find("```json\n") {
        let body = &rest[at + 8..];
        let end = body.find("\n```").expect("an unclosed json block");
        let text = &body[..end];
        // A block is one object, or several with one on each line.
        if serde_json::from_str::<Value>(text).is_err() {
            for line in text.lines() {
                serde_json::from_str::<Value>(line).unwrap_or_else(|e| panic!("not JSON ({e}):\n{text}"));
            }
        }
        blocks += 1;
        rest = &body[end..];
    }
    assert!(blocks >= 15, "only {blocks} json blocks were found");
}

#[test]
fn protocol_lists_every_error_the_host_can_send() {
    let documented = documented_errors();
    assert!(documented.len() >= 20, "{documented:?}");
    // Every code the sources send: the third argument after `self.error(`,
    // the second of `Parsed::Bad(`, and the three an input item can fail with.
    let host = include_str!("../src/bin/waldo-wrap/host.rs");
    let http = include_str!("../src/bin/waldo-wrap/http.rs");
    let mut sent: Vec<String> = vec!["bad_item".into(), "unknown_key".into(), "file_unreadable".into()];
    for (source, call) in [(host, "self.error("), (http, "Parsed::Bad(")] {
        for piece in source.split(call).skip(1) {
            let head: String = piece.chars().take(400).collect();
            let Some(open) = head.find('"') else { continue };
            let code: String = head[open + 1..].chars().take_while(|c| *c != '"').collect();
            if !code.is_empty() && code.chars().all(|c| c.is_ascii_lowercase() || c == '_') {
                sent.push(code);
            }
        }
    }
    sent.sort();
    sent.dedup();
    for code in &sent {
        assert!(documented.contains(code), "PROTOCOL.md section 9 does not list '{code}'");
    }
    for code in &documented {
        assert!(sent.contains(code), "PROTOCOL.md lists '{code}', which the host never sends");
    }
}

#[test]
fn protocol_endpoints_and_parameters_are_as_documented() {
    let t = T::new();
    t.bash("pr");
    let sock = t.session_dir("pr").join("sock");
    let sock = sock.to_str().unwrap();
    let get = |path: &str| raw_http(sock, &format!("GET {path} HTTP/1.1\r\nHost: waldo\r\nConnection: close\r\n\r\n"));
    let post = |path: &str, body: &str| {
        raw_http(sock, &format!("POST {path} HTTP/1.1\r\nHost: waldo\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}", body.len()))
    };
    let json = |response: &str| -> Value { serde_json::from_str(response.split("\r\n\r\n").nth(1).unwrap()).unwrap() };

    // The endpoint table is the host's own list.
    let listed = json(&get("/v1/nope"))["text"].as_str().unwrap().to_string();
    let table = PROTOCOL.split("\n## 4. Endpoints\n").nth(1).unwrap().split("\n### 4.1").next().unwrap();
    let mut rows = 0;
    for line in table.lines().filter(|l| l.starts_with("| `")) {
        let endpoint = line.split('`').nth(1).unwrap();
        assert!(listed.contains(endpoint), "the host does not list '{endpoint}': {listed}");
        rows += 1;
    }
    assert_eq!(rows, listed.matches("/v1/").count() - 1, "{listed}");

    // Wait parameters are read from the query, the wait object, or the top
    // level of the body (section 3.1).
    for response in [
        post("/v1/wait?until=match&pattern=%5E%5C%24%20%24", ""),
        post("/v1/wait", r#"{"wait": {"until": "match", "pattern": "^\\$ $"}}"#),
        post("/v1/wait", r#"{"until": "match", "pattern": "^\\$ $", "timeout_ms": "2000"}"#),
    ] {
        let v = json(&response);
        assert_eq!(v["reason"], "match", "{response}");
        assert_eq!(v["match"]["on_screen"], true);
        assert_eq!(v["v"], 1);
    }

    // The status object has the documented fields (section 4.1).
    let status = json(&get("/v1/status"));
    for field in ["v", "session", "name", "argv", "cwd", "socket", "dest", "host_pid", "started", "uptime_ms", "cols", "rows", "gen", "child", "messages"] {
        assert!(status.get(field).is_some(), "status has no '{field}': {status}");
    }
    assert_eq!(status.as_object().unwrap().len(), 15, "status has fields PROTOCOL.md does not list: {status}");

    // Text on request, except for triggers and errors (section 3.5).
    assert!(get("/v1/screen?format=text").contains("\r\n\r\n[pr none gen="));
    assert!(get("/v1/triggers?format=text").contains("Content-Type: application/json"));
    assert!(get("/v1/screen?until=never&format=text").contains("\"error\":\"unknown_until\""));
    assert!(raw_http(sock, "PUT /v1/screen HTTP/1.1\r\nHost: waldo\r\nConnection: close\r\n\r\n").starts_with("HTTP/1.1 405 "));
}

/// Every error in PROTOCOL.md section 9 is provoked over the socket, and
/// comes back with the status and code the table gives. A code added to the
/// table without a case here fails the test.
#[test]
fn every_documented_error_can_be_provoked() {
    let t = T::new();
    t.bash("er");
    let sock = t.session_dir("er").join("sock");
    let sock = sock.to_str().unwrap().to_string();
    let before = t.term(&["screen", "-s", "er"]).out;

    // What the table says: code -> status.
    let section = PROTOCOL.split("\n## 9. Errors\n").nth(1).unwrap().split("\n## 10.").next().unwrap();
    let table: Vec<(String, u16)> = section
        .lines()
        .filter(|l| l.starts_with("| 4"))
        .map(|l| (l.split('`').nth(1).unwrap().to_string(), l[2..5].parse().unwrap()))
        .collect();

    let json = "Content-Type: application/json\r\n";
    let post = |path: &str, body: &str| {
        format!("POST {path} HTTP/1.1\r\nHost: waldo\r\nConnection: close\r\n{json}Content-Length: {}\r\n\r\n{body}", body.len())
    };
    let get = |path: &str| format!("GET {path} HTTP/1.1\r\nHost: waldo\r\nConnection: close\r\n\r\n");
    let trigger = |body: &str| post("/v1/triggers", body);

    // A session that has exited and is lingering, for `exited`.
    let r = t.wrap(&["--detach", "--name", "lg", "--linger", "20", "--", "true"]);
    assert_eq!(r.code, 0, "{}", r.err);
    assert_eq!(t.term(&["wait", "-s", "lg", "--until", "exit"]).code, 0);
    let lingering = t.session_dir("lg").join("sock").to_str().unwrap().to_string();

    // Fill the session's triggers: one named, then up to the limit.
    assert!(raw_http(&sock, &trigger(r#"{"pattern": "x", "id": "a"}"#)).starts_with("HTTP/1.1 200"));
    for i in 0..31 {
        assert!(raw_http(&sock, &trigger(&format!(r#"{{"pattern": "p{i}"}}"#))).starts_with("HTTP/1.1 200"));
    }

    let cases: Vec<(&str, &str, String)> = vec![
        ("bad_request", &sock, "NOT HTTP AT ALL\r\n\r\n".into()),
        ("length_required", &sock, "POST /v1/input HTTP/1.1\r\nHost: waldo\r\nTransfer-Encoding: chunked\r\n\r\n".into()),
        ("body_too_large", &sock, "POST /v1/input HTTP/1.1\r\nHost: waldo\r\nContent-Length: 2000000\r\n\r\n".into()),
        ("headers_too_large", &sock, format!("GET /v1/status HTTP/1.1\r\nX-Pad: {}", "a".repeat(70_000))),
        ("not_found", &sock, get("/v1/nope")),
        ("method_not_allowed", &sock, "PUT /v1/screen HTTP/1.1\r\nHost: waldo\r\nConnection: close\r\n\r\n".into()),
        ("bad_json", &sock, post("/v1/wait", "[1, 2]")),
        ("missing_input", &sock, post("/v1/input", r#"{"keys": "x"}"#)),
        ("bad_item", &sock, post("/v1/input", r#"{"input": [{"text": 1}]}"#)),
        ("unknown_key", &sock, post("/v1/input", r#"{"input": [{"text": "typed?"}, {"keys": "<Entr>"}]}"#)),
        ("file_unreadable", &sock, post("/v1/input", r#"{"input": [{"text": "typed?"}, {"text_file": "not/absolute"}]}"#)),
        ("unknown_until", &sock, post("/v1/wait", r#"{"until": "never"}"#)),
        ("unknown_style", &sock, post("/v1/wait", r#"{"style": "neon"}"#)),
        ("bad_timeout", &sock, post("/v1/wait", r#"{"timeout_ms": "soon"}"#)),
        ("missing_pattern", &sock, post("/v1/wait", r#"{"until": "match"}"#)),
        ("bad_pattern", &sock, post("/v1/wait", r#"{"until": "match", "pattern": "("}"#)),
        ("bad_tail", &sock, get("/v1/output?tail=some")),
        ("bad_size", &sock, post("/v1/resize", r#"{"cols": 0, "rows": 10}"#)),
        ("bad_signal", &sock, post("/v1/signal", "{}")),
        ("unknown_signal", &sock, post("/v1/signal", r#"{"signal": "SIGNOPE"}"#)),
        ("unknown_mode", &sock, trigger(r#"{"pattern": "x", "mode": "level"}"#)),
        ("bad_id", &sock, trigger(r#"{"pattern": "x", "id": "no spaces"}"#)),
        ("trigger_exists", &sock, trigger(r#"{"pattern": "y", "id": "a"}"#)),
        ("too_many_triggers", &sock, trigger(r#"{"pattern": "one too many"}"#)),
        ("no_such_trigger", &sock, "DELETE /v1/triggers/nope HTTP/1.1\r\nHost: waldo\r\nConnection: close\r\n\r\n".into()),
        ("upgrade_required", &sock, get("/v1/attach")),
        ("exited", &lingering, post("/v1/input", r#"{"input": [{"text": "x"}]}"#)),
    ];

    for (code, socket, request) in &cases {
        let want = table.iter().find(|(c, _)| c == code).unwrap_or_else(|| panic!("PROTOCOL.md does not list '{code}'")).1;
        let response = raw_http(socket, request);
        assert!(response.starts_with(&format!("HTTP/1.1 {want} ")), "'{code}' should be status {want}:\n{response}");
        assert!(response.contains("Content-Type: application/json"), "{response}");
        let body: Value = serde_json::from_str(response.split("\r\n\r\n").nth(1).unwrap_or("")).unwrap_or_else(|e| panic!("'{code}': the body is not JSON ({e}):\n{response}"));
        assert_eq!(body["error"], *code, "{response}");
        // The sentence says something, and it is not just the code.
        assert!(body["text"].as_str().unwrap_or("").len() > 20, "{response}");
    }
    for (code, _) in &table {
        assert!(cases.iter().any(|(c, _, _)| c == code), "no case provokes '{code}'");
    }

    // A refused request types nothing: the screen is as it was.
    assert_eq!(t.term(&["screen", "-s", "er"]).out, before);
    assert_eq!(t.term(&["kill", "lg"]).code, 0);
}

//! Integration tests: real programs in real ptys, driven through the two
//! binaries. The list follows DESIGN.md section 12. The programs (bash, vim,
//! coreutils) come from nixpkgs through the flake.

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
fn run_reports_output_and_status() {
    let t = T::new();
    t.bash("r1");

    let r = t.term(&["run", "-s", "r1", "echo one; echo two"]);
    assert_eq!(r.code, 0, "{}\n{}", r.out, r.err);
    assert!(status_line(&r.out).starts_with("[r1 done exit=0 "), "{}", r.out);
    assert_eq!(r.out.lines().skip(1).collect::<Vec<_>>(), vec!["one", "two"]);

    let r = t.term(&["run", "-s", "r1", "echo oops >&2; exit_with() { return 3; }; exit_with"]);
    assert_eq!(r.code, 5, "{}\n{}", r.out, r.err);
    assert!(status_line(&r.out).starts_with("[r1 done exit=3 "), "{}", r.out);
    assert_eq!(r.out.lines().nth(1), Some("oops"));

    // Silent for much longer than the settle window: run still waits.
    let began = Instant::now();
    let (code, v) = t.json(&["run", "-s", "r1", "sleep 1.2; echo late"]);
    assert_eq!(code, 0);
    assert!(began.elapsed() >= Duration::from_millis(1200));
    assert_eq!(v["reason"], "done");
    assert_eq!(v["command_exit_code"], 0);
    assert_eq!(v["output"]["text"], "late");
    assert!(v["command_ms"].as_u64().unwrap() >= 1200);

    // A trailing comment or `&` does not swallow the end mark.
    let r = t.term(&["run", "-s", "r1", "echo commented # not this"]);
    assert_eq!(r.code, 0, "{}", r.out);
    assert_eq!(r.out.lines().nth(1), Some("commented"));

    // Several lines.
    let r = t.term(&["run", "-s", "r1", "for i in 1 2 3; do\n  echo n$i\ndone"]);
    assert_eq!(r.code, 0, "{}", r.out);
    assert_eq!(r.out.lines().skip(1).collect::<Vec<_>>(), vec!["n1", "n2", "n3"]);

    // More output than the screen holds.
    let (code, v) = t.json(&["run", "-s", "r1", "seq 1 300"]);
    assert_eq!(code, 0);
    let text = v["output"]["text"].as_str().unwrap();
    assert_eq!(text.lines().count(), 300);
    assert!(text.starts_with("1\n2\n") && text.ends_with("299\n300"));
    assert_eq!(v["output"]["lines_total"], 300);
}

#[test]
fn run_timeout_then_wait_until_done() {
    let t = T::new();
    t.bash("r2");
    let r = t.term(&["run", "-s", "r2", "--timeout", "300", "sleep 1.5; echo finally"]);
    assert_eq!(r.code, 124, "{}\n{}", r.out, r.err);
    assert!(status_line(&r.out).starts_with("[r2 timeout "), "{}", r.out);
    assert!(r.out.contains("hint: the command is still running"), "{}", r.out);

    // A second run is refused while the first is going.
    let r = t.term(&["run", "-s", "r2", "echo no"]);
    assert_eq!(r.code, 1);
    assert!(r.err.contains("run_in_progress"), "{}", r.err);

    let r = t.term(&["wait", "-s", "r2", "--until", "done"]);
    assert_eq!(r.code, 0, "{}\n{}", r.out, r.err);
    assert!(status_line(&r.out).starts_with("[r2 done exit=0 "), "{}", r.out);
    assert_eq!(r.out.lines().nth(1), Some("finally"));
}

#[test]
fn settle_during_a_silent_job_carries_a_hint() {
    let t = T::new();
    t.bash("h1");
    let r = t.term(&["send", "-s", "h1", "--text", "sleep 2", "--keys", "<Enter>"]);
    assert_eq!(r.code, 0);
    assert!(status_line(&r.out).contains("settle") && status_line(&r.out).contains("fg=sleep"), "{}", r.out);
    assert!(
        r.out.contains("hint: 'sleep' is still running; settle only means the screen went quiet. Use --until match, or waldo-term run."),
        "{}",
        r.out
    );
    // At the prompt there is no hint.
    let r = t.term(&["wait", "-s", "h1", "--until", "match", "--pattern", r"^\$ $", "--timeout", "5000"]);
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
    let r = t.term(&["run", "-s", "rs", "stty size"]);
    assert_eq!(r.out.lines().nth(1), Some("40 132"), "{}", r.out);
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
        let r = t.term(&["resize", "-s", "so", sizes[n % sizes.len()]]);
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
    assert!(both.contains("\"run_in_progress\":false") && both.contains("\"reason\":\"none\""), "{both}");

    // Errors are a code and a sentence.
    let resp = raw_http(socket, "GET /v1/nothing HTTP/1.1\r\nHost: waldo\r\nConnection: close\r\n\r\n");
    assert!(resp.starts_with("HTTP/1.1 404 "), "{resp}");
    assert!(resp.contains("\"error\":\"not_found\""), "{resp}");
    let resp = raw_http(socket, "POST /v1/input HTTP/1.1\r\nHost: waldo\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n");
    assert!(resp.starts_with("HTTP/1.1 411 "), "{resp}");
    let resp = raw_http(socket, "POST /v1/triggers HTTP/1.1\r\nHost: waldo\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}");
    assert!(resp.starts_with("HTTP/1.1 501 "), "{resp}");
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
    for topic in ["", "keys", "wait", "run", "messages", "commands", "exit-status"] {
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

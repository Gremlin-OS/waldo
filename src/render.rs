//! The text format: one status line, then the screen or output, then an
//! optional hint. It is what an agent reads, so it is kept short. Both the
//! host (for `?format=text`) and `waldo-term` use this.

use crate::util::seconds;
use serde_json::Value;

fn s<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(|x| x.as_str()).unwrap_or("")
}

fn n(v: &Value, key: &str) -> u64 {
    v.get(key).and_then(|x| x.as_u64()).unwrap_or(0)
}

/// How the child ended, from an exit object: `code=2`, `signal=SIGSEGV`.
fn exit_words(exit: &Value) -> String {
    let mut out = Vec::new();
    if let Some(e) = exit.get("spawn_error").and_then(|e| e.as_str()) {
        out.push(format!("spawn_error={e:?}"));
    } else if let Some(sig) = exit.get("signal").and_then(|x| x.as_str()) {
        out.push(format!("signal={sig}"));
        if exit.get("core_dumped").and_then(|c| c.as_bool()) == Some(true) {
            out.push("core_dumped".into());
        }
    } else if let Some(code) = exit.get("exit_code").and_then(|c| c.as_i64()) {
        out.push(format!("code={code}"));
    }
    if exit.get("killed_by_waldo").and_then(|k| k.as_bool()) == Some(true) {
        out.push("killed_by_waldo".into());
    }
    out.join(" ")
}

/// A response from the control API as text.
pub fn response(resp: &Value) -> String {
    let mut words: Vec<String> = vec![
        s(resp, "session").to_string(),
        s(resp, "reason").to_string(),
    ];
    if let Some(screen) = resp.get("screen") {
        words.push(format!("gen={}", n(screen, "gen")));
        words.push(format!("{}x{}", n(screen, "cols"), n(screen, "rows")));
        if let Some(c) = screen.get("cursor") {
            words.push(format!("cursor={},{}", n(c, "row"), n(c, "col")));
        }
        if screen.get("alt_screen").and_then(|a| a.as_bool()) == Some(true) {
            words.push("alt".into());
        }
    }
    let child = resp.get("child").cloned().unwrap_or(Value::Null);
    match s(&child, "state") {
        "running" => {
            words.push("running".into());
            let fg = s(&child, "foreground");
            if !fg.is_empty() {
                words.push(format!("fg={fg}"));
            }
        }
        "" => {}
        other => words.push(other.to_string()),
    }
    // The request typed something and nothing on screen changed.
    if resp.get("changed").and_then(|c| c.as_bool()) == Some(false) {
        words.push("unchanged".into());
    }
    // Someone else's input was typed during this request, so the screen
    // and a run's output may hold more than this request caused.
    if resp.get("interleaved").and_then(|i| i.as_bool()) == Some(true) {
        words.push("interleaved".into());
    }
    if let Some(exit) = resp.get("exit") {
        let w = exit_words(exit);
        if !w.is_empty() {
            words.push(w);
        }
    }
    let mut out = format!("[{}]\n", words.join(" "));
    let body = resp.get("screen").map(|sc| s(sc, "text")).unwrap_or("");
    if !body.is_empty() {
        out.push_str(body);
        out.push('\n');
    }
    if let Some(hint) = resp.get("hint").and_then(|h| h.as_str()) {
        out.push_str("hint: ");
        out.push_str(hint);
        out.push('\n');
    }
    out
}

/// An exit message (the will) as text: how the program ended, then its
/// output.
pub fn will(will: &Value) -> String {
    let mut words = vec![s(will, "session").to_string(), "exit".to_string()];
    let w = exit_words(will);
    if !w.is_empty() {
        words.push(w);
    }
    words.push(seconds(n(will, "duration_ms")));
    words.push("exited".into());
    let mut out = format!("[{}]\n", words.join(" "));
    let text = will.get("output").map(|o| s(o, "text")).unwrap_or("");
    if !text.is_empty() {
        out.push_str(text);
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn screen_response() {
        let r = json!({"v":1,"session":"k3f9q2","reason":"settle","waited_ms":212,
            "child":{"state":"running","pid":1,"foreground":"vim"},
            "screen":{"gen":41,"cols":80,"rows":24,"cursor":{"row":1,"col":19,"visible":true},
                      "alt_screen":true,"title":"","text":"#include <stdio.h>\n~"},
            "hint":"something"});
        assert_eq!(
            response(&r),
            "[k3f9q2 settle gen=41 80x24 cursor=1,19 alt running fg=vim]\n#include <stdio.h>\n~\nhint: something\n"
        );
        let mut ignored = r.clone();
        ignored["changed"] = false.into();
        assert!(response(&ignored).starts_with(
            "[k3f9q2 settle gen=41 80x24 cursor=1,19 alt running fg=vim unchanged]\n"
        ));
        ignored["changed"] = true.into();
        assert!(response(&ignored)
            .starts_with("[k3f9q2 settle gen=41 80x24 cursor=1,19 alt running fg=vim]\n"));
        let mut shared = r.clone();
        shared["interleaved"] = true.into();
        assert!(response(&shared).starts_with(
            "[k3f9q2 settle gen=41 80x24 cursor=1,19 alt running fg=vim interleaved]\n"
        ));
    }

    #[test]
    fn will_text() {
        let w = json!({"session":"a","trigger":"exit","exit_code":null,"signal":"SIGSEGV","core_dumped":true,
            "killed_by_waldo":false,"duration_ms":1250,"output":{"text":"boom"}});
        assert_eq!(
            will(&w),
            "[a exit signal=SIGSEGV core_dumped 1.2s exited]\nboom\n"
        );
    }
}

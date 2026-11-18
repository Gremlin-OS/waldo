//! The output buffer (DESIGN.md section 6.5): rendered lines that have left
//! the screen, bounded in memory. It keeps a head that fills once and a ring
//! of the most recent lines, so a capped result still shows how the output
//! began and how it ended.

use serde_json::{json, Value};
use std::collections::VecDeque;

#[derive(Clone)]
pub struct OutBuf {
    cap: usize,
    head: Vec<String>,
    head_bytes: usize,
    head_full: bool,
    tail: VecDeque<String>,
    tail_bytes: usize,
    total: u64,
    omitted: u64,
}

impl OutBuf {
    pub fn new(cap: usize) -> OutBuf {
        OutBuf {
            cap: cap.max(256),
            head: Vec::new(),
            head_bytes: 0,
            head_full: false,
            tail: VecDeque::new(),
            tail_bytes: 0,
            total: 0,
            omitted: 0,
        }
    }

    fn head_budget(&self) -> usize {
        self.cap / 4
    }

    fn tail_budget(&self) -> usize {
        self.cap - self.cap / 4
    }

    pub fn push(&mut self, line: String) {
        self.total += 1;
        let cost = line.len() + 1;
        if !self.head_full {
            if self.head_bytes + cost <= self.head_budget() {
                self.head_bytes += cost;
                self.head.push(line);
                return;
            }
            self.head_full = true;
        }
        let mut line = line;
        if line.len() + 1 > self.tail_budget() {
            // One line larger than the whole ring: keep its start.
            let mut cut = self.tail_budget().saturating_sub(16);
            while !line.is_char_boundary(cut) {
                cut -= 1;
            }
            line.truncate(cut);
            line.push_str(" [... cut ...]");
        }
        self.tail_bytes += line.len() + 1;
        self.tail.push_back(line);
        while self.tail_bytes > self.tail_budget() && self.tail.len() > 1 {
            if let Some(old) = self.tail.pop_front() {
                self.tail_bytes -= old.len() + 1;
                self.omitted += 1;
            }
        }
    }

    /// The buffer as an `output` object. `extra` lines are appended first (the
    /// lines still on screen). With `tail`, only that many final lines are
    /// returned.
    pub fn render(&self, extra: &[String], tail: Option<usize>) -> Value {
        let mut all = self.clone();
        for l in extra {
            all.push(l.clone());
        }
        let mut lines: Vec<&str> = all.head.iter().map(|s| s.as_str()).collect();
        let marker = format!("[... {} lines omitted ...]", all.omitted);
        if all.omitted > 0 {
            lines.push(&marker);
        }
        lines.extend(all.tail.iter().map(|s| s.as_str()));
        if let Some(n) = tail {
            let skip = lines.len().saturating_sub(n);
            lines.drain(..skip);
        }
        json!({
            "text": lines.join("\n"),
            "lines_total": all.total,
            "truncated": all.omitted > 0,
            "omitted_lines": all.omitted,
        })
    }
}

/// Cap a list of lines the same way the buffer does.
pub fn cap_lines(lines: Vec<String>, cap: usize) -> Value {
    let mut b = OutBuf::new(cap);
    for l in lines {
        b.push(l);
    }
    b.render(&[], None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_output_is_whole() {
        let v = cap_lines(vec!["a".into(), "b".into(), "c".into()], 1024);
        assert_eq!(v["text"], "a\nb\nc");
        assert_eq!(v["lines_total"], 3);
        assert_eq!(v["truncated"], false);
        assert_eq!(v["omitted_lines"], 0);
    }

    #[test]
    fn keeps_head_and_tail() {
        let lines: Vec<String> = (0..1000).map(|i| format!("line {i:04}")).collect();
        let v = cap_lines(lines, 1000);
        let text = v["text"].as_str().unwrap();
        assert!(text.len() <= 1100, "{}", text.len());
        assert!(text.starts_with("line 0000\nline 0001\n"));
        assert!(text.ends_with("line 0998\nline 0999"));
        let omitted = v["omitted_lines"].as_u64().unwrap();
        assert!(text.contains(&format!("[... {omitted} lines omitted ...]")));
        assert_eq!(v["lines_total"], 1000);
        assert_eq!(v["truncated"], true);
        let kept = text.lines().count() as u64 - 1;
        assert_eq!(kept + omitted, 1000);
    }

    #[test]
    fn tail_only() {
        let mut b = OutBuf::new(4096);
        for i in 0..10 {
            b.push(format!("l{i}"));
        }
        let v = b.render(&["screen".to_string()], Some(3));
        assert_eq!(v["text"], "l8\nl9\nscreen");
        assert_eq!(v["lines_total"], 11);
    }

    #[test]
    fn one_huge_line() {
        let mut b = OutBuf::new(1000);
        b.push("x".repeat(5000));
        let v = b.render(&[], None);
        assert!(v["text"].as_str().unwrap().len() < 1000);
    }
}

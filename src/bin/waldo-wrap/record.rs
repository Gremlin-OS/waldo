//! `--record FILE`: the program's raw output as an asciicast v2 file
//! (DESIGN.md section 3.4). It can be replayed with `asciinema play`.

use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::time::Instant;

pub struct Recorder {
    out: BufWriter<File>,
    start: Instant,
    /// The start of a character whose remaining bytes have not arrived.
    pending: Vec<u8>,
}

impl Recorder {
    /// Write the header. `timestamp` is seconds since the epoch.
    pub fn start(file: File, cols: u16, rows: u16, timestamp: i64, start: Instant) -> io::Result<Recorder> {
        let mut out = BufWriter::new(file);
        let header = serde_json::json!({
            "version": 2,
            "width": cols,
            "height": rows,
            "timestamp": timestamp,
            "env": {"TERM": "xterm-256color"},
        });
        writeln!(out, "{header}")?;
        out.flush()?;
        Ok(Recorder { out, start, pending: Vec::new() })
    }

    fn event(&mut self, code: &str, data: &str) -> io::Result<()> {
        let t = self.start.elapsed().as_secs_f64();
        writeln!(self.out, "[{t:.6}, {}, {}]", serde_json::Value::from(code), serde_json::Value::from(data))
    }

    /// Record bytes the program wrote. Events hold text, so a character
    /// split across two reads waits for its other half, and bytes that are
    /// not UTF-8 become U+FFFD.
    pub fn output(&mut self, data: &[u8]) -> io::Result<()> {
        let mut bytes = std::mem::take(&mut self.pending);
        bytes.extend_from_slice(data);
        let (text, rest) = split_utf8(&bytes);
        self.pending = rest.to_vec();
        if text.is_empty() {
            return Ok(());
        }
        self.event("o", &text)
    }

    pub fn resize(&mut self, cols: u16, rows: u16) -> io::Result<()> {
        self.event("r", &format!("{cols}x{rows}"))
    }

    pub fn flush(&mut self) -> io::Result<()> {
        self.out.flush()
    }
}

/// The text in `bytes`, and the incomplete character at its end, if any.
fn split_utf8(bytes: &[u8]) -> (String, &[u8]) {
    let mut text = String::new();
    let mut rest = bytes;
    loop {
        match std::str::from_utf8(rest) {
            Ok(s) => {
                text.push_str(s);
                return (text, &[]);
            }
            Err(e) => {
                let (good, bad) = rest.split_at(e.valid_up_to());
                text.push_str(std::str::from_utf8(good).unwrap_or(""));
                match e.error_len() {
                    // Bytes that can never be a character.
                    Some(n) => {
                        text.push('\u{FFFD}');
                        rest = &bad[n..];
                    }
                    // A character cut short by the end of the read.
                    None => return (text, bad),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_character_split_across_reads_is_kept_whole() {
        let e = "é".as_bytes();
        assert_eq!(split_utf8(b"abc"), ("abc".to_string(), &b""[..]));
        assert_eq!(split_utf8(&[b'a', e[0]]), ("a".to_string(), &e[..1]));
        assert_eq!(split_utf8(&[b'a', 0xff, b'b']), ("a\u{FFFD}b".to_string(), &b""[..]));
        let crab = "🦀".as_bytes();
        assert_eq!(split_utf8(&crab[..3]), (String::new(), &crab[..3]));
    }

    #[test]
    fn events_are_asciicast_lines() {
        let path = std::env::temp_dir().join(format!("waldo-rec-test-{}", std::process::id()));
        let file = File::create(&path).unwrap();
        let mut r = Recorder::start(file, 80, 24, 1_700_000_000, Instant::now()).unwrap();
        let e = "é".as_bytes();
        r.output(&[b'h', b'i', e[0]]).unwrap();
        r.output(&[e[1], b'\r', b'\n', 0x1b, b'[', b'm']).unwrap();
        r.resize(100, 30).unwrap();
        r.flush().unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        let lines: Vec<serde_json::Value> = text.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(lines[0]["version"], 2);
        assert_eq!(lines[0]["width"], 80);
        assert_eq!(lines[0]["height"], 24);
        assert_eq!(lines[1][1], "o");
        assert_eq!(lines[1][2], "hi");
        assert_eq!(lines[2][2], "é\r\n\u{1b}[m");
        assert_eq!(lines[3][1], "r");
        assert_eq!(lines[3][2], "100x30");
        assert!(lines[3][0].as_f64().unwrap() >= lines[1][0].as_f64().unwrap());
    }
}

//! The HTTP/1.1 subset served on the control socket (DESIGN.md section 2):
//! Content-Length bodies only, `Expect: 100-continue`, a 1 MiB body cap, and
//! keep-alive. Parsing is `httparse`; the sockets and the loop are ours.

use serde_json::{json, Value};

pub const MAX_BODY: usize = 1 << 20;
const MAX_HEAD: usize = 64 << 10;

#[derive(Debug, Default)]
pub struct Request {
    pub method: String,
    pub path: String,
    pub query: Vec<(String, String)>,
    pub json_body: bool,
    pub wants_text: bool,
    pub keep_alive: bool,
    pub upgrade: Option<String>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn query_get(&self, key: &str) -> Option<&str> {
        self.query
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }
}

pub enum Parsed {
    /// More bytes are needed.
    Incomplete,
    /// The client is waiting for `100 Continue` before sending its body.
    WantsContinue,
    /// A complete request, and how many bytes of the buffer it used.
    Ready(Request, usize),
    /// A request that cannot be served. The connection is closed after the
    /// reply.
    Bad(u16, &'static str, String),
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' => {
                let hex = b
                    .get(i + 1..i + 3)
                    .and_then(|h| std::str::from_utf8(h).ok());
                match hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                    Some(v) => {
                        out.push(v);
                        i += 2;
                    }
                    None => out.push(b'%'),
                }
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn parse_query(q: &str) -> Vec<(String, String)> {
    q.split('&')
        .filter(|p| !p.is_empty())
        .map(|p| match p.split_once('=') {
            Some((k, v)) => (percent_decode(k), percent_decode(v)),
            None => (percent_decode(p), String::new()),
        })
        .collect()
}

/// Try to parse one request from the start of `buf`.
pub fn parse(buf: &[u8], continue_sent: bool) -> Parsed {
    let mut headers = [httparse::EMPTY_HEADER; 48];
    let mut req = httparse::Request::new(&mut headers);
    let head_len = match req.parse(buf) {
        Ok(httparse::Status::Complete(n)) => n,
        Ok(httparse::Status::Partial) => {
            if buf.len() > MAX_HEAD {
                return Parsed::Bad(
                    431,
                    "headers_too_large",
                    "request headers are larger than 64 KiB".into(),
                );
            }
            return Parsed::Incomplete;
        }
        Err(e) => return Parsed::Bad(400, "bad_request", format!("not a valid HTTP request: {e}")),
    };

    let mut content_length: Option<usize> = None;
    let mut out = Request {
        keep_alive: req.version == Some(1),
        ..Default::default()
    };
    let mut expect_continue = false;
    for h in req.headers.iter() {
        let value = String::from_utf8_lossy(h.value);
        let value = value.trim();
        if h.name.eq_ignore_ascii_case("content-length") {
            match value.parse::<usize>() {
                Ok(n) => content_length = Some(n),
                Err(_) => {
                    return Parsed::Bad(400, "bad_request", "Content-Length is not a number".into())
                }
            }
        } else if h.name.eq_ignore_ascii_case("transfer-encoding") {
            return Parsed::Bad(
                411,
                "length_required",
                "chunked request bodies are not supported; send a Content-Length".into(),
            );
        } else if h.name.eq_ignore_ascii_case("content-type") {
            out.json_body = value.to_ascii_lowercase().starts_with("application/json");
        } else if h.name.eq_ignore_ascii_case("accept") {
            out.wants_text = value.to_ascii_lowercase().contains("text/plain");
        } else if h.name.eq_ignore_ascii_case("connection") {
            let v = value.to_ascii_lowercase();
            if v.contains("close") {
                out.keep_alive = false;
            } else if v.contains("keep-alive") {
                out.keep_alive = true;
            }
        } else if h.name.eq_ignore_ascii_case("expect") {
            expect_continue = value.eq_ignore_ascii_case("100-continue");
        } else if h.name.eq_ignore_ascii_case("upgrade") {
            out.upgrade = Some(value.to_ascii_lowercase());
        }
    }

    let body_len = content_length.unwrap_or(0);
    if body_len > MAX_BODY {
        return Parsed::Bad(
            413,
            "body_too_large",
            "request body is larger than 1 MiB; send long input in several requests".into(),
        );
    }
    if buf.len() < head_len + body_len {
        if expect_continue && !continue_sent {
            return Parsed::WantsContinue;
        }
        return Parsed::Incomplete;
    }

    out.method = req.method.unwrap_or("").to_string();
    let target = req.path.unwrap_or("/");
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    out.path = path.to_string();
    out.query = parse_query(query);
    if out.query_get("format") == Some("text") {
        out.wants_text = true;
    } else if out.query_get("format") == Some("json") {
        out.wants_text = false;
    }
    out.body = buf[head_len..head_len + body_len].to_vec();
    Parsed::Ready(out, head_len + body_len)
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        411 => "Length Required",
        413 => "Payload Too Large",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        _ => "Error",
    }
}

pub fn response(status: u16, content_type: &str, body: &[u8], keep_alive: bool) -> Vec<u8> {
    let mut out = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: {}\r\n\r\n",
        status,
        reason(status),
        content_type,
        body.len(),
        if keep_alive { "keep-alive" } else { "close" }
    )
    .into_bytes();
    out.extend_from_slice(body);
    out
}

pub fn json_response(status: u16, value: &Value, keep_alive: bool) -> Vec<u8> {
    let mut body = serde_json::to_vec(value).unwrap_or_default();
    body.push(b'\n');
    response(status, "application/json", &body, keep_alive)
}

/// An error body: a stable `error` code and a sentence for people that says
/// what to do next.
pub fn error_value(code: &str, text: &str) -> Value {
    json!({ "error": code, "text": text })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_with_query() {
        let raw = b"GET /v1/screen?until=settle&format=text&pattern=a%20b%2Bc+d HTTP/1.1\r\nHost: waldo\r\n\r\n";
        match parse(raw, false) {
            Parsed::Ready(r, n) => {
                assert_eq!(n, raw.len());
                assert_eq!(r.method, "GET");
                assert_eq!(r.path, "/v1/screen");
                assert_eq!(r.query_get("until"), Some("settle"));
                assert_eq!(r.query_get("pattern"), Some("a b+c d"));
                assert!(r.wants_text);
                assert!(r.keep_alive);
            }
            _ => panic!("not ready"),
        }
    }

    #[test]
    fn post_body_in_pieces() {
        let raw = b"POST /v1/input HTTP/1.1\r\nContent-Type: application/json\r\nContent-Length: 13\r\n\r\n{\"input\": []}";
        assert!(matches!(parse(&raw[..20], false), Parsed::Incomplete));
        assert!(matches!(
            parse(&raw[..raw.len() - 3], false),
            Parsed::Incomplete
        ));
        match parse(raw, false) {
            Parsed::Ready(r, _) => {
                assert!(r.json_body);
                assert_eq!(r.body, b"{\"input\": []}");
            }
            _ => panic!("not ready"),
        }
    }

    #[test]
    fn chunked_is_refused() {
        let raw = b"POST /v1/input HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n";
        assert!(matches!(
            parse(raw, false),
            Parsed::Bad(411, "length_required", _)
        ));
    }

    #[test]
    fn expect_continue() {
        let raw = b"POST /v1/input HTTP/1.1\r\nContent-Length: 5\r\nExpect: 100-continue\r\n\r\n";
        assert!(matches!(parse(raw, false), Parsed::WantsContinue));
        assert!(matches!(parse(raw, true), Parsed::Incomplete));
    }

    #[test]
    fn body_cap() {
        let raw = b"POST /v1/input HTTP/1.1\r\nContent-Length: 2000000\r\n\r\n";
        assert!(matches!(
            parse(raw, false),
            Parsed::Bad(413, "body_too_large", _)
        ));
    }

    #[test]
    fn decoding() {
        assert_eq!(percent_decode("a%3Cb%3e+c%"), "a<b> c%");
        assert_eq!(percent_decode("100%25"), "100%");
        assert_eq!(percent_decode("%zz"), "%zz");
    }
}

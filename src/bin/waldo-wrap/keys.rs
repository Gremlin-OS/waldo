//! Key notation (DESIGN.md section 2.1): vim-style `<Name>` tokens inside
//! otherwise literal text. Strict: a `<...>` that is not a known key is an
//! error, never literal text.

use crate::emu::{key, MOD_ALT, MOD_CTRL, MOD_SHIFT};

#[derive(Clone, Debug, PartialEq)]
pub enum Tok {
    /// Literal characters.
    Text(String),
    /// A named or modified key. `text` is what the key types with no
    /// modifier, for keys that type something.
    Key {
        key: i32,
        mods: u16,
        text: Option<char>,
        is_esc: bool,
    },
}

/// The key names accepted inside `<...>`, for help and error messages.
pub const KEY_NAMES: &str =
    "Esc Enter Tab BS Del Space Up Down Left Right Home End PageUp PageDown Insert F1..F12 lt";

fn named(name: &str) -> Option<(i32, Option<char>)> {
    let lower = name.to_ascii_lowercase();
    Some(match lower.as_str() {
        "esc" | "escape" => (key::ESCAPE, None),
        "enter" | "cr" | "return" => (key::ENTER, None),
        "tab" => (key::TAB, None),
        "bs" | "backspace" => (key::BACKSPACE, None),
        "del" | "delete" => (key::DELETE, None),
        "space" => (key::SPACE, Some(' ')),
        "up" => (key::ARROW_UP, None),
        "down" => (key::ARROW_DOWN, None),
        "left" => (key::ARROW_LEFT, None),
        "right" => (key::ARROW_RIGHT, None),
        "home" => (key::HOME, None),
        "end" => (key::END, None),
        "pageup" | "pgup" => (key::PAGE_UP, None),
        "pagedown" | "pgdn" => (key::PAGE_DOWN, None),
        "insert" | "ins" => (key::INSERT, None),
        "lt" => (key::COMMA, Some('<')),
        _ => {
            let num = lower.strip_prefix('f')?.parse::<i32>().ok()?;
            if !(1..=12).contains(&num) {
                return None;
            }
            (key::F1 + num - 1, None)
        }
    })
}

/// The physical key that types a character, on a US layout.
fn key_for_char(c: char) -> Option<i32> {
    Some(match c.to_ascii_lowercase() {
        l @ 'a'..='z' => key::A + (l as i32 - 'a' as i32),
        d @ '0'..='9' => key::DIGIT_0 + (d as i32 - '0' as i32),
        '`' | '~' => key::BACKQUOTE,
        '\\' | '|' => key::BACKSLASH,
        '[' | '{' => key::BRACKET_LEFT,
        ']' | '}' => key::BRACKET_RIGHT,
        ',' => key::COMMA,
        '=' | '+' => key::EQUAL,
        '-' | '_' => key::MINUS,
        '.' => key::PERIOD,
        '\'' | '"' => key::QUOTE,
        ';' | ':' => key::SEMICOLON,
        '/' | '?' => key::SLASH,
        ' ' => key::SPACE,
        _ => return None,
    })
}

/// Parse the inside of `<...>`. `None` if it is not a key.
fn parse_token(inner: &str) -> Option<Tok> {
    let mut mods = 0u16;
    let mut rest = inner;
    loop {
        let b = rest.as_bytes();
        if b.len() >= 3 && b[1] == b'-' {
            let m = match b[0].to_ascii_uppercase() {
                b'C' => MOD_CTRL,
                b'M' | b'A' => MOD_ALT,
                b'S' => MOD_SHIFT,
                _ => break,
            };
            mods |= m;
            rest = &rest[2..];
        } else {
            break;
        }
    }
    if rest.is_empty() {
        return None;
    }
    if let Some((k, text)) = named(rest) {
        if rest.eq_ignore_ascii_case("lt") && mods == 0 {
            return Some(Tok::Text("<".into()));
        }
        return Some(Tok::Key {
            key: k,
            mods,
            text,
            is_esc: k == key::ESCAPE && mods == 0,
        });
    }
    let mut chars = rest.chars();
    let c = chars.next()?;
    if chars.next().is_some() || mods == 0 {
        // A single character needs a modifier to be a key: `<x>` is not one.
        return None;
    }
    let k = key_for_char(c)?;
    Some(Tok::Key {
        key: k,
        mods,
        text: Some(c),
        is_esc: false,
    })
}

/// Parse key notation. On error returns a sentence that says what to do.
pub fn parse(input: &str) -> Result<Vec<Tok>, String> {
    let mut out: Vec<Tok> = Vec::new();
    let mut lit = String::new();
    let mut i = 0;
    let bytes = input.as_bytes();
    while i < input.len() {
        if bytes[i] != b'<' {
            let c = input[i..].chars().next().unwrap();
            lit.push(c);
            i += c.len_utf8();
            continue;
        }
        // A `<` starts a token if a `>` follows with no whitespace between.
        let rest = &input[i + 1..];
        let close = rest
            .char_indices()
            .take(40)
            .take_while(|(_, c)| !c.is_whitespace() && *c != '<')
            .find(|(_, c)| *c == '>')
            .map(|(j, _)| j);
        let Some(close) = close.filter(|&j| j > 0) else {
            lit.push('<');
            i += 1;
            continue;
        };
        let inner = &rest[..close];
        match parse_token(inner) {
            Some(Tok::Text(t)) => lit.push_str(&t),
            Some(tok) => {
                if !lit.is_empty() {
                    out.push(Tok::Text(std::mem::take(&mut lit)));
                }
                out.push(tok);
            }
            None => {
                return Err(format!(
                    "unknown key name <{inner}> at offset {i}; use <lt> for a literal <, or send literal text as a 'text' item (--text). Key names: {KEY_NAMES}; modifiers: <C-x> <M-x> <S-Tab>"
                ));
            }
        }
        i += close + 2;
    }
    if !lit.is_empty() {
        out.push(Tok::Text(lit));
    }
    Ok(out)
}

/// If literal text contains something that reads as a key name, return it.
/// Used for a hint: the caller probably meant `keys`.
pub fn looks_like_keys(text: &str) -> Option<String> {
    let mut i = 0;
    while let Some(off) = text[i..].find('<') {
        let start = i + off;
        let rest = &text[start + 1..];
        if let Some(close) = rest.find('>') {
            let inner = &rest[..close];
            if !inner.is_empty() && inner.len() <= 12 && !inner.eq_ignore_ascii_case("lt") {
                if let Some(Tok::Key { .. }) = parse_token(inner) {
                    return Some(format!("<{inner}>"));
                }
            }
        }
        i = start + 1;
    }
    None
}

/// The traditional encoding of a key, for when the emulator's encoder has
/// none: Ctrl masks the character, Alt prefixes ESC.
pub fn legacy(mods: u16, text: Option<char>) -> Vec<u8> {
    let Some(c) = text else { return Vec::new() };
    let mut out = Vec::new();
    if mods & MOD_ALT != 0 {
        out.push(0x1b);
    }
    if mods & MOD_CTRL != 0 {
        let u = c.to_ascii_uppercase();
        match u {
            '@'..='_' => out.push(u as u8 & 0x1f),
            ' ' | '2' => out.push(0),
            '?' => out.push(0x7f),
            _ => out.extend_from_slice(c.to_string().as_bytes()),
        }
    } else {
        out.extend_from_slice(c.to_string().as_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k(key: i32, mods: u16, text: Option<char>) -> Tok {
        Tok::Key {
            key,
            mods,
            text,
            is_esc: key == key::ESCAPE && mods == 0,
        }
    }

    #[test]
    fn literal_and_keys() {
        assert_eq!(
            parse("<Esc>ggO").unwrap(),
            vec![k(key::ESCAPE, 0, None), Tok::Text("ggO".into())]
        );
        assert_eq!(
            parse(":q!<Enter>").unwrap(),
            vec![Tok::Text(":q!".into()), k(key::ENTER, 0, None)]
        );
        assert_eq!(
            parse("<C-c>").unwrap(),
            vec![k(key::A + 2, MOD_CTRL, Some('c'))]
        );
        assert_eq!(
            parse("<M-x>").unwrap(),
            vec![k(key::A + 23, MOD_ALT, Some('x'))]
        );
        assert_eq!(
            parse("<S-Tab>").unwrap(),
            vec![k(key::TAB, MOD_SHIFT, None)]
        );
        assert_eq!(
            parse("<C-S-Left>").unwrap(),
            vec![k(key::ARROW_LEFT, MOD_CTRL | MOD_SHIFT, None)]
        );
        assert_eq!(
            parse("<F5><f12>").unwrap(),
            vec![k(key::F1 + 4, 0, None), k(key::F1 + 11, 0, None)]
        );
    }

    #[test]
    fn less_than() {
        assert_eq!(parse("a<lt>b").unwrap(), vec![Tok::Text("a<b".into())]);
        // A `<` that cannot be a token is literal.
        assert_eq!(parse("a < b").unwrap(), vec![Tok::Text("a < b".into())]);
        assert_eq!(parse("x <").unwrap(), vec![Tok::Text("x <".into())]);
        assert_eq!(parse("a <> b").unwrap(), vec![Tok::Text("a <> b".into())]);
    }

    #[test]
    fn unknown_names_are_errors() {
        let e = parse("abc <Entr>").unwrap_err();
        assert!(
            e.contains("<Entr>") && e.contains("offset 4") && e.contains("<lt>"),
            "{e}"
        );
        assert!(parse("#include <stdio.h>").is_err());
        assert!(parse("<x>").is_err());
        assert!(parse("<F13>").is_err());
    }

    #[test]
    fn hint_detection() {
        assert_eq!(looks_like_keys("foo<Esc>:wq"), Some("<Esc>".into()));
        assert_eq!(looks_like_keys("#include <stdio.h>"), None);
        assert_eq!(looks_like_keys("a < b > c"), None);
        assert_eq!(looks_like_keys("<C-c>"), Some("<C-c>".into()));
    }

    #[test]
    fn legacy_encoding() {
        assert_eq!(legacy(MOD_CTRL, Some('c')), vec![3]);
        assert_eq!(legacy(MOD_CTRL, Some('[')), vec![0x1b]);
        assert_eq!(legacy(MOD_CTRL, Some(' ')), vec![0]);
        assert_eq!(legacy(MOD_ALT, Some('x')), vec![0x1b, b'x']);
    }
}

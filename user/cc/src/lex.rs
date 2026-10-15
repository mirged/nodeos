//! A C tokenizer.
//!
//! Only the token shapes this compiler's subset needs: identifiers and
//! keywords, integer and character constants, string literals, and the
//! operator set. Comments and preprocessor lines are skipped -- `#include` is
//! accepted and ignored, because the standard library is built in and there is
//! nothing to include.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

/// One token.
#[derive(Clone, Debug, PartialEq)]
pub enum Tok {
    /// An identifier or keyword; keywords are recognised by the parser.
    Ident(String),
    /// An integer constant.
    Num(i64),
    /// A string literal, NUL-terminated.
    Str(Vec<u8>),
    /// An operator or punctuation mark.
    Punct(&'static str),
    /// End of input.
    Eof,
}

/// Tokenize a C source file.
pub fn lex(src: &[u8]) -> Result<Vec<Tok>, String> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < src.len() {
        let c = src[i];

        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        // Preprocessor lines are skipped whole.
        if c == b'#' {
            while i < src.len() && src[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if c == b'/' && i + 1 < src.len() && src[i + 1] == b'/' {
            while i < src.len() && src[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if c == b'/' && i + 1 < src.len() && src[i + 1] == b'*' {
            i += 2;
            while i + 1 < src.len() && !(src[i] == b'*' && src[i + 1] == b'/') {
                i += 1;
            }
            i = (i + 2).min(src.len());
            continue;
        }

        if c == b'_' || c.is_ascii_alphabetic() {
            let start = i;
            while i < src.len() && (src[i] == b'_' || src[i].is_ascii_alphanumeric()) {
                i += 1;
            }
            out.push(Tok::Ident(String::from_utf8_lossy(&src[start..i]).into_owned()));
            continue;
        }

        if c.is_ascii_digit() {
            let mut value: i64 = 0;
            if c == b'0' && i + 1 < src.len() && (src[i + 1] | 0x20) == b'x' {
                i += 2;
                while i < src.len() && src[i].is_ascii_hexdigit() {
                    value = value * 16 + (src[i] as char).to_digit(16).unwrap_or(0) as i64;
                    i += 1;
                }
            } else {
                while i < src.len() && src[i].is_ascii_digit() {
                    value = value * 10 + (src[i] - b'0') as i64;
                    i += 1;
                }
            }
            // Integer suffixes are accepted and dropped.
            while i < src.len() && matches!(src[i], b'u' | b'U' | b'l' | b'L') {
                i += 1;
            }
            out.push(Tok::Num(value));
            continue;
        }

        if c == b'\'' {
            i += 1;
            let value = if i < src.len() && src[i] == b'\\' {
                i += 1;
                let value = escape(*src.get(i).unwrap_or(&0));
                i += 1;
                value
            } else {
                let value = *src.get(i).unwrap_or(&0) as i64;
                i += 1;
                value
            };
            if i < src.len() && src[i] == b'\'' {
                i += 1;
            }
            out.push(Tok::Num(value));
            continue;
        }

        if c == b'"' {
            i += 1;
            let mut bytes = Vec::new();
            while i < src.len() && src[i] != b'"' {
                if src[i] == b'\\' {
                    i += 1;
                    bytes.push(escape(*src.get(i).unwrap_or(&0)) as u8);
                    i += 1;
                } else {
                    bytes.push(src[i]);
                    i += 1;
                }
            }
            if i < src.len() {
                i += 1;
            }
            bytes.push(0);
            out.push(Tok::Str(bytes));
            continue;
        }

        let rest = &src[i..];
        let punct = PUNCT3
            .iter()
            .chain(PUNCT2.iter())
            .chain(PUNCT1.iter())
            .find(|p| rest.starts_with(p.as_bytes()));
        let Some(punct) = punct else {
            return Err(format!("unexpected byte {:#x}", c));
        };
        i += punct.len();
        out.push(Tok::Punct(punct));
    }
    out.push(Tok::Eof);
    Ok(out)
}

/// The value of a backslash escape.
fn escape(c: u8) -> i64 {
    match c {
        b'n' => 10,
        b't' => 9,
        b'r' => 13,
        b'0' => 0,
        b'a' => 7,
        b'b' => 8,
        b'f' => 12,
        b'v' => 11,
        b'\\' => 92,
        b'\'' => 39,
        b'"' => 34,
        other => other as i64,
    }
}

const PUNCT3: &[&str] = &[">>=", "<<="];
const PUNCT2: &[&str] = &[
    "==", "!=", "<=", ">=", "&&", "||", "++", "--", "+=", "-=", "*=", "/=", "%=", "&=", "|=", "^=", "<<",
    ">>", "->",
];
const PUNCT1: &[&str] = &[
    "+", "-", "*", "/", "%", "=", "<", ">", "!", "~", "&", "|", "^", "?", ":", ";", ",", "(", ")", "{", "}",
    "[", "]", ".",
];



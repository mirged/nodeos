//! Syntax highlighting, one line at a time.
//!
//! A highlighter is a small lexer that classifies every character of a line.
//! Constructs that span lines -- Maylang's nested block comments and
//! multi-line strings, C's block comments -- are carried in a [`State`] from
//! one line to the next, so the editor can colour any visible line after
//! running the lines above it. Nothing here allocates more than the line's
//! class vector, and nothing depends on NodeOS, so it is tested on the host.

extern crate alloc;
use alloc::vec::Vec;

/// The language of a document, chosen from its file name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lang {
    Plain,
    Maylang,
    C,
    Lua,
    Shell,
}

impl Lang {
    pub fn for_path(path: &str) -> Self {
        let lower = path.to_ascii_lowercase();
        let ext = lower.rsplit_once('.').map(|(_, ext)| ext).unwrap_or("");
        match ext {
            "may" => Self::Maylang,
            "c" | "h" | "rs" | "cpp" | "js" => Self::C,
            "lua" => Self::Lua,
            "nsh" | "sh" => Self::Shell,
            _ => Self::Plain,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Plain => "Text",
            Self::Maylang => "Maylang",
            Self::C => "C",
            Self::Lua => "Lua",
            Self::Shell => "Shell",
        }
    }

    /// What starts a line comment, for toggling comments.
    pub fn line_comment(self) -> Option<&'static str> {
        match self {
            Self::Maylang | Self::C => Some("//"),
            Self::Lua => Some("--"),
            Self::Shell => Some("#"),
            Self::Plain => None,
        }
    }

    fn keywords(self) -> &'static [&'static str] {
        match self {
            Self::Maylang => &[
                "fun", "let", "mut", "if", "else", "while", "return", "break", "continue", "and", "or",
                "not", "for", "in", "unless", "match", "may", "otherwise", "struct", "enum", "pub",
                "extern", "import", "from", "as",
            ],
            Self::C => &[
                "auto", "break", "case", "char", "const", "continue", "default", "do", "double", "else",
                "enum", "extern", "float", "for", "goto", "if", "int", "long", "register", "return",
                "short", "signed", "sizeof", "static", "struct", "switch", "typedef", "union",
                "unsigned", "void", "volatile", "while", "fn", "let", "mut", "pub", "use", "impl",
                "match", "loop", "mod", "self", "Self", "trait", "where", "unsafe", "as", "in",
            ],
            Self::Lua => &[
                "and", "break", "do", "else", "elseif", "end", "for", "function", "goto", "if", "in",
                "local", "not", "or", "repeat", "return", "then", "until", "while",
            ],
            Self::Shell => &[
                "cd", "ls", "cat", "echo", "export", "unset", "source", "run", "cp", "mv", "rm",
                "mkdir", "write", "append", "grep", "head", "tail", "wc", "find", "true", "false",
            ],
            Self::Plain => &[],
        }
    }

    fn constants(self) -> &'static [&'static str] {
        match self {
            Self::Maylang => &["true", "false", "nil"],
            Self::C => &["true", "false", "NULL", "None", "Some", "Ok", "Err"],
            Self::Lua => &["true", "false", "nil"],
            _ => &[],
        }
    }
}

/// What a character is, for choosing its colour.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    Normal,
    Keyword,
    Type,
    Constant,
    Function,
    String,
    Escape,
    Number,
    Comment,
}

/// What an unfinished line leaves open for the next one.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct State {
    /// Depth of open block comments (Maylang nests them; C does not).
    comment: u32,
    /// Inside a string that continues onto the next line (Maylang).
    string: bool,
    /// Braces open inside a `${...}` interpolation of that string.
    interpolation: u32,
}

/// Classify every character of `line`, continuing from `state`.
pub fn classify(lang: Lang, line: &str, state: &mut State) -> Vec<Class> {
    let chars: Vec<char> = line.chars().collect();
    let mut out = alloc::vec![Class::Normal; chars.len()];
    if lang == Lang::Plain {
        return out;
    }
    let block = matches!(lang, Lang::Maylang | Lang::C);
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        // Inside a block comment.
        if state.comment > 0 {
            out[i] = Class::Comment;
            if c == '*' && next == Some('/') {
                out[i + 1] = Class::Comment;
                state.comment -= 1;
                i += 2;
                continue;
            }
            if lang == Lang::Maylang && c == '/' && next == Some('*') {
                out[i + 1] = Class::Comment;
                state.comment += 1;
                i += 2;
                continue;
            }
            i += 1;
            continue;
        }
        // Inside a string, possibly one that began on an earlier line.
        if state.string && state.interpolation == 0 {
            out[i] = Class::String;
            match c {
                '\\' => {
                    out[i] = Class::Escape;
                    if i + 1 < chars.len() {
                        out[i + 1] = Class::Escape;
                    }
                    i += 2;
                }
                '"' => {
                    state.string = false;
                    i += 1;
                }
                '$' if lang == Lang::Maylang && next == Some('{') => {
                    out[i] = Class::Escape;
                    out[i + 1] = Class::Escape;
                    state.interpolation = 1;
                    i += 2;
                }
                _ => i += 1,
            }
            continue;
        }
        if state.string && state.interpolation > 0 {
            // Code inside `${...}`; its closing brace returns to the string.
            if c == '{' {
                state.interpolation += 1;
            } else if c == '}' {
                state.interpolation -= 1;
                if state.interpolation == 0 {
                    out[i] = Class::Escape;
                    i += 1;
                    continue;
                }
            }
        }
        // Line comments.
        let line_comment = match lang {
            Lang::Maylang | Lang::C => c == '/' && next == Some('/'),
            Lang::Lua => c == '-' && next == Some('-'),
            Lang::Shell => c == '#' && (i == 0 || chars[i - 1].is_whitespace()),
            Lang::Plain => false,
        };
        if line_comment || (lang == Lang::C && c == '#' && chars[..i].iter().all(|c| c.is_whitespace())) {
            let class = if line_comment { Class::Comment } else { Class::Keyword };
            for slot in &mut out[i..] {
                *slot = class;
            }
            break;
        }
        if block && c == '/' && next == Some('*') {
            out[i] = Class::Comment;
            out[i + 1] = Class::Comment;
            state.comment = 1;
            i += 2;
            continue;
        }
        // Strings. Maylang's may span lines; the others end with the line.
        if c == '"' || (c == '\'' && lang != Lang::Maylang) {
            let quote = c;
            out[i] = Class::String;
            i += 1;
            if lang == Lang::Maylang && state.interpolation == 0 {
                state.string = true;
                continue;
            }
            while i < chars.len() {
                out[i] = Class::String;
                if chars[i] == '\\' {
                    out[i] = Class::Escape;
                    if i + 1 < chars.len() {
                        out[i + 1] = Class::Escape;
                    }
                    i += 2;
                    continue;
                }
                i += 1;
                if chars[i - 1] == quote {
                    break;
                }
            }
            continue;
        }
        if c.is_ascii_digit() {
            let start = i;
            while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '_' || chars[i] == '.') {
                // A range `0..n` is two numbers, not one.
                if chars[i] == '.' && chars.get(i + 1) == Some(&'.') {
                    break;
                }
                i += 1;
            }
            for slot in &mut out[start..i] {
                *slot = Class::Number;
            }
            continue;
        }
        if c.is_alphabetic() || c == '_' {
            let start = i;
            while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            let word: alloc::string::String = chars[start..i].iter().collect();
            let mut after = i;
            while after < chars.len() && chars[after] == ' ' {
                after += 1;
            }
            let class = if lang.keywords().contains(&word.as_str()) {
                Class::Keyword
            } else if lang.constants().contains(&word.as_str()) {
                Class::Constant
            } else if chars.get(after) == Some(&'(') {
                Class::Function
            } else if lang != Lang::Shell && word.starts_with(|c: char| c.is_ascii_uppercase()) {
                Class::Type
            } else {
                Class::Normal
            };
            for slot in &mut out[start..i] {
                *slot = class;
            }
            continue;
        }
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(lang: Lang, lines: &[&str]) -> Vec<Vec<Class>> {
        let mut state = State::default();
        lines.iter().map(|line| classify(lang, line, &mut state)).collect()
    }

    #[test]
    fn maylang_tokens() {
        let classes = &run(Lang::Maylang, &["fun f(x: Int) -> Str { return \"a\\n\"; } // c"])[0];
        let text: Vec<char> = "fun f(x: Int) -> Str { return \"a\\n\"; } // c".chars().collect();
        let at = |needle: &str| {
            let s: alloc::string::String = text.iter().collect();
            s.find(needle).unwrap()
        };
        assert_eq!(classes[0], Class::Keyword);
        assert_eq!(classes[at("f(")], Class::Function);
        assert_eq!(classes[at("Int")], Class::Type);
        assert_eq!(classes[at("return")], Class::Keyword);
        assert_eq!(classes[at("\"a")], Class::String);
        assert_eq!(classes[at("\\n")], Class::Escape);
        assert_eq!(classes[at("// c")], Class::Comment);
    }

    #[test]
    fn nested_comments_and_multiline_strings_carry_over() {
        let classes = run(Lang::Maylang, &["/* a /* b */ still", "end */ let", "let s = \"one", "two ${x} three\";"]);
        assert!(classes[0].iter().all(|c| *c == Class::Comment));
        assert_eq!(classes[1][0], Class::Comment);
        assert_eq!(*classes[1].last().unwrap(), Class::Keyword);
        assert_eq!(*classes[2].last().unwrap(), Class::String);
        assert_eq!(classes[3][0], Class::String);
        assert_eq!(classes[3][4], Class::Escape); // $
        assert_eq!(classes[3][6], Class::Normal); // x
        assert_eq!(classes[3][7], Class::Escape); // }
        assert_eq!(classes[3][9], Class::String);
    }

    #[test]
    fn numbers_ranges_and_constants() {
        let classes = &run(Lang::Maylang, &["for i in 0..10 { x = true; }"])[0];
        assert_eq!(classes[9], Class::Number);
        assert_eq!(classes[10], Class::Normal);
        assert_eq!(classes[12], Class::Number);
        assert_eq!(classes[22], Class::Constant);
    }

    #[test]
    fn other_languages() {
        assert_eq!(run(Lang::C, &["#include <stdio.h>"])[0][0], Class::Keyword);
        assert_eq!(run(Lang::Lua, &["local x = 1 -- note"])[0][12], Class::Comment);
        assert_eq!(run(Lang::Shell, &["echo a#b # note"])[0][6], Class::Normal);
        assert_eq!(run(Lang::Shell, &["echo a#b # note"])[0][9], Class::Comment);
        assert_eq!(Lang::for_path("/src/MAIN.MAY"), Lang::Maylang);
        assert_eq!(Lang::for_path("/notes.txt"), Lang::Plain);
    }
}

//! Parse shell words and command files before any commands are executed.
extern crate alloc;
use alloc::{string::String, vec::Vec};
/// Quotes and escaping preserve empty arguments and literal separators.
/// Newlines and semicolons separate commands; comments start at word boundaries.
pub fn parse(input: &str) -> Result<Vec<Vec<String>>, &'static str> {
    let mut commands = Vec::new();
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quote = None;
    let mut active = false;
    let mut comment = false;
    let mut chars = input.chars();
    while let Some(ch) = chars.next() {
        if comment {
            if ch != '\n' {
                continue;
            }
            comment = false;
        }
        if quote == Some('\'') {
            if ch == '\'' {
                quote = None
            } else {
                word.push(ch)
            }
            continue;
        }
        if ch == '\\' {
            let Some(next) = chars.next() else {
                return Err("unfinished escape");
            };
            if next != '\n' {
                word.push(if quote == Some('"') {
                    match next {
                        'n' => '\n',
                        't' => '\t',
                        other => other,
                    }
                } else {
                    next
                });
                active = true;
            }
            continue;
        }
        if quote == Some('"') {
            if ch == '"' {
                quote = None
            } else {
                word.push(ch)
            }
            continue;
        }
        match ch {
            '\'' | '"' => {
                quote = Some(ch);
                active = true;
            }
            '#' => {
                if !active {
                    comment = true
                } else {
                    word.push(ch)
                }
            }
            ';' | '\n' => {
                if active {
                    words.push(core::mem::take(&mut word));
                    active = false;
                }
                if !words.is_empty() {
                    commands.push(core::mem::take(&mut words));
                }
            }
            ch if ch.is_whitespace() => {
                if active {
                    words.push(core::mem::take(&mut word));
                    active = false;
                }
            }
            '|' | '&' | '>' | '<' => {
                return Err("operators | & < > are not supported; quote them for literal text")
            }
            _ => {
                word.push(ch);
                active = true;
            }
        }
    }
    if quote.is_some() {
        return Err("unclosed quote");
    }
    if active {
        words.push(word)
    }
    if !words.is_empty() {
        commands.push(words)
    }
    Ok(commands)
}
#[cfg(test)]
mod tests {
    use super::parse;
    #[test]
    fn quotes_and_empty_words() {
        assert_eq!(
            parse("echo one 'two three' \"\" pre\"fix\" escaped\\ word").unwrap()[0],
            ["echo", "one", "two three", "", "prefix", "escaped word"]
        );
    }
    #[test]
    fn scripts_comments_and_separators() {
        assert_eq!(
            parse("# comment\necho 'a;b'; echo x#y # end\necho \"a\\nb\"\n").unwrap(),
            vec![
                vec!["echo", "a;b"],
                vec!["echo", "x#y"],
                vec!["echo", "a\nb"]
            ]
        );
    }
    #[test]
    fn failures_are_not_partial_commands() {
        for text in [
            "echo ok; echo '",
            "echo \\",
            "echo x | cat",
            "echo x > y",
            "echo x && echo y",
        ] {
            assert!(parse(text).is_err(), "{text}");
        }
    }
    #[test]
    fn unicode_and_literal_operators() {
        assert_eq!(
            parse("echo 'λ > α'\\\n x").unwrap()[0],
            ["echo", "λ > α", "x"]
        );
    }
}

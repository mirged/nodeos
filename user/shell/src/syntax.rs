//! Parse shell words and command files before any commands are executed.
//!
//! A line is a list of commands joined by `;`, newlines, `&&` or `||`. A
//! command is words plus redirections (`> file`, `>> file`, `< file`). Words
//! keep their variable references (`$NAME`, `${NAME}`, `$?`) unexpanded,
//! because an earlier command on the same line may set them: expansion happens
//! when each command runs. Expansion never splits a word, so a value with
//! spaces stays one argument.
extern crate alloc;
use alloc::{string::String, vec::Vec};

/// Part of a word: literal text, or a variable to substitute when it runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Piece {
    Text(String),
    Var(String),
}

/// One word, as pieces. A quoted empty string is a word with no text.
pub type Word = Vec<Piece>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RedirectKind {
    /// `> file`: replace the file with the output.
    Out,
    /// `>> file`: add the output to the end of the file.
    Append,
    /// `< file`: read input from the file.
    In,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Redirect {
    pub kind: RedirectKind,
    pub target: Word,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Command {
    pub words: Vec<Word>,
    pub redirects: Vec<Redirect>,
}

/// When a command runs, given the status of the one before it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Connector {
    /// First on a line, or after `;` or a newline.
    Always,
    /// After `&&`: only if the previous command succeeded.
    IfSuccess,
    /// After `||`: only if the previous command failed.
    IfFailure,
}

struct Parser {
    commands: Vec<(Connector, Command)>,
    connector: Connector,
    command: Command,
    word: Word,
    text: String,
    active: bool,
    redirect: Option<RedirectKind>,
}

impl Parser {
    /// Close the text run into the current word.
    fn flush_text(&mut self) {
        if !self.text.is_empty() {
            self.word.push(Piece::Text(core::mem::take(&mut self.text)));
        }
    }

    /// Close the current word, as an argument or a pending redirection target.
    fn end_word(&mut self) {
        if !self.active {
            return;
        }
        self.flush_text();
        let word = core::mem::take(&mut self.word);
        match self.redirect.take() {
            Some(kind) => self.command.redirects.push(Redirect { kind, target: word }),
            None => self.command.words.push(word),
        }
        self.active = false;
    }

    /// Close the current command, which the next one follows by `next`.
    fn end_command(&mut self, next: Connector) -> Result<(), &'static str> {
        self.end_word();
        if self.redirect.is_some() {
            return Err("a redirection needs a file name");
        }
        let command = core::mem::take(&mut self.command);
        if command.words.is_empty() {
            if !command.redirects.is_empty() {
                return Err("a redirection needs a command");
            }
            if next != Connector::Always || self.connector != Connector::Always {
                return Err("&& and || need a command on each side");
            }
        } else {
            self.commands.push((self.connector, command));
        }
        self.connector = next;
        Ok(())
    }

    fn start_redirect(&mut self, kind: RedirectKind) -> Result<(), &'static str> {
        self.end_word();
        if self.redirect.is_some() {
            return Err("a redirection needs a file name");
        }
        self.redirect = Some(kind);
        Ok(())
    }
}

/// Read a variable name after `$`, if one follows.
fn variable(chars: &mut core::iter::Peekable<core::str::Chars<'_>>) -> Result<Option<String>, &'static str> {
    match chars.peek().copied() {
        Some('?') => {
            chars.next();
            Ok(Some(String::from("?")))
        }
        Some('{') => {
            chars.next();
            let mut name = String::new();
            loop {
                match chars.next() {
                    Some('}') if !name.is_empty() => return Ok(Some(name)),
                    Some(ch) if ch == '_' || ch.is_ascii_alphanumeric() => name.push(ch),
                    _ => return Err("bad ${...} variable"),
                }
            }
        }
        Some(ch) if ch == '_' || ch.is_ascii_alphabetic() => {
            let mut name = String::new();
            while let Some(&ch) = chars.peek() {
                if ch == '_' || ch.is_ascii_alphanumeric() {
                    name.push(ch);
                    chars.next();
                } else {
                    break;
                }
            }
            Ok(Some(name))
        }
        _ => Ok(None),
    }
}

/// Quotes and escaping preserve empty arguments and literal separators.
/// Newlines and semicolons separate commands; comments start at word boundaries.
pub fn parse(input: &str) -> Result<Vec<(Connector, Command)>, &'static str> {
    let mut p = Parser {
        commands: Vec::new(),
        connector: Connector::Always,
        command: Command::default(),
        word: Word::new(),
        text: String::new(),
        active: false,
        redirect: None,
    };
    let mut quote = None;
    let mut comment = false;
    let mut chars = input.chars().peekable();
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
                p.text.push(ch)
            }
            continue;
        }
        if ch == '\\' {
            let Some(next) = chars.next() else {
                return Err("unfinished escape");
            };
            if next != '\n' {
                p.text.push(if quote == Some('"') {
                    match next {
                        'n' => '\n',
                        't' => '\t',
                        other => other,
                    }
                } else {
                    next
                });
                p.active = true;
            }
            continue;
        }
        if ch == '$' {
            match variable(&mut chars)? {
                Some(name) => {
                    p.flush_text();
                    p.word.push(Piece::Var(name));
                }
                None => p.text.push('$'),
            }
            p.active = true;
            continue;
        }
        if quote == Some('"') {
            if ch == '"' {
                quote = None
            } else {
                p.text.push(ch)
            }
            continue;
        }
        match ch {
            '\'' | '"' => {
                quote = Some(ch);
                p.active = true;
            }
            '#' => {
                if !p.active {
                    comment = true
                } else {
                    p.text.push(ch)
                }
            }
            ';' | '\n' => p.end_command(Connector::Always)?,
            ch if ch.is_whitespace() => p.end_word(),
            '&' => {
                if chars.next_if_eq(&'&').is_none() {
                    return Err("background jobs (&) are not supported; use && to chain commands");
                }
                p.end_command(Connector::IfSuccess)?;
            }
            '|' => {
                if chars.next_if_eq(&'|').is_none() {
                    return Err("pipes (|) are not supported; use > and < with a file");
                }
                p.end_command(Connector::IfFailure)?;
            }
            '>' => {
                let kind = if chars.next_if_eq(&'>').is_some() { RedirectKind::Append } else { RedirectKind::Out };
                p.start_redirect(kind)?;
            }
            '<' => p.start_redirect(RedirectKind::In)?,
            _ => {
                p.text.push(ch);
                p.active = true;
            }
        }
    }
    if quote.is_some() {
        return Err("unclosed quote");
    }
    p.end_command(Connector::Always)?;
    Ok(p.commands)
}

/// A word's text with no variables substituted; for words known to be literal.
#[cfg(test)]
fn literal(word: &Word) -> String {
    word.iter()
        .map(|piece| match piece {
            Piece::Text(text) => text.clone(),
            Piece::Var(name) => alloc::format!("${{{name}}}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(input: &str) -> Vec<Vec<String>> {
        parse(input)
            .unwrap()
            .iter()
            .map(|(_, command)| command.words.iter().map(literal).collect())
            .collect()
    }

    #[test]
    fn quotes_and_empty_words() {
        assert_eq!(
            words("echo one 'two three' \"\" pre\"fix\" escaped\\ word")[0],
            ["echo", "one", "two three", "", "prefix", "escaped word"]
        );
    }

    #[test]
    fn scripts_comments_and_separators() {
        assert_eq!(
            words("# comment\necho 'a;b'; echo x#y # end\necho \"a\\nb\"\n"),
            vec![vec!["echo", "a;b"], vec!["echo", "x#y"], vec!["echo", "a\nb"]]
        );
    }

    #[test]
    fn failures_are_not_partial_commands() {
        for text in [
            "echo ok; echo '",
            "echo \\",
            "echo x | cat",
            "echo x &",
            "echo x >",
            "> file",
            "&& echo y",
            "echo x &&",
            "echo x || || echo y",
            "echo ${bad name}",
        ] {
            assert!(parse(text).is_err(), "{text}");
        }
    }

    #[test]
    fn unicode_and_literal_operators() {
        assert_eq!(words("echo 'λ > α'\\\n x")[0], ["echo", "λ > α", "x"]);
        assert_eq!(words("echo a\\>b '$HOME' \"&&\"")[0], ["echo", "a>b", "$HOME", "&&"]);
    }

    #[test]
    fn connectors() {
        let commands = parse("true && echo yes || echo no; echo end").unwrap();
        let connectors: Vec<Connector> = commands.iter().map(|(c, _)| *c).collect();
        assert_eq!(
            connectors,
            [Connector::Always, Connector::IfSuccess, Connector::IfFailure, Connector::Always]
        );
    }

    #[test]
    fn redirections() {
        let commands = parse("echo hi >out.txt >> \"log file\" < in").unwrap();
        let command = &commands[0].1;
        assert_eq!(command.words.iter().map(literal).collect::<Vec<_>>(), ["echo", "hi"]);
        let targets: Vec<(RedirectKind, String)> =
            command.redirects.iter().map(|r| (r.kind, literal(&r.target))).collect();
        assert_eq!(
            targets,
            [
                (RedirectKind::Out, String::from("out.txt")),
                (RedirectKind::Append, String::from("log file")),
                (RedirectKind::In, String::from("in")),
            ]
        );
    }

    #[test]
    fn variables_stay_unexpanded_and_unsplit() {
        let commands = parse("echo $HOME/x \"${A}b\" $? $ 'q$HOME'").unwrap();
        assert_eq!(
            commands[0].1.words,
            vec![
                vec![Piece::Text("echo".into())],
                vec![Piece::Var("HOME".into()), Piece::Text("/x".into())],
                vec![Piece::Var("A".into()), Piece::Text("b".into())],
                vec![Piece::Var("?".into())],
                vec![Piece::Text("$".into())],
                vec![Piece::Text("q$HOME".into())],
            ]
        );
    }
}

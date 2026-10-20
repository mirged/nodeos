//! The NodeOS shell: text commands, command files and foreground programs.
#![no_std]
#![no_main]
extern crate alloc;
mod files;
mod lineedit;
mod syntax;
use alloc::{format, string::String, vec::Vec};
use libnode::abi::{proto, Signals};
use libnode::{println, Channel, Env, Message};
libnode::main! {fn main(env:Env)->i32{run(env)}}
struct Session {
    console: Channel,
    files: Option<Channel>,
    display: Option<Channel>,
    audio: Option<Channel>,
    cwd: String,
    previous: String,
    history: Vec<String>,
}
fn run(mut env: Env) -> i32 {
    let Some(console) = env.take_extra(0).map(Channel::from_handle) else {
        return 1;
    };
    let mut session = Session {
        console,
        files: env.take_extra(0).map(Channel::from_handle),
        display: env.take_extra(0).map(Channel::from_handle),
        audio: env.take_extra(0).map(Channel::from_handle),
        cwd: String::from("/"),
        previous: String::from("/"),
        history: Vec::new(),
    };
    write_text(
        &session.console,
        "\x1b[1mNodeOS\x1b[0m\n\x1b[2mType help to get started.  Tab completes commands.\x1b[0m\n\n",
    );
    println!("shell: ready (fullscreen)");
    loop {
        let line = lineedit::read_line(
            &session.console,
            session.files.as_ref(),
            &session.cwd,
            &mut session.history,
        );
        execute(&mut session, &line, 0);
        if !line.trim().is_empty() {
            write_text(&session.console, "\n");
        }
    }
}
fn write_text(console: &Channel, mut text: &str) {
    // Console requests have finite receive buffers. Keep large grep/pager
    // output bounded without splitting UTF-8 code points between messages.
    while !text.is_empty() {
        let mut end = text.len().min(2048);
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        let _ = console.call_with(
            Message::new(proto::CONSOLE_WRITE_TEXT).with_payload(text[..end].as_bytes()),
            64,
            0,
        );
        text = &text[end..];
    }
}

fn execute(s: &mut Session, text: &str, depth: usize) {
    let commands = match syntax::parse(text) {
        Ok(commands) => commands,
        Err(error) => {
            write_text(&s.console, &format!("\x1b[31mSyntax: {error}.\x1b[0m\n"));
            println!("shell: syntax rejected");
            return;
        }
    };
    let count = commands.len();
    for args in commands {
        if args.is_empty() {
            continue;
        }
        command(s, &args, depth);
        println!("shell: command complete: {}", args[0]);
    }
    if depth > 0 {
        println!("shell: source completed ({count} commands)");
    }
}
fn command(s: &mut Session, args: &[String], depth: usize) {
    let name = args[0].as_str();
    let tail = &args[1..];
    match name {
        "font" => {
            let size = match tail.first().map(String::as_str) {
                Some("small") => 0u32,
                Some("normal") => 1,
                Some("large") => 2,
                _ => {
                    write_text(&s.console,"Usage: font small | normal | large\nNormal is the default. Earlier output stays in scrollback.\n");
                    return;
                }
            };
            let result = s.console.call_with(
                Message::new(proto::CONSOLE_SET_FONT).with_payload(&size.to_le_bytes()),
                128,
                0,
            );
            match result {
                Ok(reply)
                    if !reply
                        .flags
                        .contains(libnode::abi::message::MessageFlags::ERROR) =>
                {
                    let geometry = proto::decode::<proto::ConsoleInfo>(&reply.payload);
                    if let Some(info) = geometry {
                        write_text(
                            &s.console,
                            &format!(
                                "Text size: {} ({} columns, {} lines).\n",
                                tail[0], info.cols, info.rows
                            ),
                        );
                    }
                }
                _ => write_text(&s.console, "Could not change text size.\n"),
            }
        }
        "help" => help(&s.console, tail.first().map(String::as_str).unwrap_or("")),
        "echo" => write_text(&s.console, &format!("{}\n", tail.join(" "))),
        "pwd" => write_text(&s.console, &format!("{}\n", s.cwd)),
        "cd" => {
            let old = s.cwd.clone();
            let target = tail.first().map(String::as_str).unwrap_or("/");
            let target = if target == "-" {
                s.previous.as_str()
            } else {
                target
            };
            if let Some(files) = s.files.as_ref() {
                files::change_dir(&s.console, files, &mut s.cwd, target);
                if old != s.cwd {
                    s.previous = old;
                }
            }
        }
        "clear" => {
            let _ = s
                .console
                .call_with(Message::new(proto::CONSOLE_CLEAR), 64, 0);
        }
        "history" => {
            for (i, line) in s.history.iter().enumerate() {
                write_text(&s.console, &format!("{:>4}  {}\n", i + 1, line));
            }
        }
        "about" | "uptime" => write_text(
            &s.console,
            &format!(
                "NodeOS / x86-64 capability microkernel\nuptime: {} seconds\n",
                libnode::clock_ns() / 1_000_000_000
            ),
        ),
        "source" => {
            if depth >= 8 {
                write_text(&s.console, "source: maximum nesting is 8\n");
                return;
            }
            let Some(path) = tail.first() else {
                write_text(&s.console, "usage: source <file>\n");
                return;
            };
            if let Some(files) = s.files.as_ref() {
                match files::read_file(files, &files::resolve(&s.cwd, path), 65536) {
                    Ok(bytes) => match core::str::from_utf8(&bytes) {
                        Ok(text) => execute(s, text, depth + 1),
                        Err(_) => write_text(&s.console, "source: file is not UTF-8 text\n"),
                    },
                    Err(error) => show_error(&s.console, "source", error),
                }
            }
        }
        "ls" | "cat" | "write" | "append" | "mkdir" | "rm" | "stat" | "df" | "fstest" | "touch"
        | "cp" | "grep" | "more" => {
            if let Some(files) = s.files.as_ref() {
                if !valid_arguments(name, tail) {
                    write_text(&s.console, &format!("Usage: {}\n", usage(name).unwrap()));
                    return;
                }
                if let Err(error) = files::command(&s.console, files, &s.cwd, name, tail) {
                    show_error(&s.console, name, error);
                }
            } else {
                write_text(&s.console, "no filesystem connected\n");
            }
        }
        _ => {
            let (program, arguments) = if name == "run" {
                let Some(program) = tail.first() else {
                    write_text(&s.console, "usage: run <program> [args...]\n");
                    return;
                };
                (program.as_str(), &tail[1..])
            } else {
                (name, tail)
            };
            let path = if program.contains('/') {
                files::resolve(&s.cwd, program)
            } else {
                format!(
                    "/bin/{}{}",
                    program,
                    if program.to_ascii_lowercase().ends_with(".nxf") {
                        ""
                    } else {
                        ".nxf"
                    }
                )
            };
            let Some(files) = s.files.as_ref() else {
                write_text(&s.console, "no filesystem connected\n");
                return;
            };
            let arguments: Vec<&str> = arguments.iter().map(String::as_str).collect();
            match libnode::session::launch(
                &s.console,
                files,
                s.display.as_ref(),
                s.audio.as_ref(),
                &path,
                &arguments,
            ) {
                Ok(process) => {
                    println!("shell: foreground {path}");
                    let _ = process.handle().wait(Signals::PROCESS_TERMINATED, 0);
                    println!("shell: completed {path}");
                }
                Err(libnode::Error::NotFound) if name != "run" && !name.contains('/') => {
                    write_text(
                        &s.console,
                        &format!(
                            "\x1b[31mUnknown command: {program}.\x1b[0m  Type help or press Tab.\n"
                        ),
                    )
                }
                Err(error) => show_error(&s.console, program, error),
            }
        }
    }
}
fn usage(name: &str) -> Option<&'static str> {
    Some(match name {
        "ls" => "ls [directory]",
        "cat" => "cat <file>",
        "more" => "more <file>",
        "grep" => "grep <text> <file>",
        "cp" => "cp [-f] <source> <destination>",
        "write" => "write <file> <text>",
        "append" => "append <file> <text>",
        "touch" => "touch <file>",
        "mkdir" => "mkdir <directory>",
        "rm" => "rm <path>",
        "stat" => "stat <path>",
        "cd" => "cd [directory | -]",
        "source" => "source <file>",
        "run" => "run <program> [arguments...]",
        "font" => "font small | normal | large",
        "df" => "df",
        "fstest" => "fstest",
        "pwd" => "pwd",
        "echo" => "echo [text...]",
        "clear" => "clear",
        "history" => "history",
        "about" => "about",
        "uptime" => "uptime",
        _ => return None,
    })
}
fn valid_arguments(name: &str, args: &[String]) -> bool {
    match name {
        "ls" => args.len() <= 1,
        "df" | "fstest" => args.is_empty(),
        "cp" => {
            args.len()
                == if args.first().is_some_and(|s| s == "-f") {
                    3
                } else {
                    2
                }
        }
        "grep" => args.len() == 2,
        "write" | "append" => !args.is_empty(),
        _ => args.len() == 1,
    }
}
fn show_error(console: &Channel, context: &str, error: libnode::Error) {
    use libnode::Error::*;
    let message = match error {
        NotFound => "No such file or directory.",
        AlreadyExists if context == "cp" => "Destination exists. Use cp -f to replace it.",
        AlreadyExists => "That name already exists.",
        InvalidArgument => "Invalid path or arguments.",
        AccessDenied | CapabilityDenied => "Permission denied.",
        NoSpace => "No space left.",
        NoMemory => "Not enough memory.",
        NotSupported => "This operation is not supported.",
        LimitExceeded => "Size or resource limit exceeded.",
        _ => error.message(),
    };
    write_text(console, &format!("\x1b[31m{context}: {message}\x1b[0m\n"));
}
fn help(console: &Channel, topic: &str) {
    let text=match topic{
        ""=>"\x1b[1mShell commands\x1b[0m\n\n\x1b[36mNavigate & read\x1b[0m\n  pwd, cd, ls       Find your way around\n  cat, more, grep   Read, page through and search files\n\n\x1b[36mWork with files\x1b[0m\n  write, append     Write text to disk\n  cp, touch         Copy files or create an empty file\n  mkdir, rm         Create folders or remove entries\n  stat, df          File and disk information\n\n\x1b[36mYour session\x1b[0m\n  history, clear    Recall commands or clear the screen\n  font large        Make text larger (small/normal/large)\n  source, run       Run a command file or an application\n\n\x1b[2mTry: ls /bin   or   help cp\nMore help: keys, files, syntax, apps\x1b[0m\n",
        "keys"=>"\x1b[1mKeyboard shortcuts\x1b[0m\n\n  Left / Right          Move the cursor\n  Ctrl+Left / Right     Move by word\n  Home / End            Start / end of line\n  Up / Down             Command history\n  Delete / Ctrl+D       Delete the next character\n  Ctrl+U / K / W        Erase left / right / previous word\n  Ctrl+C / L            Cancel the line / clear the screen\n  Tab                   Complete a command or path\n  Tab twice             Show matching completions\n  Shift+PgUp / PgDn     Browse scrollback\n  Meta+Q                Close a full-screen app\n  Meta / Scroll Lock    Switch full-screen sessions\n",
        "files"=>"\x1b[1mFiles\x1b[0m\n\n  ls [dir]                   List directory contents\n  more <file>                Read a page at a time (Space/Enter/q)\n  grep <text> <file>          Find matching lines\n  cp [-f] <source> <dest>     Copy; -f allows replacement\n  write <file> <text>        Replace file contents\n  append <file> <text>       Add text to a file\n  touch <file>               Create a file if missing\n  mkdir <dir> / rm <path>    Create a folder / remove an entry\n  stat <path> / df          Inspect a file / disk usage\n\nPaths can be relative. Use .. for the parent folder.\nNew FAT filenames use 8.3 names, such as NOTES.TXT.\n",
        "syntax"=>"\x1b[1mQuotes and command files\x1b[0m\n\nSingle/double quotes keep spaces in an argument.\nInside double quotes, \\n starts a new line and \\t adds a tab.\nSemicolons or newlines separate commands; # starts a comment.\nsource <file> reads UTF-8 commands (64 KiB, 8 nested files).\n\n\x1b[36mExamples\x1b[0m\n  write /NOTES.TXT \"one\\ntwo\\n\"\n  cp /NOTES.TXT /BACKUP.TXT; grep two /BACKUP.TXT\n  cd /docs; more NODEOS.MD\n",
        "apps"=>"\x1b[1mApplications\x1b[0m\n\n  sysinfo             System report\n  tree                Directory tree\n  bench               Memory / scheduler benchmark\n  edit /NOTES.TXT     Text editor (Ctrl+S saves, Ctrl+Q exits)\n  cc /tests/hello.c   Compile and run C\n  doom                Play Doom, if an IWAD is installed\n\nPrograms run by name from /bin. The shell waits for them.\nMeta+Q closes the foreground full-screen application.\n",
        _=>{
            if let Some(usage)=usage(topic){write_text(console,&format!("\x1b[1m{topic}\x1b[0m\n\n  {usage}\n\n"));
                let detail=match topic{"cp"=>"Copies file contents. An existing destination is protected unless -f is used.\nExample: cp /NOTES.TXT /BACKUP.TXT\n","grep"=>"Searches literal text and prints matching lines with their line numbers.\nExample: grep \"hello world\" /NOTES.TXT\n","font"=>"Changes text size while retaining output in scrollback.\nExample: font large\n","write"|"append"=>"Quote text containing spaces or newlines.\nExample: write /NOTES.TXT \"hello\\n\"\n","cd"=>"With no argument, returns to /. Use cd - to return to the previous folder.\n","more"=>"Space moves a page, Enter moves one line, q exits.\n",_=>"See help files, help syntax or help keys for examples.\n"};write_text(console,detail);return;
            }
            write_text(console,&format!("No help for '{topic}'. Try help for a list of topics.\n"));return;
        }
    };
    write_text(console, text);
}

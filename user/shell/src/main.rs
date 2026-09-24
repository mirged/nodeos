//! `shell` -- a minimal interactive command line for NodeOS.
//!
//! Holds no device capability of its own. Everything it draws or reads comes
//! from a channel to `console`, wired up by the boot launcher -- this process
//! could be replaced by any other client of the same protocol without
//! `console` changing at all.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::String;

mod files;

use libnode::abi::proto;
use libnode::{println, Channel, Env, Message};
use nodeos_abi::{Syscall, Rights, Signals};

libnode::main! {
    fn main(env: Env) -> i32 {
        run(env)
    }
}

const PROMPT: &str = "nodeos> ";

fn run(mut env: Env) -> i32 {
    let Some(handle) = env.take_extra(0) else {
        println!("shell: no console connection granted; exiting");
        return 1;
    };
    let console = Channel::from_handle(handle);
    // The filesystem is optional: a boot without an `fs` module still gets a
    // shell, and the file commands say so rather than the shell refusing to
    // start over a disk it may not need.
    let files = env.take_extra(0).map(Channel::from_handle);
    let mut cwd = String::from("/");

    write_text(&console, "NodeOS shell. Type 'help' for a list of commands.\n");
    if files.is_none() {
        write_text(&console, "(no filesystem connected; file commands are unavailable)\n");
    }

    loop {
        write_text(&console, PROMPT);
        let line = read_line(&console);
        run_command(&console, files.as_ref(), &mut cwd, line.trim());
    }
}

fn write_text(console: &Channel, text: &str) {
    let _ = console.call(Message::new(proto::CONSOLE_WRITE_TEXT).with_payload(text.as_bytes()));
}

fn read_line(console: &Channel) -> String {
    match console.call(Message::new(proto::CONSOLE_READ_LINE)) {
        Ok(reply) => reply.text().unwrap_or("").into(),
        Err(_) => String::new(),
    }
}

fn run_command(console: &Channel, files: Option<&Channel>, cwd: &mut String, line: &str) {
    let mut parts = line.splitn(2, ' ');
    let command = parts.next().unwrap_or("");
    let rest = parts.next().unwrap_or("");

    match command {
        "" => {}
        "help" => write_text(
            console,
                "NodeOS shell commands\n\
                 session:  help, pwd, cd <dir>, echo <text>, clear, about\n\
                 files:    ls [dir], cat <file>, touch <file>, write <file> <text>,\n\
                 \x20         append <file> <text>, mkdir <dir>, rm <path>, stat <path>,\n\
                 \x20         df, fstest, run <program>\n\
                 demos:    draw, chaos\n\
                 paths may be absolute (/docs/NODEOS.MD) or relative to pwd.\n\
                 NXF programs installed by make disk are visible under /bin.\n\
                 compositor: Meta/Windows or Scroll Lock switches focus;\n\
                 Meta+Enter maximizes, Meta+Tab switches, Meta+Q closes.\n",
        ),
        "pwd" => {
            write_text(console, cwd);
            write_text(console, "\n");
        }
        "cd" => match files {
            Some(files) => files::change_dir(console, files, cwd, rest),
            None => write_text(console, "no filesystem is connected\n"),
        },
        "ls" | "cat" | "write" | "append" | "mkdir" | "rm" | "stat" | "df" | "fstest" | "touch" | "run" => {
            match files {
                Some(files) => files::run(console, files, cwd, command, rest),
                None => write_text(console, "no filesystem is connected\n"),
            }
        }
        "echo" => {
            write_text(console, rest);
            write_text(console, "\n");
        }
        "clear" => {
            let _ = console.call(Message::new(proto::CONSOLE_CLEAR));
        }
        "about" => {
            let uptime_s = libnode::clock_ns() / 1_000_000_000;
            let mut line = String::new();
            let _ = core::fmt::write(
                &mut line,
                format_args!("NodeOS -- capability-based research microkernel. uptime: {uptime_s}s\n"),
            );
            write_text(console, &line);
        }
        "draw" => run_draw_demo(console),
        other => {
            let mut line = String::new();
            let _ = core::fmt::write(&mut line, format_args!("unknown command: {other}\n"));
            write_text(console, &line);
        }
    }
}

/// A fixed demo exercising the pixel-graphics side of the protocol, not just
/// the text console.
fn run_draw_demo(console: &Channel) {
    let _ = console.call(Message::new(proto::CONSOLE_CLEAR));

    let rect = proto::Encoded::new(proto::ConsoleFillRect { x: 100, y: 100, w: 200, h: 120, color: 0x00_3060_C0 });
    let _ = console.call(Message::new(proto::CONSOLE_FILL_RECT).with_payload(rect.bytes()));

    let diagonals = [
        (100u32, 100u32, 300u32, 220u32),
        (300, 100, 100, 220),
    ];
    for (x0, y0, x1, y1) in diagonals {
        let line = proto::Encoded::new(proto::ConsoleDrawLine { x0, y0, x1, y1, color: 0x00_E0_E0_20 });
        let _ = console.call(Message::new(proto::CONSOLE_DRAW_LINE).with_payload(line.bytes()));
    }

    let cursor = proto::Encoded::new(proto::ConsoleCursor { col: 0, row: 20 });
    let _ = console.call(Message::new(proto::CONSOLE_SET_CURSOR).with_payload(cursor.bytes()));
    write_text(console, "a filled rectangle and two lines, drawn by 'draw'\n");
}

//! Redirection for programs: `program > file`, `>> file` and `< file`.
//!
//! A program's output is not a file descriptor the shell could reopen: it is
//! the console channel the program was handed. So the shell hands it a
//! different one. It creates a channel, gives the program one end as its
//! console, and serves the other end itself while the program runs: text the
//! program writes goes into the output file, a line it reads comes from the
//! input file, and every other console request (its size, the cursor, a key)
//! is passed through to the real console. The program cannot tell, and needs
//! no authority it did not have; the shell gives it strictly less than the
//! console.

use alloc::vec::Vec;

use libnode::abi::message::MessageFlags;
use libnode::abi::{proto, Signals, WaitItem};
use libnode::process::Process;
use libnode::{Channel, Error, Message};

use crate::files;
use crate::syntax::RedirectKind;

/// Largest console request served, as the console itself accepts.
const REQUEST_CAPACITY: usize = 64 * 1024;

struct Output {
    path: alloc::string::String,
    node: u64,
    offset: u64,
}

struct Input {
    data: Vec<u8>,
    at: usize,
}

/// Start `path` with its console redirected, serve it until it exits, and
/// return its process handle for the exit status.
#[allow(clippy::too_many_arguments)]
pub fn run(
    console: &Channel,
    fs: &Channel,
    display: Option<&Channel>,
    audio: Option<&Channel>,
    path: &str,
    args: &[&str],
    environment: &[&str],
    redirects: &[(RedirectKind, alloc::string::String)],
) -> Result<Process, Error> {
    // Open everything before the program starts, so a bad path fails the
    // command instead of a program that has already run.
    let mut input = None;
    let mut output = None;
    for (kind, target) in redirects {
        match kind {
            RedirectKind::In => {
                input = Some(Input { data: files::read_file(fs, target, 16 * 1024 * 1024)?, at: 0 });
            }
            RedirectKind::Out | RedirectKind::Append => {
                if let Some(previous) = output.take() {
                    finish(fs, previous);
                }
                output = Some(open_output(fs, target, *kind == RedirectKind::Append)?);
            }
        }
    }
    let (served, handed) = Channel::create()?;
    let launched = libnode::session::launch(&handed, fs, display, audio, path, args, environment);
    // The program holds its own duplicate; ours would keep the channel open
    // after it exits.
    drop(handed);
    let process = match launched {
        Ok(process) => process,
        Err(error) => {
            if let Some(output) = output {
                finish(fs, output);
            }
            return Err(error);
        }
    };
    libnode::println!("shell: foreground {path} (redirected)");

    loop {
        let mut items = [
            WaitItem { handle: served.handle().raw(), wanted: Signals::READABLE.0, observed: 0, _reserved: 0 },
            WaitItem {
                handle: process.handle().raw(),
                wanted: Signals::PROCESS_TERMINATED.0,
                observed: 0,
                _reserved: 0,
            },
        ];
        if libnode::wait(&mut items, 0).is_err() {
            break;
        }
        // Serve what is queued before noticing an exit, so nothing written
        // last is lost.
        if items[0].observed & Signals::READABLE.0 != 0 {
            if let Ok(request) = served.receive_with(REQUEST_CAPACITY, 0, true) {
                serve(console, fs, &served, request, output.as_mut(), input.as_mut());
                continue;
            }
        }
        if items[1].observed & Signals::PROCESS_TERMINATED.0 != 0 {
            break;
        }
    }
    if let Some(output) = output {
        finish(fs, output);
    }
    Ok(process)
}

fn open_output(fs: &Channel, path: &str, append: bool) -> Result<Output, Error> {
    let node = if append {
        match files::open(fs, path) {
            Ok(node) => node,
            Err(Error::NotFound) => files::create(fs, path)?,
            Err(error) => return Err(error),
        }
    } else {
        files::create(fs, path)?
    };
    if node.kind != proto::fs_kind::FILE {
        files::close(fs, node.node);
        return Err(Error::InvalidArgument);
    }
    Ok(Output { path: path.into(), node: node.node, offset: if append { node.size } else { 0 } })
}

fn finish(fs: &Channel, output: Output) {
    files::close(fs, output.node);
    libnode::println!("shell: wrote {} bytes to {}", output.offset, output.path);
}

fn serve(
    console: &Channel,
    fs: &Channel,
    served: &Channel,
    request: Message,
    output: Option<&mut Output>,
    input: Option<&mut Input>,
) {
    let reply = match (request.kind, output, input) {
        (proto::CONSOLE_WRITE_TEXT, Some(output), _) => {
            let mut status = Ok(());
            for chunk in request.payload.chunks(32768) {
                match files::write_at(fs, output.node, output.offset, chunk) {
                    Ok(written) => output.offset += written as u64,
                    Err(error) => {
                        status = Err(error);
                        break;
                    }
                }
            }
            match status {
                Ok(()) => Message::new(proto::reply_kind(request.kind)),
                Err(error) => error_reply(request.kind, error),
            }
        }
        (proto::CONSOLE_READ_LINE, _, Some(input)) => {
            if input.at >= input.data.len() {
                // End of input: a reader sees the console go away.
                error_reply(request.kind, Error::PeerClosed)
            } else {
                let rest = &input.data[input.at..];
                let end = rest.iter().position(|b| *b == b'\n').unwrap_or(rest.len());
                let line = &rest[..end];
                let line = line.strip_suffix(b"\r").unwrap_or(line);
                input.at += (end + 1).min(rest.len());
                Message::new(proto::reply_kind(request.kind)).with_payload(line)
            }
        }
        // Everything else is the real console's to answer.
        _ => match console.call_with(
            Message::new(request.kind).with_payload(&request.payload),
            REQUEST_CAPACITY,
            0,
        ) {
            Ok(answer) => {
                let mut reply = Message::new(answer.kind).with_payload(&answer.payload);
                if answer.flags.contains(MessageFlags::ERROR) {
                    reply.flags = MessageFlags::ERROR;
                }
                reply
            }
            Err(error) => error_reply(request.kind, error),
        },
    };
    let _ = served.reply(&request, reply);
}

fn error_reply(kind: u32, error: Error) -> Message {
    let mut reply = Message::new(proto::reply_kind(kind)).with_payload(&proto::encode_error(error));
    reply.flags = MessageFlags::ERROR;
    reply
}

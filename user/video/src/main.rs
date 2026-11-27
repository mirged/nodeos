//! `video` -- the NodeOS video player, for `.BAP` files.
//!
//! ```text
//!   video                 the list of videos in /VIDEOS (and / )
//!   video FILE [FILE...]  play these, in order, as a playlist
//! ```
//!
//! It needs the same session as Doom: a filesystem channel to read videos, a
//! display connection for the picture, and, optionally, the mixer for sound.
//! Without the mixer it plays silently against the system clock.
//!
//! `.BAP` is NodeOS's own format: 1-bit pictures, run-length coded, each frame
//! carrying its own slice of a mono soundtrack (see `bap.rs`). It is what a
//! machine with no codecs can play at full rate, and any video converts to it
//! with `tools/encode.sh`. The disk ships with a homebrew demo,
//! `/VIDEOS/DEMO.BAP`, generated from `tools/gen_demo.c`.
//!
//! The pieces: `files` reads from the filesystem, `bap` parses and decodes,
//! `screen` owns the surface, `sound` the mixer, `input` the keys, `browser`
//! is the list and `player` plays one video with its on-screen controls.

#![no_std]
#![no_main]

extern crate alloc;

mod bap;
mod browser;
mod files;
mod input;
mod player;
mod screen;
mod sound;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use libnode::abi::proto;
use libnode::{println, Channel, Env, Message};

use bap::Video;
use browser::Choice;
use files::{Entry, File};
use player::{Outcome, Player, Repeat, Settings};
use screen::{Screen, CELL};
use sound::Sound;

/// Where the list looks, in order.
const DIRECTORIES: &[&str] = &["/VIDEOS", "/"];

libnode::main! {
    fn main(env: Env) -> i32 {
        run(env)
    }
}

fn run(mut env: Env) -> i32 {
    // The shell's session layout: console, filesystem, display, then the
    // mixer. `take_extra(0)` pops from the front.
    let console = env.take_extra(0).map(Channel::from_handle);
    let Some(files) = env.take_extra(0).map(Channel::from_handle) else {
        println!("video: no filesystem connection granted; exiting");
        return 1;
    };
    let Some(wm) = env.take_extra(0).map(Channel::from_handle) else {
        say(&console, "video: needs the display; run it from the shell\n");
        return 1;
    };
    let sound = env.take_extra(0).map(Channel::from_handle).and_then(Sound::open);
    println!("video: sound is {}", if sound.is_some() { "on" } else { "off (no audio service)" });

    let args: Vec<String> = env.args().iter().skip(1).cloned().collect();
    let Some(mut screen) = Screen::create(&wm) else {
        say(&console, "video: the display granted no surface\n");
        return 1;
    };

    let mut app = App { files: &files, screen: &mut screen, sound: sound.as_ref(), settings: Settings::default(), cache: None };
    let mut errors = Vec::new();

    if args.is_empty() {
        app.browse();
    } else {
        // A playlist from the command line: Esc or the end of it leaves.
        let entries: Vec<Entry> = args
            .into_iter()
            .map(|path| Entry { name: base_name(&path), path, size: 0, header: None })
            .collect();
        let _ = app.play_list(&entries, 0, &mut errors);
    }

    drop(app);
    drop(screen);
    for error in &errors {
        say(&console, &format!("video: {error}\n"));
    }
    if errors.is_empty() {
        0
    } else {
        1
    }
}

/// Write to the console, if there is one. Only seen while the surface is down.
fn say(console: &Option<Channel>, text: &str) {
    if let Some(console) = console {
        let _ = console.call_with(Message::new(proto::CONSOLE_WRITE_TEXT).with_payload(text.as_bytes()), 64, 0);
    }
}

fn base_name(path: &str) -> String {
    String::from(path.rsplit('/').next().unwrap_or(path))
}

struct App<'a, 'w> {
    files: &'a Channel,
    screen: &'a mut Screen<'w>,
    sound: Option<&'a Sound>,
    settings: Settings,
    /// The last file loaded, so repeating or going back to it is instant.
    cache: Option<(String, File)>,
}

impl App<'_, '_> {
    fn browse(&mut self) {
        let mut entries = scan(self.files);
        println!("video: {} video(s) in the list", entries.len());
        let mut selected = 0;
        let mut message: Option<String> = None;
        loop {
            match browser::choose(self.screen, &entries, &mut selected, message.as_deref()) {
                Choice::Quit => return,
                Choice::Rescan => {
                    entries = scan(self.files);
                    message = Some(format!("Found {} video(s).", entries.len()));
                }
                Choice::Play(index) => {
                    let mut errors = Vec::new();
                    let (outcome, last) = self.play_list(&entries, index, &mut errors);
                    selected = last;
                    message = errors.pop();
                    if outcome == Outcome::Quit {
                        return;
                    }
                }
            }
        }
    }

    /// Play `entries` from `start`, following next, previous and repeat.
    /// Returns how it ended and the index it ended on.
    fn play_list(&mut self, entries: &[Entry], start: usize, errors: &mut Vec<String>) -> (Outcome, usize) {
        let count = entries.len();
        let mut index = start;
        let mut forward = true;
        loop {
            let entry = &entries[index];
            let outcome = match self.play_one(entry, (index, count)) {
                Ok(outcome) => outcome,
                Err(error) => {
                    let error = format!("{}: {error}", entry.path);
                    println!("video: {error}");
                    errors.push(error);
                    // Skip a broken file in the direction we were going, but
                    // never loop forever over a list of broken files.
                    if errors.len() >= count {
                        return (Outcome::Back, index);
                    }
                    if forward { Outcome::Next } else { Outcome::Previous }
                }
            };
            let wrap = self.settings.repeat == Repeat::All;
            index = match outcome {
                Outcome::Quit | Outcome::Back => return (outcome, index),
                Outcome::Next | Outcome::Ended if index + 1 < count => {
                    forward = true;
                    index + 1
                }
                Outcome::Next | Outcome::Ended if wrap => {
                    forward = true;
                    0
                }
                Outcome::Next | Outcome::Ended => return (Outcome::Back, index),
                Outcome::Previous => {
                    forward = false;
                    if index > 0 {
                        index - 1
                    } else if wrap {
                        count - 1
                    } else {
                        0
                    }
                }
            };
        }
    }

    fn play_one(&mut self, entry: &Entry, place: (usize, usize)) -> Result<Outcome, &'static str> {
        if self.cache.as_ref().is_none_or(|(path, _)| *path != entry.path) {
            // Let go of the previous file before reading the next: two large
            // videos at once may not fit.
            self.cache = None;
            let screen = &mut *self.screen;
            let file = files::load(self.files, &entry.path, |done, total| loading(screen, &entry.name, done, total))?;
            self.cache = Some((entry.path.clone(), file));
        }
        let (_, file) = self.cache.as_ref().unwrap();
        let video = Video::parse(file.bytes())?;
        let h = &video.header;
        println!(
            "video: playing {} ({}x{} at {} fps, {} frames, {} Hz)",
            entry.path, h.width, h.height, h.fps, h.frames, h.rate
        );
        Ok(Player::new(&video, self.screen, self.sound, &mut self.settings, &entry.name, place).run())
    }
}

/// Every video in [`DIRECTORIES`].
fn scan(files: &Channel) -> Vec<Entry> {
    let mut entries = Vec::new();
    for dir in DIRECTORIES {
        entries.extend(files::list(files, dir));
    }
    entries
}

/// The loading screen: the name and a bar.
fn loading(screen: &mut Screen, name: &str, done: u64, total: u64) {
    const BACKGROUND: u32 = 0x101418;
    screen.fill(BACKGROUND);
    let (w, h) = (screen.width, screen.height);
    let text = format!("Loading {name}");
    screen.text(w.saturating_sub(Screen::text_width(&text)) / 2, h / 2 - CELL.1 as usize - 16, &text, 0xe8ecf2);
    let bar_w = w / 2;
    let x0 = (w - bar_w) / 2;
    let filled = if total == 0 { 0 } else { (bar_w as u64 * done / total) as usize };
    screen.rect(x0, h / 2, bar_w, 6, 0x2a3344);
    screen.rect(x0, h / 2, filled, 6, 0x6ea8ff);
    let size = format!("{} / {} KB", done / 1024, total / 1024);
    screen.text(w.saturating_sub(Screen::text_width(&size)) / 2, h / 2 + 20, &size, 0x8a93a3);
    screen.commit();
}

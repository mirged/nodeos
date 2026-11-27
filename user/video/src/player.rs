//! Playing one video: the clock, the soundtrack feed, the keys, and the
//! on-screen display drawn over the picture.
//!
//! # Keeping picture and sound together
//!
//! The soundtrack is the clock. It is streamed to the mixer with
//! `AUDIO_QUEUE` a quarter of a second ahead of the card, and
//! `AUDIO_QUERY_VOICE` says how much of that has not been played yet;
//! subtracting gives the sample the listener is hearing, and that names the
//! frame to show. A frame that cannot be drawn in time is skipped rather than
//! delayed, so a slow display costs smoothness but never sync. Without the
//! mixer -- or with it refusing -- the same arithmetic runs on the system clock.
//!
//! Pausing and seeking both drop what the mixer holds and restart the stream
//! from the new frame, so there is exactly one way the clock is ever reset.

use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use libnode::println;

use crate::bap::Video;
use crate::input::{self, Event, Key};
use crate::screen::{Layout, Screen, CELL, THEMES};
use crate::sound::Sound;

/// How far ahead of the card the soundtrack is kept, in milliseconds. Long
/// enough that a slow frame cannot starve the mixer, short enough that seeking
/// and pausing feel immediate.
const AUDIO_AHEAD_MS: u64 = 250;

/// Largest `AUDIO_QUEUE` payload this player sends, samples only. The kernel's
/// message limit is 64 KiB and the header has to fit beside it.
const MAX_AUDIO_CHUNK: usize = 48 * 1024;

/// How long the controls stay up after a key, and how long a message shows.
const OSD_NS: u64 = 3_000_000_000;
const TOAST_NS: u64 = 1_500_000_000;

/// Colours of the overlays. They sit on a dimmed picture, so they are the same
/// whatever the theme.
const TEXT: u32 = 0xf0f0f0;
const MUTED: u32 = 0xa0a0a0;
const ACCENT: u32 = 0x6ea8ff;
const TRACK: u32 = 0x505050;

/// Choices that outlive one video.
pub struct Settings {
    /// 0..=100.
    pub volume: u32,
    pub muted: bool,
    pub theme: usize,
    pub repeat: Repeat,
    /// Keep the controls and the statistics line up all the time.
    pub info: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self { volume: 100, muted: false, theme: 0, repeat: Repeat::Off, info: false }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Repeat {
    Off,
    One,
    All,
}

/// Why playback stopped.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Outcome {
    /// Leave the program.
    Quit,
    /// Back to the list.
    Back,
    Next,
    Previous,
    /// The video played to its end.
    Ended,
}

pub struct Player<'a, 'w> {
    video: &'a Video<'a>,
    screen: &'a mut Screen<'w>,
    sound: Option<&'a Sound>,
    settings: &'a mut Settings,
    title: &'a str,
    /// Where this video is in the playlist: index and count.
    place: (usize, usize),

    layout: Layout,
    pixels: Vec<u8>,
    /// The frame `pixels` holds.
    decoded: Option<usize>,

    /// The soundtrack sample the clock was last reset to.
    base_sample: u64,
    /// Wall-clock time of that reset, for playing without sound.
    base_ns: u64,
    /// The next frame whose audio has not been sent to the mixer.
    next_audio: usize,
    /// Samples sent to the mixer since the reset.
    queued: u64,
    /// The frame on screen, if any.
    shown: Option<usize>,
    paused: bool,
    /// Frames skipped because the display could not keep up.
    dropped: u64,

    osd_until: u64,
    toast: Option<(String, u64)>,
    help: bool,
    /// The overlays as last drawn, so a change to them alone forces a redraw.
    drawn: (bool, bool, bool),
    dirty: bool,
}

impl<'a, 'w> Player<'a, 'w> {
    pub fn new(
        video: &'a Video<'a>,
        screen: &'a mut Screen<'w>,
        sound: Option<&'a Sound>,
        settings: &'a mut Settings,
        title: &'a str,
        place: (usize, usize),
    ) -> Self {
        let layout = Layout::new(screen, video.header.width, video.header.height);
        let now = libnode::clock_ns();
        Self {
            pixels: vec![0; video.header.width * video.header.height],
            video,
            screen,
            sound,
            settings,
            title,
            place,
            layout,
            decoded: None,
            base_sample: 0,
            base_ns: now,
            next_audio: 0,
            queued: 0,
            shown: None,
            paused: false,
            dropped: 0,
            osd_until: now + OSD_NS,
            toast: None,
            help: false,
            drawn: (false, false, false),
            dirty: true,
        }
    }

    pub fn run(mut self) -> Outcome {
        let outcome = self.play();
        if let Some(sound) = self.sound {
            sound.stop();
        }
        println!("video: {} stopped ({outcome:?}), {} frame(s) dropped", self.title, self.dropped);
        outcome
    }

    fn play(&mut self) -> Outcome {
        let last = self.video.len() - 1;
        loop {
            for event in input::poll(self.screen) {
                if let Some(outcome) = self.handle(event) {
                    return outcome;
                }
            }

            if self.paused {
                if self.dirty || self.overlays() != self.drawn {
                    let frame = self.shown.unwrap_or(0);
                    self.draw(frame);
                }
                input::wait(self.screen, 100_000_000);
                continue;
            }

            self.feed();
            let frame = self.video.frame_at(self.clock());
            if frame > last {
                // The picture is over; let the last of the sound play out.
                if self.sound.is_none_or(|sound| sound.remaining() == 0) {
                    if self.settings.repeat == Repeat::One {
                        self.reset(0);
                        continue;
                    }
                    return Outcome::Ended;
                }
                input::wait(self.screen, 10_000_000);
                continue;
            }
            if self.shown != Some(frame) || self.dirty || self.overlays() != self.drawn {
                if let Some(shown) = self.shown.filter(|&shown| frame > shown + 1) {
                    self.dropped += (frame - shown - 1) as u64;
                }
                self.draw(frame);
            }

            // Sleep until the next frame is due, waking for keys.
            let next = self.video.sample_of(frame + 1);
            let ahead = next.saturating_sub(self.clock());
            let ns = ahead * 1_000_000_000 / self.video.header.rate as u64;
            input::wait(self.screen, ns.clamp(1_000_000, 20_000_000));
        }
    }

    // --- Keys ---------------------------------------------------------------

    fn handle(&mut self, event: Event) -> Option<Outcome> {
        let key = match event {
            Event::Close => return Some(Outcome::Quit),
            Event::Resized => {
                self.layout = Layout::new(self.screen, self.video.header.width, self.video.header.height);
                self.dirty = true;
                return None;
            }
            Event::Key(key) => key,
        };
        self.osd_until = libnode::clock_ns() + OSD_NS;
        self.dirty = true;
        let fps = self.video.header.fps as i64;
        match key {
            Key::Escape if self.help => self.help = false,
            Key::Escape => return Some(Outcome::Back),
            Key::Char(b'q') => return Some(Outcome::Quit),
            Key::Char(b'n') => return Some(Outcome::Next),
            Key::Char(b'p') => return Some(Outcome::Previous),
            Key::Space | Key::Char(b'k') => self.toggle_pause(),
            Key::Left => self.seek_by(-5 * fps),
            Key::Right => self.seek_by(5 * fps),
            Key::Char(b'j') => self.seek_by(-10 * fps),
            Key::Char(b'l') => self.seek_by(10 * fps),
            Key::PageUp => self.seek_by(-60 * fps),
            Key::PageDown => self.seek_by(60 * fps),
            Key::Home => self.seek_to(0),
            Key::End => self.seek_to(self.video.len().saturating_sub(fps as usize)),
            Key::Char(digit @ b'0'..=b'9') => {
                let tenth = (digit - b'0') as usize;
                self.seek_to(self.video.len() * tenth / 10);
            }
            Key::Char(b',') if self.paused => self.seek_by(-1),
            Key::Char(b'.') if self.paused => self.seek_by(1),
            Key::Up => self.change_volume(10),
            Key::Down => self.change_volume(-10),
            Key::Char(b'm') => {
                self.settings.muted = !self.settings.muted;
                self.apply_volume();
                let text = if self.settings.muted { String::from("Muted") } else { self.volume_text() };
                self.say(text);
            }
            Key::Char(b'r') => {
                self.settings.repeat = match self.settings.repeat {
                    Repeat::Off => Repeat::One,
                    Repeat::One => Repeat::All,
                    Repeat::All => Repeat::Off,
                };
                let text = match self.settings.repeat {
                    Repeat::Off => "Repeat off",
                    Repeat::One => "Repeat this video",
                    Repeat::All => "Repeat the playlist",
                };
                self.say(String::from(text));
            }
            Key::Char(b'c') => {
                self.settings.theme = (self.settings.theme + 1) % THEMES.len();
                self.say(format!("Colours: {}", THEMES[self.settings.theme].name));
            }
            Key::Char(b'i') | Key::Tab => self.settings.info = !self.settings.info,
            Key::Char(b'h') | Key::Char(b'?') => self.help = !self.help,
            _ => {}
        }
        None
    }

    fn toggle_pause(&mut self) {
        let frame = self.shown.unwrap_or(0);
        self.paused = !self.paused;
        // Pausing drops what the mixer had queued; resuming re-sends it from
        // the frame on screen, so nothing is skipped.
        self.reset(frame);
    }

    fn seek_by(&mut self, frames: i64) {
        let current = self.shown.unwrap_or(0) as i64;
        let target = (current + frames).clamp(0, self.video.len() as i64 - 1);
        self.seek_to(target as usize);
    }

    fn seek_to(&mut self, frame: usize) {
        self.reset(frame.min(self.video.len() - 1));
    }

    fn change_volume(&mut self, delta: i32) {
        self.settings.volume = (self.settings.volume as i32 + delta).clamp(0, 100) as u32;
        self.settings.muted = false;
        self.apply_volume();
        let text = self.volume_text();
        self.say(text);
    }

    fn volume_text(&self) -> String {
        format!("Volume {}%", self.settings.volume)
    }

    /// The mixer's 0..=255 for the current settings.
    fn mixer_volume(&self) -> u32 {
        if self.settings.muted {
            0
        } else {
            self.settings.volume * 255 / 100
        }
    }

    fn apply_volume(&self) {
        if let Some(sound) = self.sound {
            sound.set_volume(self.mixer_volume());
        }
    }

    fn say(&mut self, text: String) {
        self.toast = Some((text, libnode::clock_ns() + TOAST_NS));
    }

    // --- The clock and the soundtrack ---------------------------------------

    /// The soundtrack sample being heard now.
    fn clock(&self) -> u64 {
        let rate = self.video.header.rate as u64;
        match self.sound {
            Some(sound) => {
                let mixed = self.queued.saturating_sub(sound.remaining());
                let latency = sound.latency_ms as u64 * rate / 1000;
                self.base_sample + mixed.saturating_sub(latency)
            }
            None => {
                let elapsed = libnode::clock_ns() - self.base_ns;
                self.base_sample + elapsed * rate / 1_000_000_000
            }
        }
    }

    /// Keep the mixer [`AUDIO_AHEAD_MS`] ahead of the card.
    fn feed(&mut self) {
        let Some(sound) = self.sound else { return };
        let rate = self.video.header.rate;
        let volume = self.mixer_volume();
        let target = AUDIO_AHEAD_MS * rate as u64 / 1000;
        let mut remaining = sound.remaining();
        let mut chunk: Vec<u8> = Vec::new();
        while remaining < target && self.next_audio < self.video.len() {
            // Gather whole frames into one message, as many as fit.
            chunk.clear();
            while self.next_audio < self.video.len() {
                let audio = self.video.audio(self.next_audio);
                if !chunk.is_empty() && chunk.len() + audio.len() > MAX_AUDIO_CHUNK {
                    break;
                }
                chunk.extend_from_slice(&audio[..audio.len().min(MAX_AUDIO_CHUNK)]);
                self.next_audio += 1;
                if remaining + chunk.len() as u64 / 2 >= target {
                    break;
                }
            }
            if chunk.is_empty() {
                continue;
            }
            if !sound.queue(&chunk, rate, volume) {
                println!("video: the mixer refused the soundtrack; playing silently");
                // Carry on against the wall clock from where the sound had got to.
                let now = self.clock();
                self.sound = None;
                self.base_sample = now;
                self.base_ns = libnode::clock_ns();
                return;
            }
            let samples = chunk.len() as u64 / 2;
            self.queued += samples;
            remaining += samples;
        }
    }

    /// Restart the clock and the soundtrack at `frame`.
    fn reset(&mut self, frame: usize) {
        if let Some(sound) = self.sound {
            sound.stop();
        }
        self.base_sample = self.video.sample_of(frame);
        self.base_ns = libnode::clock_ns();
        self.next_audio = frame;
        self.queued = 0;
        self.shown = if self.paused { Some(frame) } else { None };
        self.dirty = true;
    }

    // --- Drawing ------------------------------------------------------------

    /// Which overlays are up right now: controls, message, help.
    fn overlays(&self) -> (bool, bool, bool) {
        let now = libnode::clock_ns();
        let osd = self.paused || self.settings.info || self.help || now < self.osd_until;
        let toast = self.toast.as_ref().is_some_and(|(_, until)| now < *until);
        (osd, toast, self.help)
    }

    fn draw(&mut self, frame: usize) {
        if self.decoded != Some(frame) {
            self.video.decode(frame, &mut self.pixels);
            self.decoded = Some(frame);
        }
        let theme = THEMES[self.settings.theme];
        self.screen.picture(&self.layout, &self.pixels, self.video.header.width, theme);
        let overlays = self.overlays();
        if overlays.0 {
            self.draw_controls(frame);
        }
        if overlays.1 {
            self.draw_toast();
        }
        if overlays.2 {
            self.draw_help();
        }
        self.screen.commit();
        self.shown = Some(frame);
        self.drawn = overlays;
        self.dirty = false;
    }

    /// The bar along the bottom: progress, times, title, and settings.
    fn draw_controls(&mut self, frame: usize) {
        let (cw, ch) = (CELL.0 as usize, CELL.1 as usize);
        let lines = if self.settings.info { 2 } else { 1 };
        let width = self.screen.width;
        let band = 26 + ch * lines + 10;
        let top = self.screen.height.saturating_sub(band);
        self.screen.dim(0, top, width, band);

        // Progress bar, with the playhead as a knob.
        let (x0, bar_w) = (20, width.saturating_sub(40));
        let bar_y = top + 12;
        let total = self.video.len().max(1);
        let played = bar_w * frame / total;
        self.screen.rect(x0, bar_y, bar_w, 4, TRACK);
        self.screen.rect(x0, bar_y, played, 4, ACCENT);
        self.screen.rect(x0 + played.saturating_sub(2), bar_y - 5, 5, 14, TEXT);

        let y = bar_y + 14;
        let fps = self.video.header.fps as usize;
        let status = if self.paused { "||" } else { "> " };
        let times = format!("{status} {} / {}", clock_text(frame / fps), clock_text(total / fps));
        let mut x = self.screen.text(x0, y, &times, TEXT);

        // Settings on the right, then the title in whatever room is left.
        let mut right: Vec<String> = Vec::new();
        if self.place.1 > 1 {
            right.push(format!("{}/{}", self.place.0 + 1, self.place.1));
        }
        match self.settings.repeat {
            Repeat::Off => {}
            Repeat::One => right.push(String::from("Repeat 1")),
            Repeat::All => right.push(String::from("Repeat all")),
        }
        right.push(if self.sound.is_none() {
            String::from("No sound")
        } else if self.settings.muted {
            String::from("Muted")
        } else {
            format!("Vol {}%", self.settings.volume)
        });
        right.push(String::from("H: help"));
        let right = right.join("   ");
        let right_x = width.saturating_sub(20 + Screen::text_width(&right));
        self.screen.text(right_x, y, &right, MUTED);

        x += 3 * cw;
        if right_x > x + 2 * cw {
            let room = (right_x - x) / cw - 2;
            let title = ellipsize(self.title, room);
            self.screen.text(x, y, &title, TEXT);
        }

        if self.settings.info {
            let h = &self.video.header;
            let info = format!(
                "{}x{}  {} fps  {} Hz mono  frame {}/{}  dropped {}  {}",
                h.width,
                h.height,
                h.fps,
                h.rate,
                frame + 1,
                total,
                self.dropped,
                THEMES[self.settings.theme].name
            );
            self.screen.text(x0, y + ch, &info, MUTED);
        }
    }

    /// A message in a dimmed box at the top.
    fn draw_toast(&mut self) {
        let Some((text, _)) = &self.toast else { return };
        let text = text.clone();
        let w = Screen::text_width(&text) + 32;
        let h = CELL.1 as usize + 16;
        let x = self.screen.width.saturating_sub(w) / 2;
        self.screen.dim(x, 24, w, h);
        self.screen.text(x + 16, 24 + 8, &text, TEXT);
    }

    fn draw_help(&mut self) {
        const KEYS: &[(&str, &str)] = &[
            ("Space  K", "Play / pause"),
            ("Left  Right", "Back / forward 5 s"),
            ("J  L", "Back / forward 10 s"),
            ("PgUp  PgDn", "Back / forward 1 min"),
            ("Home  End", "Start / end"),
            ("0 - 9", "Jump to 0% - 90%"),
            (",  .", "Step one frame (paused)"),
            ("Up  Down", "Volume"),
            ("M", "Mute"),
            ("R", "Repeat: off, this video, playlist"),
            ("C", "Colour theme"),
            ("I  Tab", "Statistics"),
            ("N  P", "Next / previous video"),
            ("Esc", "Back to the list"),
            ("Q", "Quit"),
        ];
        let (cw, ch) = (CELL.0 as usize, CELL.1 as usize);
        let w = 54 * cw;
        let h = (KEYS.len() + 2) * ch + 16;
        let x = self.screen.width.saturating_sub(w) / 2;
        let y = self.screen.height.saturating_sub(h) / 2;
        self.screen.dim(x, y, w, h);
        self.screen.text(x + 2 * cw, y + 8, "Keys", ACCENT);
        for (i, (key, what)) in KEYS.iter().enumerate() {
            let row = y + 8 + (i + 2) * ch;
            self.screen.text(x + 2 * cw, row, key, TEXT);
            self.screen.text(x + 16 * cw, row, what, MUTED);
        }
    }
}

/// `m:ss`, or `h:mm:ss` past an hour.
pub fn clock_text(seconds: usize) -> String {
    let (h, m, s) = (seconds / 3600, seconds / 60 % 60, seconds % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// `text`, cut to `room` characters with a trailing `...` if it did not fit.
pub fn ellipsize(text: &str, room: usize) -> String {
    if text.len() <= room {
        String::from(text)
    } else if room <= 3 {
        String::from(&"..."[..room])
    } else {
        let mut cut = room - 3;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        format!("{}...", &text[..cut])
    }
}

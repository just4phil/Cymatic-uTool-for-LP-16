//! The playback engine of the rehearsal player.
//!
//! The engine knows no audio device: whoever owns it calls `render` to pull
//! interleaved stereo `f32` blocks (the WASAPI adapter in `audio_out.rs` does
//! it from the audio callback, the tests do it directly).
//!
//! Reading: one thread per loaded song fills a `rtrb` ring buffer for every
//! track (2 s, refilled in blocks of 4096 frames). `render` only pops from
//! these rings and mixes: no allocation, no file access, no blocking lock.
//!
//! Changing the position (internal for now, `stop` jumps back to 0) works
//! with a generation counter: the engine publishes (position, generation);
//! the reader thread seeks every track, records for each track how many
//! samples it had pushed before the jump (`start_count`) and then publishes
//! the generation for that track. The engine drops everything up to
//! `start_count` and only then uses the track. Until all tracks are in the
//! new generation (and sufficiently filled) a playing engine outputs silence
//! and does not advance; that wait is no dropout.
//!
//! The rehearsal mix (`crate::mix`) is applied here: every track goes to the
//! strip of its LP-16 output (`ProjectTrack.channel`), each strip has level,
//! pan, mute and solo, then comes the master and a limiter at +-1.0. The
//! target gains of a new mix are reached with a linear 10 ms ramp, so
//! changes while playing do not click. The meters (peak per strip after
//! fader and mute, master peak after the limiter, clip flag) are collected
//! in `render` and handed out with `take_status`.
//!
//! `seek` builds on this. While playing it first fades the audio out over
//! 5 ms from the current position (`Fade::Out`; the status already shows the
//! target), then changes the position, waits in silence (`Fade::Wait`) and
//! fades in over 5 ms (`Fade::In`). Several seeks in a row only replace the
//! target; there is one fade-out and one fade-in.
//!
//! The queue (`set_queue`, `goto`, `next`, `prev`) plays the songs of a
//! playlist one after the other. After the end of a song the next readable
//! one becomes the current song at position 0 and the state is `Waiting`
//! (start mode "wait_for_button_press": `play` starts it) or `Countdown`
//! (any other mode: it starts after round(delay x rate) frames of silence,
//! counted in `render`). The song after the current one is opened early (in
//! the last 10 s of a song, or while waiting) into a second slot; the swap
//! at the end of a song touches no file. Files are never opened in `render`:
//! the engine posts requests to an `Opener` (see `rehearsal_queue.rs`) and
//! takes the result with `try_lock`. Songs the engine replaces go to a small
//! garbage list that the caller empties (dropping one joins its thread).

use crate::mix::{MixParams, StripGains, STRIPS};
use crate::rehearsal_midi::{ChaseState, MidiList, MidiMsg, MidiStatus, MidiTrack};
use crate::rehearsal_queue::{find_path, Opener, QueueEntry, Request};
use crate::{read_midi_events, SongProject, WavReader};
use rtrb::{Consumer, Producer, RingBuffer};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Frames the reader thread reads and pushes at once.
const READ_BLOCK: usize = 4096;
/// Frames `render` mixes per round (size of the scratch buffer of a strip).
const MIX_CHUNK: usize = 1024;
/// Length of the ramp to the gains of a new mix.
const MIX_RAMP_SECONDS: f64 = 0.01;
/// A song is opened early when less than this is left of the current one.
const PRELOAD_SECONDS: f64 = 10.0;
/// `prev` restarts the song when it is further than this into it.
pub const PREV_RESTART_SECONDS: f64 = 3.0;
/// Songs the engine can hand back without allocating.
const GARBAGE_SLOTS: usize = 8;
/// Length of the fade-out and fade-in around a jump while playing.
const FADE_SECONDS: f64 = 0.005;
/// How long the reader thread sleeps when every ring is full.
const READER_IDLE: Duration = Duration::from_millis(5);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlayState {
    /// no song loaded
    Empty,
    Stopped,
    Playing,
    Paused,
    /// the longest track is over; silence follows (queue: after the last song)
    Ended,
    /// queue: the next song is loaded at position 0 and starts with `play`
    Waiting,
    /// queue: the next song is loaded and starts by itself when the
    /// countdown (`Status::countdown_seconds`) is over
    Countdown,
}

/// The meters. The peaks are absolute values (1.0 = full scale), the
/// maximum since the last `Engine::take_status`.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Levels {
    /// per strip after fader and mute, before pan
    pub strips: [f32; STRIPS],
    /// left and right after the limiter
    pub master: [f32; 2],
    /// the master was limited; stays set until `Engine::reset_clip`
    pub clip: bool,
}

impl Levels {
    /// Keep the larger peaks of `self` and `other`; the clip flag of `self`.
    pub fn merge_peaks(&mut self, other: &Levels) {
        for (a, b) in self.strips.iter_mut().zip(other.strips.iter()) {
            *a = a.max(*b);
        }
        self.master[0] = self.master[0].max(other.master[0]);
        self.master[1] = self.master[1].max(other.master[1]);
    }

    pub fn clear_peaks(&mut self) {
        self.strips = [0.0; STRIPS];
        self.master = [0.0; 2];
    }
}

/// What the user interface shows. Cheap to clone (no string copies), so the
/// audio callback can hand it over without allocating.
#[derive(Clone, Debug)]
pub struct Status {
    pub state: PlayState,
    pub song_name: Arc<str>,
    pub position_frames: u64,
    pub position_seconds: f64,
    /// length of the longest track
    pub length_frames: u64,
    pub length_seconds: f64,
    pub output_rate: u32,
    /// 0 without a song
    pub song_rate: u32,
    /// blocks in which a playing track had no data
    pub underruns: u64,
    pub warnings: Arc<Vec<String>>,
    pub levels: Levels,
    /// queue: index of the current song (while `Waiting` or `Countdown`:
    /// the song that starts next); the target of a switch that is still
    /// opening the song
    pub index: Option<usize>,
    /// queue: the readable song after `index`
    pub next_index: Option<usize>,
    pub queue_len: usize,
    /// remaining time of the `Countdown`
    pub countdown_seconds: f64,
    /// the current song has another rate than the output: the owner has to
    /// open the output with this rate (`Engine::set_output_rate` in tests)
    pub needs_rate: Option<u32>,
    /// queue: one line per entry that cannot be played (skipped)
    pub queue_warnings: Arc<Vec<String>>,
    /// the MIDI of the current song
    pub midi: MidiStatus,
}

/// "m:ss" for a time in seconds (rounded down).
pub fn fmt_clock(seconds: f64) -> String {
    let s = if seconds.is_finite() && seconds > 0.0 { seconds as u64 } else { 0 };
    format!("{}:{:02}", s / 60, s % 60)
}

// ---------------------------------------------------------------------------
// reader thread
// ---------------------------------------------------------------------------

/// Shared between the engine and the reader thread.
struct Shared {
    /// generation and start position of the latest position change; the
    /// start is stored first, so a reader that sees a generation sees a
    /// start that is at least as new
    generation: AtomicU64,
    start: AtomicU64,
    stop: AtomicBool,
}

/// Per track: the reader thread tells the engine where a generation begins.
struct TrackShared {
    generation: AtomicU64,
    /// samples pushed in all earlier generations
    start_count: AtomicU64,
}

struct ReaderTrack {
    reader: WavReader,
    producer: Producer<f32>,
    shared: Arc<TrackShared>,
    pushed: u64,
    /// frames of the current generation that still have to be pushed
    remaining: u64,
}

fn reader_thread(mut tracks: Vec<ReaderTrack>, shared: Arc<Shared>) {
    let mut buf = vec![0f32; READ_BLOCK];
    let mut current = 0u64; // generations start at 1
    while !shared.stop.load(Ordering::Acquire) {
        // consistent (generation, start)
        let (g, start) = loop {
            let g1 = shared.generation.load(Ordering::Acquire);
            let s = shared.start.load(Ordering::Acquire);
            if g1 == shared.generation.load(Ordering::Acquire) {
                break (g1, s);
            }
        };
        if g != current {
            for t in tracks.iter_mut() {
                // a failing seek leaves the position wrong, which only
                // matters for a file that was just readable
                let _ = t.reader.seek(start);
                t.remaining = t.reader.frames().saturating_sub(start);
                t.shared.start_count.store(t.pushed, Ordering::Relaxed);
                t.shared.generation.store(g, Ordering::Release);
            }
            current = g;
        }
        let mut worked = false;
        for t in tracks.iter_mut() {
            let want = (t.remaining as usize).min(READ_BLOCK);
            if want == 0 || t.producer.slots() < want {
                continue;
            }
            let got = t.reader.read(&mut buf[..want]);
            // a disk error ends the file early: pad with silence so the
            // ring keeps its lockstep with the position
            buf[got..want].fill(0.0);
            if let Ok(mut chunk) = t.producer.write_chunk(want) {
                let (a, b) = chunk.as_mut_slices();
                let n = a.len();
                a.copy_from_slice(&buf[..n]);
                b.copy_from_slice(&buf[n..want]);
                chunk.commit_all();
            }
            t.pushed += want as u64;
            t.remaining -= want as u64;
            worked = true;
        }
        if !worked {
            std::thread::park_timeout(READER_IDLE);
        }
    }
}

// ---------------------------------------------------------------------------
// loaded song
// ---------------------------------------------------------------------------

/// The engine side of one track.
struct PlayTrack {
    /// strip of the rehearsal mix (0-based LP-16 output)
    channel: usize,
    consumer: Consumer<f32>,
    shared: Arc<TrackShared>,
    frames: u64,
    capacity: usize,
    /// samples popped so far, all generations
    popped: u64,
    /// in the current generation
    synced: bool,
    /// frames to skip because they were due while no data was there
    debt: usize,
}

impl PlayTrack {
    fn pop(&mut self, n: usize) {
        if n == 0 {
            return;
        }
        if let Ok(chunk) = self.consumer.read_chunk(n) {
            chunk.commit_all();
            self.popped += n as u64;
        }
    }

    /// Adds `n` frames to `mix` (the buffer of the track's strip).
    fn add_to(&mut self, n: usize, mix: &mut [f32]) {
        if let Ok(chunk) = self.consumer.read_chunk(n) {
            let (a, b) = chunk.as_slices();
            for (m, s) in mix.iter_mut().zip(a.iter().chain(b.iter())) {
                *m += *s;
            }
            chunk.commit_all();
            self.popped += n as u64;
        }
    }

    /// Takes part in generation `generation`: drops what is left of the
    /// older ones. False while the reader thread has not got there yet.
    fn sync(&mut self, generation: u64) -> bool {
        if self.synced {
            return true;
        }
        if self.shared.generation.load(Ordering::Acquire) != generation {
            return false;
        }
        let start = self.shared.start_count.load(Ordering::Acquire);
        self.pop(start.saturating_sub(self.popped) as usize);
        self.synced = true;
        self.debt = 0;
        true
    }

    fn pay_debt(&mut self) {
        let n = self.debt.min(self.consumer.slots());
        self.pop(n);
        self.debt -= n;
    }
}

/// The fade around a jump while playing; the numbers are frames done.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fade {
    None,
    /// fading out, the jump to `pending` follows
    Out(usize),
    /// jumped; silence until the buffers are filled, then fade in
    Wait,
    In(usize),
}

/// A song opened for playing: its tracks, the reader thread, the facts.
/// Building one opens the files, so it belongs on the user interface
/// thread; the engine (in the audio callback) only takes the finished
/// object. Dropping it ends the reader thread and releases the files.
pub struct LoadedSong {
    path: String,
    name: Arc<str>,
    song_rate: u32,
    length_frames: u64,
    warnings: Arc<Vec<String>>,
    tracks: Vec<PlayTrack>,
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
    /// bit n: a track plays on strip n
    strip_mask: u16,
    /// the events of the MIDI file; None without a (readable) file
    midi: Option<MidiTrack>,
    // transport
    generation: u64,
    pos: u64,
    /// all tracks of the generation are filled enough to start
    primed: bool,
    fade: Fade,
    /// target of the jump that follows the fade-out
    pending: u64,
}

impl LoadedSong {
    /// Open the WAVs of `project` and start reading. Tracks that are
    /// missing, unreadable, unsupported, not mono or of another sample rate
    /// than the first readable track stay silent and add a warning.
    pub fn open(project: &SongProject) -> LoadedSong {
        let mut warnings = Vec::new();
        let mut readers: Vec<(String, usize, WavReader)> = Vec::new();
        for (i, t) in project.tracks.iter().enumerate() {
            let name = if t.name.is_empty() { format!("channel {}", i + 1) } else { t.name.clone() };
            if t.channel < 0 || t.channel as usize >= STRIPS {
                warnings.push(format!(
                    "Track \"{}\": output channel {} is not in 1-{}, left silent",
                    name,
                    t.channel as i64 + 1,
                    STRIPS
                ));
                continue;
            }
            match WavReader::open(&project.wav(t)) {
                Ok(r) => readers.push((name, t.channel as usize, r)),
                Err(e) => warnings.push(format!("Track \"{}\": {}", name, e)),
            }
        }
        let song_rate = readers.first().map(|(_, _, r)| r.rate()).unwrap_or(project.sampling_rate);
        let mut used = Vec::new();
        for (name, channel, r) in readers {
            if r.channels() != 1 {
                warnings.push(format!("Track \"{}\": not mono ({} channels), left silent", name, r.channels()));
            } else if r.rate() != song_rate {
                warnings.push(format!(
                    "Track \"{}\": sample rate {} Hz differs from the song rate {} Hz, left silent",
                    name,
                    r.rate(),
                    song_rate
                ));
            } else {
                used.push((channel, r));
            }
        }
        // 2 s, rounded up to whole read blocks so that the ring can really be full
        let capacity = (2 * song_rate as usize).div_ceil(READ_BLOCK).max(1) * READ_BLOCK;
        let midi = match project.midi_path() {
            None => None,
            Some(path) => match read_midi_events(&path) {
                Some(events) => Some(MidiTrack::from_events(&events, song_rate)),
                None => {
                    let file = std::path::Path::new(&path)
                        .file_name()
                        .map(|f| f.to_string_lossy().into_owned())
                        .unwrap_or(path);
                    warnings.push(format!("MIDI file unreadable: {}", file));
                    None
                }
            },
        };
        let shared = Arc::new(Shared {
            generation: AtomicU64::new(1),
            start: AtomicU64::new(0),
            stop: AtomicBool::new(false),
        });
        let mut length_frames = 0;
        let mut play = Vec::new();
        let mut read = Vec::new();
        let mut strip_mask = 0u16;
        for (channel, reader) in used {
            strip_mask |= 1 << channel;
            let (producer, consumer) = RingBuffer::new(capacity);
            let ts = Arc::new(TrackShared { generation: AtomicU64::new(0), start_count: AtomicU64::new(0) });
            let frames = reader.frames();
            length_frames = length_frames.max(frames);
            play.push(PlayTrack {
                channel,
                consumer,
                shared: ts.clone(),
                frames,
                capacity,
                popped: 0,
                synced: false,
                debt: 0,
            });
            read.push(ReaderTrack { reader, producer, shared: ts, pushed: 0, remaining: 0 });
        }
        let thread = {
            let shared = shared.clone();
            std::thread::Builder::new()
                .name("rehearsal-reader".into())
                .spawn(move || reader_thread(read, shared))
                .ok()
        };
        if thread.is_none() {
            warnings.push("The file reader thread could not be started".to_string());
        }
        LoadedSong {
            path: project.path.clone(),
            name: Arc::from(project.name.as_str()),
            song_rate,
            length_frames,
            warnings: Arc::new(warnings),
            tracks: play,
            shared,
            thread,
            strip_mask,
            midi,
            generation: 1,
            pos: 0,
            primed: false,
            fade: Fade::None,
            pending: 0,
        }
    }

    /// The sample rate of the song (that of its first readable track).
    pub fn rate(&self) -> u32 {
        self.song_rate
    }

    /// path of the .songprj the song was loaded from
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Jump to `frame` (clamped to the end): new generation, nothing
    /// plays until the readers have caught up.
    fn seek_to(&mut self, frame: u64) {
        let frame = frame.min(self.length_frames);
        self.shared.start.store(frame, Ordering::Release);
        self.generation += 1;
        self.shared.generation.store(self.generation, Ordering::Release);
        if let Some(t) = &self.thread {
            t.thread().unpark();
        }
        self.pos = frame;
        self.primed = false;
        if let Some(m) = self.midi.as_mut() {
            m.seek(frame);
        }
        for t in self.tracks.iter_mut() {
            t.synced = false;
            t.debt = 0;
        }
    }

    /// The fade-out is over (or the song ended during it): do the jump.
    /// Returns the MIDI channels that need All Notes Off now.
    fn finish_fade_out(&mut self) -> u16 {
        let notes = self.notes_mask();
        self.seek_to(self.pending);
        self.fade = Fade::Wait;
        notes
    }

    /// MIDI channels that need All Notes Off when the song stops sounding.
    fn notes_mask(&self) -> u16 {
        self.midi.as_ref().map(|m| m.notes_mask()).unwrap_or(0)
    }

    /// The events are for this output rate (in place, no allocation).
    fn midi_rate(&mut self, rate: u32) {
        let pos = self.pos;
        if let Some(m) = self.midi.as_mut() {
            m.set_rate(rate, pos);
        }
    }

    /// Position shown to the user: the target of a jump that is still fading out.
    fn shown_pos(&self) -> u64 {
        match self.fade {
            Fade::Out(_) => self.pending,
            _ => self.pos,
        }
    }

    /// Every track is in the current generation and holds at least
    /// 1/`divisor` of a full ring (limited by its end).
    fn filled(&mut self, divisor: usize) -> bool {
        let (generation, pos) = (self.generation, self.pos);
        let mut ok = true;
        for t in self.tracks.iter_mut() {
            if !t.sync(generation) {
                ok = false;
                continue;
            }
            t.pay_debt();
            // the reader only pushes whole blocks, so a ring that is not at
            // its end is "full" when less than one block is free
            let remaining = t.frames.saturating_sub(pos) as usize;
            let fillable = if remaining >= t.capacity { t.capacity - (READ_BLOCK - 1) } else { remaining };
            let expected = fillable.min(t.capacity / divisor);
            if t.debt > 0 || t.consumer.slots() < expected {
                ok = false;
            }
        }
        ok
    }
}

impl Drop for LoadedSong {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            t.thread().unpark();
            let _ = t.join();
        }
    }
}

// ---------------------------------------------------------------------------
// engine
// ---------------------------------------------------------------------------

/// The gains the mixer applies: per strip and the master.
#[derive(Clone, Copy)]
struct Gains {
    strips: [StripGains; STRIPS],
    master: f32,
}

impl Gains {
    fn neutral() -> Gains {
        let (strips, master) = MixParams::default().gains();
        Gains { strips, master }
    }

    /// `start` + (`target` - `start`) x `t`
    fn lerp(start: &Gains, target: &Gains, t: f32) -> Gains {
        let mix = |a: f32, b: f32| a + (b - a) * t;
        let mut g = *target;
        for ((o, a), b) in g.strips.iter_mut().zip(start.strips.iter()).zip(target.strips.iter()) {
            *o = StripGains { level: mix(a.level, b.level), left: mix(a.left, b.left), right: mix(a.right, b.right) };
        }
        g.master = mix(start.master, target.master);
        g
    }
}

/// A request to the opener that has not been answered yet.
#[derive(Clone, Copy)]
struct Outstanding {
    ticket: u64,
    index: usize,
    /// the song is wanted as the current one (else: preload into the slot)
    switch: bool,
    /// the opener has the request
    posted: bool,
}

pub struct Engine {
    output_rate: u32,
    /// frames of a fade: round(5 ms x rate)
    fade_len: usize,
    state: PlayState,
    song: Option<LoadedSong>,
    underruns: u64,
    /// one buffer of `MIX_CHUNK` frames per strip
    chan: Vec<f32>,
    /// frames of the ramp to a new mix: round(10 ms x rate)
    ramp_len: usize,
    /// frames of the current ramp that are done (`ramp_len` = finished)
    ramp_done: usize,
    ramp_start: Gains,
    ramp_target: Gains,
    /// gains of the last frame
    gains: Gains,
    peaks: Levels,
    clip: bool,
    no_name: Arc<str>,
    no_warnings: Arc<Vec<String>>,
    // queue
    queue: Arc<Vec<QueueEntry>>,
    /// index of the song in `song`
    cur: Option<usize>,
    /// a switch to this song waits for the opener; `song` is the old one
    /// and untouched until the new one arrives, `render` gives silence
    pending: Option<usize>,
    /// a `seek` that came while the song was still opening
    pending_seek: Option<f64>,
    outstanding: Option<Outstanding>,
    ticket: u64,
    /// the song after the current one, opened early
    slot: Option<(usize, LoadedSong)>,
    /// frames of silence left in `Countdown`
    countdown: u64,
    opener: Arc<Opener>,
    /// replaced songs; the owner drops them (`take_garbage`)
    garbage: Vec<LoadedSong>,
    skipped: Arc<Vec<String>>,
    // MIDI
    /// the messages of the last `render` (fixed capacity)
    midi: MidiList,
    chase: Box<ChaseState>,
    /// channels that get All Notes Off at the start of the next block
    ano_mask: u16,
    midi_sent: u64,
    last_midi: Option<Instant>,
}

/// The lines of `Status::queue_warnings`.
fn skip_messages(queue: &[QueueEntry]) -> Vec<String> {
    queue
        .iter()
        .enumerate()
        .filter_map(|(i, e)| {
            e.project.as_ref().err().map(|why| format!("Song {} \"{}\" is skipped: {}", i + 1, e.name, why))
        })
        .collect()
}

impl Engine {
    pub fn new(output_rate: u32) -> Engine {
        Engine {
            output_rate,
            fade_len: ((FADE_SECONDS * output_rate as f64).round() as usize).max(1),
            state: PlayState::Empty,
            song: None,
            underruns: 0,
            chan: vec![0.0; STRIPS * MIX_CHUNK],
            ramp_len: ((MIX_RAMP_SECONDS * output_rate as f64).round() as usize).max(1),
            ramp_done: ((MIX_RAMP_SECONDS * output_rate as f64).round() as usize).max(1),
            ramp_start: Gains::neutral(),
            ramp_target: Gains::neutral(),
            gains: Gains::neutral(),
            peaks: Levels::default(),
            clip: false,
            no_name: Arc::from(""),
            no_warnings: Arc::new(Vec::new()),
            queue: Arc::new(Vec::new()),
            cur: None,
            pending: None,
            pending_seek: None,
            outstanding: None,
            ticket: 0,
            slot: None,
            countdown: 0,
            opener: Arc::new(Opener::default()),
            garbage: Vec::with_capacity(GARBAGE_SLOTS),
            skipped: Arc::new(Vec::new()),
            midi: MidiList::new(),
            chase: Box::new(ChaseState::new()),
            ano_mask: 0,
            midi_sent: 0,
            last_midi: None,
        }
    }

    /// The rate the engine renders at.
    pub fn output_rate(&self) -> u32 {
        self.output_rate
    }

    /// The mailbox through which the engine gets songs opened. Whoever
    /// moves the engine into an audio callback keeps this and calls
    /// `Opener::work` on another thread (`OpenerThread`).
    pub fn opener(&self) -> Arc<Opener> {
        self.opener.clone()
    }

    /// The engine runs at another output rate from now on (the owner
    /// opened the device again). Nothing is audible then: the mix gains
    /// jump to their targets.
    pub fn set_output_rate(&mut self, rate: u32) {
        self.output_rate = rate;
        self.fade_len = ((FADE_SECONDS * rate as f64).round() as usize).max(1);
        self.ramp_len = ((MIX_RAMP_SECONDS * rate as f64).round() as usize).max(1);
        self.ramp_done = self.ramp_len;
        self.ramp_start = self.ramp_target;
        self.gains = self.ramp_target;
        if let Some(song) = self.song.as_mut() {
            song.midi_rate(rate);
        }
        if let Some((_, song)) = self.slot.as_mut() {
            song.midi_rate(rate);
        }
        if let Some(song) = self.song.as_mut() {
            if song.song_rate == rate && song.warnings.iter().any(|w| w.starts_with("Song sample rate")) {
                let kept: Vec<String> =
                    song.warnings.iter().filter(|w| !w.starts_with("Song sample rate")).cloned().collect();
                song.warnings = Arc::new(kept);
            }
        }
    }

    /// Open the tracks of `project` and make it the current song (position
    /// 0, `Stopped`). The previous song is dropped, which ends its thread.
    pub fn load(&mut self, project: &SongProject) {
        let old = self.set_song(LoadedSong::open(project));
        drop(old);
    }

    /// Take a song that was opened elsewhere (`LoadedSong::open`). Returns
    /// the previous one so the caller decides where it is dropped (dropping
    /// joins its thread, which an audio callback must not do). This leaves
    /// the queue: the song is the whole "queue" then.
    pub fn set_song(&mut self, mut song: LoadedSong) -> Option<LoadedSong> {
        self.leave_queue();
        if song.song_rate != self.output_rate {
            Arc::make_mut(&mut song.warnings).push(format!(
                "Song sample rate {} Hz differs from output {} Hz",
                song.song_rate, self.output_rate
            ));
        }
        song.midi_rate(self.output_rate);
        self.state = PlayState::Stopped;
        self.underruns = 0;
        self.midi_sent = 0;
        let old = self.song.replace(song);
        if let Some(o) = &old {
            self.ano_mask |= o.notes_mask();
        }
        old
    }

    /// Remove the current song, see `set_song` about who drops it.
    pub fn take_song(&mut self) -> Option<LoadedSong> {
        self.leave_queue();
        self.state = PlayState::Empty;
        let old = self.song.take();
        if let Some(o) = &old {
            self.ano_mask |= o.notes_mask();
        }
        old
    }

    /// Drop the current song (ends its thread, releases the files).
    pub fn unload(&mut self) {
        drop(self.take_song());
    }

    // -- queue ---------------------------------------------------------------

    /// Forget the queue (not the current song).
    fn leave_queue(&mut self) {
        if let Some((_, s)) = self.slot.take() {
            self.discard(s);
        }
        if !self.queue.is_empty() {
            self.queue = Arc::new(Vec::new());
            self.skipped = Arc::new(Vec::new());
        }
        self.cur = None;
        self.pending = None;
        self.pending_seek = None;
        self.outstanding = None;
        self.countdown = 0;
    }

    /// Hand a replaced song to the garbage list (no allocation, no join).
    fn discard(&mut self, song: LoadedSong) {
        self.ano_mask |= song.notes_mask();
        if self.garbage.len() < GARBAGE_SLOTS {
            self.garbage.push(song);
        }
        // else the song is dropped here, which joins its thread; only if
        // the owner did not empty the list for a long time
    }

    /// A song the engine has replaced, for the owner to drop.
    pub fn take_garbage(&mut self) -> Option<LoadedSong> {
        self.garbage.pop()
    }

    /// Use `entries` as the queue (see `set_queue_arc`).
    pub fn set_queue(&mut self, entries: Vec<QueueEntry>) {
        drop(self.set_queue_arc(Arc::new(entries)));
    }

    /// Use a new queue. The current song is unloaded and the state is
    /// `Empty`; `goto` loads a song. Returns the old queue so the caller
    /// decides where it is dropped.
    pub fn set_queue_arc(&mut self, queue: Arc<Vec<QueueEntry>>) -> Arc<Vec<QueueEntry>> {
        if let Some(s) = self.song.take() {
            self.discard(s);
        }
        if let Some((_, s)) = self.slot.take() {
            self.discard(s);
        }
        self.cur = None;
        self.pending = None;
        self.pending_seek = None;
        self.outstanding = None;
        self.countdown = 0;
        self.state = PlayState::Empty;
        self.underruns = 0;
        self.skipped = Arc::new(skip_messages(&queue));
        std::mem::replace(&mut self.queue, queue)
    }

    /// The playlist was changed: use `queue` instead. If the song that is
    /// loaded is still in it, it goes on undisturbed (same position, same
    /// state, now at its new index) and true is returned. Otherwise the
    /// player stops on the first song (`Stopped`). Returns the old queue.
    pub fn replace_queue(&mut self, queue: Arc<Vec<QueueEntry>>) -> (Arc<Vec<QueueEntry>>, bool) {
        let path = match (self.pending, self.cur) {
            (None, Some(c)) if self.song.is_some() => self.queue.get(c).and_then(|e| e.path()).map(str::to_string),
            _ => None,
        };
        let found = path.and_then(|p| find_path(&queue, &p, self.cur));
        match found {
            Some(i) => {
                if let Some((_, s)) = self.slot.take() {
                    self.discard(s);
                }
                self.outstanding = None;
                self.cur = Some(i);
                self.skipped = Arc::new(skip_messages(&queue));
                (std::mem::replace(&mut self.queue, queue), true)
            }
            None => {
                let old = self.set_queue_arc(queue);
                self.goto(0, false);
                (old, false)
            }
        }
    }

    /// First readable entry at or after `from`.
    fn next_readable(&self, from: usize) -> Option<usize> {
        (from..self.queue.len()).find(|&i| self.queue[i].readable())
    }

    /// Last readable entry before `before`.
    fn prev_readable(&self, before: usize) -> Option<usize> {
        (0..before.min(self.queue.len())).rev().find(|&i| self.queue[i].readable())
    }

    /// Start song `index` at position 0: playing if `autoplay`, else
    /// `Stopped`. Unreadable entries are skipped (forward); nothing happens
    /// if there is no readable one from `index` on.
    pub fn goto(&mut self, index: usize, autoplay: bool) {
        if self.queue.is_empty() {
            return;
        }
        let i = index.min(self.queue.len() - 1);
        if let Some(j) = self.next_readable(i) {
            self.switch_to(j, if autoplay { PlayState::Playing } else { PlayState::Stopped }, 0);
        }
    }

    /// The next song. While playing it starts at once; otherwise it is
    /// loaded at position 0 and the state is `Stopped`. At the last song
    /// nothing happens.
    pub fn next(&mut self) {
        let Some(base) = self.pending.or(self.cur) else { return };
        let Some(n) = self.next_readable(base + 1) else { return };
        let st = if self.state == PlayState::Playing { PlayState::Playing } else { PlayState::Stopped };
        self.switch_to(n, st, 0);
    }

    /// Further than 3 s into the song: back to its start. Otherwise the
    /// song before (state as for `next`); at the first song: its start.
    pub fn prev(&mut self) {
        let Some(base) = self.pending.or(self.cur) else { return };
        let pos = match (&self.song, self.pending) {
            (Some(s), None) => s.shown_pos() as f64 / s.song_rate.max(1) as f64,
            _ => 0.0,
        };
        if pos > PREV_RESTART_SECONDS {
            self.seek(0.0);
            return;
        }
        match self.prev_readable(base) {
            None => self.seek(0.0),
            Some(p) => {
                let st = if self.state == PlayState::Playing { PlayState::Playing } else { PlayState::Stopped };
                self.switch_to(p, st, 0);
            }
        }
    }

    /// Wait with the current song at position 0 (`Waiting`), or let it
    /// start by itself after `countdown` seconds (`Countdown`). For the
    /// owner that opened the output again and carries the queue over.
    pub fn hold(&mut self, countdown: Option<f64>) {
        if self.song.is_none() && self.pending.is_none() {
            return;
        }
        match countdown {
            None => {
                self.state = PlayState::Waiting;
                self.countdown = 0;
            }
            Some(secs) => {
                self.countdown = (secs.max(0.0) * self.output_rate as f64).round() as u64;
                self.state = if self.countdown == 0 { PlayState::Playing } else { PlayState::Countdown };
            }
        }
    }

    /// Make `idx` the current song and enter `state` (`Countdown` with
    /// `countdown` frames; 0 frames start at once).
    fn switch_to(&mut self, idx: usize, state: PlayState, countdown: u64) {
        self.state = if state == PlayState::Countdown && countdown == 0 { PlayState::Playing } else { state };
        self.countdown = countdown;
        if self.pending.is_none() && self.cur == Some(idx) {
            if let Some(song) = self.song.as_mut() {
                // the song that is loaded: from its start
                song.fade = Fade::None;
                song.seek_to(0);
                return;
            }
        }
        if matches!(&self.slot, Some((i, _)) if *i == idx) {
            if let Some((_, new)) = self.slot.take() {
                self.install(idx, new);
            }
            return;
        }
        self.pending = Some(idx);
        self.pending_seek = None;
        self.request(idx, true);
    }

    /// `song` (queue entry `idx`) becomes the current song.
    fn install(&mut self, idx: usize, mut song: LoadedSong) {
        song.midi_rate(self.output_rate);
        self.midi_sent = 0;
        if let Some(old) = self.song.replace(song) {
            self.discard(old);
        }
        self.cur = Some(idx);
        self.pending = None;
        if let Some(seconds) = self.pending_seek.take() {
            self.seek(seconds);
        }
    }

    /// Ask the opener for entry `idx`.
    fn request(&mut self, idx: usize, switch: bool) {
        if let Some(o) = self.outstanding.as_mut() {
            if o.index == idx {
                o.switch |= switch;
                return;
            }
        }
        self.ticket += 1;
        self.outstanding = Some(Outstanding { ticket: self.ticket, index: idx, switch, posted: false });
    }

    /// Hand the request to the opener and take its result. No blocking.
    fn poll_opener(&mut self) {
        if let Some(o) = self.outstanding.as_mut() {
            if !o.posted {
                let request = Request { ticket: o.ticket, queue: self.queue.clone(), index: o.index };
                if self.opener.post(request).is_ok() {
                    o.posted = true;
                }
            }
        }
        if let Some(done) = self.opener.take() {
            match self.outstanding {
                Some(o) if o.ticket == done.ticket => {
                    self.outstanding = None;
                    if o.switch {
                        self.install(done.index, done.song);
                    } else if let Some((_, old)) = self.slot.replace((done.index, done.song)) {
                        self.discard(old);
                    }
                }
                _ => self.discard(done.song),
            }
        }
    }

    /// Open the next song early: in the last 10 s of the current one, or
    /// while waiting for it to start.
    fn maybe_preload(&mut self) {
        if self.pending.is_some() || self.outstanding.is_some() {
            return;
        }
        let (Some(cur), Some(song)) = (self.cur, self.song.as_ref()) else { return };
        let remaining = song.length_frames.saturating_sub(song.pos) as f64 / song.song_rate.max(1) as f64;
        let want = match self.state {
            PlayState::Playing | PlayState::Paused | PlayState::Stopped => remaining < PRELOAD_SECONDS,
            PlayState::Waiting | PlayState::Countdown => true,
            _ => false,
        };
        if !want {
            return;
        }
        let next = self.next_readable(cur + 1);
        let in_slot = self.slot.as_ref().map(|s| s.0);
        if in_slot.is_some() && in_slot != next {
            if let Some((_, s)) = self.slot.take() {
                self.discard(s);
            }
        }
        if let (Some(n), true) = (next, self.slot.is_none()) {
            self.request(n, false);
        }
    }

    /// The current song played to its end: the next readable one is loaded
    /// and waits or counts down; after the last one the state is `Ended`.
    fn end_of_song(&mut self) {
        let next = self.cur.and_then(|c| self.next_readable(c + 1));
        match next {
            None => {
                self.state = PlayState::Ended;
                if let Some(s) = &self.song {
                    self.ano_mask |= s.notes_mask();
                }
            }
            Some(n) => {
                let e = &self.queue[n];
                if e.auto_start() {
                    let frames = (e.delay * self.output_rate as f64).round() as u64;
                    self.switch_to(n, PlayState::Countdown, frames);
                } else {
                    self.switch_to(n, PlayState::Waiting, 0);
                }
            }
        }
    }

    // -- transport -------------------------------------------------------------

    /// Start or continue. Without a song, or if the song rate differs from
    /// the output rate, nothing happens. After the end it starts again at 0
    /// (a queue: at its first readable song). `Waiting` and `Countdown`
    /// start the loaded song now.
    pub fn play(&mut self) {
        if self.pending.is_some() {
            self.state = PlayState::Playing;
            self.countdown = 0;
            return;
        }
        let Some(rate) = self.song.as_ref().map(|s| s.song_rate) else { return };
        if rate != self.output_rate {
            return;
        }
        if self.state == PlayState::Ended {
            if let Some(first) = self.next_readable(0) {
                self.switch_to(first, PlayState::Playing, 0);
                return;
            }
            if let Some(song) = self.song.as_mut() {
                song.fade = Fade::None;
                song.seek_to(0);
            }
        }
        self.countdown = 0;
        self.state = PlayState::Playing;
    }

    pub fn pause(&mut self) {
        match self.state {
            PlayState::Playing => {
                self.state = PlayState::Paused;
                if let Some(song) = &self.song {
                    self.ano_mask |= song.notes_mask();
                }
            }
            // the countdown is cancelled; Play starts the song
            PlayState::Countdown => {
                self.state = PlayState::Waiting;
                self.countdown = 0;
            }
            _ => {}
        }
        if let Some(song) = self.song.as_mut() {
            if matches!(song.fade, Fade::Out(_)) {
                self.ano_mask |= song.finish_fade_out(); // the sound is gone anyway
            }
        }
    }

    /// Jump to `seconds` (clamped to 0 ..= length, rounded to whole frames).
    /// The length itself gives `Ended` (in a queue: the end of the song, so
    /// the next one follows). While playing the sound is faded out
    /// over 5 ms, the position changes, and after a silent wait it is faded
    /// in over 5 ms; the status shows the target at once. Paused, stopped
    /// or ended, only the position changes (`Ended` becomes `Paused` if the
    /// target is before the end). Several calls in a row: the last target
    /// counts. While a song of the queue is still being opened the jump
    /// waits for it.
    pub fn seek(&mut self, seconds: f64) {
        let fade_len = self.fade_len;
        let queued = !self.queue.is_empty();
        if self.pending.is_some() {
            self.pending_seek = Some(seconds);
            return;
        }
        let Some(song) = self.song.as_mut() else { return };
        if self.state == PlayState::Empty {
            return;
        }
        let target = if seconds > 0.0 { (seconds * song.song_rate as f64).round() as u64 } else { 0 };
        let target = target.min(song.length_frames);
        let end = song.length_frames > 0 && target >= song.length_frames;
        // in a queue the end of the song goes on with the next song
        let playing_state = if end && !queued { PlayState::Ended } else { PlayState::Playing };
        match (self.state, song.fade) {
            // a fade-out is running: only the target changes
            (_, Fade::Out(_)) => {
                song.pending = target;
                self.state = playing_state;
            }
            (PlayState::Playing, fade) => {
                self.state = playing_state;
                if !song.primed {
                    // nothing audible yet: jump at once
                    song.seek_to(target);
                } else {
                    // from a fade-in the fade-out continues at the same gain
                    let done = match fade {
                        Fade::In(d) => fade_len.saturating_sub(d),
                        _ => 0,
                    };
                    song.pending = target;
                    if done >= fade_len {
                        self.ano_mask |= song.finish_fade_out();
                    } else {
                        song.fade = Fade::Out(done);
                    }
                }
            }
            (state, _) => {
                song.fade = Fade::None;
                song.seek_to(target);
                if end && queued {
                    self.end_of_song();
                } else {
                    self.state = if end {
                        PlayState::Ended
                    } else if state == PlayState::Ended {
                        PlayState::Paused
                    } else {
                        state
                    };
                }
            }
        }
    }

    /// Use a new rehearsal mix. While playing, the gains of every strip
    /// and of the master move linearly to their new values over
    /// round(10 ms x rate) frames, from where they are now; otherwise
    /// nothing is audible and they jump at once. No allocation.
    pub fn set_mix(&mut self, mix: &MixParams) {
        let (strips, master) = mix.gains();
        self.ramp_target = Gains { strips, master };
        if self.state == PlayState::Playing && self.song.is_some() {
            self.ramp_start = self.gains;
            self.ramp_done = 0;
        } else {
            self.gains = self.ramp_target;
            self.ramp_start = self.ramp_target;
            self.ramp_done = self.ramp_len;
        }
    }

    /// Clear the clip flag of the master.
    pub fn reset_clip(&mut self) {
        self.clip = false;
    }

    /// Pause plus position 0 (also ends a countdown). No fade: silence
    /// follows anyway.
    pub fn stop(&mut self) {
        if let Some(song) = self.song.as_mut() {
            if self.state != PlayState::Stopped {
                self.ano_mask |= song.notes_mask();
            }
            song.fade = Fade::None;
            song.seek_to(0);
            self.state = PlayState::Stopped;
            self.countdown = 0;
        }
    }

    // -- rendering -------------------------------------------------------------

    /// Fill `out` (interleaved stereo, 2 x frames) completely: the mix
    /// while playing, silence otherwise. No allocation, no file access, no
    /// blocking lock.
    ///
    /// Returns the MIDI messages of the block, each with its frame in the
    /// block (`MidiMsg::offset`), in the order of the file. A message that
    /// is due on frame f of the song comes with the block that contains f.
    /// After a jump (and when a song starts from a position after 0) the
    /// block in which the audio starts again begins with the chasing
    /// messages at offset 0; All Notes Off (after a stop, a pause, a jump
    /// or a song change) is at offset 0 of the next block. The slice is
    /// valid until the next call.
    pub fn render(&mut self, out: &mut [f32]) -> &[MidiMsg] {
        out.fill(0.0);
        self.midi.clear();
        if self.ano_mask != 0 {
            self.midi.all_notes_off(self.ano_mask, 0);
            self.ano_mask = 0;
        }
        self.poll_opener();
        self.maybe_preload();
        let frames = out.len() / 2;
        let mut done = 0;
        while done < frames {
            if self.pending.is_some() {
                // the next song is not here yet: silence, the countdown goes on
                if self.state == PlayState::Countdown {
                    self.countdown = self.countdown.saturating_sub((frames - done) as u64);
                    if self.countdown == 0 {
                        self.state = PlayState::Playing;
                    }
                }
                break;
            }
            // a jump to the end sets `Ended` at once but still fades out
            let fading_out = self.song.as_ref().map(|s| matches!(s.fade, Fade::Out(_))).unwrap_or(false);
            if self.state == PlayState::Playing || fading_out {
                let (used, ended) = self.render_playing(&mut out[done * 2..], done);
                done += used;
                if ended {
                    self.end_of_song();
                } else {
                    break;
                }
            } else if self.state == PlayState::Countdown {
                let n = ((frames - done) as u64).min(self.countdown);
                self.countdown -= n;
                done += n as usize;
                if self.countdown == 0 {
                    self.state = PlayState::Playing;
                }
            } else {
                // keep the tracks in step, so `wait_ready` sees the progress
                if let Some(song) = self.song.as_mut() {
                    song.filled(1);
                }
                break;
            }
        }
        if !self.midi.is_empty() {
            self.midi_sent += self.midi.len() as u64;
            self.last_midi = Some(Instant::now());
        }
        self.midi.as_slice()
    }

    /// Mix the current song into `out`. Returns the frames of `out` that
    /// belong to this song and whether the song has ended (the rest of
    /// `out` is then for what follows). `base`: the frame of the block that
    /// `out` starts at (for the offsets of the MIDI messages).
    fn render_playing(&mut self, out: &mut [f32], base: usize) -> (usize, bool) {
        let fade_len = self.fade_len;
        let frames = out.len() / 2;
        let Some(song) = self.song.as_mut() else { return (frames, false) };
        if song.song_rate != self.output_rate {
            return (frames, false); // waiting for an output with the song's rate
        }
        if !song.primed {
            // 0.5 s (a quarter of the ring) is enough to start
            if !song.filled(4) {
                return (frames, false); // waiting is no dropout
            }
            song.primed = true;
            if let Some(m) = song.midi.as_mut() {
                m.begin(song.pos, &mut self.chase, &mut self.midi, base);
            }
            if song.fade == Fade::Wait {
                song.fade = Fade::In(0);
            }
        }
        let mut done = 0;
        let mut underrun = false;
        while done < frames && song.pos < song.length_frames {
            let mut m = (frames - done).min(MIX_CHUNK).min((song.length_frames - song.pos) as usize);
            if let Fade::Out(d) | Fade::In(d) = song.fade {
                m = m.min(fade_len - d);
            }
            if let Some(mt) = song.midi.as_mut() {
                mt.emit(song.pos, song.pos + m as u64, &mut self.midi, base + done);
            }
            for c in 0..STRIPS {
                if song.strip_mask & (1 << c) != 0 {
                    self.chan[c * MIX_CHUNK..c * MIX_CHUNK + m].fill(0.0);
                }
            }
            for t in song.tracks.iter_mut() {
                let k = (t.frames.saturating_sub(song.pos) as usize).min(m);
                if k == 0 {
                    continue; // this track is over
                }
                t.pay_debt();
                if t.debt == 0 && t.consumer.slots() >= k {
                    t.add_to(k, &mut self.chan[t.channel * MIX_CHUNK..]);
                } else {
                    // silence for this block; the frames due now are
                    // skipped as soon as they arrive, keeping the sync
                    let take = if t.debt == 0 { t.consumer.slots().min(k) } else { 0 };
                    t.pop(take);
                    t.debt += k - take;
                    underrun = true;
                }
            }
            for i in 0..m {
                if self.ramp_done < self.ramp_len {
                    self.ramp_done += 1;
                    self.gains = if self.ramp_done == self.ramp_len {
                        self.ramp_target
                    } else {
                        Gains::lerp(&self.ramp_start, &self.ramp_target, self.ramp_done as f32 / self.ramp_len as f32)
                    };
                }
                let (mut l, mut r) = (0.0f32, 0.0f32);
                for c in 0..STRIPS {
                    if song.strip_mask & (1 << c) == 0 {
                        continue;
                    }
                    let x = self.chan[c * MIX_CHUNK + i];
                    let g = &self.gains.strips[c];
                    self.peaks.strips[c] = self.peaks.strips[c].max((x * g.level).abs());
                    l += x * g.left;
                    r += x * g.right;
                }
                l *= self.gains.master;
                r *= self.gains.master;
                let f = match song.fade {
                    Fade::Out(d) => 1.0 - (d + i) as f32 / fade_len as f32,
                    Fade::In(d) => (d + i) as f32 / fade_len as f32,
                    _ => 1.0,
                };
                l *= f;
                r *= f;
                if l.abs() > 1.0 || r.abs() > 1.0 {
                    self.clip = true;
                }
                let (l, r) = (l.clamp(-1.0, 1.0), r.clamp(-1.0, 1.0));
                self.peaks.master[0] = self.peaks.master[0].max(l.abs());
                self.peaks.master[1] = self.peaks.master[1].max(r.abs());
                let o = &mut out[(done + i) * 2..(done + i) * 2 + 2];
                o[0] = l;
                o[1] = r;
            }
            song.pos += m as u64;
            done += m;
            match song.fade {
                Fade::Out(d) if d + m >= fade_len => {
                    self.ano_mask |= song.finish_fade_out();
                    break; // silence until the new position is ready
                }
                Fade::Out(d) => song.fade = Fade::Out(d + m),
                Fade::In(d) if d + m >= fade_len => song.fade = Fade::None,
                Fade::In(d) => song.fade = Fade::In(d + m),
                _ => {}
            }
        }
        if underrun {
            self.underruns += 1;
        }
        if matches!(song.fade, Fade::Out(_)) && song.pos >= song.length_frames {
            self.ano_mask |= song.finish_fade_out(); // the song ended during the fade-out
            (frames, false)
        } else if song.fade != Fade::Wait && song.pos >= song.length_frames {
            (done, true)
        } else {
            (frames, false)
        }
    }

    /// The status for the audio callback: like `status`, and the peaks start
    /// again (call it only when the status is really handed on).
    pub fn take_status(&mut self) -> Status {
        let st = self.status();
        self.peaks.clear_peaks();
        st
    }

    pub fn status(&self) -> Status {
        let levels = Levels { clip: self.clip, ..self.peaks };
        let index = self.pending.or(self.cur);
        let mut st = match &self.song {
            None => Status {
                state: self.state,
                song_name: self.no_name.clone(),
                position_frames: 0,
                position_seconds: 0.0,
                length_frames: 0,
                length_seconds: 0.0,
                output_rate: self.output_rate,
                song_rate: 0,
                underruns: self.underruns,
                warnings: self.no_warnings.clone(),
                levels,
                index: None,
                next_index: None,
                queue_len: 0,
                countdown_seconds: 0.0,
                needs_rate: None,
                queue_warnings: self.no_warnings.clone(),
                midi: MidiStatus { has_file: false, events_sent: self.midi_sent, last_event: self.last_midi },
            },
            Some(s) => {
                let rate = s.song_rate.max(1) as f64;
                // while the next song is opening: position 0, length unknown
                let opening = self.pending.is_some();
                let (pos, len) = if opening { (0, 0) } else { (s.shown_pos(), s.length_frames) };
                let rate_differs = !opening && s.song_rate != 0 && s.song_rate != self.output_rate;
                Status {
                    state: self.state,
                    song_name: s.name.clone(),
                    position_frames: pos,
                    position_seconds: pos as f64 / rate,
                    length_frames: len,
                    length_seconds: len as f64 / rate,
                    output_rate: self.output_rate,
                    song_rate: s.song_rate,
                    underruns: self.underruns,
                    warnings: s.warnings.clone(),
                    levels,
                    index: None,
                    next_index: None,
                    queue_len: 0,
                    countdown_seconds: 0.0,
                    needs_rate: if rate_differs { Some(s.song_rate) } else { None },
                    queue_warnings: self.no_warnings.clone(),
                    midi: MidiStatus {
                        has_file: !opening && s.midi.is_some(),
                        events_sent: self.midi_sent,
                        last_event: self.last_midi,
                    },
                }
            }
        };
        st.index = index;
        st.next_index = index.and_then(|i| self.next_readable(i + 1));
        st.queue_len = self.queue.len();
        st.countdown_seconds = self.countdown as f64 / self.output_rate.max(1) as f64;
        st.queue_warnings = self.skipped.clone();
        st
    }

    /// Let the opener finish what the engine asked for (no waiting for the
    /// buffers to fill). For the owner of an engine that is not in a
    /// callback.
    pub fn settle(&mut self) {
        for _ in 0..5000 {
            self.poll_opener();
            if self.outstanding.is_none() {
                return;
            }
            if !self.opener.work() {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
    }

    /// Blocks until the ring of every track (of the current song, and of
    /// the one opened early) is full or at its file end, or `timeout` has
    /// passed (false). Also opens the songs the engine asked for. For tests
    /// and for the start after `load` or a position change; never call it
    /// from the audio callback.
    pub fn wait_ready(&mut self, timeout: Duration) -> bool {
        let start = Instant::now();
        loop {
            self.opener.work();
            self.poll_opener();
            self.maybe_preload();
            if self.ready() {
                return true;
            }
            if start.elapsed() >= timeout {
                return false;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn ready(&mut self) -> bool {
        if self.pending.is_some() || self.outstanding.is_some() {
            return false;
        }
        let mut ok = true;
        if let Some(song) = self.song.as_mut() {
            ok &= song.filled(1);
        }
        if let Some((_, song)) = self.slot.as_mut() {
            ok &= song.filled(1);
        }
        ok
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::tests::temp_dir;
    use crate::{generate_songprj, ProjectTrack};
    use std::fs;
    use std::path::Path;

    pub(crate) const RATE: u32 = 48000;
    const WAIT: Duration = Duration::from_secs(10);
    const G: f32 = 0.707_106_77;

    /// 24 bit WAV with the given integer samples
    pub(crate) fn wav24(dir: &Path, name: &str, rate: u32, channels: u16, samples: &[i32]) -> String {
        let data_len = samples.len() * 3;
        let mut b = Vec::new();
        b.extend_from_slice(b"RIFF");
        b.extend_from_slice(&(36 + data_len as u32).to_le_bytes());
        b.extend_from_slice(b"WAVEfmt ");
        b.extend_from_slice(&16u32.to_le_bytes());
        b.extend_from_slice(&1u16.to_le_bytes());
        b.extend_from_slice(&channels.to_le_bytes());
        b.extend_from_slice(&rate.to_le_bytes());
        b.extend_from_slice(&(rate * 3 * channels as u32).to_le_bytes());
        b.extend_from_slice(&(3 * channels).to_le_bytes());
        b.extend_from_slice(&24u16.to_le_bytes());
        b.extend_from_slice(b"data");
        b.extend_from_slice(&(data_len as u32).to_le_bytes());
        for s in samples {
            b.extend_from_slice(&s.to_le_bytes()[..3]);
        }
        let p = dir.join(format!("{}.wav", name));
        fs::write(&p, b).unwrap();
        p.to_str().unwrap().to_string()
    }

    /// constant value `v` (a fraction of full scale, exact in 24 bit)
    pub(crate) fn constant(frames: usize, v: f32) -> Vec<i32> {
        vec![(v * 8388608.0) as i32; frames]
    }

    /// 0, 1/32768, 2/32768, ... (exact in 24 bit and f32)
    fn ramp(frames: usize) -> Vec<i32> {
        (0..frames as i32).map(|i| i * 256).collect()
    }

    fn ramp_value(i: usize) -> f32 {
        i as f32 / 32768.0
    }

    /// a .songprj written with the library's generator and read back
    pub(crate) fn song(dir: &Path, wavs: &[&str]) -> SongProject {
        let audio_lib = dir.to_str().unwrap();
        let xml = generate_songprj("Test Song", 0, RATE, 24, 1);
        let mut prj = SongProject::parse(&xml, audio_lib);
        prj.tracks = wavs.iter().enumerate().map(|(i, w)| ProjectTrack::new(i as i32, w, audio_lib)).collect();
        let file = dir.join("test.songprj");
        prj.save_as(file.to_str().unwrap()).unwrap();
        SongProject::load(file.to_str().unwrap(), audio_lib).unwrap()
    }

    fn ready_engine(prj: &SongProject) -> Engine {
        let mut e = Engine::new(RATE);
        e.load(prj);
        assert!(e.wait_ready(WAIT));
        e
    }

    fn render(e: &mut Engine, frames: usize) -> Vec<f32> {
        let mut out = vec![9.0; frames * 2];
        e.render(&mut out);
        out
    }

    #[test]
    fn two_constant_tracks_mix_at_centre() {
        let dir = temp_dir("rh-mix");
        let a = wav24(&dir, "a", RATE, 1, &constant(1000, 0.25));
        let b = wav24(&dir, "b", RATE, 1, &constant(1000, 0.125));
        let mut e = ready_engine(&song(&dir, &[&a, &b]));
        e.play();
        let out = render(&mut e, 500);
        let want = (0.25 + 0.125) * G;
        for s in &out {
            assert!((s - want).abs() < 1e-6, "{} vs {}", s, want);
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn mix_is_limited_to_full_scale() {
        let dir = temp_dir("rh-clip");
        let a = wav24(&dir, "a", RATE, 1, &constant(100, 0.99));
        let b = wav24(&dir, "b", RATE, 1, &constant(100, 0.99));
        let mut e = ready_engine(&song(&dir, &[&a, &b]));
        e.play();
        assert!(render(&mut e, 50).iter().all(|&s| s == 1.0));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn block_size_does_not_change_the_samples() {
        let dir = temp_dir("rh-blocks");
        let a = wav24(&dir, "a", RATE, 1, &ramp(5000));
        let b = wav24(&dir, "b", RATE, 1, &constant(3000, 0.1));
        let prj = song(&dir, &[&a, &b]);
        let play_all = |block: usize| {
            let mut e = ready_engine(&prj);
            e.play();
            let mut all = Vec::new();
            while all.len() < 2 * 6000 {
                all.extend(render(&mut e, block));
            }
            all.truncate(2 * 6000);
            all.iter().map(|s| s.to_bits()).collect::<Vec<_>>()
        };
        let reference = play_all(6000);
        assert_ne!(reference[2 * 4000], 0, "the test needs real samples");
        for block in [1, 37, 512] {
            assert_eq!(play_all(block), reference, "block size {}", block);
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn shorter_track_ends_alone_and_the_longest_ends_the_song() {
        let dir = temp_dir("rh-lengths");
        let a = wav24(&dir, "a", RATE, 1, &constant(100, 0.25));
        let b = wav24(&dir, "b", RATE, 1, &constant(250, 0.125));
        let mut e = ready_engine(&song(&dir, &[&a, &b]));
        assert_eq!(e.status().length_frames, 250);
        e.play();
        let out = render(&mut e, 100);
        assert!((out[0] - 0.375 * G).abs() < 1e-6);
        assert_eq!(e.status().state, PlayState::Playing);
        let out = render(&mut e, 150);
        assert!((out[0] - 0.125 * G).abs() < 1e-6, "only the share of a is gone");
        assert!((out[2 * 149] - 0.125 * G).abs() < 1e-6);
        assert_eq!(e.status().state, PlayState::Ended);
        assert!(render(&mut e, 64).iter().all(|&s| s == 0.0));
        assert_eq!(e.status().position_frames, 250);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_wav_warns_with_the_track_name_and_the_others_play() {
        let dir = temp_dir("rh-missing");
        let a = wav24(&dir, "guitar", RATE, 1, &constant(100, 0.25));
        let gone = dir.join("bass.wav").to_str().unwrap().to_string();
        let mut e = ready_engine(&song(&dir, &[&gone, &a]));
        let st = e.status();
        assert_eq!(st.warnings.len(), 1);
        assert!(st.warnings[0].contains("bass"), "{}", st.warnings[0]);
        e.play();
        let out = render(&mut e, 10);
        assert!((out[0] - 0.25 * G).abs() < 1e-6);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn stereo_wav_warns_and_stays_silent() {
        let dir = temp_dir("rh-stereo");
        let a = wav24(&dir, "mono", RATE, 1, &constant(100, 0.25));
        let s = wav24(&dir, "pad", RATE, 2, &constant(200, 0.5));
        let mut e = ready_engine(&song(&dir, &[&a, &s]));
        let st = e.status();
        assert_eq!(st.warnings.len(), 1);
        assert!(st.warnings[0].contains("pad") && st.warnings[0].contains("mono"));
        assert_eq!(st.length_frames, 100, "the silent track does not count");
        e.play();
        let out = render(&mut e, 10);
        assert!((out[0] - 0.25 * G).abs() < 1e-6);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn pause_is_silent_and_play_continues_without_a_gap() {
        let dir = temp_dir("rh-pause");
        let a = wav24(&dir, "a", RATE, 1, &ramp(2000));
        let mut e = ready_engine(&song(&dir, &[&a]));
        e.play();
        let first = render(&mut e, 300);
        e.pause();
        assert_eq!(e.status().state, PlayState::Paused);
        assert!(render(&mut e, 500).iter().all(|&s| s == 0.0));
        assert_eq!(e.status().position_frames, 300);
        e.play();
        let second = render(&mut e, 300);
        for i in 0..300 {
            assert_eq!(first[2 * i], ramp_value(i) * G);
            assert_eq!(second[2 * i], ramp_value(300 + i) * G);
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn stop_returns_to_the_start() {
        let dir = temp_dir("rh-stop");
        let a = wav24(&dir, "a", RATE, 1, &ramp(2000));
        let mut e = ready_engine(&song(&dir, &[&a]));
        e.play();
        render(&mut e, 700);
        assert_eq!(e.status().position_frames, 700);
        e.stop();
        assert_eq!(e.status().state, PlayState::Stopped);
        assert_eq!(e.status().position_frames, 0);
        assert!(e.wait_ready(WAIT));
        assert!(render(&mut e, 10).iter().all(|&s| s == 0.0), "stopped is silent");
        e.play();
        let out = render(&mut e, 20);
        for i in 0..20 {
            assert_eq!(out[2 * i], ramp_value(i) * G);
        }
        // and after the end, play starts over
        render(&mut e, 5000);
        assert_eq!(e.status().state, PlayState::Ended);
        e.play();
        assert!(e.wait_ready(WAIT));
        let out = render(&mut e, 5);
        assert_eq!(out[2], ramp_value(1) * G);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn stop_discards_a_full_ring_of_the_old_position() {
        let dir = temp_dir("rh-stop-ring");
        let saw: Vec<i32> = (0..3 * 48000).map(|i| (i % 32768) * 256).collect();
        let a = wav24(&dir, "a", RATE, 1, &saw);
        let mut e = ready_engine(&song(&dir, &[&a]));
        for round in 0..3 {
            e.play();
            render(&mut e, 10000 + round * 777);
            e.wait_ready(WAIT); // the ring is full of frames far from 0
            e.stop();
            assert!(e.wait_ready(WAIT));
            e.play();
            let out = render(&mut e, 100);
            for i in 0..100 {
                assert_eq!(out[2 * i], ramp_value(i) * G, "round {} frame {}", round, i);
            }
            assert_eq!(e.status().position_frames, 100);
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn position_counts_the_played_frames() {
        let dir = temp_dir("rh-pos");
        let a = wav24(&dir, "a", RATE, 1, &ramp(48000));
        let mut e = ready_engine(&song(&dir, &[&a]));
        let st = e.status();
        assert_eq!((st.state, st.position_frames, st.length_frames), (PlayState::Stopped, 0, 48000));
        assert_eq!((st.song_rate, st.output_rate), (RATE, RATE));
        assert_eq!(&*st.song_name, "Test Song");
        e.play();
        let mut played = 0;
        for block in [1, 37, 512, 1000] {
            render(&mut e, block);
            played += block as u64;
            assert_eq!(e.status().position_frames, played);
        }
        let st = e.status();
        assert!((st.position_seconds - played as f64 / 48000.0).abs() < 1e-9);
        assert!((st.length_seconds - 1.0).abs() < 1e-9);
        assert_eq!(st.underruns, 0);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn long_song_survives_a_ring_refill() {
        // 3 s are more than the 2 s ring: the reader thread has to refill
        let dir = temp_dir("rh-refill");
        // a saw that stays inside 24 bit
        let saw: Vec<i32> = (0..3 * 48000).map(|i| (i % 32768) * 256).collect();
        let a = wav24(&dir, "a", RATE, 1, &saw);
        let mut e = ready_engine(&song(&dir, &[&a]));
        e.play();
        let mut all = Vec::new();
        while e.status().state == PlayState::Playing {
            all.extend(render(&mut e, 4800));
            // like a real device: the thread gets time between blocks
            e.wait_ready(WAIT);
        }
        assert_eq!(all.len(), 2 * 3 * 48000);
        for i in (0..3 * 48000).step_by(97) {
            assert_eq!(all[2 * i], ramp_value(i % 32768) * G, "frame {}", i);
        }
        assert_eq!(e.status().underruns, 0);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn other_sample_rate_does_not_play() {
        let dir = temp_dir("rh-rate");
        let a = wav24(&dir, "a", 44100, 1, &constant(1000, 0.25));
        let mut e = ready_engine(&song(&dir, &[&a]));
        e.play();
        assert_eq!(e.status().state, PlayState::Stopped);
        assert!(render(&mut e, 100).iter().all(|&s| s == 0.0));
        let st = e.status();
        assert_eq!(st.position_frames, 0);
        assert_eq!(st.song_rate, 44100);
        assert!(st.warnings.iter().any(|w| w == "Song sample rate 44100 Hz differs from output 48000 Hz"));
        let _ = fs::remove_dir_all(&dir);
    }

    /// a song of another rate: the player opens a new stream and with it a
    /// new engine with the song's rate, and then plays without a warning
    #[test]
    fn an_engine_of_the_song_rate_plays_without_a_rate_warning() {
        let dir = temp_dir("rh-newrate");
        let a = wav24(&dir, "a", 44100, 1, &constant(2000, 0.25));
        let prj = song(&dir, &[&a]);
        let first = LoadedSong::open(&prj);
        assert_eq!(first.rate(), 44100);

        let mut old = Engine::new(RATE);
        assert!(old.set_song(LoadedSong::open(&prj)).is_none());
        assert_eq!(old.status().warnings.len(), 1, "the old rate warns");

        let mut e = Engine::new(first.rate());
        assert!(e.set_song(first).is_none());
        assert!(e.wait_ready(WAIT));
        let st = e.status();
        assert_eq!((st.output_rate, st.song_rate), (44100, 44100));
        assert!(st.warnings.is_empty(), "{:?}", st.warnings);
        e.play();
        assert_eq!(e.status().state, PlayState::Playing);
        let out = render(&mut e, 100);
        assert!((out[0] - 0.25 * G).abs() < 1e-6 && (out[199] - 0.25 * G).abs() < 1e-6);
        assert_eq!(e.status().position_frames, 100);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn track_with_another_rate_is_silent_with_a_warning() {
        let dir = temp_dir("rh-rate2");
        let a = wav24(&dir, "a", RATE, 1, &constant(100, 0.25));
        let b = wav24(&dir, "b", 44100, 1, &constant(100, 0.25));
        let mut e = ready_engine(&song(&dir, &[&a, &b]));
        assert_eq!(e.status().warnings.len(), 1);
        e.play();
        assert!((render(&mut e, 4)[0] - 0.25 * G).abs() < 1e-6);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn loading_another_song_ends_the_old_thread() {
        let dir = temp_dir("rh-reload");
        let a = wav24(&dir, "a", RATE, 1, &constant(1000, 0.25));
        let b = wav24(&dir, "b", RATE, 1, &constant(1000, 0.5));
        let mut e = ready_engine(&song(&dir, &[&a]));
        e.load(&song(&dir, &[&b]));
        assert!(e.wait_ready(WAIT));
        // the old files are free again (Windows refuses to delete open files)
        fs::remove_file(&a).expect("the first song's file is still open");
        e.play();
        assert!((render(&mut e, 4)[0] - 0.5 * G).abs() < 1e-6);
        e.unload();
        assert_eq!(e.status().state, PlayState::Empty);
        fs::remove_file(&b).expect("the second song's file is still open");
        assert!(render(&mut e, 4).iter().all(|&s| s == 0.0));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_engine_renders_silence() {
        let mut e = Engine::new(RATE);
        e.play();
        assert_eq!(e.status().state, PlayState::Empty);
        assert!(render(&mut e, 10).iter().all(|&s| s == 0.0));
        assert!(e.wait_ready(Duration::from_millis(1)));
    }

    // ---- seek ----

    const FADE: usize = 240; // round(0.005 x 48000)

    /// like `ramp`, but never 0 (so zeros mean silence)
    fn ramp1(frames: usize) -> Vec<i32> {
        (0..frames as i32).map(|i| (i + 1) * 256).collect()
    }

    fn ramp1_value(i: usize) -> f32 {
        (i + 1) as f32 / 32768.0
    }

    /// exactly `total` frames in blocks of `block`
    fn render_exact(e: &mut Engine, total: usize, block: usize) -> Vec<f32> {
        let mut all = Vec::new();
        while all.len() < 2 * total {
            let n = block.min(total - all.len() / 2);
            all.extend(render(e, n));
        }
        all
    }

    #[test]
    fn seek_while_stopped_gives_the_samples_from_the_target() {
        let dir = temp_dir("rh-seek-stopped");
        let a = wav24(&dir, "a", RATE, 1, &ramp(20000));
        let mut e = ready_engine(&song(&dir, &[&a]));
        e.seek(0.1);
        let st = e.status();
        assert_eq!((st.state, st.position_frames), (PlayState::Stopped, 4800));
        assert!(e.wait_ready(WAIT));
        e.play();
        let out = render(&mut e, 300);
        for i in 0..300 {
            assert_eq!(out[2 * i], ramp_value(4800 + i) * G, "frame {}", i);
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn seek_rounds_to_whole_frames() {
        let dir = temp_dir("rh-seek-round");
        let a = wav24(&dir, "a", RATE, 1, &ramp(20000));
        let mut e = ready_engine(&song(&dir, &[&a]));
        e.seek(0.1 + 0.4 / 48000.0);
        assert_eq!(e.status().position_frames, 4800);
        e.seek(0.1 + 0.6 / 48000.0);
        assert_eq!(e.status().position_frames, 4801);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn seek_while_playing_fades_out_and_in_over_5_ms() {
        let dir = temp_dir("rh-seek-fade");
        let a = wav24(&dir, "a", RATE, 1, &constant(30000, 0.5));
        let mut e = ready_engine(&song(&dir, &[&a]));
        let level = 0.5 * G;
        e.play();
        let mut all = render_exact(&mut e, 1000, 100);
        e.seek(0.25);
        // the status shows the target at once
        assert_eq!(e.status().position_frames, 12000);
        // like a device: small blocks, the reader thread gets time
        let start = Instant::now();
        while all.len() < 2 * (1000 + FADE + 20) || all.last() != Some(&level) {
            all.extend(render(&mut e, 16));
            assert!(start.elapsed() < WAIT, "the engine never came back");
        }
        let frames: Vec<f32> = all.chunks(2).map(|c| c[0]).collect();
        for (i, v) in frames.iter().take(1000).enumerate() {
            assert!((v - level).abs() < 1e-6, "before the jump, frame {}", i);
        }
        // fade out: exactly FADE frames, linear, from the full level
        for i in 0..FADE {
            let want = level * (1.0 - i as f32 / FADE as f32);
            assert!((frames[1000 + i] - want).abs() < 1e-6, "fade-out frame {}", i);
        }
        // silence, then the fade-in starts at 0 and takes exactly FADE frames
        let first = 1000 + FADE + frames[1000 + FADE..].iter().position(|&v| v > 0.0).unwrap();
        assert!(frames[1000 + FADE..first].iter().all(|&v| v == 0.0));
        for i in 0..FADE {
            let want = level * i as f32 / FADE as f32;
            assert!((frames[first - 1 + i] - want).abs() < 1e-6, "fade-in frame {}", i);
        }
        assert!((frames[first - 1 + FADE] - level).abs() < 1e-6);
        // no step larger than 1/240 of the level
        let max_step = frames.windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0f32, f32::max);
        assert!(max_step <= level / FADE as f32 + 1e-6, "step {}", max_step);
        let st = e.status();
        assert_eq!(st.underruns, 0, "the wait after the jump is no dropout");
        assert_eq!(st.state, PlayState::Playing);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn seek_while_playing_continues_at_the_target() {
        let dir = temp_dir("rh-seek-play");
        let a = wav24(&dir, "a", RATE, 1, &ramp(20000));
        let mut e = ready_engine(&song(&dir, &[&a]));
        e.play();
        render_exact(&mut e, 500, 500);
        e.seek(0.3);
        render_exact(&mut e, FADE, 64);
        assert!(e.wait_ready(WAIT));
        let out = render(&mut e, 1000);
        for i in 0..1000 {
            let gain = if i < FADE { i as f32 / FADE as f32 } else { 1.0 };
            let want = ramp_value(14400 + i) * G * gain;
            assert!((out[2 * i] - want).abs() < 1e-5, "frame {}: {} vs {}", i, out[2 * i], want);
        }
        assert_eq!(e.status().position_frames, 14400 + 1000);
        assert_eq!(e.status().underruns, 0);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn seek_beyond_the_length_ends_and_negative_goes_to_the_start() {
        let dir = temp_dir("rh-seek-clamp");
        let a = wav24(&dir, "a", RATE, 1, &ramp(20000));
        let mut e = ready_engine(&song(&dir, &[&a]));
        e.seek(1000.0);
        let st = e.status();
        assert_eq!((st.state, st.position_frames), (PlayState::Ended, 20000));
        e.seek(-5.0);
        let st = e.status();
        assert_eq!(st.position_frames, 0);
        assert_eq!(st.state, PlayState::Paused, "from Ended with a target before the end");
        e.seek(f64::NAN);
        assert_eq!(e.status().position_frames, 0);
        // exactly the length is the end, also while playing
        assert!(e.wait_ready(WAIT));
        e.play();
        render_exact(&mut e, 100, 100);
        e.seek(20000.0 / 48000.0);
        assert_eq!(e.status().state, PlayState::Ended);
        render_exact(&mut e, FADE, 64);
        assert!(render(&mut e, 50).iter().all(|&s| s == 0.0));
        let st = e.status();
        assert_eq!((st.state, st.position_frames), (PlayState::Ended, 20000));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn seek_while_paused_keeps_the_state_and_play_continues_there() {
        let dir = temp_dir("rh-seek-paused");
        let a = wav24(&dir, "a", RATE, 1, &ramp(20000));
        let mut e = ready_engine(&song(&dir, &[&a]));
        e.play();
        render_exact(&mut e, 200, 100);
        e.pause();
        e.seek(0.2);
        let st = e.status();
        assert_eq!((st.state, st.position_frames), (PlayState::Paused, 9600));
        assert!(render(&mut e, 100).iter().all(|&s| s == 0.0));
        assert!(e.wait_ready(WAIT));
        e.play();
        let out = render(&mut e, 50);
        for i in 0..50 {
            assert_eq!(out[2 * i], ramp_value(9600 + i) * G, "frame {}", i);
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_last_of_several_seeks_counts_with_one_fade() {
        let dir = temp_dir("rh-seek-many");
        let a = wav24(&dir, "a", RATE, 1, &ramp1(20000));
        let mut e = ready_engine(&song(&dir, &[&a]));
        e.play();
        render_exact(&mut e, 1000, 100);
        e.seek(0.1);
        e.seek(0.2);
        e.seek(0.3);
        assert_eq!(e.status().position_frames, 14400);
        let mut all = render_exact(&mut e, FADE, 64);
        assert!(e.wait_ready(WAIT));
        all.extend(render_exact(&mut e, 600, 64));
        // after the fade-out: the fade-in of the last target, once
        let frames: Vec<f32> = all.chunks(2).map(|c| c[0]).collect();
        let first = frames[FADE..].iter().position(|&v| v != 0.0);
        assert_eq!(first, Some(1), "frame 0 of the fade-in is 0, then it rises");
        assert!((frames[FADE + 300] - ramp1_value(14400 + 300) * G).abs() < 1e-5);
        assert_eq!(e.status().position_frames, 14400 + 600);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn block_size_does_not_change_the_samples_across_a_jump() {
        let dir = temp_dir("rh-seek-blocks");
        let a = wav24(&dir, "a", RATE, 1, &ramp1(20000));
        let prj = song(&dir, &[&a]);
        let run = |block: usize| {
            let mut e = ready_engine(&prj);
            e.play();
            let mut all = render_exact(&mut e, 1000, block);
            e.seek(0.25);
            all.extend(render_exact(&mut e, FADE, block));
            assert!(e.wait_ready(WAIT));
            all.extend(render_exact(&mut e, 2000, block));
            // the silent frames differ in number; everything audible must not
            all.iter().filter(|&&s| s != 0.0).map(|s| s.to_bits()).collect::<Vec<_>>()
        };
        let reference = run(3000);
        assert!(reference.len() > 3000);
        for block in [1, 37, 512] {
            assert_eq!(run(block), reference, "block size {}", block);
        }
        let _ = fs::remove_dir_all(&dir);
    }

    // ---- rehearsal mix ----

    use crate::mix::{MixParams, Strip};

    /// a ready, playing engine for tracks of constant values `vals` on the
    /// strips `channels`, `frames` long each, with `mix`
    fn mix_engine(name: &str, vals: &[f32], channels: &[i32], frames: usize, mix: &MixParams) -> Engine {
        let dir = temp_dir(name);
        let wavs: Vec<String> =
            vals.iter().enumerate().map(|(i, v)| wav24(&dir, &format!("t{}", i), RATE, 1, &constant(frames, *v))).collect();
        let refs: Vec<&str> = wavs.iter().map(|s| s.as_str()).collect();
        let mut prj = song(&dir, &refs);
        for (t, c) in prj.tracks.iter_mut().zip(channels) {
            t.channel = *c;
        }
        let mut e = ready_engine(&prj);
        e.set_mix(mix);
        e.play();
        let _ = fs::remove_dir_all(&dir);
        e
    }

    fn first(out: &[f32]) -> (f32, f32) {
        (out[0], out[1])
    }

    fn near(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-6
    }

    #[test]
    fn strip_level_and_pan_shape_the_signal() {
        let mut mix = MixParams::default();
        mix.strips[0] = Strip { level_db: -6.0, pan: -127, mute: false, solo: false };
        let mut e = mix_engine("mx-level", &[0.5], &[0], 2000, &mix);
        let (l, r) = first(&render(&mut e, 100));
        assert!((l - 0.5 * 0.501187).abs() < 1e-5, "{}", l);
        assert_eq!(r, 0.0);
        mix.strips[0] = Strip { level_db: 6.0, pan: 127, mute: false, solo: false };
        let mut e = mix_engine("mx-level2", &[0.25], &[0], 2000, &mix);
        let (l, r) = first(&render(&mut e, 100));
        assert_eq!(l, 0.0);
        assert!((r - 0.25 * 1.995262).abs() < 1e-5, "{}", r);
        mix.strips[0].level_db = f32::NEG_INFINITY;
        let mut e = mix_engine("mx-off", &[0.25], &[0], 2000, &mix);
        assert!(render(&mut e, 100).iter().all(|&s| s == 0.0));
    }

    #[test]
    fn mute_solo_and_master_in_the_engine() {
        let g = 0.707_106_77f32;
        let two = |mix: &MixParams| {
            let mut e = mix_engine("mx-ms", &[0.125, 0.25], &[0, 1], 2000, mix);
            first(&render(&mut e, 100)).0
        };
        let mut mix = MixParams::default();
        assert!(near(two(&mix), 0.375 * g));
        mix.strips[0].mute = true;
        assert!(near(two(&mix), 0.25 * g));
        mix.strips[0].mute = false;
        mix.strips[1].solo = true;
        assert!(near(two(&mix), 0.25 * g), "solo silences the other");
        mix.strips[0].solo = true;
        assert!(near(two(&mix), 0.375 * g), "solo on both");
        mix.strips[0].mute = true;
        assert!(near(two(&mix), 0.25 * g), "mute beats solo");
        mix = MixParams { master_db: -6.0, ..MixParams::default() };
        assert!((two(&mix) - 0.375 * g * 0.501187).abs() < 1e-5);
    }

    #[test]
    fn overload_is_limited_and_sets_the_clip_flag_until_reset() {
        let mut e = mix_engine("mx-clip", &[0.9, 0.9], &[0, 0], 2000, &MixParams::default());
        assert!(!e.status().levels.clip);
        let out = render(&mut e, 50);
        assert!(out.iter().all(|&s| s == 1.0), "1.8 x 0.707 limited to 1.0");
        assert!(e.status().levels.clip);
        // the flag stays, also after the peaks were taken
        assert!(e.take_status().levels.clip);
        assert!(e.status().levels.clip);
        e.reset_clip();
        assert!(!e.status().levels.clip);
        // negative side
        let mut e = mix_engine("mx-clip2", &[-0.9, -0.9], &[0, 1], 2000, &MixParams::default());
        assert!(render(&mut e, 10).iter().all(|&s| s == -1.0));
        assert!(e.status().levels.clip);
        // no clip below full scale
        let mut e = mix_engine("mx-clip3", &[0.9], &[0], 2000, &MixParams::default());
        render(&mut e, 10);
        assert!(!e.status().levels.clip);
    }

    #[test]
    fn a_mix_change_while_playing_ramps_linearly_over_10_ms() {
        let n = 480; // round(0.01 x 48000)
        let x = 0.5f32;
        let g = 0.707_106_77f32;
        let mut e = mix_engine("mx-ramp", &[x], &[0], 4000, &MixParams::default());
        let before = render(&mut e, 20);
        assert!(near(before[2 * 19], x * g));
        let mut mix = MixParams::default();
        mix.strips[0].level_db = f32::NEG_INFINITY;
        e.set_mix(&mix);
        let out = render(&mut e, n + 20);
        // linear steps, starting without a jump
        for i in 0..n {
            let want = x * g * (1.0 - (i + 1) as f32 / n as f32);
            assert!((out[2 * i] - want).abs() < 2e-6, "frame {}: {} vs {}", i, out[2 * i], want);
        }
        assert_eq!(out[2 * (n - 1)], 0.0, "the target after exactly {} frames", n);
        assert!(out[2 * n..].iter().all(|&s| s == 0.0));
        // a second change starts from the current value
        let mut e = mix_engine("mx-ramp2", &[x], &[0], 4000, &MixParams::default());
        render(&mut e, 20);
        e.set_mix(&mix);
        let half = render(&mut e, n / 2);
        let mid = half[2 * (n / 2 - 1)];
        e.set_mix(&MixParams::default());
        let back = render(&mut e, 4);
        assert!(back[0] > mid && back[0] - mid < x * g * 0.01, "continues from {} to {}", mid, back[0]);
    }

    #[test]
    fn a_mix_change_while_stopped_applies_at_once() {
        let dir = temp_dir("mx-stopped");
        let a = wav24(&dir, "a", RATE, 1, &constant(2000, 0.5));
        let mut e = ready_engine(&song(&dir, &[&a]));
        let mut mix = MixParams::default();
        mix.strips[0].level_db = f32::NEG_INFINITY;
        e.set_mix(&mix);
        mix.strips[0].level_db = -6.0;
        e.set_mix(&mix);
        e.play();
        let (l, _) = first(&render(&mut e, 10));
        assert!((l - 0.5 * 0.707_106_77 * 0.501187).abs() < 1e-5, "no ramp from an earlier value: {}", l);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn tracks_go_to_the_strip_of_their_channel() {
        let g = 0.707_106_77f32;
        // two tracks on the same channel are summed in that strip
        let mut mix = MixParams::default();
        mix.strips[3].level_db = -6.0;
        let mut e = mix_engine("mx-same", &[0.125, 0.25], &[3, 3], 2000, &mix);
        let (l, _) = first(&render(&mut e, 10));
        assert!((l - 0.375 * g * 0.501187).abs() < 1e-5);
        assert!((e.status().levels.strips[3] - 0.375 * 0.501187).abs() < 1e-5);
        // the channel decides, not the order of the tracks
        let mut mix = MixParams::default();
        mix.strips[0].mute = true;
        let mut e = mix_engine("mx-order", &[0.125, 0.25], &[1, 0], 2000, &mix);
        assert!(near(first(&render(&mut e, 10)).0, 0.125 * g));
        // channel 16 (strip 17) does not exist: warning and silence
        let mut e = mix_engine("mx-beyond", &[0.5, 0.25], &[16, 15], 2000, &MixParams::default());
        let st = e.status();
        assert_eq!(st.warnings.len(), 1);
        assert!(st.warnings[0].contains("t0") && st.warnings[0].contains("17"), "{}", st.warnings[0]);
        assert!(near(first(&render(&mut e, 10)).0, 0.25 * g));
        let mut e = mix_engine("mx-neg", &[0.5], &[-1], 2000, &MixParams::default());
        assert_eq!(e.status().warnings.len(), 1);
        assert!(render(&mut e, 10).iter().all(|&s| s == 0.0));
    }

    #[test]
    fn monitor_values_of_the_song_are_ignored() {
        let dir = temp_dir("mx-monitor");
        let a = wav24(&dir, "a", RATE, 1, &constant(2000, 0.25));
        let b = wav24(&dir, "b", RATE, 1, &ramp(2000));
        let plain = song(&dir, &[&a, &b]);
        let mut loud = plain.clone();
        for t in loud.tracks.iter_mut() {
            t.monitor_level = "-20.0".into();
            t.monitor_pan = "50.0".into();
            t.monitor_mute = "1".into();
        }
        let play = |prj: &SongProject| {
            let mut e = ready_engine(prj);
            e.play();
            render(&mut e, 1000).iter().map(|s| s.to_bits()).collect::<Vec<_>>()
        };
        let reference = play(&plain);
        assert!(reference.iter().any(|&b| f32::from_bits(b) != 0.0));
        assert_eq!(play(&loud), reference);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_meters_give_the_peak_since_the_last_reading() {
        let mut mix = MixParams::default();
        mix.strips[2] = Strip { level_db: -6.0, pan: 127, mute: false, solo: false };
        mix.strips[5].mute = true;
        let mut e = mix_engine("mx-meter", &[0.5, 0.4], &[2, 5], 4000, &mix);
        render(&mut e, 100);
        let lv = e.status().levels;
        assert_eq!(e.status().levels, lv, "reading the status does not reset");
        let lv = e.take_status().levels;
        assert!((lv.strips[2] - 0.5 * 0.501187).abs() < 1e-5, "after the fader, before the pan");
        assert_eq!(lv.strips[5], 0.0, "after the mute");
        assert_eq!(lv.strips[0], 0.0);
        assert_eq!(lv.master[0], 0.0, "panned fully right");
        assert!((lv.master[1] - 0.5 * 0.501187).abs() < 1e-5);
        // reset after the reading, then the new peak
        let lv = e.take_status().levels;
        assert_eq!((lv.strips[2], lv.master), (0.0, [0.0, 0.0]));
        render(&mut e, 10);
        assert!(e.take_status().levels.strips[2] > 0.2);
        // the peak is the largest one of several blocks
        let mut e = mix_engine("mx-meter2", &[0.5], &[0], 4000, &MixParams::default());
        render(&mut e, 10);
        let mut quiet = MixParams::default();
        quiet.strips[0].level_db = -60.0;
        e.set_mix(&quiet);
        render(&mut e, 600);
        assert!((e.take_status().levels.strips[0] - 0.5).abs() < 1e-6);
    }

    #[test]
    fn seek_without_a_song_does_nothing() {
        let mut e = Engine::new(RATE);
        e.seek(5.0);
        assert_eq!(e.status().state, PlayState::Empty);
    }

    #[test]
    fn clock_format() {
        assert_eq!(fmt_clock(0.0), "0:00");
        assert_eq!(fmt_clock(59.99), "0:59");
        assert_eq!(fmt_clock(61.0), "1:01");
        assert_eq!(fmt_clock(3725.0), "62:05");
        assert_eq!(fmt_clock(-3.0), "0:00");
    }

    // -- queue ---------------------------------------------------------------

    use crate::rehearsal_queue::entries_from_playlist;
    use crate::{generate_playlistprj, parse_playlist, PlaylistSong};

    /// A song project `name` in `dir` with one mono track of `frames`
    /// frames of the constant `v`; returns the path of the .songprj.
    fn qsong(dir: &Path, name: &str, rate: u32, frames: usize, v: f32) -> String {
        let audio_lib = dir.to_str().unwrap();
        let wav = wav24(dir, &format!("{}-t0", name), rate, 1, &constant(frames, v));
        let xml = generate_songprj(name, 0, rate, 24, 1);
        let mut prj = SongProject::parse(&xml, audio_lib);
        prj.tracks = vec![ProjectTrack::new(0, &wav, audio_lib)];
        let file = dir.join(format!("{}.songprj", name));
        prj.save_as(file.to_str().unwrap()).unwrap();
        file.to_str().unwrap().to_string()
    }

    /// A .playlistprj with the songs `(name, start mode, delay)`, written
    /// and read back with `parse_playlist`; a song whose .songprj does not
    /// exist is unreadable.
    fn queue_of(dir: &Path, specs: &[(&str, &str, &str)]) -> Vec<QueueEntry> {
        let lib = dir.to_str().unwrap();
        let songs: Vec<PlaylistSong> = specs
            .iter()
            .map(|(name, mode, delay)| PlaylistSong {
                name: name.to_string(),
                songprj: dir.join(format!("{}.songprj", name)).to_str().unwrap().to_string(),
                mode: mode.to_string(),
                delay: delay.to_string(),
            })
            .collect();
        let xml = generate_playlistprj("List", 0, lib, &songs);
        let file = dir.join("list.playlistprj");
        fs::write(&file, &xml).unwrap();
        let (name, parsed) = parse_playlist(&fs::read_to_string(&file).unwrap(), lib);
        assert_eq!(name, "List");
        entries_from_playlist(&parsed, lib)
    }

    /// three songs of 0.5 s with the constants 0.1, 0.2 and 0.3
    fn three(dir: &Path, rate: u32, modes: [(&str, &str); 3]) -> Vec<QueueEntry> {
        for (i, v) in [0.1f32, 0.2, 0.3].iter().enumerate() {
            qsong(dir, &format!("s{}", i + 1), rate, rate as usize / 2, *v);
        }
        queue_of(dir, &[("s1", modes[0].0, modes[0].1), ("s2", modes[1].0, modes[1].1), ("s3", modes[2].0, modes[2].1)])
    }

    const WAIT_MODES: [(&str, &str); 3] = [("wait_for_button_press", "0"); 3];

    fn queue_engine(entries: Vec<QueueEntry>) -> Engine {
        let mut e = Engine::new(RATE);
        e.set_queue(entries);
        e
    }

    /// render `total` frames in blocks of `block`
    fn pump(e: &mut Engine, total: usize, block: usize) -> Vec<f32> {
        let mut all = Vec::with_capacity(total * 2);
        while all.len() < total * 2 {
            let n = block.min(total - all.len() / 2);
            all.extend_from_slice(&render(e, n));
        }
        all
    }

    /// the left value of a constant song at the centre
    fn lv(v: f32) -> f32 {
        v * G
    }

    #[test]
    fn the_start_mode_and_delay_come_from_a_parsed_playlistprj() {
        let dir = temp_dir("q-parse");
        let q = three(&dir, RATE, [("wait_for_button_press", "0"), ("", ""), ("auto_start", "1.5")]);
        assert_eq!(q.len(), 3);
        assert!(q.iter().all(|e| e.readable()));
        assert_eq!(q[0].name, "s1");
        assert!(!q[0].auto_start() && q[0].delay == 0.0);
        assert!(!q[1].auto_start() && q[1].delay == 0.0);
        assert!(q[2].auto_start() && q[2].delay == 1.5);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn wait_mode_waits_after_the_end_and_play_starts_the_next_song_without_a_gap() {
        let dir = temp_dir("q-wait");
        let mut e = queue_engine(three(&dir, RATE, WAIT_MODES));
        assert_eq!(e.status().state, PlayState::Empty);
        e.goto(0, false);
        assert!(e.wait_ready(WAIT));
        assert_eq!(e.status().state, PlayState::Stopped);
        e.play();
        let out = pump(&mut e, 24000, 1000);
        assert!((out[0] - lv(0.1)).abs() < 1e-6 && (out[47998] - lv(0.1)).abs() < 1e-6);
        let st = e.status();
        assert_eq!(st.state, PlayState::Waiting);
        assert_eq!((st.index, st.song_name.to_string(), st.position_frames), (Some(1), "s2".to_string(), 0));
        assert_eq!(st.next_index, Some(2));
        // ten seconds of blocks: only silence
        assert!(e.wait_ready(WAIT));
        let quiet = pump(&mut e, 10 * RATE as usize, 4800);
        assert!(quiet.iter().all(|s| *s == 0.0));
        assert_eq!(e.status().state, PlayState::Waiting);
        assert_eq!(e.status().position_frames, 0);
        // Play: the very first frame is song 2
        e.play();
        let out = render(&mut e, 64);
        assert!((out[0] - lv(0.2)).abs() < 1e-6 && (out[1] - lv(0.2)).abs() < 1e-6, "no silent gap");
        assert_eq!(e.status().state, PlayState::Playing);
        assert_eq!(e.status().underruns, 0);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_automatic_mode_starts_the_next_song_after_exactly_the_delay_in_frames() {
        let dir = temp_dir("q-auto");
        let modes = [("wait_for_button_press", "0"), ("auto", "1.5"), ("wait_for_button_press", "0")];
        let mut e = queue_engine(three(&dir, RATE, modes));
        e.goto(0, false);
        assert!(e.wait_ready(WAIT));
        e.play();
        // odd block size, so the end of the song is inside a block
        let total = 24000 + 72000 + 2000;
        let out = pump(&mut e, total, 777);
        let first_of = |from: usize| (from..total).find(|&i| out[2 * i] != 0.0);
        assert!((out[2 * 23999] - lv(0.1)).abs() < 1e-6);
        assert_eq!(first_of(24000), Some(24000 + (1.5 * RATE as f64).round() as usize));
        assert!((out[2 * 96000] - lv(0.2)).abs() < 1e-6);
        assert_eq!(e.status().state, PlayState::Playing);
        assert_eq!(e.status().index, Some(1));
        assert_eq!(e.status().underruns, 0);
        // the countdown is visible while it runs
        let mut e = queue_engine(three(&temp_dir("q-auto2"), RATE, modes));
        e.goto(0, false);
        assert!(e.wait_ready(WAIT));
        e.play();
        pump(&mut e, 24000 + 4800, 1000);
        let st = e.status();
        assert_eq!(st.state, PlayState::Countdown);
        assert!((st.countdown_seconds - (72000.0 - 4800.0) / 48000.0).abs() < 1e-9);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_zero_delay_in_an_automatic_mode_continues_at_once() {
        let dir = temp_dir("q-auto0");
        let mut e = queue_engine(three(&dir, RATE, [("w", "0"), ("auto", ""), ("w", "0")]));
        e.goto(0, false);
        assert!(e.wait_ready(WAIT));
        e.play();
        let out = pump(&mut e, 24000 + 100, 24100);
        assert!((out[2 * 23999] - lv(0.1)).abs() < 1e-6);
        assert!((out[2 * 24000] - lv(0.2)).abs() < 1e-6);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn after_the_last_song_the_state_is_ended_and_play_starts_song_1() {
        let dir = temp_dir("q-ended");
        let mut e = queue_engine(three(&dir, RATE, WAIT_MODES));
        e.goto(2, true);
        assert!(e.wait_ready(WAIT));
        let out = pump(&mut e, 24000 + 500, 1000);
        assert!((out[0] - lv(0.3)).abs() < 1e-6);
        assert!(out[2 * 24000..].iter().all(|s| *s == 0.0));
        assert_eq!(e.status().state, PlayState::Ended);
        assert_eq!(e.status().index, Some(2));
        e.play();
        assert!(e.wait_ready(WAIT));
        assert_eq!(e.status().index, Some(0));
        let out = render(&mut e, 32);
        assert!((out[0] - lv(0.1)).abs() < 1e-6);
        assert_eq!(e.status().state, PlayState::Playing);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_single_song_is_a_queue_of_one_that_waits_for_the_button() {
        let dir = temp_dir("q-single");
        let path = qsong(&dir, "only", RATE, 4800, 0.25);
        let mut e = queue_engine(vec![QueueEntry::single(SongProject::load(&path, dir.to_str().unwrap()).unwrap())]);
        e.goto(0, true);
        assert!(e.wait_ready(WAIT));
        pump(&mut e, 4800, 512);
        assert_eq!(e.status().state, PlayState::Ended);
        e.play();
        assert!(e.wait_ready(WAIT));
        assert!((render(&mut e, 4)[0] - lv(0.25)).abs() < 1e-6);
        assert_eq!(e.status().queue_len, 1);
        assert_eq!(e.status().next_index, None);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unreadable_entry_is_skipped_at_the_end_and_by_next_and_prev_with_a_warning() {
        let dir = temp_dir("q-skip");
        qsong(&dir, "a", RATE, 24000, 0.1);
        qsong(&dir, "c", RATE, 24000, 0.3);
        // "b" has no .songprj
        let q = queue_of(&dir, &[("a", "", ""), ("b", "", ""), ("c", "", "")]);
        assert!(!q[1].readable());
        let mut e = queue_engine(q);
        let st = e.status();
        assert_eq!(st.queue_warnings.len(), 1);
        assert!(st.queue_warnings[0].contains("Song 2") && st.queue_warnings[0].contains("b"), "{:?}", st.queue_warnings);
        // song end
        e.goto(0, true);
        assert!(e.wait_ready(WAIT));
        pump(&mut e, 24000, 1000);
        let st = e.status();
        assert_eq!((st.state, st.index), (PlayState::Waiting, Some(2)));
        // prev skips it backwards, next forwards
        e.prev();
        assert!(e.wait_ready(WAIT));
        assert_eq!((e.status().index, e.status().state), (Some(0), PlayState::Stopped));
        e.next();
        assert!(e.wait_ready(WAIT));
        assert_eq!(e.status().index, Some(2));
        // goto an unreadable entry goes on to the next readable one
        e.goto(1, false);
        assert!(e.wait_ready(WAIT));
        assert_eq!(e.status().index, Some(2));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_queue_without_a_readable_entry_is_empty_with_a_warning() {
        let dir = temp_dir("q-none");
        let q = queue_of(&dir, &[("x", "", ""), ("y", "", "")]);
        let mut e = queue_engine(q);
        e.goto(0, true);
        e.next();
        e.play();
        assert!(e.wait_ready(WAIT));
        let st = e.status();
        assert_eq!(st.state, PlayState::Empty);
        assert_eq!(st.queue_warnings.len(), 2);
        assert_eq!(st.queue_len, 2);
        assert!(render(&mut e, 16).iter().all(|s| *s == 0.0));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn next_plays_at_once_while_playing_and_stops_at_position_0_otherwise() {
        let dir = temp_dir("q-next");
        let mut e = queue_engine(three(&dir, RATE, WAIT_MODES));
        e.goto(0, true);
        assert!(e.wait_ready(WAIT));
        render(&mut e, 100);
        e.next();
        assert_eq!(e.status().state, PlayState::Playing);
        let out = render(&mut e, 16);
        assert!((out[0] - lv(0.2)).abs() < 1e-6, "the preloaded song starts at once");
        assert_eq!((e.status().index, e.status().position_frames), (Some(1), 16));
        assert!(e.wait_ready(WAIT));
        // paused: the next song is loaded, state Stopped, position 0
        e.pause();
        render(&mut e, 100);
        e.next();
        let st = e.status();
        assert_eq!((st.state, st.index, st.position_frames), (PlayState::Stopped, Some(2), 0));
        assert!(render(&mut e, 64).iter().all(|s| *s == 0.0));
        e.play();
        assert!(e.wait_ready(WAIT));
        assert!((render(&mut e, 4)[0] - lv(0.3)).abs() < 1e-6);
        // at the last song nothing changes
        e.pause();
        e.next();
        let st = e.status();
        assert_eq!((st.state, st.index), (PlayState::Paused, Some(2)));
        assert!(st.position_frames > 0);
        // stopped and waiting behave like paused
        e.stop();
        e.prev();
        assert!(e.wait_ready(WAIT));
        assert_eq!((e.status().state, e.status().index), (PlayState::Stopped, Some(1)));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn next_while_waiting_skips_the_waiting_song() {
        let dir = temp_dir("q-next-wait");
        let mut e = queue_engine(three(&dir, RATE, WAIT_MODES));
        e.goto(0, true);
        assert!(e.wait_ready(WAIT));
        pump(&mut e, 24000, 1000);
        assert_eq!(e.status().state, PlayState::Waiting);
        assert_eq!(e.status().index, Some(1));
        assert!(e.wait_ready(WAIT));
        e.next();
        assert!(e.wait_ready(WAIT));
        let st = e.status();
        assert_eq!((st.state, st.index, st.position_frames), (PlayState::Stopped, Some(2), 0));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn prev_restarts_after_3_s_and_goes_back_before_that() {
        // 5 s songs at 8 kHz
        let dir = temp_dir("q-prev");
        for (i, v) in [0.1f32, 0.2, 0.3].iter().enumerate() {
            qsong(&dir, &format!("s{}", i + 1), 8000, 40000, *v);
        }
        let q = queue_of(&dir, &[("s1", "", ""), ("s2", "", ""), ("s3", "", "")]);
        let mut e = Engine::new(8000);
        e.set_queue(q);
        e.goto(1, false);
        assert!(e.wait_ready(WAIT));
        // further than 3 s: back to 0 of the same song
        e.seek(4.0);
        assert!(e.wait_ready(WAIT));
        e.prev();
        assert!(e.wait_ready(WAIT));
        let st = e.status();
        assert_eq!((st.index, st.position_frames, st.state), (Some(1), 0, PlayState::Stopped));
        // 3 s or less: the song before, at 0
        e.seek(3.0);
        e.prev();
        assert!(e.wait_ready(WAIT));
        let st = e.status();
        assert_eq!((st.index, st.position_frames, st.state), (Some(0), 0, PlayState::Stopped));
        // at the first song: back to 0
        e.seek(2.0);
        e.prev();
        assert!(e.wait_ready(WAIT));
        let st = e.status();
        assert_eq!((st.index, st.position_frames), (Some(0), 0));
        // while playing the state stays
        e.play();
        let _ = render(&mut e, 100);
        e.prev();
        assert_eq!(e.status().state, PlayState::Playing);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn goto_plays_the_song_from_the_start() {
        let dir = temp_dir("q-goto");
        let mut e = queue_engine(three(&dir, RATE, WAIT_MODES));
        e.goto(0, false);
        assert!(e.wait_ready(WAIT));
        e.goto(2, true);
        assert_eq!(e.status().state, PlayState::Playing);
        assert_eq!(e.status().index, Some(2));
        // the song is still being opened: silence, no dropout
        assert!(render(&mut e, 256).iter().all(|s| *s == 0.0));
        assert!(e.wait_ready(WAIT));
        let out = render(&mut e, 64);
        assert!((out[0] - lv(0.3)).abs() < 1e-6 && (out[127] - lv(0.3)).abs() < 1e-6);
        assert_eq!(e.status().position_frames, 64);
        assert_eq!(e.status().underruns, 0);
        // goto the song that is loaded: from its start
        e.goto(2, false);
        assert_eq!((e.status().state, e.status().position_frames), (PlayState::Stopped, 0));
        // beyond the end: the last song
        e.goto(99, false);
        assert_eq!(e.status().index, Some(2));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_next_song_is_preloaded_and_the_change_needs_no_file_access() {
        let dir = temp_dir("q-preload");
        let mut e = queue_engine(three(&dir, RATE, [("w", "0"), ("auto", "0"), ("w", "0")]));
        e.goto(0, false);
        assert!(e.wait_ready(WAIT)); // opens song 1, preloads song 2
        e.play();
        // the callback never opens a file: without an opener nothing could arrive
        let out = pump(&mut e, 24000 - 500, 1000);
        assert!((out[0] - lv(0.1)).abs() < 1e-6);
        let out = render(&mut e, 1000);
        assert!((out[2 * 499] - lv(0.1)).abs() < 1e-6);
        assert!((out[2 * 500] - lv(0.2)).abs() < 1e-6, "the first block after the change has the data of song 2");
        assert_eq!(e.status().underruns, 0);
        assert_eq!(e.status().index, Some(1));
        // the replaced song is handed back, not dropped in render
        assert!(e.take_garbage().is_some());
        assert!(e.take_garbage().is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_song_that_is_not_preloaded_is_waited_for_in_silence() {
        let dir = temp_dir("q-nopreload");
        let mut e = queue_engine(three(&dir, RATE, [("w", "0"), ("auto", "0"), ("w", "0")]));
        e.goto(0, false);
        e.settle(); // opens song 1 only; nobody preloads song 2
        while !e.ready() {
            std::thread::sleep(Duration::from_millis(1));
        }
        e.play();
        let out = pump(&mut e, 24000 + 2000, 1000);
        assert!(out[2 * 24000..].iter().all(|s| *s == 0.0));
        let st = e.status();
        assert_eq!((st.index, st.position_frames, st.state), (Some(1), 0, PlayState::Playing));
        assert!(e.wait_ready(WAIT));
        let out = render(&mut e, 8);
        assert!((out[0] - lv(0.2)).abs() < 1e-6);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_next_song_of_another_rate_asks_for_the_rate_and_plays_after_the_change() {
        let dir = temp_dir("q-rate");
        qsong(&dir, "s1", RATE, 24000, 0.1);
        qsong(&dir, "s2", 44100, 22050, 0.2);
        let q = queue_of(&dir, &[("s1", "", ""), ("s2", "", "")]);
        let mut e = queue_engine(q);
        e.goto(0, false);
        assert!(e.wait_ready(WAIT));
        assert_eq!(e.status().needs_rate, None);
        e.play();
        pump(&mut e, 24000, 1000);
        let st = e.status();
        assert_eq!((st.state, st.index, st.needs_rate), (PlayState::Waiting, Some(1), Some(44100)));
        // it does not play at the wrong rate
        assert!(e.wait_ready(WAIT));
        e.play();
        assert_eq!(e.status().state, PlayState::Waiting);
        assert!(pump(&mut e, 1000, 100).iter().all(|s| *s == 0.0));
        // the output was opened again with the new rate
        e.set_output_rate(44100);
        let st = e.status();
        assert_eq!((st.needs_rate, st.output_rate), (None, 44100));
        assert!(!st.warnings.iter().any(|w| w.contains("sample rate")));
        e.play();
        let out = render(&mut e, 64);
        assert!((out[0] - lv(0.2)).abs() < 1e-6 && (out[127] - lv(0.2)).abs() < 1e-6);
        assert_eq!(e.status().state, PlayState::Playing);
        // the length is that of the song at its own rate
        pump(&mut e, 22050 - 64, 1000);
        assert_eq!(e.status().state, PlayState::Ended);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_countdown_can_be_cancelled_or_cut_short() {
        let dir = temp_dir("q-countdown");
        let modes = [("w", "0"), ("auto", "5"), ("w", "0")];
        let mut e = queue_engine(three(&dir, RATE, modes));
        e.goto(0, true);
        assert!(e.wait_ready(WAIT));
        pump(&mut e, 24000 + 100, 1000);
        assert_eq!(e.status().state, PlayState::Countdown);
        // Pause: waiting for the button
        e.pause();
        assert_eq!((e.status().state, e.status().countdown_seconds), (PlayState::Waiting, 0.0));
        assert!(pump(&mut e, 6 * RATE as usize, 4800).iter().all(|s| *s == 0.0));
        // Play during a countdown starts at once
        e.goto(0, true);
        assert!(e.wait_ready(WAIT));
        pump(&mut e, 24000 + 100, 1000);
        assert_eq!(e.status().state, PlayState::Countdown);
        e.play();
        assert!(e.wait_ready(WAIT));
        assert!((render(&mut e, 4)[0] - lv(0.2)).abs() < 1e-6);
        // Stop ends it, too
        e.goto(0, true);
        assert!(e.wait_ready(WAIT));
        pump(&mut e, 24000 + 100, 1000);
        e.stop();
        assert_eq!((e.status().state, e.status().position_frames), (PlayState::Stopped, 0));
        // hold carries a countdown over to another engine
        e.hold(Some(0.5));
        assert_eq!(e.status().state, PlayState::Countdown);
        assert!((e.status().countdown_seconds - 0.5).abs() < 1e-9);
        e.hold(None);
        assert_eq!(e.status().state, PlayState::Waiting);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_jump_before_the_song_is_open_is_done_when_it_arrives() {
        let dir = temp_dir("q-pendingseek");
        let mut e = queue_engine(three(&dir, RATE, WAIT_MODES));
        e.goto(1, false);
        e.seek(0.25); // still opening
        assert_eq!(e.status().position_frames, 0);
        assert!(e.wait_ready(WAIT));
        let st = e.status();
        assert_eq!((st.index, st.state, st.position_frames), (Some(1), PlayState::Stopped, 12000));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn jumping_to_the_end_of_a_song_goes_on_with_the_next_one() {
        let dir = temp_dir("q-seekend");
        let mut e = queue_engine(three(&dir, RATE, WAIT_MODES));
        e.goto(0, true);
        assert!(e.wait_ready(WAIT));
        render(&mut e, 100);
        e.seek(100.0); // the end, while playing: fade out, then the change
        pump(&mut e, 1000, 100);
        assert!(e.wait_ready(WAIT));
        pump(&mut e, 1000, 100);
        let st = e.status();
        assert_eq!((st.state, st.index), (PlayState::Waiting, Some(1)));
        // paused at the end of the song
        let mut e = queue_engine(three(&temp_dir("q-seekend2"), RATE, WAIT_MODES));
        e.goto(0, false);
        assert!(e.wait_ready(WAIT));
        e.seek(100.0);
        assert_eq!((e.status().state, e.status().index), (PlayState::Waiting, Some(1)));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_changed_playlist_keeps_the_loaded_song_or_stops() {
        let dir = temp_dir("q-replace");
        let q = three(&dir, RATE, WAIT_MODES);
        let mut e = queue_engine(q.clone());
        e.goto(1, true);
        assert!(e.wait_ready(WAIT));
        render(&mut e, 1000);
        // the song moved to index 0: it plays on at the same position
        let new = queue_of(&dir, &[("s2", "", ""), ("s3", "", "")]);
        let (_old, kept) = e.replace_queue(Arc::new(new));
        assert!(kept);
        let st = e.status();
        assert_eq!((st.state, st.index, st.queue_len, st.position_frames), (PlayState::Playing, Some(0), 2, 1000));
        assert_eq!(st.next_index, Some(1));
        let out = render(&mut e, 4);
        assert!((out[0] - lv(0.2)).abs() < 1e-6);
        // the song is gone: stopped on the first song
        let new = queue_of(&dir, &[("s1", "", ""), ("s3", "", "")]);
        let (_old, kept) = e.replace_queue(Arc::new(new));
        assert!(!kept);
        assert!(e.wait_ready(WAIT));
        let st = e.status();
        assert_eq!((st.state, st.index, st.position_frames), (PlayState::Stopped, Some(0), 0));
        assert_eq!(&*st.song_name, "s1");
        // an empty playlist unloads everything
        e.set_queue(Vec::new());
        assert_eq!(e.status().state, PlayState::Empty);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn songs_set_without_a_queue_still_end_with_ended() {
        let dir = temp_dir("q-legacy");
        let wav = wav24(&dir, "w", RATE, 1, &constant(2400, 0.5));
        let prj = song(&dir, &[&wav]);
        let mut e = ready_engine(&prj);
        e.play();
        pump(&mut e, 2400, 500);
        assert_eq!(e.status().state, PlayState::Ended);
        assert_eq!(e.status().index, None);
        let _ = fs::remove_dir_all(&dir);
    }

    // -- MIDI ------------------------------------------------------------------

    use crate::rehearsal_queue::WAIT_MODE;
    use crate::tests::{smf, trk, write_mid, EOT};

    /// 8 kHz keeps the long songs of the chasing tests small.
    const MR: u32 = 8000;

    /// Track body from (absolute tick, event) pairs; 480 ticks per quarter
    /// at 120 BPM are 960 ticks per second.
    fn abs_trk(events: &[(u32, &[u8])]) -> Vec<u8> {
        let mut last = 0;
        let mut list: Vec<(u32, &[u8])> = Vec::new();
        for (t, e) in events {
            list.push((t - last, e));
            last = *t;
        }
        list.push((0, EOT));
        trk(&list)
    }

    fn sec(s: f64) -> u32 {
        (s * 960.0).round() as u32
    }

    /// A song of `seconds` at `rate` with the MIDI file `mid`.
    fn midi_song(dir: &Path, rate: u32, seconds: f64, mid: &[u8]) -> SongProject {
        let wav = wav24(dir, "m", rate, 1, &constant((seconds * rate as f64) as usize, 0.25));
        let mut prj = song(dir, &[&wav]);
        prj.midi_file = write_mid(dir, "song.mid", mid);
        prj
    }

    fn midi_engine(prj: &SongProject, rate: u32) -> Engine {
        let mut e = Engine::new(rate);
        e.load(prj);
        assert!(e.wait_ready(WAIT));
        e
    }

    /// Render `total` frames in blocks of `block`; every message as
    /// (frame counted from the first block, bytes).
    fn midi_run(e: &mut Engine, total: usize, block: usize) -> Vec<(usize, Vec<u8>)> {
        let mut all = Vec::new();
        let mut buf = vec![0f32; block * 2];
        let mut done = 0;
        while done < total {
            let n = block.min(total - done);
            for m in e.render(&mut buf[..n * 2]) {
                assert!(m.offset < n, "offset {} in a block of {}", m.offset, n);
                all.push((done + m.offset, m.data().to_vec()));
            }
            done += n;
        }
        all
    }

    fn bytes(list: &[(usize, Vec<u8>)]) -> Vec<Vec<u8>> {
        list.iter().map(|m| m.1.clone()).collect()
    }

    const CC69: u8 = 0x45;
    const ANO1: [u8; 3] = [0xb0, 123, 0];

    #[test]
    fn a_midi_event_lands_on_the_right_frame_after_a_tempo_change_at_both_rates() {
        // 120 BPM, from tick 480 (0.5 s) on 60 BPM: tick 720 is at 1.0 s
        let tempo =
            trk(&[(0, &[0xff, 0x51, 0x03, 0x07, 0xa1, 0x20]), (480, &[0xff, 0x51, 0x03, 0x0f, 0x42, 0x40]), (0, EOT)]);
        let ev = trk(&[(720, &[0xb0, CC69, 1]), (0, EOT)]);
        let mid = smf(1, 480, &[tempo, ev]);
        for (rate, frame) in [(48000u32, 48000usize), (44100, 44100)] {
            let dir = temp_dir(&format!("mi-tempo-{}", rate));
            let prj = midi_song(&dir, rate, 1.5, &mid);
            let mut e = midi_engine(&prj, rate);
            e.play();
            let got = midi_run(&mut e, 70000, 512);
            assert_eq!(got, vec![(frame, vec![0xb0, CC69, 1])], "rate {}", rate);
            // the offset in the block: the frame inside its block of 512
            let mut e = midi_engine(&prj, rate);
            e.play();
            let mut buf = vec![0f32; 1024];
            let mut hit = None;
            for block in 0..140 {
                for m in e.render(&mut buf) {
                    hit = Some((block * 512 + m.offset, m.offset));
                }
            }
            assert_eq!(hit, Some((frame, frame % 512)));
            let _ = fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn block_sizes_1_37_and_512_give_the_same_events_on_the_same_frames() {
        // 1 tick = 50 frames at 48 kHz
        let t = abs_trk(&[
            (0, &[0xc0, 4]),
            (0, &[0xb0, CC69, 1]),
            (1, &[0xb0, CC69, 2]),
            (7, &[0xb1, 7, 100]),
            (13, &[0x90, 60, 90]),
            (13, &[0x80, 60, 0]),
            (40, &[0xb0, CC69, 3]),
        ]);
        let dir = temp_dir("mi-blocks");
        let prj = midi_song(&dir, RATE, 0.1, &smf(0, 480, &[t]));
        let mut runs = Vec::new();
        for block in [1usize, 37, 512] {
            let mut e = midi_engine(&prj, RATE);
            e.play();
            runs.push(midi_run(&mut e, 4000, block));
        }
        assert_eq!(runs[0].len(), 7);
        assert_eq!(runs[0].iter().map(|m| m.0).collect::<Vec<_>>(), vec![0, 0, 50, 350, 650, 650, 2000]);
        assert_eq!(runs[0], runs[1]);
        assert_eq!(runs[0], runs[2]);
        let _ = fs::remove_dir_all(&dir);
    }

    /// bank, program, snapshots, a CC on channel 10, notes, pitch bend
    fn chase_file() -> Vec<u8> {
        let t = abs_trk(&[
            (0, &[0xb0, 0, 3]),
            (0, &[0xb0, 32, 5]),
            (0, &[0xc0, 9]),
            (sec(1.0), &[0xb0, CC69, 1]),
            (sec(2.0), &[0xb9, 22, 7]),
            (sec(3.0), &[0x90, 60, 100]),
            (sec(3.5), &[0x80, 60, 0]),
            (sec(5.0), &[0xb0, CC69, 2]),
            (sec(6.0), &[0xe0, 0, 64]),
            (sec(9.0), &[0xb0, CC69, 4]),
            (sec(11.0), &[0xb0, CC69, 5]),
            (sec(12.0), &[0xb9, 22, 9]),
        ]);
        smf(0, 480, &[t])
    }

    #[test]
    fn jumping_chases_bank_program_and_the_last_controller_values_without_notes() {
        let dir = temp_dir("mi-chase");
        let prj = midi_song(&dir, MR, 12.5, &chase_file());
        let mut e = midi_engine(&prj, MR);
        e.play();
        let start = midi_run(&mut e, 400, 128);
        assert_eq!(bytes(&start), vec![vec![0xb0, 0, 3], vec![0xb0, 32, 5], vec![0xc0, 9]], "from 0: the events of frame 0");
        e.pause();
        assert_eq!(bytes(&midi_run(&mut e, 128, 128)), vec![ANO1.to_vec()], "the file has notes on channel 1");
        e.seek(10.0);
        assert!(e.wait_ready(WAIT));
        e.play();
        let mut buf = vec![0f32; 256];
        let msgs: Vec<MidiMsg> = e.render(&mut buf).to_vec();
        let got: Vec<Vec<u8>> = msgs.iter().map(|m| m.data().to_vec()).collect();
        assert_eq!(
            got,
            vec![vec![0xb0, 0, 3], vec![0xb0, 32, 5], vec![0xc0, 9], vec![0xb0, CC69, 4], vec![0xb9, 22, 7]]
        );
        assert!(msgs.iter().all(|m| m.offset == 0), "chasing comes at offset 0 of the first block");
        // the events after the jump follow in time
        let rest = midi_run(&mut e, 2 * MR as usize + 4000, 512);
        // the song ended: the notes are ended, too
        assert_eq!(bytes(&rest), vec![vec![0xb0, CC69, 5], vec![0xb9, 22, 9], ANO1.to_vec()]);
        let st = e.status();
        assert!(st.midi.has_file && st.midi.events_sent == 3 + 1 + 5 + 3, "{:?}", st.midi);
        assert!(st.midi.last_event.is_some());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_jump_while_playing_sends_all_notes_off_before_the_chasing() {
        let dir = temp_dir("mi-chase-play");
        let prj = midi_song(&dir, MR, 12.5, &chase_file());
        let mut e = midi_engine(&prj, MR);
        e.play();
        midi_run(&mut e, 400, 100);
        e.seek(5.5);
        let mut got = Vec::new();
        for _ in 0..3000 {
            got.extend(bytes(&midi_run(&mut e, 64, 64)));
            if got.contains(&vec![0xb9, 22, 7]) {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(
            got,
            vec![
                ANO1.to_vec(),
                vec![0xb0, 0, 3],
                vec![0xb0, 32, 5],
                vec![0xc0, 9],
                vec![0xb0, CC69, 2],
                vec![0xb9, 22, 7]
            ]
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_event_on_the_jump_position_is_sent_once_as_a_normal_event() {
        let t = abs_trk(&[(sec(1.0), &[0xb0, CC69, 1]), (sec(2.0), &[0xb0, CC69, 3])]);
        let dir = temp_dir("mi-on-target");
        let prj = midi_song(&dir, MR, 4.0, &smf(0, 480, &[t]));
        let mut e = midi_engine(&prj, MR);
        e.seek(2.0);
        assert!(e.wait_ready(WAIT));
        e.play();
        let mut all = midi_run(&mut e, 256, 256);
        assert_eq!(all, vec![(0, vec![0xb0, CC69, 1]), (0, vec![0xb0, CC69, 3])], "the state before, then the event");
        all.extend(midi_run(&mut e, 2 * MR as usize, 500));
        assert_eq!(bytes(&all).iter().filter(|m| **m == vec![0xb0, CC69, 3]).count(), 1);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn starting_at_0_has_no_chasing_and_sends_the_events_of_frame_0() {
        let t = abs_trk(&[(0, &[0xb0, 0, 2]), (0, &[0xc0, 7]), (sec(0.5), &[0xb0, CC69, 1])]);
        let dir = temp_dir("mi-start");
        let prj = midi_song(&dir, MR, 1.0, &smf(0, 480, &[t]));
        let mut e = midi_engine(&prj, MR);
        e.play();
        let first = midi_run(&mut e, 100, 100);
        assert_eq!(first, vec![(0, vec![0xb0, 0, 2]), (0, vec![0xc0, 7])]);
        let all = midi_run(&mut e, 2 * MR as usize, 400);
        assert_eq!(bytes(&all), vec![vec![0xb0, CC69, 1]]);
        // Stop and Play again: from 0, the events of frame 0 once more, no chasing
        e.stop();
        e.play();
        assert!(e.wait_ready(WAIT));
        let again = midi_run(&mut e, 100, 100);
        assert_eq!(bytes(&again), vec![vec![0xb0, 0, 2], vec![0xc0, 7]]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn pause_and_continue_loses_and_doubles_no_event() {
        let list: Vec<(u32, Vec<u8>)> = (0..6).map(|i| (sec(i as f64 * 0.15), vec![0xb0, CC69, i as u8 + 1])).collect();
        let refs: Vec<(u32, &[u8])> = list.iter().map(|(t, b)| (*t, b.as_slice())).collect();
        let dir = temp_dir("mi-pause");
        let prj = midi_song(&dir, MR, 1.0, &smf(0, 480, &[abs_trk(&refs)]));
        let mut e = midi_engine(&prj, MR);
        e.play();
        let mut all = midi_run(&mut e, 3000, 37);
        e.pause();
        all.extend(midi_run(&mut e, 500, 37));
        e.play();
        all.extend(midi_run(&mut e, 6000, 37));
        assert_eq!(e.status().state, PlayState::Ended);
        let expect: Vec<Vec<u8>> = list.iter().map(|(_, b)| b.clone()).collect();
        assert_eq!(bytes(&all), expect, "all events of the file, once, in order");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn stop_sends_all_notes_off_only_on_channels_with_notes() {
        let with_notes = abs_trk(&[(0, &[0x90, 60, 100]), (0, &[0x92, 62, 100]), (sec(0.5), &[0x80, 60, 0])]);
        let dir = temp_dir("mi-stop");
        let prj = midi_song(&dir, MR, 2.0, &smf(0, 480, &[with_notes]));
        let mut e = midi_engine(&prj, MR);
        assert!(midi_run(&mut e, 100, 100).is_empty(), "nothing is sent before Play");
        e.stop();
        assert!(midi_run(&mut e, 100, 100).is_empty(), "stopped already: nothing to end");
        assert!(e.wait_ready(WAIT));
        e.play();
        midi_run(&mut e, 100, 100);
        e.stop();
        let got = midi_run(&mut e, 100, 100);
        assert_eq!(got, vec![(0, vec![0xb0, 123, 0]), (0, vec![0xb2, 123, 0])]);
        assert!(midi_run(&mut e, 100, 100).is_empty(), "only once");
        // unloading ends the notes as well
        e.play();
        assert!(e.wait_ready(WAIT));
        midi_run(&mut e, 100, 100);
        e.unload();
        assert_eq!(bytes(&midi_run(&mut e, 100, 100)), vec![vec![0xb0, 123, 0], vec![0xb2, 123, 0]]);

        // without notes: nothing
        let no_notes = abs_trk(&[(0, &[0xc0, 1]), (sec(0.5), &[0xb0, CC69, 2])]);
        let prj = midi_song(&dir, MR, 2.0, &smf(0, 480, &[no_notes]));
        let mut e = midi_engine(&prj, MR);
        e.play();
        midi_run(&mut e, 100, 100);
        e.pause();
        e.stop();
        assert!(midi_run(&mut e, 100, 100).is_empty());
        assert!(e.wait_ready(WAIT));
        e.play();
        midi_run(&mut e, 100, 100);
        e.unload();
        assert!(midi_run(&mut e, 100, 100).is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_song_without_a_midi_file_sends_nothing_and_an_unreadable_one_warns() {
        let dir = temp_dir("mi-none");
        let wav = wav24(&dir, "a", RATE, 1, &constant(2000, 0.25));
        let plain = song(&dir, &[&wav]);
        let mut e = midi_engine(&plain, RATE);
        assert!(!e.status().midi.has_file);
        e.play();
        let reference = render(&mut e, 100);
        assert!(midi_run(&mut e, 500, 100).is_empty());
        assert!(e.status().warnings.is_empty());
        assert_eq!(e.status().midi.events_sent, 0);

        for name in ["bad.mid", "missing.mid"] {
            let mut prj = plain.clone();
            if name == "bad.mid" {
                prj.midi_file = write_mid(&dir, name, b"this is not a midi file");
            } else {
                prj.midi_file = dir.join(name).to_str().unwrap().to_string();
            }
            let mut e = midi_engine(&prj, RATE);
            let st = e.status();
            assert_eq!(*st.warnings, vec![format!("MIDI file unreadable: {}", name)]);
            assert!(!st.midi.has_file);
            e.play();
            assert_eq!(render(&mut e, 100), reference, "the audio is unchanged");
            assert!(midi_run(&mut e, 500, 100).is_empty());
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_new_output_rate_computes_the_frames_again() {
        let t = abs_trk(&[(sec(1.0), &[0xb0, CC69, 1])]);
        let dir = temp_dir("mi-rate");
        let prj = midi_song(&dir, RATE, 1.5, &smf(0, 480, &[t]));
        // loaded into an engine of another rate: the events count in its frames
        let mut e = Engine::new(44100);
        drop(e.set_song(LoadedSong::open(&prj)));
        e.set_output_rate(RATE);
        assert!(e.wait_ready(WAIT));
        e.play();
        assert_eq!(midi_run(&mut e, 50000, 500), vec![(48000, vec![0xb0, CC69, 1])]);
        let _ = fs::remove_dir_all(&dir);
    }

    /// A song of the queue with a MIDI file that has a program change on
    /// frame 0 (`program`) and a CC69 at 0.1 s.
    fn qsong_midi(dir: &Path, name: &str, program: u8) -> String {
        let path = qsong(dir, name, RATE, 6000, 0.25);
        let t = abs_trk(&[(0, &[0xc0, program]), (96, &[0xb0, CC69, program])]);
        let mut prj = SongProject::load(&path, dir.to_str().unwrap()).unwrap();
        prj.midi_file = write_mid(dir, &format!("{}.mid", name), &smf(0, 480, &[t]));
        prj.save_as(&path).unwrap();
        path
    }

    #[test]
    fn the_next_song_of_a_playlist_sends_its_events_from_frame_0() {
        let dir = temp_dir("mi-queue");
        qsong_midi(&dir, "a", 1);
        qsong_midi(&dir, "b", 2);
        // automatic: song b follows song a at once
        let mut e = queue_engine(queue_of(&dir, &[("a", WAIT_MODE, "0"), ("b", "start_after_delay", "0")]));
        e.goto(0, true);
        assert!(e.wait_ready(WAIT));
        let all = midi_run(&mut e, 12000, 480);
        assert_eq!(
            bytes(&all),
            vec![vec![0xc0, 1], vec![0xb0, CC69, 1], vec![0xc0, 2], vec![0xb0, CC69, 2]],
            "{:?}",
            all
        );
        assert_eq!(all[2].0, 6000, "song b starts on its first frame");
        assert_eq!(e.status().midi.events_sent, 2, "the count is per song");
        // by hand while playing: Next sends the frame 0 events of the next song at once
        let mut e = queue_engine(queue_of(&dir, &[("a", WAIT_MODE, "0"), ("b", WAIT_MODE, "0")]));
        e.goto(0, true);
        assert!(e.wait_ready(WAIT));
        midi_run(&mut e, 1000, 100);
        e.next();
        assert!(e.wait_ready(WAIT));
        let got = midi_run(&mut e, 100, 100);
        assert_eq!(got, vec![(0, vec![0xc0, 2])]);
        let _ = fs::remove_dir_all(&dir);
    }
}

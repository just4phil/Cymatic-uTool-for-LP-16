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

use crate::mix::{MixParams, StripGains, STRIPS};
use crate::{SongProject, WavReader};
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
    /// the longest track is over; silence follows
    Ended,
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
        for t in self.tracks.iter_mut() {
            t.synced = false;
            t.debt = 0;
        }
    }

    /// The fade-out is over (or the song ended during it): do the jump.
    fn finish_fade_out(&mut self) {
        self.seek_to(self.pending);
        self.fade = Fade::Wait;
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
    /// joins its thread, which an audio callback must not do).
    pub fn set_song(&mut self, mut song: LoadedSong) -> Option<LoadedSong> {
        if song.song_rate != self.output_rate {
            Arc::make_mut(&mut song.warnings).push(format!(
                "Song sample rate {} Hz differs from output {} Hz",
                song.song_rate, self.output_rate
            ));
        }
        self.state = PlayState::Stopped;
        self.underruns = 0;
        self.song.replace(song)
    }

    /// Remove the current song, see `set_song` about who drops it.
    pub fn take_song(&mut self) -> Option<LoadedSong> {
        self.state = PlayState::Empty;
        self.song.take()
    }

    /// Drop the current song (ends its thread, releases the files).
    pub fn unload(&mut self) {
        drop(self.take_song());
    }

    /// Start or continue. Without a song, or if the song rate differs from
    /// the output rate, nothing happens. After the end it starts again at 0.
    pub fn play(&mut self) {
        let Some(song) = self.song.as_mut() else { return };
        if song.song_rate != self.output_rate {
            return;
        }
        if self.state == PlayState::Ended {
            song.fade = Fade::None;
            song.seek_to(0);
        }
        self.state = PlayState::Playing;
    }

    pub fn pause(&mut self) {
        if self.state == PlayState::Playing {
            self.state = PlayState::Paused;
        }
        if let Some(song) = self.song.as_mut() {
            if matches!(song.fade, Fade::Out(_)) {
                song.finish_fade_out(); // the sound is gone anyway
            }
        }
    }

    /// Jump to `seconds` (clamped to 0 ..= length, rounded to whole frames).
    /// The length itself gives `Ended`. While playing the sound is faded out
    /// over 5 ms, the position changes, and after a silent wait it is faded
    /// in over 5 ms; the status shows the target at once. Paused, stopped
    /// or ended, only the position changes (`Ended` becomes `Paused` if the
    /// target is before the end). Several calls in a row: the last target
    /// counts.
    pub fn seek(&mut self, seconds: f64) {
        let fade_len = self.fade_len;
        let Some(song) = self.song.as_mut() else { return };
        if self.state == PlayState::Empty {
            return;
        }
        let target = if seconds > 0.0 { (seconds * song.song_rate as f64).round() as u64 } else { 0 };
        let target = target.min(song.length_frames);
        let end = song.length_frames > 0 && target >= song.length_frames;
        let playing_state = if end { PlayState::Ended } else { PlayState::Playing };
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
                        song.finish_fade_out();
                    } else {
                        song.fade = Fade::Out(done);
                    }
                }
            }
            (state, _) => {
                song.fade = Fade::None;
                song.seek_to(target);
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

    /// Pause plus position 0. No fade: silence follows anyway.
    pub fn stop(&mut self) {
        if let Some(song) = self.song.as_mut() {
            song.fade = Fade::None;
            song.seek_to(0);
            self.state = PlayState::Stopped;
        }
    }

    /// Fill `out` (interleaved stereo, 2 x frames) completely: the mix
    /// while playing, silence otherwise. No allocation, no file access, no
    /// blocking lock.
    pub fn render(&mut self, out: &mut [f32]) {
        out.fill(0.0);
        let fade_len = self.fade_len;
        let Some(song) = self.song.as_mut() else { return };
        // a jump to the end sets `Ended` at once but still fades out
        let active = self.state == PlayState::Playing || matches!(song.fade, Fade::Out(_));
        if !active {
            // keep the tracks in step, so `wait_ready` sees the progress
            song.filled(1);
            return;
        }
        if !song.primed {
            // 0.5 s (a quarter of the ring) is enough to start
            if !song.filled(4) {
                return; // waiting is no dropout
            }
            song.primed = true;
            if song.fade == Fade::Wait {
                song.fade = Fade::In(0);
            }
        }
        let frames = out.len() / 2;
        let mut done = 0;
        let mut underrun = false;
        while done < frames && song.pos < song.length_frames {
            let mut m = (frames - done).min(MIX_CHUNK).min((song.length_frames - song.pos) as usize);
            if let Fade::Out(d) | Fade::In(d) = song.fade {
                m = m.min(fade_len - d);
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
                    song.finish_fade_out();
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
            song.finish_fade_out(); // the song ended during the fade-out
        } else if song.fade != Fade::Wait && song.pos >= song.length_frames {
            self.state = PlayState::Ended;
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
        match &self.song {
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
            },
            Some(s) => {
                let rate = s.song_rate.max(1) as f64;
                Status {
                    state: self.state,
                    song_name: s.name.clone(),
                    position_frames: s.shown_pos(),
                    position_seconds: s.shown_pos() as f64 / rate,
                    length_frames: s.length_frames,
                    length_seconds: s.length_frames as f64 / rate,
                    output_rate: self.output_rate,
                    song_rate: s.song_rate,
                    underruns: self.underruns,
                    warnings: s.warnings.clone(),
                    levels,
                }
            }
        }
    }

    /// Blocks until the ring of every track is full or at its file end, or
    /// `timeout` has passed (false). For tests and for the start after
    /// `load` or a position change; never call it from the audio callback.
    pub fn wait_ready(&mut self, timeout: Duration) -> bool {
        let start = Instant::now();
        loop {
            match self.song.as_mut() {
                None => return true,
                Some(song) => {
                    if song.filled(1) {
                        return true;
                    }
                }
            }
            if start.elapsed() >= timeout {
                return false;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
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
}

//! The audio output adapter of the rehearsal player: opens an output device
//! of one of cpal's hosts (WASAPI today, ASIO later as one more host) and
//! calls the engine from its callback. Everything that can be tested without
//! a device is plain functions and types: host and device names, the choice
//! of the stream configuration (`pick_config`), the channel distribution
//! (`distribute`), the test tone (`test_tone`), commands and sample
//! conversion. `Player::open`, `start_test_tone` and the listings are the
//! only parts that need hardware.
//!
//! The stream is opened with the sample rate of the song. A song with
//! another rate means a new `Player` (the owner closes the old one and
//! opens a new one), so engine and device always run at the same rate.
//!
//! The audio callback owns the engine. The user interface talks to it with
//! `Command`s over an `mpsc` queue (taken with `try_recv` at the start of
//! every callback) and reads the status from an `Arc<Mutex<Status>>` that
//! the callback fills with `try_lock` after each block. A stream error
//! (device unplugged) ends up in `Player::error`.

use crate::midi_out::{MidiLink, MidiSink};
use crate::mix::MixParams;
use crate::rehearsal::{Engine, Levels, LoadedSong, PlayState, Status};
use crate::rehearsal_queue::{Opener, OpenerThread, QueueEntry};
use crate::{Config, SongProject};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::Stream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Sample rate of the test tone (if the device cannot do it: its default).
pub const TONE_RATE: u32 = 48000;

/// What the user interface can ask of the engine in the callback.
pub enum Command {
    /// a song that is already open (the files are opened on the caller's
    /// thread, the callback only swaps objects)
    Load(Box<LoadedSong>),
    Unload,
    Play,
    Pause,
    Stop,
    /// jump to a position in seconds (`Engine::seek`)
    Seek(f64),
    /// a new rehearsal mix (plain numbers, so the callback frees nothing)
    SetMix(MixParams),
    /// clear the clip flag of the master
    ResetClip,
    /// a new queue (`Engine::set_queue_arc`); the state is `Empty` until a
    /// `Goto`
    SetQueue(Arc<Vec<QueueEntry>>),
    /// the playlist was changed (`Engine::replace_queue`)
    ReplaceQueue(Arc<Vec<QueueEntry>>),
    /// `Engine::goto`
    Goto(usize, bool),
    Next,
    Prev,
    /// `Engine::hold`: wait with the current song, or count down
    Hold(Option<f64>),
}

/// What the callback hands to the reaper thread: dropping these can join a
/// thread or free memory, which the callback must not do.
pub enum Trash {
    Song(Box<LoadedSong>),
    Queue(Arc<Vec<QueueEntry>>),
}

/// The audio output chosen in the settings (the `audio_*` fields of
/// `Config`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioSettings {
    /// host name as `list_hosts` gives it; empty = default host
    pub host: String,
    /// device name; empty = the default device of the host
    pub device: String,
    /// output channels of the left and right side (1-based)
    pub left: u16,
    pub right: u16,
    /// frames; 0 = driver default
    pub buffer: u32,
}

impl From<&Config> for AudioSettings {
    fn from(c: &Config) -> AudioSettings {
        AudioSettings {
            host: c.audio_host.clone(),
            device: c.audio_device.clone(),
            left: c.audio_out_left,
            right: c.audio_out_right,
            buffer: c.audio_buffer,
        }
    }
}

impl Default for AudioSettings {
    fn default() -> AudioSettings {
        AudioSettings::from(&Config::default())
    }
}

impl AudioSettings {
    /// Channels the stream needs at least.
    pub fn channels_needed(&self) -> u16 {
        self.left.max(self.right).max(1)
    }
}

/// An output device with the number of channels it offers at most.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceInfo {
    pub name: String,
    pub max_channels: u16,
    pub is_default: bool,
}

/// What a running stream really uses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StreamInfo {
    pub host: String,
    pub device: String,
    pub rate: u32,
    pub channels: u16,
    /// frames; 0 = driver default
    pub buffer: u32,
    /// e.g. the buffer size the driver did not accept
    pub note: Option<String>,
}

impl StreamInfo {
    /// "48000 Hz, buffer 256 frames" for the settings.
    pub fn describe(&self) -> String {
        let buffer = if self.buffer == 0 { "driver default".to_string() } else { format!("{} frames", self.buffer) };
        format!("{} Hz, buffer {}", self.rate, buffer)
    }
}

// ---------------------------------------------------------------------------
// sample conversion, channel distribution, test tone (no device needed)
// ---------------------------------------------------------------------------

/// A device sample format the engine's `f32` can be converted to.
pub trait OutSample: Copy {
    fn from_f32(v: f32) -> Self;
}

impl OutSample for f32 {
    fn from_f32(v: f32) -> f32 {
        v
    }
}

impl OutSample for i16 {
    fn from_f32(v: f32) -> i16 {
        (v.clamp(-1.0, 1.0) * 32767.0).round() as i16
    }
}

impl OutSample for i32 {
    /// ASIO drivers mostly use 32-bit integers
    fn from_f32(v: f32) -> i32 {
        (f64::from(v.clamp(-1.0, 1.0)) * 2147483647.0).round() as i32
    }
}

impl OutSample for f64 {
    fn from_f32(v: f32) -> f64 {
        f64::from(v)
    }
}

impl OutSample for u16 {
    /// silence (0.0) is 32768
    fn from_f32(v: f32) -> u16 {
        (v.clamp(-1.0, 1.0) * 32767.0 + 32768.0).round() as u16
    }
}

/// Spread interleaved stereo (`stereo`) over a device buffer with
/// `channels` channels: L goes to channel `left`, R to channel `right`
/// (1-based), all other channels get silence. With `left == right` that
/// channel gets (L+R)/2. A channel beyond `channels` is skipped.
pub fn distribute<T: OutSample>(stereo: &[f32], out: &mut [T], channels: usize, left: usize, right: usize) {
    if channels == 0 {
        return;
    }
    for (frame, lr) in out.chunks_exact_mut(channels).zip(stereo.chunks_exact(2)) {
        frame.fill(T::from_f32(0.0));
        if left == right {
            if (1..=channels).contains(&left) {
                frame[left - 1] = T::from_f32((lr[0] + lr[1]) * 0.5);
            }
        } else {
            if (1..=channels).contains(&left) {
                frame[left - 1] = T::from_f32(lr[0]);
            }
            if (1..=channels).contains(&right) {
                frame[right - 1] = T::from_f32(lr[1]);
            }
        }
    }
}

/// The test tone as interleaved stereo, `2 * rate` frames: 1 kHz sine at
/// -20 dBFS (amplitude 0.1), 1 s on the left side only, then 1 s on the
/// right side only, each section with 5 ms fade-in and fade-out.
pub fn test_tone(rate: u32) -> Vec<f32> {
    let n = rate as usize;
    let fade = ((0.005 * rate as f64).round() as usize).max(1);
    let mut out = vec![0.0f32; 4 * n];
    for section in 0..2 {
        for i in 0..n {
            let phase = 2.0 * std::f64::consts::PI * 1000.0 * i as f64 / rate as f64;
            let env = (i as f64 / fade as f64).min((n - 1 - i) as f64 / fade as f64).min(1.0);
            out[2 * (section * n + i) + section] = (0.1 * phase.sin() * env) as f32;
        }
    }
    out
}

// ---------------------------------------------------------------------------
// the stream configuration (no device needed)
// ---------------------------------------------------------------------------

/// Sample format of a supported configuration, in the order of preference.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fmt {
    F32,
    F64,
    I32,
    I16,
    U16,
    Other,
}

/// One supported configuration range of a device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RangeInfo {
    pub channels: u16,
    pub min_rate: u32,
    pub max_rate: u32,
    pub format: Fmt,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PickError {
    /// no usable configuration has this sample rate
    Rate,
    /// the rate works, but the device has fewer channels (it has at most
    /// this many)
    Channels(u16),
}

/// The configuration to open: the index of the range that has `rate`, at
/// least `need` channels and a usable sample format; fewest channels first,
/// then `f32`, `f64`, `i32`, `i16`, `u16`.
pub fn pick_config(ranges: &[RangeInfo], rate: u32, need: u16) -> Result<usize, PickError> {
    let rank = |f: Fmt| match f {
        Fmt::F32 => 0,
        Fmt::F64 => 1,
        Fmt::I32 => 2,
        Fmt::I16 => 3,
        Fmt::U16 => 4,
        Fmt::Other => 5,
    };
    let with_rate: Vec<(usize, &RangeInfo)> = ranges
        .iter()
        .enumerate()
        .filter(|(_, r)| r.format != Fmt::Other && r.min_rate <= rate && rate <= r.max_rate)
        .collect();
    if with_rate.is_empty() {
        return Err(PickError::Rate);
    }
    with_rate
        .iter()
        .filter(|(_, r)| r.channels >= need)
        .min_by_key(|(_, r)| (r.channels, rank(r.format)))
        .map(|(i, _)| *i)
        .ok_or_else(|| PickError::Channels(with_rate.iter().map(|(_, r)| r.channels).max().unwrap_or(0)))
}

fn open_error(device: &str, what: impl std::fmt::Display) -> String {
    format!("Cannot open {}: {}. It may be in use by another application (e.g. your DAW).", device, what)
}

/// The message for a configuration that does not exist on the device.
pub fn pick_error(device: &str, rate: u32, need: u16, e: PickError) -> String {
    match e {
        PickError::Rate => format!("{} does not support {} Hz", device, rate),
        PickError::Channels(max) => format!(
            "{} has only {} output channel{}, but output channel {} is selected",
            device,
            max,
            if max == 1 { "" } else { "s" },
            need
        ),
    }
}

/// What to add to a message about an ignored buffer size: ASIO drivers
/// have their own setting.
pub fn buffer_hint(host: &str) -> &'static str {
    if host.eq_ignore_ascii_case("asio") {
        " Tip: set the buffer size in the ASIO driver's control panel."
    } else {
        ""
    }
}

fn range_info(r: &cpal::SupportedStreamConfigRange) -> RangeInfo {
    RangeInfo {
        channels: r.channels(),
        min_rate: r.min_sample_rate().0,
        max_rate: r.max_sample_rate().0,
        format: match r.sample_format() {
            cpal::SampleFormat::F32 => Fmt::F32,
            cpal::SampleFormat::F64 => Fmt::F64,
            cpal::SampleFormat::I32 => Fmt::I32,
            cpal::SampleFormat::I16 => Fmt::I16,
            cpal::SampleFormat::U16 => Fmt::U16,
            _ => Fmt::Other,
        },
    }
}

// ---------------------------------------------------------------------------
// hosts and devices
// ---------------------------------------------------------------------------

/// The names of the audio hosts that are compiled in (WASAPI; ASIO joins
/// the list with the `asio` feature of cpal).
pub fn list_hosts() -> Vec<String> {
    cpal::available_hosts().into_iter().map(|id| id.name().to_string()).collect()
}

/// Name of the default host.
pub fn default_host_name() -> String {
    cpal::default_host().id().name().to_string()
}

/// The host called `name`; an empty name is the default host.
fn host_by_name(name: &str) -> Result<cpal::Host, String> {
    if name.trim().is_empty() {
        return Ok(cpal::default_host());
    }
    cpal::available_hosts()
        .into_iter()
        .find(|id| id.name().eq_ignore_ascii_case(name.trim()))
        .ok_or_else(|| format!("The audio host {} is not available in this build.", name))
        .and_then(|id| cpal::host_from_id(id).map_err(|e| format!("The audio host {} cannot be used: {}", name, e)))
}

fn max_channels(device: &cpal::Device) -> u16 {
    match device.supported_output_configs() {
        Ok(ranges) => ranges.map(|r| r.channels()).max().unwrap_or(0),
        Err(_) => device.default_output_config().map(|c| c.channels()).unwrap_or(0),
    }
}

/// The message for an ASIO host without a usable driver. cpal lists only the
/// ASIO drivers it can initialise, so the driver of a device that is not
/// connected (e.g. the XR18) is missing, as is a driver another application
/// has open.
pub fn no_asio_driver_message() -> String {
    "No ASIO driver could be opened. Drivers of devices that are not connected (e.g. the XR18), or that another application (e.g. your DAW) uses, are not listed."
        .to_string()
}

/// The output devices of host `host` (empty = default host).
pub fn list_devices(host: &str) -> Result<Vec<DeviceInfo>, String> {
    let is_asio = host.trim().eq_ignore_ascii_case("asio");
    let list = list_devices_of(host)?;
    if is_asio && list.is_empty() {
        return Err(no_asio_driver_message());
    }
    Ok(list)
}

fn list_devices_of(host: &str) -> Result<Vec<DeviceInfo>, String> {
    let host = host_by_name(host)?;
    let default = host.default_output_device().and_then(|d| d.name().ok());
    let devices = host.output_devices().map_err(|e| format!("The audio devices cannot be listed: {}", e))?;
    Ok(devices
        .filter_map(|d| {
            let name = d.name().ok()?;
            Some(DeviceInfo { max_channels: max_channels(&d), is_default: default.as_deref() == Some(name.as_str()), name })
        })
        .collect())
}

fn find_device(host: &cpal::Host, name: &str) -> Result<cpal::Device, String> {
    if name.is_empty() {
        return host
            .default_output_device()
            .ok_or_else(|| "No audio output device found. Check the default playback device of Windows.".to_string());
    }
    let mut devices = host.output_devices().map_err(|e| open_error(name, e))?;
    devices.find(|d| d.name().map(|n| n == name).unwrap_or(false)).ok_or_else(|| open_error(name, "device not found"))
}

// ---------------------------------------------------------------------------
// the callback
// ---------------------------------------------------------------------------

/// Something that fills the device buffer in the audio callback.
pub trait Source: Send + 'static {
    fn fill<T: OutSample>(&mut self, out: &mut [T], channels: usize);

    /// Time until the block that `fill` is called for is played (from the
    /// stream timestamp); called before every `fill`.
    fn set_latency(&mut self, _latency: Duration) {}
}

/// What runs in the audio callback.
pub struct Callback {
    engine: Engine,
    commands: Receiver<Command>,
    /// songs that were replaced: dropping one joins its thread, which must
    /// not happen in the callback
    trash: Sender<Trash>,
    status: Arc<Mutex<Status>>,
    stereo: Vec<f32>,
    left: usize,
    right: usize,
    /// the queue to the MIDI thread; None without MIDI
    midi: Option<MidiSink>,
}

impl Callback {
    pub fn new(
        engine: Engine,
        commands: Receiver<Command>,
        trash: Sender<Trash>,
        status: Arc<Mutex<Status>>,
    ) -> Callback {
        Callback { engine, commands, trash, status, stereo: vec![0.0; 16384], left: 1, right: 2, midi: None }
    }

    /// Hand the MIDI messages of every block to the MIDI thread.
    pub fn with_midi(mut self, sink: Option<MidiSink>) -> Callback {
        self.midi = sink;
        self
    }

    /// The device channels (1-based) of the left and right side.
    pub fn with_outputs(mut self, left: u16, right: u16) -> Callback {
        self.left = left as usize;
        self.right = right as usize;
        self
    }

    fn apply(&mut self, cmd: Command) {
        let old = match cmd {
            Command::Load(song) => self.engine.set_song(*song),
            Command::Unload => self.engine.take_song(),
            Command::SetQueue(queue) => {
                let _ = self.trash.send(Trash::Queue(self.engine.set_queue_arc(queue)));
                None
            }
            Command::ReplaceQueue(queue) => {
                let _ = self.trash.send(Trash::Queue(self.engine.replace_queue(queue).0));
                None
            }
            Command::Goto(index, autoplay) => {
                self.engine.goto(index, autoplay);
                None
            }
            Command::Next => {
                self.engine.next();
                None
            }
            Command::Prev => {
                self.engine.prev();
                None
            }
            Command::Hold(countdown) => {
                self.engine.hold(countdown);
                None
            }
            Command::Play => {
                self.engine.play();
                None
            }
            Command::Pause => {
                self.engine.pause();
                None
            }
            Command::Stop => {
                self.engine.stop();
                None
            }
            Command::Seek(seconds) => {
                self.engine.seek(seconds);
                None
            }
            Command::SetMix(mix) => {
                self.engine.set_mix(&mix);
                None
            }
            Command::ResetClip => {
                self.engine.reset_clip();
                None
            }
        };
        if let Some(old) = old {
            let _ = self.trash.send(Trash::Song(Box::new(old)));
        }
    }

    /// Songs the engine replaced go to the reaper (a rare, small hand-over;
    /// `render` itself only fills the engine's fixed list).
    fn empty_garbage(&mut self) {
        while let Some(song) = self.engine.take_garbage() {
            let _ = self.trash.send(Trash::Song(Box::new(song)));
        }
    }

    /// One audio callback: commands, render, distribute, status.
    pub fn fill<T: OutSample>(&mut self, out: &mut [T], channels: usize) {
        while let Ok(cmd) = self.commands.try_recv() {
            self.apply(cmd);
        }
        let need = out.len() / channels.max(1) * 2;
        if self.stereo.len() < need {
            self.stereo.resize(need, 0.0); // only if the device asks for more than usual
        }
        let rate = self.engine.output_rate();
        let msgs = self.engine.render(&mut self.stereo[..need]);
        if let (Some(sink), false) = (self.midi.as_mut(), msgs.is_empty()) {
            sink.push_block(Instant::now(), msgs, rate);
        }
        self.empty_garbage();
        distribute(&self.stereo[..need], out, channels, self.left, self.right);
        // the peaks are only taken when they are really handed on; the ones
        // the user interface has not read yet stay (the larger peak wins)
        if let Ok(mut st) = self.status.try_lock() {
            let unread = st.levels;
            *st = self.engine.take_status();
            st.levels.merge_peaks(&unread);
        }
    }
}

impl Source for Callback {
    fn fill<T: OutSample>(&mut self, out: &mut [T], channels: usize) {
        Callback::fill(self, out, channels)
    }

    fn set_latency(&mut self, latency: Duration) {
        if let Some(sink) = self.midi.as_mut() {
            sink.set_latency(latency);
        }
    }
}

/// Plays the test tone once, then silence.
struct ToneSource {
    /// interleaved stereo
    samples: Vec<f32>,
    pos: usize,
    /// frames of silence after the tone, so the device buffer runs empty
    tail: usize,
    left: usize,
    right: usize,
    done: Arc<AtomicBool>,
}

impl Source for ToneSource {
    fn fill<T: OutSample>(&mut self, out: &mut [T], channels: usize) {
        let frames = self.samples.len() / 2;
        for frame in out.chunks_exact_mut(channels.max(1)) {
            let lr = if self.pos < frames { [self.samples[2 * self.pos], self.samples[2 * self.pos + 1]] } else { [0.0, 0.0] };
            distribute(&lr, frame, channels, self.left, self.right);
            self.pos += 1;
        }
        if self.pos >= frames + self.tail {
            self.done.store(true, Ordering::Release);
        }
    }
}

// ---------------------------------------------------------------------------
// opening a stream
// ---------------------------------------------------------------------------

/// Which sample rate to open.
#[derive(Clone, Copy)]
enum RateChoice {
    /// exactly this one, else an error
    Exact(u32),
    /// this one, else the default rate of the device
    Prefer(u32),
}

type Lost = Arc<Mutex<Option<String>>>;

/// The error handler of a stream: the first error is kept for `Player::error`.
fn lost_handler(lost: Lost, device: String) -> impl FnMut(cpal::StreamError) + Send + 'static {
    move |e: cpal::StreamError| {
        let text = match e {
            cpal::StreamError::DeviceNotAvailable => {
                format!("Audio device lost: {}. Check the connection and press Play.", device)
            }
            other => format!("Audio stream error on {}: {}. Check the connection and press Play.", device, other),
        };
        if let Ok(mut l) = lost.lock() {
            if l.is_none() {
                *l = Some(text);
            }
        }
    }
}

fn build<T, S>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    mut source: S,
    channels: usize,
    on_error: impl FnMut(cpal::StreamError) + Send + 'static,
) -> Result<Stream, cpal::BuildStreamError>
where
    T: cpal::SizedSample + OutSample,
    S: Source,
{
    device.build_output_stream(
        config,
        move |data: &mut [T], info: &cpal::OutputCallbackInfo| {
            let ts = info.timestamp();
            if let Some(latency) = ts.playback.duration_since(&ts.callback) {
                source.set_latency(latency.min(Duration::from_secs(1)));
            }
            source.fill(data, channels)
        },
        on_error,
        None,
    )
}

/// Open and start a stream on the device of `settings`. `make` creates the
/// source for the final rate; it is called again if the buffer size is
/// rejected and the stream is built a second time.
fn open_stream<S: Source>(
    settings: &AudioSettings,
    rate: RateChoice,
    lost: Lost,
    mut make: impl FnMut(u32) -> S,
) -> Result<(Stream, StreamInfo), String> {
    let host = host_by_name(&settings.host)?;
    let device = find_device(&host, &settings.device)?;
    let name = device.name().unwrap_or_else(|_| "default device".to_string());
    let supported: Vec<cpal::SupportedStreamConfigRange> =
        device.supported_output_configs().map_err(|e| open_error(&name, e))?.collect();
    let infos: Vec<RangeInfo> = supported.iter().map(range_info).collect();
    let need = settings.channels_needed();

    let (rate, index) = match rate {
        RateChoice::Exact(r) => (r, pick_config(&infos, r, need)),
        RateChoice::Prefer(r) => match pick_config(&infos, r, need) {
            Ok(i) => (r, Ok(i)),
            Err(_) => {
                let fallback = device.default_output_config().map(|c| c.sample_rate().0).unwrap_or(r);
                (fallback, pick_config(&infos, fallback, need))
            }
        },
    };
    let index = index.map_err(|e| pick_error(&name, rate, need, e))?;
    let chosen = supported[index].clone().with_sample_rate(cpal::SampleRate(rate));
    let format = chosen.sample_format();
    let channels = chosen.channels();
    let mut config = chosen.config();

    let mut note = None;
    let mut buffer = settings.buffer;
    if buffer > 0 {
        if let cpal::SupportedBufferSize::Range { min, max } = chosen.buffer_size() {
            if buffer < *min || buffer > *max {
                note = Some(format!(
                    "The driver accepts buffers of {} to {} frames, not {}. It uses its default buffer.{}",
                    min,
                    max,
                    buffer,
                    buffer_hint(host.id().name())
                ));
                buffer = 0;
            }
        }
    }

    let mut attempt = |buffer: u32| -> Result<Stream, cpal::BuildStreamError> {
        config.buffer_size = if buffer == 0 { cpal::BufferSize::Default } else { cpal::BufferSize::Fixed(buffer) };
        let source = make(rate);
        let on_error = lost_handler(lost.clone(), name.clone());
        let ch = channels as usize;
        match format {
            cpal::SampleFormat::F32 => build::<f32, S>(&device, &config, source, ch, on_error),
            cpal::SampleFormat::F64 => build::<f64, S>(&device, &config, source, ch, on_error),
            cpal::SampleFormat::I32 => build::<i32, S>(&device, &config, source, ch, on_error),
            cpal::SampleFormat::I16 => build::<i16, S>(&device, &config, source, ch, on_error),
            cpal::SampleFormat::U16 => build::<u16, S>(&device, &config, source, ch, on_error),
            _ => Err(cpal::BuildStreamError::StreamConfigNotSupported),
        }
    };
    let describe = |e: cpal::BuildStreamError| match e {
        cpal::BuildStreamError::StreamConfigNotSupported => pick_error(&name, rate, need, PickError::Rate),
        other => open_error(&name, other),
    };
    let stream = match attempt(buffer) {
        Ok(s) => s,
        Err(_) if buffer > 0 => {
            // the driver refuses the fixed size: its own default instead
            note = Some(format!(
                "The driver did not accept a buffer of {} frames. It uses its default buffer.{}",
                buffer,
                buffer_hint(host.id().name())
            ));
            buffer = 0;
            attempt(0).map_err(describe)?
        }
        Err(e) => return Err(describe(e)),
    };
    stream.play().map_err(|e| open_error(&name, e))?;
    let info = StreamInfo { host: host.id().name().to_string(), device: name, rate, channels, buffer, note };
    Ok((stream, info))
}

// ---------------------------------------------------------------------------
// the player
// ---------------------------------------------------------------------------

/// The open output device with its engine. It runs at one sample rate; a
/// song with another rate needs a new `Player`.
pub struct Player {
    commands: Sender<Command>,
    status: Arc<Mutex<Status>>,
    lost: Lost,
    stream: Option<Stream>,
    reaper: Option<JoinHandle<()>>,
    /// opens the songs of the queue for the engine
    opener: Option<OpenerThread>,
    info: StreamInfo,
    settings: AudioSettings,
}

impl Player {
    /// Open the output of `settings` with sample rate `rate` (that of the
    /// song) and start the stream. The error text is meant for the user.
    /// With a `midi` link the MIDI messages of the song go to its port,
    /// in time with the audio.
    pub fn open(settings: &AudioSettings, rate: u32, midi: Option<&MidiLink>) -> Result<Player, String> {
        let status = Arc::new(Mutex::new(Engine::new(rate).status()));
        let lost: Lost = Arc::new(Mutex::new(None));
        // `make` runs once per build attempt; the ends of the queues of
        // the last attempt stay here
        let mut ends: Option<(Sender<Command>, Receiver<Trash>, Arc<Opener>)> = None;
        let (stream, info) = open_stream(settings, RateChoice::Exact(rate), lost.clone(), |rate| {
            let (commands, rx) = channel();
            let (trash_tx, trash_rx) = channel::<Trash>();
            let engine = Engine::new(rate);
            ends = Some((commands, trash_rx, engine.opener()));
            Callback::new(engine, rx, trash_tx, status.clone())
                .with_outputs(settings.left, settings.right)
                .with_midi(midi.map(|l| l.sink()))
        })?;
        let (commands, trash_rx, opener) = ends.ok_or("the audio stream was not created")?;
        let reaper = std::thread::Builder::new()
            .name("rehearsal-reaper".into())
            .spawn(move || {
                for song in trash_rx {
                    drop(song);
                }
            })
            .map_err(|e| e.to_string())?;
        Ok(Player {
            commands,
            status,
            lost,
            stream: Some(stream),
            reaper: Some(reaper),
            opener: Some(OpenerThread::spawn(opener)),
            info,
            settings: settings.clone(),
        })
    }

    pub fn output_rate(&self) -> u32 {
        self.info.rate
    }

    pub fn device_name(&self) -> &str {
        &self.info.device
    }

    /// What the stream really runs with.
    pub fn info(&self) -> &StreamInfo {
        &self.info
    }

    /// Whether this stream fits `settings` and `rate`; if not, it has to be
    /// closed and opened again.
    pub fn matches(&self, settings: &AudioSettings, rate: u32) -> bool {
        self.info.rate == rate && self.settings == *settings && self.error().is_none()
    }

    /// Set once the stream failed (device unplugged, driver error); the
    /// text is for the user. The owner closes the player then and opens a
    /// new one on the next Play.
    pub fn error(&self) -> Option<String> {
        self.lost.lock().ok().and_then(|l| l.clone())
    }

    pub fn send(&self, cmd: Command) {
        let _ = self.commands.send(cmd);
    }

    /// Open the song's files (here, on the caller's thread) and hand it
    /// to the callback.
    pub fn load(&self, project: &SongProject) {
        self.send(Command::Load(Box::new(LoadedSong::open(project))));
    }

    /// The peaks of the meters since the last call (the clip flag stays
    /// until `Command::ResetClip`).
    pub fn take_levels(&self) -> Levels {
        let mut st = match self.status.lock() {
            Ok(s) => s,
            Err(p) => p.into_inner(),
        };
        let levels = st.levels;
        st.levels.clear_peaks();
        levels
    }

    /// The status of the last callback. After a stream error nothing
    /// plays any more: `Playing` shows as `Paused`.
    pub fn status(&self) -> Status {
        let mut st = match self.status.lock() {
            Ok(s) => s.clone(),
            Err(p) => p.into_inner().clone(),
        };
        if st.state == PlayState::Playing && self.error().is_some() {
            st.state = PlayState::Paused;
        }
        st
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        // ends the callback and with it the engine and its reader thread
        drop(self.stream.take());
        drop(self.opener.take());
        if let Some(r) = self.reaper.take() {
            let _ = r.join();
        }
    }
}

/// A running test tone. Dropping it stops it.
pub struct ToneStream {
    _stream: Stream,
    done: Arc<AtomicBool>,
    info: StreamInfo,
}

impl ToneStream {
    /// The tone has been played to its end.
    pub fn is_done(&self) -> bool {
        self.done.load(Ordering::Acquire)
    }

    pub fn info(&self) -> &StreamInfo {
        &self.info
    }
}

/// Play the test tone on the output of `settings`, at 48 kHz or at the
/// default rate of the device if it cannot do that.
pub fn start_test_tone(settings: &AudioSettings) -> Result<ToneStream, String> {
    let done = Arc::new(AtomicBool::new(false));
    let lost: Lost = Arc::new(Mutex::new(None));
    let flag = done.clone();
    let (stream, info) = open_stream(settings, RateChoice::Prefer(TONE_RATE), lost, |rate| ToneSource {
        samples: test_tone(rate),
        pos: 0,
        tail: rate as usize / 5,
        left: settings.left as usize,
        right: settings.right as usize,
        done: flag.clone(),
    })?;
    Ok(ToneStream { _stream: stream, done, info })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rehearsal::tests::{constant, song, wav24, RATE};
    use crate::mix::MixParams;
    use crate::rehearsal::PlayState;
    use crate::tests::temp_dir;

    #[test]
    fn sample_conversion() {
        assert_eq!(<i16 as OutSample>::from_f32(0.0), 0);
        assert_eq!(<i16 as OutSample>::from_f32(1.0), 32767);
        assert_eq!(<i16 as OutSample>::from_f32(-1.0), -32767);
        assert_eq!(<i16 as OutSample>::from_f32(3.0), 32767);
        assert_eq!(<u16 as OutSample>::from_f32(0.0), 32768);
        assert_eq!(<u16 as OutSample>::from_f32(1.0), 65535);
        assert_eq!(<u16 as OutSample>::from_f32(-1.0), 1);
        assert_eq!(<u16 as OutSample>::from_f32(-5.0), 1);
        // the formats ASIO drivers use: i32 and f64 (and f32 unchanged)
        assert_eq!(<i32 as OutSample>::from_f32(0.0), 0);
        assert_eq!(<i32 as OutSample>::from_f32(1.0), i32::MAX);
        assert_eq!(<i32 as OutSample>::from_f32(-1.0), -i32::MAX);
        assert_eq!(<i32 as OutSample>::from_f32(2.5), i32::MAX);
        assert_eq!(<i32 as OutSample>::from_f32(-2.5), -i32::MAX);
        assert_eq!(<i32 as OutSample>::from_f32(0.5), 1073741824);
        assert_eq!(<f64 as OutSample>::from_f32(0.0), 0.0);
        assert_eq!(<f64 as OutSample>::from_f32(1.0), 1.0);
        assert_eq!(<f64 as OutSample>::from_f32(-1.0), -1.0);
        assert_eq!(<f64 as OutSample>::from_f32(0.25), 0.25);
        assert_eq!(<f32 as OutSample>::from_f32(1.0), 1.0);
        assert_eq!(<f32 as OutSample>::from_f32(-1.0), -1.0);
        assert_eq!(<f32 as OutSample>::from_f32(0.0), 0.0);
    }

    #[test]
    fn integer_and_f64_formats_fill_the_channels() {
        let st = [0.5f32, -0.5, 1.0, -1.0];
        let mut a = [7i32; 6];
        distribute(&st, &mut a, 3, 1, 3);
        assert_eq!(a, [1073741824, 0, -1073741824, i32::MAX, 0, -i32::MAX]);
        let mut b = [7.0f64; 6];
        distribute(&st, &mut b, 3, 1, 3);
        assert_eq!(b, [0.5, 0.0, -0.5, 1.0, 0.0, -1.0]);
    }

    #[test]
    fn asio_formats_are_picked_in_the_order_of_preference() {
        let r = |f| range(18, 44100, 48000, f);
        assert_eq!(pick_config(&[r(Fmt::I16), r(Fmt::I32)], 48000, 2), Ok(1));
        assert_eq!(pick_config(&[r(Fmt::I32), r(Fmt::F64)], 48000, 2), Ok(1));
        assert_eq!(pick_config(&[r(Fmt::F64), r(Fmt::F32)], 48000, 2), Ok(1));
        assert_eq!(pick_config(&[r(Fmt::I32)], 48000, 18), Ok(0));
        assert_eq!(pick_config(&[r(Fmt::I32)], 48000, 19), Err(PickError::Channels(18)));
    }

    /// With the `asio` feature the host list has ASIO. Needs no device.
    #[cfg(feature = "asio")]
    #[test]
    fn asio_is_a_host_with_the_asio_feature() {
        assert!(list_hosts().iter().any(|h| h == "ASIO"));
    }

    /// Prints the installed ASIO drivers (needs the `asio` feature and
    /// drivers on this PC): `cargo test --features asio -- --ignored --nocapture asio_drivers`
    #[cfg(feature = "asio")]
    #[test]
    #[ignore]
    fn asio_drivers_are_listed() {
        match list_devices("ASIO") {
            Ok(devices) => {
                for d in &devices {
                    println!("ASIO driver: {} ({} output channels)", d.name, d.max_channels);
                }
            }
            Err(e) => println!("{}", e),
        }
    }

    #[test]
    fn the_message_for_a_missing_asio_driver_names_the_reasons() {
        let m = no_asio_driver_message();
        assert!(m.contains("not connected") && m.contains("DAW"));
    }

    #[test]
    fn the_buffer_hint_names_the_asio_control_panel() {
        assert!(buffer_hint("ASIO").contains("set the buffer size in the ASIO driver's control panel"));
        assert_eq!(buffer_hint("WASAPI"), "");
    }

    /// the interleaved stereo of the frames `0..n`: L = 0.1 * (k+1), R = -0.1 * (k+1)
    fn stereo(n: usize) -> Vec<f32> {
        (0..n).flat_map(|k| [0.1 * (k + 1) as f32, -0.1 * (k + 1) as f32]).collect()
    }

    #[test]
    fn channel_distribution_for_2_4_and_18_channels() {
        let st = stereo(2);
        let mut two = [9.0f32; 4];
        distribute(&st, &mut two, 2, 1, 2);
        assert_eq!(two, [0.1, -0.1, 0.2, -0.2]);
        let mut four = [9.0f32; 8];
        distribute(&st, &mut four, 4, 1, 2);
        assert_eq!(four, [0.1, -0.1, 0.0, 0.0, 0.2, -0.2, 0.0, 0.0]);
        // the XR18: 18 channels, the mix on 17 and 18
        let mut xr = vec![9.0f32; 36];
        distribute(&st, &mut xr, 18, 17, 18);
        for (f, want) in [(0, (0.1, -0.1)), (1, (0.2, -0.2))] {
            let frame = &xr[f * 18..(f + 1) * 18];
            for (c, s) in frame.iter().enumerate() {
                match c {
                    16 => assert_eq!(*s, want.0),
                    17 => assert_eq!(*s, want.1),
                    _ => assert_eq!(*s, 0.0, "frame {} channel {}", f, c + 1),
                }
            }
        }
    }

    #[test]
    fn swapped_and_equal_channels() {
        let st = stereo(1);
        // L = 4, R = 3: left goes to channel 4, right to channel 3
        let mut out = [9.0f32; 4];
        distribute(&st, &mut out, 4, 4, 3);
        assert_eq!(out, [0.0, 0.0, -0.1, 0.1]);
        // left == right: the mean on that channel, the others silent
        let mut out = [9.0f32; 4];
        distribute(&[0.5, 0.25], &mut out, 4, 2, 2);
        assert_eq!(out, [0.0, 0.375, 0.0, 0.0]);
        // a channel beyond the device is skipped, nothing panics
        let mut out = [9.0f32; 2];
        distribute(&[0.5, 0.25], &mut out, 2, 1, 5);
        assert_eq!(out, [0.5, 0.0]);
        let mut none: [f32; 0] = [];
        distribute(&[0.5, 0.25], &mut none, 0, 1, 2);
        // other sample formats
        let mut ints = [9i16; 4];
        distribute(&stereo(2), &mut ints, 2, 1, 2);
        assert_eq!(ints, [3277, -3277, 6553, -6553]);
    }

    #[test]
    fn test_tone_shape() {
        for rate in [48000u32, 44100, 96000] {
            let t = test_tone(rate);
            let n = rate as usize;
            assert_eq!(t.len(), 4 * n, "2 x rate frames");
            let ch = |c: usize, from: usize, to: usize| -> Vec<f32> { (from..to).map(|f| t[2 * f + c]).collect() };
            let peak = |v: &[f32]| v.iter().fold(0.0f32, |m, s| m.max(s.abs()));
            // first second: left only; second second: right only
            assert!((peak(&ch(0, 0, n)) - 0.1).abs() < 1e-3, "rate {}", rate);
            assert_eq!(peak(&ch(1, 0, n)), 0.0);
            assert_eq!(peak(&ch(0, n, 2 * n)), 0.0);
            assert!((peak(&ch(1, n, 2 * n)) - 0.1).abs() < 1e-3);
            // edges fade in and out
            assert!(t[0].abs() < 1e-6);
            assert!(t[2 * (n - 1)].abs() < 1e-6);
            assert!(t[2 * n + 1].abs() < 1e-6);
            assert!(t[2 * (2 * n - 1) + 1].abs() < 1e-6);
            let fade = (0.005 * rate as f64).round() as usize;
            assert!(t[2 * (fade / 2)].abs() < 0.06, "half way into the fade");
            // 1 kHz: upward zero crossings in the middle of the left section
            let mid = ch(0, n / 4, 3 * n / 4);
            let crossings = mid.windows(2).filter(|w| w[0] <= 0.0 && w[1] > 0.0).count();
            let seconds = mid.len() as f64 / rate as f64;
            let freq = crossings as f64 / seconds;
            assert!((freq - 1000.0).abs() < 10.0, "rate {}: {} Hz", rate, freq);
        }
    }

    #[test]
    fn test_tone_goes_to_the_chosen_channels() {
        let t = test_tone(48000);
        let mut out = vec![0.0f32; 4 * 48000 * 2];
        distribute(&t, &mut out, 4, 4, 3);
        let col = |c: usize, f: usize| out[f * 4 + c];
        // first second: the left side on channel 4, silence on channel 3
        let first: f32 = (0..48000).map(|f| col(3, f).abs()).fold(0.0, f32::max);
        assert!((first - 0.1).abs() < 1e-3);
        assert!((0..48000).all(|f| col(2, f) == 0.0));
        // second: the right side on channel 3
        let second: f32 = (48000..96000).map(|f| col(2, f).abs()).fold(0.0, f32::max);
        assert!((second - 0.1).abs() < 1e-3);
        assert!((48000..96000).all(|f| col(3, f) == 0.0));
    }

    fn range(channels: u16, min_rate: u32, max_rate: u32, format: Fmt) -> RangeInfo {
        RangeInfo { channels, min_rate, max_rate, format }
    }

    #[test]
    fn the_stream_configuration_follows_rate_and_channels() {
        let ranges = [
            range(2, 44100, 48000, Fmt::I16),
            range(2, 44100, 48000, Fmt::F32),
            range(8, 44100, 96000, Fmt::F32),
            range(18, 48000, 48000, Fmt::I16),
            range(2, 8000, 96000, Fmt::Other),
        ];
        // fewest channels that are enough, f32 before i16
        assert_eq!(pick_config(&ranges, 48000, 2), Ok(1));
        assert_eq!(pick_config(&ranges, 48000, 1), Ok(1));
        assert_eq!(pick_config(&ranges, 48000, 3), Ok(2));
        assert_eq!(pick_config(&ranges, 48000, 18), Ok(3));
        // 96 kHz is only there on 8 channels (the "Other" format does not count)
        assert_eq!(pick_config(&ranges, 96000, 2), Ok(2));
        assert_eq!(pick_config(&ranges, 96000, 9), Err(PickError::Channels(8)));
        assert_eq!(pick_config(&ranges, 22050, 2), Err(PickError::Rate));
        assert_eq!(pick_config(&[], 48000, 2), Err(PickError::Rate));
        assert_eq!(pick_error("XR18", 22050, 2, PickError::Rate), "XR18 does not support 22050 Hz");
        assert_eq!(
            pick_error("Speakers", 48000, 4, PickError::Channels(2)),
            "Speakers has only 2 output channels, but output channel 4 is selected"
        );
        assert!(pick_error("Mono", 48000, 2, PickError::Channels(1)).contains("only 1 output channel,"));
    }

    #[test]
    fn settings_come_from_the_config() {
        let c = Config {
            audio_host: "WASAPI".into(),
            audio_device: "XR18".into(),
            audio_out_left: 17,
            audio_out_right: 3,
            audio_buffer: 512,
            ..Config::default()
        };
        let s = AudioSettings::from(&c);
        assert_eq!((s.host.as_str(), s.device.as_str(), s.left, s.right, s.buffer), ("WASAPI", "XR18", 17, 3, 512));
        assert_eq!(s.channels_needed(), 17);
        let d = AudioSettings::default();
        assert_eq!((d.host.as_str(), d.device.as_str(), d.left, d.right, d.buffer), ("", "", 1, 2, 0));
        let info = StreamInfo { host: "WASAPI".into(), device: "x".into(), rate: 44100, channels: 2, buffer: 0, note: None };
        assert_eq!(info.describe(), "44100 Hz, buffer driver default");
        assert_eq!(StreamInfo { buffer: 256, ..info }.describe(), "44100 Hz, buffer 256 frames");
    }

    /// no device needed: the answers come from the names alone
    #[test]
    fn missing_host_or_device_gives_a_clear_message() {
        assert!(list_hosts().contains(&default_host_name()));
        let gone = AudioSettings { device: "Gibt es nicht".into(), ..AudioSettings::default() };
        let e = Player::open(&gone, 48000, None).err().expect("a device that does not exist cannot be opened");
        assert!(e.starts_with("Cannot open Gibt es nicht: "), "{}", e);
        assert!(e.ends_with("It may be in use by another application (e.g. your DAW)."), "{}", e);
        let e = start_test_tone(&gone).err().expect("no tone without the device");
        assert!(e.starts_with("Cannot open Gibt es nicht: "), "{}", e);
        let no_host = AudioSettings { host: "NoSuchHost".into(), ..AudioSettings::default() };
        let e = Player::open(&no_host, 48000, None).err().unwrap();
        assert!(e.contains("NoSuchHost"), "{}", e);
        assert!(list_devices("NoSuchHost").is_err());
    }

    #[test]
    fn the_tone_source_ends_after_the_tone_and_its_tail() {
        let done = Arc::new(AtomicBool::new(false));
        let mut src = ToneSource { samples: test_tone(8000), pos: 0, tail: 100, left: 2, right: 1, done: done.clone() };
        let mut out = vec![9.0f32; 4 * 8000]; // one section = 8000 frames, 4 channels
        src.fill(&mut out, 4);
        // first section on the left side = channel 2, nothing elsewhere
        assert!(out.chunks(4).all(|f| f[0] == 0.0 && f[2] == 0.0 && f[3] == 0.0));
        assert!(out.chunks(4).any(|f| f[1] > 0.09));
        assert!(!done.load(Ordering::Acquire));
        src.fill(&mut out, 4); // the right side on channel 1
        assert!(out.chunks(4).any(|f| f[0] > 0.09));
        assert!(!done.load(Ordering::Acquire));
        src.fill(&mut out, 4); // past the tone, 100 frames of tail
        assert!(out.iter().all(|&s| s == 0.0));
        assert!(done.load(Ordering::Acquire));
    }

    /// the callback without a device: commands in, samples and status out
    #[test]
    fn callback_runs_commands_and_publishes_the_status() {
        let dir = temp_dir("ao-callback");
        let a = wav24(&dir, "a", RATE, 1, &constant(2000, 0.25));
        let b = wav24(&dir, "b", RATE, 1, &constant(2000, 0.5));
        let (tx, rx) = channel();
        let (trash_tx, trash_rx) = channel();
        let engine = Engine::new(RATE);
        let status = Arc::new(Mutex::new(engine.status()));
        let mut cb = Callback::new(engine, rx, trash_tx, status.clone());

        let mut out = [1.0f32; 8];
        cb.fill(&mut out, 2);
        assert_eq!(out, [0.0; 8], "no song: silence");
        assert_eq!(status.lock().unwrap().state, PlayState::Empty);

        tx.send(Command::Load(Box::new(LoadedSong::open(&song(&dir, &[&a]))))).unwrap();
        tx.send(Command::Play).unwrap();
        // the callback may have to wait for the reader thread
        let mut out = [0.0f32; 8];
        for _ in 0..2000 {
            cb.fill(&mut out, 2);
            if out[0] != 0.0 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert!((out[0] - 0.25 * 0.707_106_77).abs() < 1e-6);
        assert_eq!(status.lock().unwrap().state, PlayState::Playing);
        assert!(status.lock().unwrap().position_frames >= 4);

        // replacing the song hands the old one to the trash, not to a join
        tx.send(Command::Load(Box::new(LoadedSong::open(&song(&dir, &[&b]))))).unwrap();
        cb.fill(&mut out, 2);
        let old = trash_rx.try_recv().expect("the old song goes to the trash");
        drop(old);
        assert_eq!(status.lock().unwrap().state, PlayState::Stopped);
        assert_eq!(status.lock().unwrap().position_frames, 0);

        tx.send(Command::Unload).unwrap();
        cb.fill(&mut out, 2);
        assert_eq!(status.lock().unwrap().state, PlayState::Empty);
        assert!(trash_rx.try_recv().is_ok());

        // a busy lock only skips one update
        let guard = status.lock().unwrap();
        cb.fill(&mut out, 2);
        drop(guard);
        drop(cb);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_mix_commands_reach_the_engine_and_the_peaks_are_kept_until_read() {
        let dir = temp_dir("ao-mix");
        let a = wav24(&dir, "a", RATE, 1, &constant(48000, 0.9));
        let b = wav24(&dir, "b", RATE, 1, &constant(48000, 0.9));
        let (tx, rx) = channel();
        let (trash_tx, _trash_rx) = channel();
        let engine = Engine::new(RATE);
        let status = Arc::new(Mutex::new(engine.status()));
        let mut cb = Callback::new(engine, rx, trash_tx, status.clone());
        let mut prj = song(&dir, &[&a, &b]);
        prj.tracks[1].channel = 0;
        let mut mix = MixParams::default();
        mix.master_db = -6.0;
        tx.send(Command::SetMix(mix)).unwrap();
        tx.send(Command::Load(Box::new(LoadedSong::open(&prj)))).unwrap();
        tx.send(Command::Play).unwrap();
        let mut out = [0.0f32; 64];
        for _ in 0..2000 {
            cb.fill(&mut out, 2); // the first blocks wait for the reader thread
            if status.lock().unwrap().position_frames >= 32 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        // 1.8 x 0.7071 x 0.5012 = 0.638
        assert!((out[0] - 0.638).abs() < 1e-3, "{}", out[0]);
        let peak = status.lock().unwrap().levels.strips[0];
        assert!((peak - 1.8).abs() < 1e-5, "{}", peak);
        // the user interface has not read: a later, quieter update keeps the peak
        tx.send(Command::SetMix({
            let mut m = MixParams::default();
            m.strips[0].level_db = -60.0;
            m
        }))
        .unwrap();
        for _ in 0..40 {
            cb.fill(&mut [0.0f32; 64], 2);
        }
        assert!(status.lock().unwrap().levels.strips[0] >= 1.8);
        assert!(!status.lock().unwrap().levels.clip);
        tx.send(Command::ResetClip).unwrap();
        cb.fill(&mut out, 2);
        drop(cb);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// the queue commands, with the opener working between the callbacks
    #[test]
    fn the_queue_commands_reach_the_engine_and_replaced_songs_go_to_the_trash() {
        let dir = temp_dir("ao-queue");
        let a = wav24(&dir, "a", RATE, 1, &constant(4800, 0.25));
        let prj = song(&dir, &[&a]);
        let queue = Arc::new(vec![QueueEntry::single(prj.clone()), QueueEntry::single(prj)]);
        let (tx, rx) = channel();
        let (trash_tx, trash_rx) = channel();
        let engine = Engine::new(RATE);
        let opener = engine.opener();
        let status = Arc::new(Mutex::new(engine.status()));
        let mut cb = Callback::new(engine, rx, trash_tx, status.clone());
        tx.send(Command::SetQueue(queue.clone())).unwrap();
        tx.send(Command::Goto(0, true)).unwrap();
        let mut out = [0.0f32; 64];
        for _ in 0..2000 {
            cb.fill(&mut out, 2);
            opener.work(); // the opener thread of the Player
            if out[0] != 0.0 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert!((out[0] - 0.25 * 0.707_106_77).abs() < 1e-6);
        let st = status.lock().unwrap().clone();
        assert_eq!((st.state, st.index, st.queue_len), (PlayState::Playing, Some(0), 2));
        // next while playing, then the playlist is replaced by an empty one
        tx.send(Command::Next).unwrap();
        tx.send(Command::Hold(None)).unwrap();
        cb.fill(&mut out, 2);
        opener.work();
        cb.fill(&mut out, 2);
        let st = status.lock().unwrap().clone();
        assert_eq!((st.state, st.index), (PlayState::Waiting, Some(1)));
        tx.send(Command::ReplaceQueue(Arc::new(Vec::new()))).unwrap();
        cb.fill(&mut out, 2);
        assert_eq!(status.lock().unwrap().state, PlayState::Empty);
        // songs and the old queue went to the trash, nothing was dropped in the callback
        let mut songs = 0;
        let mut queues = 0;
        while let Ok(t) = trash_rx.try_recv() {
            match t {
                Trash::Song(_) => songs += 1,
                Trash::Queue(_) => queues += 1,
            }
        }
        assert!(songs >= 1 && queues >= 2, "{} songs, {} queues", songs, queues);
        drop(cb);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_seek_command_moves_the_position() {
        let dir = temp_dir("ao-seek");
        let a = wav24(&dir, "a", RATE, 1, &constant(48000, 0.25));
        let (tx, rx) = channel();
        let (trash_tx, _trash_rx) = channel();
        let engine = Engine::new(RATE);
        let status = Arc::new(Mutex::new(engine.status()));
        let mut cb = Callback::new(engine, rx, trash_tx, status.clone());
        tx.send(Command::Load(Box::new(LoadedSong::open(&song(&dir, &[&a]))))).unwrap();
        tx.send(Command::Seek(0.5)).unwrap();
        let mut out = [0.0f32; 8];
        cb.fill(&mut out, 2);
        let st = status.lock().unwrap().clone();
        assert_eq!((st.state, st.position_frames), (PlayState::Stopped, 24000));
        drop(cb);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn large_device_buffers_are_handled() {
        let (_tx, rx) = channel();
        let (trash_tx, _trash_rx) = channel();
        let engine = Engine::new(RATE);
        let status = Arc::new(Mutex::new(engine.status()));
        let mut cb = Callback::new(engine, rx, trash_tx, status);
        let mut out = vec![1i16; 40000 * 6];
        cb.fill(&mut out, 6);
        assert!(out.iter().all(|&s| s == 0));
    }
}

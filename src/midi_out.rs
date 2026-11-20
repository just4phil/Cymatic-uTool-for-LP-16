//! The MIDI output adapter of the rehearsal player (`midir`).
//!
//! The audio callback never sends MIDI itself. It puts the messages of a
//! block into a `rtrb` queue (`MidiSink`), each with a target time:
//!
//! target = `Instant::now()` in the callback + (playback - callback of
//! `OutputCallbackInfo::timestamp()`) + offset / rate
//!
//! (`target_time`). A MIDI thread (`MidiLink`) takes the messages out,
//! sleeps until 1 ms before a target, waits for the rest with `yield_now`
//! and sends the message to the port. A target in the past is sent at once.
//! The thread also tracks which channels have sounding notes: when the
//! audio stream ends (or the program does) it sends All Notes Off on them
//! first.
//!
//! The link outlives the audio stream: the owner (the Rehearsal tab) keeps
//! one `MidiLink`, hands a new `MidiSink` to each stream it opens and uses
//! it for the MIDI test of the settings. The port is only opened when it
//! is needed (`MidiLink::open`: Play and the test), and again at the next
//! Play if that failed.
//!
//! Without hardware only the plain parts can be tested: `target_time`, the
//! `Pump` (queue to port) and `list_midi_outputs` (which needs no port).

use crate::rehearsal_midi::{MidiMsg, ALL_NOTES_OFF};
use midir::{MidiOutput, MidiOutputConnection};
use rtrb::{Consumer, Producer, RingBuffer};
use std::collections::VecDeque;
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const CLIENT_NAME: &str = "lp16-tool";
/// Messages the queue between callback and MIDI thread holds.
const QUEUE_SLOTS: usize = 8192;
/// The MIDI thread sleeps until this long before a target, then spins.
const SPIN: Duration = Duration::from_millis(1);

/// A message with the moment it is due.
#[derive(Clone, Copy, Debug)]
pub struct TimedMsg {
    pub target: Instant,
    pub len: u8,
    pub bytes: [u8; 3],
}

impl TimedMsg {
    fn data(&self) -> &[u8] {
        &self.bytes[..self.len as usize]
    }
}

/// When a message at frame `offset` of a block is heard: the time of the
/// callback, plus the time until the block is played (`latency`, from the
/// stream timestamp), plus the offset in the block.
pub fn target_time(now: Instant, latency: Duration, offset: usize, rate: u32) -> Instant {
    now + latency + Duration::from_secs_f64(offset as f64 / rate.max(1) as f64)
}

/// The MIDI output ports of the system (names). An error text if MIDI
/// cannot be used at all.
pub fn list_midi_outputs() -> Result<Vec<String>, String> {
    let out = MidiOutput::new(CLIENT_NAME).map_err(|e| format!("MIDI is not available: {}", e))?;
    Ok(out.ports().iter().filter_map(|p| out.port_name(p).ok()).collect())
}

fn not_available(name: &str) -> String {
    format!("MIDI port not available: {}", name)
}

fn connect(name: &str) -> Result<MidiOutputConnection, String> {
    let out = MidiOutput::new(CLIENT_NAME).map_err(|_| not_available(name))?;
    let port = out
        .ports()
        .into_iter()
        .find(|p| out.port_name(p).map(|n| n == name).unwrap_or(false))
        .ok_or_else(|| not_available(name))?;
    out.connect(&port, CLIENT_NAME).map_err(|_| not_available(name))
}

// ---------------------------------------------------------------------------
// callback side
// ---------------------------------------------------------------------------

/// The end of the queue the audio callback writes to.
pub struct MidiSink {
    producer: Producer<TimedMsg>,
    latency: Duration,
}

impl MidiSink {
    /// Time until the block of the running callback is played.
    pub fn set_latency(&mut self, latency: Duration) {
        self.latency = latency;
    }

    /// Queue the messages of a block (no allocation, never blocks). If the
    /// queue is full the rest is dropped.
    pub fn push_block(&mut self, now: Instant, msgs: &[MidiMsg], rate: u32) {
        for m in msgs {
            let t = TimedMsg { target: target_time(now, self.latency, m.offset, rate), len: m.len, bytes: m.bytes };
            if self.producer.push(t).is_err() {
                break;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// the pump: queue -> port, in time (no port needed to test it)
// ---------------------------------------------------------------------------

/// Takes the messages of one queue, holds them until they are due and
/// remembers which channels have sounding notes.
pub struct Pump {
    consumer: Consumer<TimedMsg>,
    pending: VecDeque<TimedMsg>,
    /// bit n: a Note On was sent on channel n since the last All Notes Off
    sounding: u16,
}

impl Pump {
    pub fn new(consumer: Consumer<TimedMsg>) -> Pump {
        Pump { consumer, pending: VecDeque::with_capacity(QUEUE_SLOTS), sounding: 0 }
    }

    fn track(&mut self, bytes: &[u8]) {
        if bytes.len() == 3 {
            let ch = bytes[0] & 0x0f;
            match bytes[0] & 0xf0 {
                0x90 if bytes[2] > 0 => self.sounding |= 1 << ch,
                0xb0 if bytes[1] == ALL_NOTES_OFF => self.sounding &= !(1 << ch),
                _ => {}
            }
        }
    }

    /// Send everything that is due at `now` (in the order it came), then
    /// return the target of the next message, if any.
    pub fn pump(&mut self, now: Instant, send: &mut impl FnMut(&[u8])) -> Option<Instant> {
        while let Ok(m) = self.consumer.pop() {
            self.pending.push_back(m);
        }
        while let Some(m) = self.pending.front().copied() {
            if m.target > now {
                return Some(m.target);
            }
            self.pending.pop_front();
            self.track(m.data());
            send(m.data());
        }
        None
    }

    /// The writing end is gone (the stream ended).
    pub fn abandoned(&self) -> bool {
        self.consumer.is_abandoned()
    }

    /// Drop what is not yet due and send All Notes Off on the channels that
    /// have sounding notes.
    pub fn finish(&mut self, send: &mut impl FnMut(&[u8])) {
        while self.consumer.pop().is_ok() {}
        self.pending.clear();
        for ch in 0..16u8 {
            if self.sounding & (1 << ch) != 0 {
                send(&[0xb0 | ch, ALL_NOTES_OFF, 0]);
            }
        }
        self.sounding = 0;
    }
}

// ---------------------------------------------------------------------------
// the MIDI thread
// ---------------------------------------------------------------------------

/// What the user interface shows about the port.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PortStatus {
    /// the chosen port; empty = MIDI is off
    pub port: String,
    pub open: bool,
    /// why the port could not be opened ("MIDI port not available: <Name>")
    pub error: Option<String>,
}

enum Control {
    SetPort(String),
    Open,
    Send(Vec<MidiMsg>),
    Attach(Consumer<TimedMsg>),
    Quit,
}

struct Worker {
    port: String,
    conn: Option<MidiOutputConnection>,
    pump: Option<Pump>,
    status: Arc<Mutex<PortStatus>>,
}

impl Worker {
    fn publish(&self, error: Option<String>) {
        if let Ok(mut s) = self.status.lock() {
            *s = PortStatus { port: self.port.clone(), open: self.conn.is_some(), error };
        }
    }

    fn send(conn: &mut Option<MidiOutputConnection>, bytes: &[u8]) {
        if let Some(c) = conn.as_mut() {
            let _ = c.send(bytes);
        }
    }

    /// Open the port if it is chosen and not open yet.
    fn ensure_open(&mut self) {
        if self.port.is_empty() || self.conn.is_some() {
            return;
        }
        match connect(&self.port) {
            Ok(c) => {
                self.conn = Some(c);
                self.publish(None);
            }
            Err(e) => self.publish(Some(e)),
        }
    }

    /// End the queue that is attached: All Notes Off for what sounds.
    fn end_pump(&mut self) {
        if let Some(mut p) = self.pump.take() {
            let conn = &mut self.conn;
            p.finish(&mut |b| Worker::send(conn, b));
        }
    }

    fn close(&mut self) {
        self.end_pump();
        if let Some(c) = self.conn.take() {
            let _ = c.close();
        }
    }

    /// False when the thread should end.
    fn control(&mut self, c: Control) -> bool {
        match c {
            Control::SetPort(name) => {
                if name != self.port {
                    self.close();
                    self.port = name;
                    self.publish(None);
                }
            }
            Control::Open => {
                self.ensure_open();
            }
            Control::Send(msgs) => {
                self.ensure_open();
                for m in msgs {
                    Worker::send(&mut self.conn, m.data());
                }
            }
            Control::Attach(consumer) => {
                self.end_pump();
                self.pump = Some(Pump::new(consumer));
            }
            Control::Quit => {
                self.close();
                return false;
            }
        }
        true
    }

    fn run(mut self, rx: Receiver<Control>) {
        loop {
            // pump first: a message that is due must not wait for control
            let now = Instant::now();
            let mut next = None;
            let mut gone = false;
            if let Some(p) = self.pump.as_mut() {
                let conn = &mut self.conn;
                next = p.pump(now, &mut |b| Worker::send(conn, b));
                gone = p.abandoned() && next.is_none();
            }
            if gone {
                self.end_pump();
            }
            let wait = match (&self.pump, next) {
                (None, _) => None,
                (Some(_), None) => Some(SPIN),
                (Some(_), Some(t)) => {
                    let left = t.saturating_duration_since(Instant::now());
                    if left > SPIN + SPIN / 2 {
                        Some(SPIN)
                    } else {
                        std::thread::yield_now();
                        Some(Duration::ZERO)
                    }
                }
            };
            let msg = match wait {
                None => match rx.recv_timeout(Duration::from_millis(50)) {
                    Ok(c) => Some(c),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => Some(Control::Quit),
                },
                Some(d) if d.is_zero() => match rx.try_recv() {
                    Ok(c) => Some(c),
                    Err(TryRecvError::Empty) => None,
                    Err(TryRecvError::Disconnected) => Some(Control::Quit),
                },
                Some(d) => match rx.try_recv() {
                    Ok(c) => Some(c),
                    Err(TryRecvError::Empty) => {
                        std::thread::sleep(d);
                        None
                    }
                    Err(TryRecvError::Disconnected) => Some(Control::Quit),
                },
            };
            if let Some(c) = msg {
                if !self.control(c) {
                    return;
                }
            }
        }
    }
}

/// The MIDI thread and its port. Dropping it sends All Notes Off for
/// sounding notes and closes the port.
pub struct MidiLink {
    tx: Sender<Control>,
    status: Arc<Mutex<PortStatus>>,
    handle: Option<JoinHandle<()>>,
}

impl MidiLink {
    /// Start the thread; no port is open yet.
    pub fn new() -> MidiLink {
        let (tx, rx) = channel();
        let status = Arc::new(Mutex::new(PortStatus::default()));
        let worker = Worker { port: String::new(), conn: None, pump: None, status: status.clone() };
        let handle = std::thread::Builder::new().name("rehearsal-midi".into()).spawn(move || worker.run(rx)).ok();
        MidiLink { tx, status, handle }
    }

    /// Choose the port (empty = off). A port that is open and differs is
    /// closed; the new one is opened by `open`.
    pub fn set_port(&self, name: &str) {
        let _ = self.tx.send(Control::SetPort(name.to_string()));
    }

    /// Open the chosen port now if it is not open (again after an error).
    pub fn open(&self) {
        let _ = self.tx.send(Control::Open);
    }

    /// Send messages at once (the MIDI test); opens the port if needed.
    pub fn send(&self, msgs: &[MidiMsg]) {
        let _ = self.tx.send(Control::Send(msgs.to_vec()));
    }

    /// A new queue for the audio callback of a stream. It replaces the
    /// queue of the stream before.
    pub fn sink(&self) -> MidiSink {
        let (producer, consumer) = RingBuffer::new(QUEUE_SLOTS);
        let _ = self.tx.send(Control::Attach(consumer));
        MidiSink { producer, latency: Duration::ZERO }
    }

    pub fn status(&self) -> PortStatus {
        self.status.lock().map(|s| s.clone()).unwrap_or_default()
    }
}

impl Default for MidiLink {
    fn default() -> MidiLink {
        MidiLink::new()
    }
}

impl MidiLink {
    /// End the thread: All Notes Off for sounding notes, then the port is
    /// closed. Later calls do nothing.
    pub fn shutdown(&mut self) {
        let _ = self.tx.send(Control::Quit);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for MidiLink {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn timed(target: Instant, bytes: &[u8]) -> TimedMsg {
        let mut b = [0u8; 3];
        b[..bytes.len()].copy_from_slice(bytes);
        TimedMsg { target, len: bytes.len() as u8, bytes: b }
    }

    #[test]
    fn the_target_is_callback_time_plus_latency_plus_offset() {
        let now = Instant::now();
        // no offset, no latency: now
        assert_eq!(target_time(now, Duration::ZERO, 0, 48000), now);
        // 10 ms until the block is heard
        assert_eq!(target_time(now, Duration::from_millis(10), 0, 48000), now + Duration::from_millis(10));
        // 480 frames at 48 kHz = 10 ms, 441 frames at 44.1 kHz = 10 ms
        let d = |a: Instant, b: Instant| (b - a).as_secs_f64();
        assert!((d(now, target_time(now, Duration::ZERO, 480, 48000)) - 0.010).abs() < 1e-9);
        assert!((d(now, target_time(now, Duration::ZERO, 441, 44100)) - 0.010).abs() < 1e-9);
        // all three
        let t = target_time(now, Duration::from_micros(5500), 240, 48000);
        assert!((d(now, t) - 0.0105).abs() < 1e-9);
        // a rate of 0 does not divide by zero
        let _ = target_time(now, Duration::ZERO, 10, 0);
    }

    #[test]
    fn the_sink_stamps_every_message_of_a_block() {
        let (producer, mut consumer) = RingBuffer::new(16);
        let mut sink = MidiSink { producer, latency: Duration::ZERO };
        sink.set_latency(Duration::from_millis(4));
        let now = Instant::now();
        sink.push_block(now, &[MidiMsg::new(0, &[0xc0, 5]), MidiMsg::new(48, &[0xb0, 69, 3])], 48000);
        let a = consumer.pop().unwrap();
        let b = consumer.pop().unwrap();
        assert_eq!((a.data(), b.data()), (&[0xc0, 5][..], &[0xb0, 69, 3][..]));
        assert_eq!(a.target, now + Duration::from_millis(4));
        assert_eq!(b.target, now + Duration::from_millis(5)); // 48 frames = 1 ms
        assert!(consumer.pop().is_err());
        // a full queue drops the rest instead of blocking
        let (producer, _consumer) = RingBuffer::new(1);
        let mut small = MidiSink { producer, latency: Duration::ZERO };
        small.push_block(now, &[MidiMsg::new(0, &[0xfe]), MidiMsg::new(0, &[0xfe])], 48000);
    }

    #[test]
    fn the_pump_sends_in_order_when_due_and_late_messages_at_once() {
        let (mut producer, consumer) = RingBuffer::new(16);
        let mut pump = Pump::new(consumer);
        let t0 = Instant::now();
        let later = t0 + Duration::from_millis(20);
        producer.push(timed(t0 - Duration::from_millis(5), &[0xc0, 1])).unwrap(); // already late
        producer.push(timed(t0, &[0xb0, 69, 2])).unwrap();
        producer.push(timed(later, &[0xb0, 69, 3])).unwrap();
        let mut sent: Vec<Vec<u8>> = Vec::new();
        let next = pump.pump(t0, &mut |b| sent.push(b.to_vec()));
        assert_eq!(sent, vec![vec![0xc0, 1], vec![0xb0, 69, 2]]);
        assert_eq!(next, Some(later));
        pump.pump(later, &mut |b| sent.push(b.to_vec()));
        assert_eq!(sent.len(), 3);
        assert_eq!(sent[2], vec![0xb0, 69, 3]);
        assert_eq!(pump.pump(later, &mut |_| panic!("nothing is left")), None);
    }

    #[test]
    fn finishing_sends_all_notes_off_only_on_channels_with_sounding_notes() {
        let (mut producer, consumer) = RingBuffer::new(16);
        let mut pump = Pump::new(consumer);
        let t0 = Instant::now();
        for m in [
            &[0x90, 60, 100][..],
            &[0x93, 62, 100],
            &[0x95, 64, 100],
            &[0x95, 64, 0],   // velocity 0 is a Note Off, but the note is not tracked
            &[0xb3, 123, 0],  // channel 4 got All Notes Off already
            &[0xb0, 69, 1],
        ] {
            producer.push(timed(t0, m)).unwrap();
        }
        let mut sent = Vec::new();
        pump.pump(t0, &mut |b| sent.push(b.to_vec()));
        // a message that is not due yet is dropped by finish
        producer.push(timed(t0 + Duration::from_secs(60), &[0xb0, 69, 9])).unwrap();
        pump.pump(t0, &mut |b| sent.push(b.to_vec()));
        let mut end = Vec::new();
        pump.finish(&mut |b| end.push(b.to_vec()));
        assert_eq!(end, vec![vec![0xb0, 123, 0], vec![0xb5, 123, 0]]);
        // and it is done: nothing a second time
        let mut again = Vec::new();
        pump.finish(&mut |b| again.push(b.to_vec()));
        assert!(again.is_empty());
        assert!(!pump.abandoned());
        drop(producer);
        assert!(pump.abandoned());
    }

    #[test]
    fn a_link_without_a_port_stays_off() {
        // no MIDI device is needed: nothing is opened for an empty port
        let link = MidiLink::new();
        link.open();
        link.send(&[MidiMsg::new(0, &[0xc0, 1])]);
        let _sink = link.sink();
        link.set_port("");
        std::thread::sleep(Duration::from_millis(30));
        let st = link.status();
        assert!(!st.open);
        assert_eq!(st.error, None);
        drop(link); // ends the thread
    }

    #[test]
    fn a_port_that_does_not_exist_gives_the_message() {
        let link = MidiLink::new();
        link.set_port("No such MIDI port 42");
        link.open();
        let mut st = link.status();
        for _ in 0..1000 {
            if st.error.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
            st = link.status();
        }
        assert!(!st.open);
        assert_eq!(st.error.as_deref(), Some("MIDI port not available: No such MIDI port 42"));
    }
}

//! The MIDI side of the playback engine: the events of a song as frames of
//! the output rate, the list of messages `Engine::render` hands out with
//! each block, and the chasing (the MIDI state at a position).
//!
//! Everything here is plain data and maths without a MIDI port; the port
//! adapter is `midi_out.rs`. Nothing in the render path allocates: the
//! message list and the chase tables are made once, and a song's frames are
//! recomputed in place if the output rate changes.
//!
//! Events are those of the file reader (`crate::read_midi_events`). SysEx
//! messages do not fit the fixed-size `MidiMsg` and are not played (the
//! band's files have none); channel messages are 1-3 bytes.

use crate::MidiEvents;
use std::time::Instant;

/// Messages one block can hold (chasing 16 channels takes about 2000).
pub const MIDI_CAPACITY: usize = 4096;
/// CC 123 "All Notes Off".
pub const ALL_NOTES_OFF: u8 = 123;

/// One MIDI message of a block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MidiMsg {
    /// frame in the block it is due at
    pub offset: usize,
    pub len: u8,
    pub bytes: [u8; 3],
}

impl MidiMsg {
    /// `data` is 1-3 bytes (longer is cut).
    pub fn new(offset: usize, data: &[u8]) -> MidiMsg {
        let len = data.len().min(3);
        let mut bytes = [0u8; 3];
        bytes[..len].copy_from_slice(&data[..len]);
        MidiMsg { offset, len: len as u8, bytes }
    }

    pub fn data(&self) -> &[u8] {
        &self.bytes[..self.len as usize]
    }

    /// Control change `cc` = `value` on `channel` (0-based).
    pub fn cc(offset: usize, channel: u8, cc: u8, value: u8) -> MidiMsg {
        MidiMsg::new(offset, &[0xb0 | (channel & 0x0f), cc, value])
    }
}

/// What the status tells about the MIDI of the current song.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MidiStatus {
    /// the song has a readable MIDI file
    pub has_file: bool,
    /// messages the engine handed out since the song was loaded (chasing
    /// and All Notes Off included)
    pub events_sent: u64,
    /// when the engine handed out the last message
    pub last_event: Option<Instant>,
}

/// The messages of one `render` call. Fixed capacity, never grows.
pub struct MidiList {
    msgs: Vec<MidiMsg>,
}

impl MidiList {
    pub fn new() -> MidiList {
        MidiList { msgs: Vec::with_capacity(MIDI_CAPACITY) }
    }

    pub fn clear(&mut self) {
        self.msgs.clear();
    }

    /// False if the list is full (the message is not added).
    pub fn push(&mut self, msg: MidiMsg) -> bool {
        if self.msgs.len() >= MIDI_CAPACITY {
            return false;
        }
        self.msgs.push(msg);
        true
    }

    pub fn as_slice(&self) -> &[MidiMsg] {
        &self.msgs
    }

    pub fn len(&self) -> usize {
        self.msgs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.msgs.is_empty()
    }

    /// All Notes Off on every channel of `mask` (bit n = channel n).
    pub fn all_notes_off(&mut self, mask: u16, offset: usize) {
        for ch in 0..16u8 {
            if mask & (1 << ch) != 0 {
                self.push(MidiMsg::cc(offset, ch, ALL_NOTES_OFF, 0));
            }
        }
    }
}

impl Default for MidiList {
    fn default() -> MidiList {
        MidiList::new()
    }
}

/// The controller state before a position, per channel.
pub struct ChaseState {
    value: [[u8; 128]; 16],
    /// bit c: controller c has been set
    seen: [u128; 16],
    program: [Option<u8>; 16],
}

impl ChaseState {
    pub fn new() -> ChaseState {
        ChaseState { value: [[0; 128]; 16], seen: [0; 16], program: [None; 16] }
    }

    fn reset(&mut self) {
        self.seen = [0; 16];
        self.program = [None; 16];
    }
}

impl Default for ChaseState {
    fn default() -> ChaseState {
        ChaseState::new()
    }
}

/// One event of a song.
#[derive(Clone, Copy)]
struct Rec {
    time: f64,
    frame: u64,
    len: u8,
    bytes: [u8; 3],
}

/// The events of one song for the engine.
pub struct MidiTrack {
    recs: Vec<Rec>,
    /// the rate `frame` was computed for
    rate: u32,
    /// next event to hand out
    cursor: usize,
    /// the position was changed: the state before it is still to be sent
    chase_due: bool,
    /// channels (bit n) with Note On events in the file
    note_mask: u16,
    /// the song has played since it was loaded (notes may be sounding)
    started: bool,
}

fn frame_of(time: f64, rate: u32) -> u64 {
    if time > 0.0 {
        (time * rate as f64).round() as u64
    } else {
        0
    }
}

impl MidiTrack {
    /// The events of `events` as frames of `rate`: round(seconds x rate).
    pub fn from_events(events: &MidiEvents, rate: u32) -> MidiTrack {
        let recs: Vec<Rec> = events
            .events
            .iter()
            .filter(|e| !e.bytes.is_empty() && e.bytes.len() <= 3 && (0x80..0xf0).contains(&e.bytes[0]))
            .map(|e| {
                let mut bytes = [0u8; 3];
                bytes[..e.bytes.len()].copy_from_slice(&e.bytes);
                Rec { time: e.time, frame: frame_of(e.time, rate), len: e.bytes.len() as u8, bytes }
            })
            .collect();
        let mut note_mask = 0u16;
        for (ch, on) in events.note_on_channels.iter().enumerate() {
            if *on {
                note_mask |= 1 << ch;
            }
        }
        MidiTrack { recs, rate, cursor: 0, chase_due: false, note_mask, started: false }
    }

    /// Number of events that are played.
    pub fn len(&self) -> usize {
        self.recs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.recs.is_empty()
    }

    pub fn rate(&self) -> u32 {
        self.rate
    }

    /// Compute the frames again for `rate`; `position` (in frames of the
    /// new rate) keeps the next event. In place, no allocation.
    pub fn set_rate(&mut self, rate: u32, position: u64) {
        if rate == self.rate {
            return;
        }
        self.rate = rate;
        for r in self.recs.iter_mut() {
            r.frame = frame_of(r.time, rate);
        }
        self.cursor = self.recs.partition_point(|r| r.frame < position);
    }

    /// The position is now `frame`: the next event is the first one at or
    /// after it, and the state before it is sent with the next `chase`.
    pub fn seek(&mut self, frame: u64) {
        self.cursor = self.recs.partition_point(|r| r.frame < frame);
        self.chase_due = true;
    }

    /// Channels that need All Notes Off now: those with notes in the file,
    /// if the song has played.
    pub fn notes_mask(&self) -> u16 {
        if self.started {
            self.note_mask
        } else {
            0
        }
    }

    /// The song starts to sound at `frame`, the first block of its audio
    /// is at `offset`: sends the chasing messages if the position was
    /// changed since the last time.
    pub fn begin(&mut self, frame: u64, state: &mut ChaseState, out: &mut MidiList, offset: usize) {
        self.started = true;
        if self.chase_due {
            self.chase_due = false;
            self.chase(frame, state, out, offset);
        }
    }

    /// The state before `frame` for every channel: CC0, CC32, the last
    /// program change, then the other controllers 1-119 in ascending order.
    /// Notes, pitch bend, aftertouch and channel mode messages are not
    /// part of it. Messages at `offset`.
    pub fn chase(&self, frame: u64, state: &mut ChaseState, out: &mut MidiList, offset: usize) {
        state.reset();
        let end = self.recs.partition_point(|r| r.frame < frame);
        for r in &self.recs[..end] {
            let ch = (r.bytes[0] & 0x0f) as usize;
            match r.bytes[0] & 0xf0 {
                0xb0 if r.len == 3 && r.bytes[1] < 120 => {
                    let cc = r.bytes[1] as usize;
                    state.value[ch][cc] = r.bytes[2];
                    state.seen[ch] |= 1u128 << cc;
                }
                0xc0 if r.len >= 2 => state.program[ch] = Some(r.bytes[1]),
                _ => {}
            }
        }
        for ch in 0..16usize {
            let c = ch as u8;
            for first in [0usize, 32] {
                if state.seen[ch] & (1u128 << first) != 0 {
                    out.push(MidiMsg::cc(offset, c, first as u8, state.value[ch][first]));
                }
            }
            if let Some(p) = state.program[ch] {
                out.push(MidiMsg::new(offset, &[0xc0 | c, p]));
            }
            for cc in 1..120usize {
                if cc != 32 && state.seen[ch] & (1u128 << cc) != 0 {
                    out.push(MidiMsg::cc(offset, c, cc as u8, state.value[ch][cc]));
                }
            }
        }
    }

    /// Hands out the events with a frame in `from..to` (`from` is the
    /// position of the first frame; their offsets count from `offset`, the
    /// block position of `from`). Events that do not fit into `out` stay
    /// for the next call.
    pub fn emit(&mut self, from: u64, to: u64, out: &mut MidiList, offset: usize) {
        while let Some(r) = self.recs.get(self.cursor) {
            if r.frame >= to {
                break;
            }
            let at = offset + r.frame.saturating_sub(from) as usize;
            if !out.push(MidiMsg::new(at, &r.bytes[..r.len as usize])) {
                break;
            }
            self.cursor += 1;
        }
    }
}

/// The messages that give a device the state it would have right after
/// the start of the song: the chasing state just after frame 0 (bank,
/// program change and the controllers that lie on frame 0). All at
/// offset 0.
pub fn start_messages(events: &MidiEvents) -> Vec<MidiMsg> {
    let track = MidiTrack::from_events(events, 48000);
    let mut out = MidiList::new();
    track.chase(1, &mut ChaseState::new(), &mut out, 0);
    out.as_slice().to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{parse_midi_events, tests::{smf, trk, EOT}};

    /// Track body from (absolute tick, event) pairs.
    fn abs(events: &[(u32, &[u8])]) -> Vec<u8> {
        let mut last = 0;
        let mut list: Vec<(u32, &[u8])> = Vec::new();
        for (t, e) in events {
            list.push((t - last, e));
            last = *t;
        }
        list.push((0, EOT));
        trk(&list)
    }

    #[test]
    fn start_messages_are_the_state_on_frame_0() {
        let t = trk(&[
            (0, &[0xb0, 0x20, 0x02]),
            (0, &[0xc0, 0x07]),
            (0, &[0xb0, 0x45, 0x01]),
            (480, &[0xb0, 0x45, 0x03]), // later: not part of it
            (0, EOT),
        ]);
        let ev = parse_midi_events(&smf(0, 480, &[t])).unwrap();
        let msgs = start_messages(&ev);
        let data: Vec<Vec<u8>> = msgs.iter().map(|m| m.data().to_vec()).collect();
        assert_eq!(data, vec![vec![0xb0, 32, 2], vec![0xc0, 7], vec![0xb0, 69, 1]]);
        assert!(msgs.iter().all(|m| m.offset == 0));
    }

    #[test]
    fn chasing_orders_bank_program_and_controllers_per_channel_and_skips_the_rest() {
        let t = abs(&[
            (0, &[0xb1, 74, 5]), // channel 2 first in the file, but after channel 1 in the output
            (0, &[0xb0, 7, 100]),
            (1, &[0xb0, 74, 9]),
            (2, &[0xb0, 32, 4]),
            (3, &[0xc0, 3]),
            (4, &[0xc0, 8]), // the last program change counts
            (5, &[0xb0, 0, 2]),
            (6, &[0xb0, 1, 50]),
            (7, &[0xb0, 7, 90]), // the last value counts
            (8, &[0xb0, 120, 0]), // channel mode: not chased
            (9, &[0xb0, 123, 0]),
            (10, &[0xb0, 127, 1]),
            (11, &[0xe0, 1, 2]), // pitch bend
            (12, &[0xd0, 33]),   // channel pressure
            (13, &[0xa0, 60, 3]), // key pressure
            (14, &[0x90, 60, 100]),
            (100, &[0xb0, 7, 1]), // later: not part of it
        ]);
        let ev = parse_midi_events(&smf(0, 480, &[t])).unwrap();
        let track = MidiTrack::from_events(&ev, 48000);
        let mut out = MidiList::new();
        // tick 14 is at frame 14 x 50; everything up to the note is before this frame
        track.chase(15 * 50, &mut ChaseState::new(), &mut out, 7);
        let got: Vec<Vec<u8>> = out.as_slice().iter().map(|m| m.data().to_vec()).collect();
        assert_eq!(
            got,
            vec![
                vec![0xb0, 0, 2],
                vec![0xb0, 32, 4],
                vec![0xc0, 8],
                vec![0xb0, 1, 50],
                vec![0xb0, 7, 90],
                vec![0xb0, 74, 9],
                vec![0xb1, 74, 5],
            ]
        );
        assert!(out.as_slice().iter().all(|m| m.offset == 7));
        // before anything happened: nothing
        let mut none = MidiList::new();
        track.chase(0, &mut ChaseState::new(), &mut none, 0);
        assert!(none.is_empty());
        // the state is rebuilt every time (no leftovers of the call before)
        let mut again = MidiList::new();
        let mut state = ChaseState::new();
        track.chase(15 * 50, &mut state, &mut again, 0);
        let mut early = MidiList::new();
        track.chase(50, &mut state, &mut early, 0);
        assert_eq!(early.len(), 2, "only what is before frame 50: channel 1 CC7 and channel 2 CC74");
    }

    #[test]
    fn a_full_list_keeps_the_rest_for_the_next_call() {
        let mut list = MidiList::new();
        for _ in 0..MIDI_CAPACITY {
            assert!(list.push(MidiMsg::new(0, &[0xfe])));
        }
        assert!(!list.push(MidiMsg::new(0, &[0xfe])));
        assert_eq!(list.len(), MIDI_CAPACITY);
    }
}

//! The "Rehearsal" tab: the playlist and song list on the left, the song in
//! the rehearsal player in the middle, the rehearsal mix below. All the
//! logic (mixing, transport, the queue, reading) is in the library
//! (`lp16_tool::rehearsal`, `lp16_tool::rehearsal_queue`,
//! `lp16_tool::audio_out`); this only draws and sends commands.

use crate::gui_app::{DIM, LINE, ORANGE, PANEL, RED, TEXT};
use crate::gui_mixer::MixerPanel;
use crate::gui_playlist;
use eframe::egui::{self, vec2, Align, Color32, Key, Layout, Modifiers, RichText, Sense, Stroke, Ui};
use lp16_tool::audio_out::{AudioSettings, Command, Player, StreamInfo};
use lp16_tool::midi_out::{MidiLink, PortStatus};
use lp16_tool::mix::{MixLibrary, MixParams, STRIPS};
use lp16_tool::read_midi_events;
use lp16_tool::rehearsal::{fmt_clock, Engine, Levels, PlayState, Status};
use lp16_tool::rehearsal_midi::start_messages;
use lp16_tool::rehearsal_queue::{
    entries_from_playlist, find_path, next_caption, same_songs, song_rows, QueueEntry, SongRow,
};
use lp16_tool::{PlaylistSong, SongProject};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Seconds a key press or button jumps by (Shift: the long jump).
const STEP_SECONDS: f64 = 10.0;
const LONG_STEP_SECONDS: f64 = 30.0;
/// Quick jumps add up to the last target for this long, because the status
/// only shows it after the next audio block.
const SEEK_MEMORY: Duration = Duration::from_millis(400);
/// Width of the left part (playlist and song list).
const LIST_WIDTH: f32 = 290.0;
/// The song list scrolls beyond this height.
const LIST_MAX_HEIGHT: f32 = 330.0;
/// Height reserved above the song list for the autoplay checkbox.
const LIST_HEADER: f32 = 28.0;
/// How long the MIDI indicator stays lit after a message.
const MIDI_FLASH: Duration = Duration::from_millis(150);

/// Shown after the loaded song was renamed, deleted or saved.
pub const CHANGED_NOTICE: &str = "The song was changed in the library \u{2013} load it again.";

#[derive(Default)]
pub struct RehearsalPanel {
    /// the audio stream, opened with the first Play and opened again when
    /// the rate of the song or the audio settings change
    player: Option<Player>,
    /// the audio output chosen in the settings
    settings: AudioSettings,
    /// why the audio output could not be opened, or was lost
    device_error: Option<String>,
    /// the queue: the songs of the loaded playlist, or one single song
    entries: Arc<Vec<QueueEntry>>,
    /// name of the loaded playlist; None for a single song
    playlist: Option<String>,
    /// the playlist as the queue was built from it, to see a change
    loaded_songs: Vec<PlaylistSong>,
    /// {$AudioLib} of the library, and its playlists (kept up to date by
    /// `set_library`)
    audio_lib: String,
    library: Vec<(String, Vec<PlaylistSong>)>,
    /// status of the loaded song while no stream is open (lost device,
    /// test tone, nothing played yet); the position is kept
    parked: Option<Status>,
    notice: Option<String>,
    /// target time while the progress bar is being dragged
    drag_target: Option<f64>,
    /// the last target sent, and when
    last_seek: Option<(f64, Instant)>,
    /// the rehearsal mix (below the player)
    mixer: MixerPanel,
    /// the mix the engine should use; sent again to every new stream
    mix: MixParams,
    /// per LP-16 output the names of the tracks of the loaded song, and the
    /// queue index they are for
    track_names: [String; STRIPS],
    names_for: Option<usize>,
    /// the MIDI output: the thread and port of the player
    midi: MidiLink,
    /// the port chosen in the settings (empty = MIDI is off)
    midi_port: String,
    /// "Autoplay after 5 seconds"; sent again to every new stream
    autoplay: bool,
    /// the checkbox was changed (see `take_autoplay_change`)
    autoplay_changed: bool,
}

/// The height the song list and the player get: from `top` down to the
/// bottom of the window, but at least `LIST_MAX_HEIGHT`.
pub(crate) fn list_room(window_bottom: f32, top: f32) -> f32 {
    (window_bottom - top).max(LIST_MAX_HEIGHT)
}

/// The track names per strip: tracks on the same output are joined.
fn track_names(project: &SongProject) -> [String; STRIPS] {
    let mut names: [String; STRIPS] = Default::default();
    for t in &project.tracks {
        if t.channel < 0 || t.channel as usize >= STRIPS {
            continue;
        }
        let n = &mut names[t.channel as usize];
        if !n.is_empty() {
            n.push_str(" + ");
        }
        n.push_str(if t.name.is_empty() { "Track" } else { &t.name });
    }
    names
}

impl RehearsalPanel {
    /// The audio output of the settings; used the next time a stream is
    /// opened. A running stream is not touched.
    pub fn set_settings(&mut self, settings: AudioSettings) {
        self.settings = settings;
    }

    /// The MIDI output port of the settings (empty = off). It is opened
    /// by the next Play (or the MIDI test).
    pub fn set_midi_port(&mut self, port: &str) {
        self.midi_port = port.to_string();
        self.midi.set_port(port);
    }

    /// State of the MIDI port (for the settings).
    pub fn midi_status(&self) -> PortStatus {
        self.midi.status()
    }

    /// Whether the loaded song has a MIDI file (the MIDI test needs one).
    pub fn has_midi_file(&self) -> bool {
        self.current_status().map(|st| st.state != PlayState::Empty && st.midi.has_file).unwrap_or(false)
    }

    /// MIDI test: send the state the loaded song would give a device right
    /// after its start (bank, program change, the controllers on frame 0)
    /// to the port. False if there is no song with a MIDI file.
    pub fn send_midi_test(&mut self) -> bool {
        let events = self
            .current_status()
            .and_then(|st| st.index)
            .and_then(|i| self.entries.get(i))
            .and_then(|e| e.project.as_ref().ok())
            .and_then(|p| p.midi_path())
            .and_then(|path| read_midi_events(&path));
        let Some(events) = events else { return false };
        self.midi.send(&start_messages(&events));
        true
    }

    /// Autoplay on/off (from the config at startup, and by the checkbox).
    /// The engine gets it now and with every stream opened later.
    pub fn set_autoplay(&mut self, on: bool) {
        self.autoplay = on;
        if let Some(player) = &self.player {
            player.send(Command::SetAutoplay(on));
        }
    }

    /// The new value if the user changed the autoplay checkbox since the
    /// last call, so the owner saves the config.
    pub fn take_autoplay_change(&mut self) -> Option<bool> {
        std::mem::take(&mut self.autoplay_changed).then_some(self.autoplay)
    }

    /// The checkbox "Autoplay after 5 seconds"; the same one in every tab.
    pub fn autoplay_option(&mut self, ui: &mut Ui) {
        let mut on = self.autoplay;
        let resp = ui.checkbox(&mut on, "Autoplay after 5 seconds").on_hover_text(
            "On: after a song ends, the next one starts by itself after 5 seconds.\n\
             Off: the player waits for Play.",
        );
        if resp.changed() {
            self.set_autoplay(on);
            self.autoplay_changed = true;
        }
    }

    /// The rehearsal mix for the engine (kept for streams opened later).
    pub fn set_mix(&mut self, mix: MixParams) {
        self.mix = mix;
        if let Some(player) = &self.player {
            player.send(Command::SetMix(mix));
        }
    }

    /// What the open stream runs with (for the settings).
    pub fn stream_info(&self) -> Option<&StreamInfo> {
        self.player.as_ref().map(|p| p.info())
    }

    /// The last error of the audio output, if any.
    pub fn error(&self) -> Option<&str> {
        self.device_error.as_deref()
    }

    // -- the library -----------------------------------------------------------

    /// The playlists of the project library (called whenever the library is
    /// read again). If the loaded playlist was saved with other songs, the
    /// queue is rebuilt: the song that plays goes on, if it is still in it.
    pub fn set_library(&mut self, audio_lib: &str, playlists: Vec<(String, Vec<PlaylistSong>)>) {
        self.audio_lib = audio_lib.to_string();
        self.library = playlists;
        let Some(name) = self.playlist.clone() else { return };
        let changed = self
            .library
            .iter()
            .find(|(n, _)| *n == name)
            .filter(|(_, songs)| !same_songs(songs, &self.loaded_songs))
            .map(|(_, songs)| songs.clone());
        if let Some(songs) = changed {
            self.rebuild(songs);
        }
    }

    /// The loaded playlist was renamed: the combo follows.
    pub fn playlist_renamed(&mut self, old: &str, new: &str) {
        if self.playlist.as_deref() == Some(old) {
            self.playlist = Some(new.to_string());
        }
    }

    /// The loaded playlist was deleted: stop, unload, tell the user.
    pub fn playlist_deleted(&mut self, name: &str) {
        if self.playlist.as_deref() != Some(name) {
            return;
        }
        self.clear(format!("The playlist \"{}\" was deleted \u{2013} the player stopped.", name));
    }

    /// The song file `path` is about to be renamed, deleted or saved: stop
    /// and unload the queue if the song is in it.
    pub fn song_changed(&mut self, path: &str) {
        if find_path(&self.entries, path, None).is_some() {
            self.clear(CHANGED_NOTICE.to_string());
        }
    }

    /// Stop, unload everything and show `notice`.
    fn clear(&mut self, notice: String) {
        if let Some(player) = &self.player {
            player.send(Command::Stop);
            player.send(Command::SetQueue(Arc::new(Vec::new())));
        }
        self.entries = Arc::new(Vec::new());
        self.playlist = None;
        self.loaded_songs.clear();
        self.parked = None;
        self.names_for = None;
        self.track_names = Default::default();
        self.notice = Some(notice);
    }

    /// The playlist `songs` was saved: build the queue again.
    fn rebuild(&mut self, songs: Vec<PlaylistSong>) {
        let current = self.current_status();
        let current_path = current
            .as_ref()
            .and_then(|st| st.index)
            .and_then(|i| self.entries.get(i))
            .and_then(|e| e.path())
            .map(str::to_string);
        let new = Arc::new(entries_from_playlist(&songs, &self.audio_lib));
        let loaded = current.as_ref().map(|st| st.state != PlayState::Empty).unwrap_or(false);
        let at = current_path.as_deref().and_then(|p| find_path(&new, p, current.as_ref().and_then(|s| s.index)));
        self.loaded_songs = songs;
        self.entries = new.clone();
        self.names_for = None;
        if let Some(player) = &self.player {
            player.send(Command::ReplaceQueue(new));
        } else if loaded {
            match (at, self.parked.as_mut()) {
                (Some(i), Some(st)) => st.index = Some(i),
                _ => self.parked = None,
            }
            if at.is_none() {
                if let Some(first) = self.first_readable(0) {
                    self.parked = Some(self.offline_status(first, 0.0));
                }
            }
        }
        if loaded && at.is_none() {
            self.notice = Some(
                "The playlist was changed and the song that was loaded is no longer in it \u{2013} the player stopped."
                    .to_string(),
            );
        }
    }

    /// Load playlist `name` of the library, stopped on its first song. It
    /// does not start by itself: live, the first song waits for the button,
    /// too.
    pub fn play_playlist(&mut self, name: &str) {
        let Some(songs) = self.library.iter().find(|(n, _)| n == name).map(|(_, s)| s.clone()) else { return };
        let entries = entries_from_playlist(&songs, &self.audio_lib);
        self.load_queue(entries, Some(name.to_string()), songs, false);
    }

    /// Load `project` as a queue of one song and leave it stopped.
    pub fn load_song(&mut self, project: &SongProject) {
        self.load_queue(vec![QueueEntry::single(project.clone())], None, Vec::new(), false);
    }

    /// Start the first readable song of the queue (nothing is loaded yet).
    pub fn play_first(&mut self) {
        if let Some(first) = self.first_readable(0) {
            self.start(first, true, true);
        }
    }

    /// Load `project` as a queue of one song and start it. If the audio
    /// output cannot be opened the tab shows why, and Play tries again.
    pub fn play_song(&mut self, project: &SongProject) {
        self.load_queue(vec![QueueEntry::single(project.clone())], None, Vec::new(), true);
    }

    // -- the stream ------------------------------------------------------------

    /// Make sure a stream with `rate` and the current settings is open.
    /// The old one is closed first (it may hold the device).
    fn open_stream(&mut self, rate: u32) -> bool {
        if self.player.as_ref().map(|p| p.matches(&self.settings, rate)).unwrap_or(false) {
            return true;
        }
        self.player = None;
        self.midi.open();
        match Player::open(&self.settings, rate, Some(&self.midi)) {
            Ok(p) => {
                p.send(Command::SetMix(self.mix));
                p.send(Command::SetAutoplay(self.autoplay));
                self.player = Some(p);
                self.device_error = None;
                true
            }
            Err(e) => {
                self.device_error = Some(e);
                false
            }
        }
    }

    fn stream_fits(&self, rate: u32) -> bool {
        self.player.as_ref().map(|p| p.matches(&self.settings, rate)).unwrap_or(false)
    }

    /// The sample rate to open the stream with for queue entry `index`.
    fn rate_of(&self, index: usize) -> u32 {
        self.entries
            .get(index)
            .and_then(|e| e.project.as_ref().ok())
            .map(|p| p.sampling_rate)
            .filter(|r| *r > 0)
            .unwrap_or(48000)
    }

    fn first_readable(&self, from: usize) -> Option<usize> {
        (from..self.entries.len()).find(|&i| self.entries[i].readable())
    }

    /// The queue goes to a stream that was just opened.
    fn send_queue(&self) {
        if let Some(player) = &self.player {
            player.send(Command::SetQueue(self.entries.clone()));
        }
    }

    /// A status for the song `index` without any output device: a
    /// throw-away engine opens it. `position` is kept.
    fn offline_status(&self, index: usize, position: f64) -> Status {
        let mut engine = Engine::new(self.rate_of(index));
        drop(engine.set_queue_arc(self.entries.clone()));
        engine.goto(index, false);
        engine.settle();
        if position > 0.0 {
            engine.seek(position);
            engine.settle();
        }
        engine.status()
    }

    /// Change what the parked song (no stream) does, with a throw-away engine.
    fn offline(&mut self, f: impl FnOnce(&mut Engine)) {
        let Some(parked) = self.parked.clone() else { return };
        let index = parked.index.unwrap_or(0);
        let mut engine = Engine::new(self.rate_of(index));
        drop(engine.set_queue_arc(self.entries.clone()));
        engine.goto(index, false);
        engine.settle();
        if parked.position_seconds > 0.0 {
            engine.seek(parked.position_seconds);
        }
        f(&mut engine);
        engine.settle();
        self.parked = Some(engine.status());
    }

    /// Use `entries` as the queue and load its first readable song:
    /// playing (`autoplay`) or stopped.
    fn load_queue(&mut self, entries: Vec<QueueEntry>, playlist: Option<String>, songs: Vec<PlaylistSong>, autoplay: bool) {
        self.entries = Arc::new(entries);
        self.playlist = playlist;
        self.loaded_songs = songs;
        self.notice = None;
        self.drag_target = None;
        self.last_seek = None;
        self.parked = None;
        self.names_for = None;
        let Some(first) = self.first_readable(0) else {
            // nothing to play: the status carries the warnings
            if self.player.is_some() {
                self.send_queue();
            } else {
                self.parked = Some(self.offline_status(0, 0.0));
            }
            return;
        };
        if autoplay {
            self.start(first, true, true);
        } else if self.stream_fits(self.rate_of(first)) {
            self.send_queue();
            if let Some(player) = &self.player {
                player.send(Command::Goto(first, false));
            }
        } else {
            // no need to hold the audio device before Play
            self.player = None;
            self.parked = Some(self.offline_status(first, 0.0));
        }
    }

    /// Start song `index` (or the next readable one) at its beginning on a
    /// stream of its rate, opening one if needed. `new_queue`: the engine
    /// has to be given the queue even if the stream is kept.
    fn start(&mut self, index: usize, autoplay: bool, new_queue: bool) {
        let Some(i) = self.first_readable(index) else { return };
        if autoplay {
            self.midi.open(); // tries the port again if it failed before
        }
        let rate = self.rate_of(i);
        if !self.stream_fits(rate) {
            if !self.open_stream(rate) {
                self.parked = Some(self.offline_status(i, 0.0));
                return;
            }
            self.send_queue();
        } else if new_queue {
            self.send_queue();
        }
        self.parked = None;
        self.last_seek = None;
        if let Some(player) = &self.player {
            player.send(Command::Goto(i, autoplay));
        }
    }

    /// Close the stream but keep the song and its position, so Play opens
    /// the output again (lost device, test tone).
    pub fn park(&mut self) {
        let Some(player) = self.player.take() else { return };
        let mut st = player.status();
        drop(player);
        if !self.entries.is_empty() && st.state != PlayState::Empty {
            if st.state == PlayState::Playing {
                st.state = PlayState::Paused;
            }
            self.parked = Some(st);
        }
    }

    /// Open the stream again for the parked song at its position.
    fn resume(&mut self, play: bool) {
        let Some(parked) = self.parked.clone() else { return };
        let index = parked.index.unwrap_or(0);
        if !self.open_stream(self.rate_of(index)) {
            return;
        }
        self.send_queue();
        if let Some(player) = &self.player {
            player.send(Command::Goto(index, false));
            if parked.position_seconds > 0.0 {
                player.send(Command::Seek(parked.position_seconds));
            }
            match parked.state {
                PlayState::Waiting => player.send(Command::Hold(None)),
                PlayState::Countdown => player.send(Command::Hold(Some(parked.countdown_seconds))),
                _ => {}
            }
            if play {
                player.send(Command::Play);
            }
        }
        self.parked = None;
    }

    /// The stream failed (device unplugged): keep the message and the
    /// position and close the stream.
    fn check_stream(&mut self) {
        let Some(e) = self.player.as_ref().and_then(|p| p.error()) else { return };
        self.park();
        self.device_error = Some(e);
    }

    /// The song that is loaded has another sample rate than the stream
    /// (the next song of the playlist): open the stream again with the new
    /// rate, carry the queue over and go on in the same state.
    fn check_rate(&mut self) {
        let Some(player) = &self.player else { return };
        let st = player.status();
        let (Some(rate), Some(index)) = (st.needs_rate, st.index) else { return };
        if player.output_rate() == rate {
            return;
        }
        self.player = None;
        if !self.open_stream(rate) {
            self.parked = Some(self.offline_status(index, 0.0));
            return;
        }
        self.send_queue();
        if let Some(player) = &self.player {
            player.send(Command::Goto(index, st.state == PlayState::Playing));
            match st.state {
                PlayState::Waiting => player.send(Command::Hold(None)),
                PlayState::Countdown => player.send(Command::Hold(Some(st.countdown_seconds))),
                _ => {}
            }
        }
    }

    /// The status of the player, or of the parked song.
    fn current_status(&self) -> Option<Status> {
        self.player.as_ref().map(|p| p.status()).or_else(|| self.parked.clone())
    }

    pub fn is_playing(&self) -> bool {
        self.player.as_ref().map(|p| p.status().state == PlayState::Playing).unwrap_or(false)
    }

    /// End the audio stream and the reader thread (closing the program).
    pub fn shutdown(&mut self) {
        self.player = None;
        self.midi.shutdown(); // All Notes Off for sounding notes, closes the port
    }

    // -- transport -------------------------------------------------------------

    /// Jump to `seconds` (clamped to the song) in the player.
    pub fn seek(&mut self, seconds: f64, length: f64) {
        let target = seconds.clamp(0.0, length.max(0.0));
        if let Some(player) = &self.player {
            player.send(Command::Seek(target));
        } else if let Some(st) = self.parked.as_mut() {
            st.position_seconds = target;
            st.position_frames = (target * st.song_rate as f64).round() as u64;
        }
        self.last_seek = Some((target, Instant::now()));
    }

    /// Jump by `delta` seconds from the position the user last saw or chose.
    pub fn seek_by(&mut self, st: &Status, delta: f64) {
        let base = match self.last_seek {
            Some((t, when)) if when.elapsed() < SEEK_MEMORY => t,
            _ => st.position_seconds,
        };
        self.seek(base + delta, st.length_seconds);
    }

    /// Play or pause, as the big button does.
    pub fn toggle_play(&mut self, st: &Status) {
        if st.state == PlayState::Empty {
            // a playlist is selected but nothing is loaded
            if let Some(first) = self.first_readable(0) {
                self.start(first, true, true);
            }
            return;
        }
        if st.state == PlayState::Playing {
            if let Some(player) = &self.player {
                player.send(Command::Pause);
            }
            return;
        }
        self.midi.open();
        // no stream (lost, test tone) or one that does not fit the settings
        // any more: open it again at the same position
        let stale = self.player.as_ref().map(|p| !p.matches(&self.settings, st.song_rate)).unwrap_or(true);
        if stale {
            self.park();
            self.resume(true);
        } else if let Some(player) = &self.player {
            if st.song_rate == st.output_rate {
                player.send(Command::Play);
            }
        }
    }

    /// Stop: back to the start.
    pub fn stop(&mut self) {
        if let Some(player) = &self.player {
            player.send(Command::Stop);
        } else if let Some(st) = self.parked.as_mut() {
            st.position_seconds = 0.0;
            st.position_frames = 0;
            if matches!(st.state, PlayState::Waiting | PlayState::Countdown) {
                st.state = PlayState::Stopped;
            }
        }
        self.last_seek = None;
    }

    /// A click on song `index` of the list (or a choice in the song combo):
    /// the song is marked. While a song plays it goes on and `index`
    /// becomes the next song; otherwise `index` is loaded (stopped, position
    /// 0) so Play starts it.
    pub fn click_song(&mut self, index: usize) {
        if let Some(player) = &self.player {
            player.send(Command::Select(index));
        } else {
            self.offline(|e| e.select(index));
        }
        self.last_seek = None;
    }

    /// The mark moves to the next or previous song and is clicked, as the
    /// Down and Up keys do.
    pub fn move_mark(&mut self, forward: bool) {
        if let Some(player) = &self.player {
            player.send(Command::MoveMark(forward));
        } else {
            self.offline(|e| e.move_mark(forward));
        }
        self.last_seek = None;
    }

    /// The next song, as the button does.
    pub fn next_song(&mut self) {
        if let Some(player) = &self.player {
            player.send(Command::Next);
        } else {
            self.offline(|e| e.next());
        }
        self.last_seek = None;
    }

    /// The previous song (or the start of this one), as the button does.
    pub fn prev_song(&mut self) {
        if let Some(player) = &self.player {
            player.send(Command::Prev);
        } else {
            self.offline(|e| e.prev());
        }
        self.last_seek = None;
    }

    /// Keyboard shortcuts; only called while the tab is shown, and nothing
    /// happens while a text field has the focus.
    pub fn keys(&mut self, ui: &mut Ui, st: &Status) {
        if ui.ctx().wants_keyboard_input() || st.state == PlayState::Empty {
            return;
        }
        let pressed = |ui: &Ui, m: Modifiers, k: Key| ui.input_mut(|i| i.consume_key(m, k));
        // Shift first: a plain arrow must not take the Shift arrow
        if pressed(ui, Modifiers::SHIFT, Key::ArrowLeft) {
            self.seek_by(st, -LONG_STEP_SECONDS);
        } else if pressed(ui, Modifiers::SHIFT, Key::ArrowRight) {
            self.seek_by(st, LONG_STEP_SECONDS);
        } else if pressed(ui, Modifiers::NONE, Key::ArrowLeft) {
            self.seek_by(st, -STEP_SECONDS);
        } else if pressed(ui, Modifiers::NONE, Key::ArrowRight) {
            self.seek_by(st, STEP_SECONDS);
        }
        if pressed(ui, Modifiers::NONE, Key::ArrowUp) {
            self.move_mark(false);
        } else if pressed(ui, Modifiers::NONE, Key::ArrowDown) {
            self.move_mark(true);
        }
        if pressed(ui, Modifiers::NONE, Key::Space) {
            self.toggle_play(st);
        }
        if pressed(ui, Modifiers::NONE, Key::Home) {
            self.seek(0.0, st.length_seconds); // the state stays
        }
        if pressed(ui, Modifiers::NONE, Key::Escape) {
            self.stop();
        }
    }

    // -- drawing ---------------------------------------------------------------

    /// Draw the tab: the playlist and the player, below them the rehearsal
    /// mix `mixes`. Returns true if the mix (values or presets) was changed,
    /// so the caller saves the config.
    pub fn ui(&mut self, ui: &mut Ui, mixes: &mut MixLibrary) -> bool {
        let status = self.begin_frame(ui.ctx());
        let levels = self.take_levels();
        let mut changed = false;
        egui::ScrollArea::vertical().id_source("rehearsal_scroll").auto_shrink([false, false]).show(ui, |ui| {
            // the compact mixer on top
            let out = self.mixer.ui(ui, mixes, &self.track_names, &levels);
            if out.changed {
                changed = true;
                self.set_mix(mixes.active().params());
            }
            if out.reset_clip {
                self.reset_clip();
            }
            ui.add_space(10.0);
            ui.separator();
            ui.add_space(6.0);
            // below it the song list and the player; the list scrolls by
            // itself, so the player stays in view
            ui.horizontal_top(|ui| {
                let total = ui.available_width();
                let left = LIST_WIDTH.min(total * 0.4);
                // `horizontal_top` only offers one line of height, and a child
                // gets exactly the height it is allocated: so give the list and
                // the player the room down to the bottom of the window (at
                // least `LIST_MAX_HEIGHT`). The list takes what it needs and
                // scrolls beyond that.
                let room = list_room(ui.ctx().screen_rect().bottom(), ui.cursor().min.y);
                ui.allocate_ui_with_layout(vec2(left, room), Layout::top_down(Align::LEFT), |ui| {
                    ui.set_width(left);
                    self.autoplay_option(ui);
                    ui.add_space(4.0);
                    egui::ScrollArea::vertical()
                        .id_source("rehearsal_list_scroll")
                        .max_height((room - LIST_HEADER).max(LIST_MAX_HEIGHT - LIST_HEADER))
                        .auto_shrink([false, true])
                        .show(ui, |ui| self.list_area(ui, status.as_ref()));
                    self.list_warnings(ui, status.as_ref());
                });
                ui.separator();
                let right = ui.available_width().max(300.0);
                ui.allocate_ui_with_layout(vec2(right, room), Layout::top_down(Align::Center), |ui| {
                    ui.set_width(right);
                    self.player_area(ui, status.as_ref());
                });
            });
        });
        changed
    }

    /// The work every tab that shows the player does first in each frame:
    /// look after the stream (lost device, other sample rate), keep the
    /// track names of the strips up to date and ask for repaints while
    /// something moves. Returns the status to draw.
    pub fn begin_frame(&mut self, ctx: &egui::Context) -> Option<Status> {
        self.check_stream();
        self.check_rate();
        let status = self.current_status();
        // the names on the strips follow the song
        let index = status.as_ref().and_then(|s| s.index);
        if index != self.names_for {
            self.names_for = index;
            self.track_names = index
                .and_then(|i| self.entries.get(i))
                .and_then(|e| e.project.as_ref().ok())
                .map(track_names)
                .unwrap_or_default();
        }
        if let Some(st) = &status {
            if st.state == PlayState::Countdown {
                ctx.request_repaint_after(Duration::from_millis(100));
            } else if st.state != PlayState::Empty && st.length_frames == 0 {
                // the song is still being opened
                ctx.request_repaint_after(Duration::from_millis(30));
            }
        }
        status
    }

    /// The meter peaks since the last call (only the tab that is shown
    /// calls this, once per frame).
    pub fn take_levels(&self) -> Levels {
        self.player.as_ref().map(|p| p.take_levels()).unwrap_or_default()
    }

    /// The names of the tracks of the loaded song per LP-16 output (empty =
    /// no track).
    pub fn strip_names(&self) -> &[String; STRIPS] {
        &self.track_names
    }

    /// The clip LED of the master was clicked.
    pub fn reset_clip(&mut self) {
        if let Some(player) = &self.player {
            player.send(Command::ResetClip);
        }
    }

    /// The rows of the song list for `status`.
    pub fn rows(&self, status: Option<&Status>) -> Vec<SongRow> {
        let playing = status.map(|s| s.state == PlayState::Playing).unwrap_or(false);
        let loaded = status.map(|s| s.state != PlayState::Empty).unwrap_or(false);
        let (index, next, marked) = match status {
            Some(s) if loaded => (s.index, s.next_index, s.marked_index),
            _ => (None, None, None),
        };
        song_rows(&self.entries, index, next, marked, playing)
    }

    fn playlist_names(&self) -> Vec<String> {
        self.library.iter().map(|(n, _)| n.clone()).collect()
    }

    /// A single song (not a playlist) is loaded.
    fn is_single(&self) -> bool {
        self.playlist.is_none() && !self.entries.is_empty()
    }

    /// The playlist combo; choosing a playlist loads it (stopped).
    pub fn playlist_picker(&mut self, ui: &mut Ui, id: &str, width: f32) {
        let names = self.playlist_names();
        let single = self.is_single();
        if let Some(name) = gui_playlist::playlist_combo(ui, id, width, &names, self.playlist.as_deref(), single) {
            self.play_playlist(&name);
        }
    }

    /// The song combo of the loaded playlist; choosing a song is a single
    /// click in the list (`click_song`), Play starts it.
    pub fn song_picker(&mut self, ui: &mut Ui, id: &str, width: f32, status: Option<&Status>) {
        let rows = self.rows(status);
        if let Some(i) = gui_playlist::song_combo(ui, id, width, &rows) {
            self.click_song(i);
        }
    }

    /// Playlist combo and song list.
    fn list_area(&mut self, ui: &mut Ui, status: Option<&Status>) {
        let rows = self.rows(status);
        let names = self.playlist_names();
        let single = self.is_single();
        let out = gui_playlist::ui(ui, &names, self.playlist.as_deref(), single, &rows);
        if let Some(name) = out.select {
            self.play_playlist(&name);
        }
        if let Some(i) = out.click {
            self.click_song(i);
        }
        if let Some(i) = out.play {
            self.start(i, true, false);
        }
    }

    /// The skipped songs below the list.
    fn list_warnings(&self, ui: &mut Ui, status: Option<&Status>) {
        if let Some(st) = status {
            for w in st.queue_warnings.iter() {
                ui.add_space(4.0);
                ui.add(egui::Label::new(RichText::new(format!("Warning: {}", w)).size(12.0).color(ORANGE)).wrap(true));
            }
        }
    }

    /// "Next: <song> ..." while the player waits, else the song that comes
    /// next in the playlist.
    pub fn next_line(&self, st: &Status) -> Option<String> {
        if let Some(c) = next_caption(st.state, &st.song_name, st.countdown_seconds) {
            return Some(c);
        }
        let entry = st.next_index.and_then(|i| self.entries.get(i))?;
        Some(format!("Next: {}", entry.display_name()))
    }

    /// Problems of the player in a short form: (text, is an error). The
    /// notice, the audio error, dropouts and the warnings.
    pub fn problems(&self, status: Option<&Status>) -> Vec<(String, bool)> {
        let mut v = Vec::new();
        if let Some(n) = &self.notice {
            v.push((n.clone(), false));
        }
        if let Some(e) = &self.device_error {
            v.push((e.clone(), true));
        }
        if let Some(st) = status {
            if st.underruns > 0 {
                v.push((format!("Dropouts: {}", st.underruns), true));
            }
            for w in st.queue_warnings.iter().chain(st.warnings.iter()) {
                v.push((format!("Warning: {}", w), false));
            }
        }
        v
    }

    /// "pos / length" for the clock; while the progress bar is dragged it
    /// shows the target (second value true).
    pub fn time_text(&self, st: &Status) -> (String, bool) {
        let shown = self.drag_target.unwrap_or(st.position_seconds);
        (format!("{} / {}", fmt_clock(shown), fmt_clock(st.length_seconds)), self.drag_target.is_some())
    }

    /// The progress bar: click or drag to jump.
    pub fn progress(&mut self, ui: &mut Ui, st: &Status, width: f32) {
        if let Some(target) = progress_bar(ui, st, &mut self.drag_target, width) {
            self.seek(target, st.length_seconds);
        }
    }

    /// The transport buttons: previous song, -10 s, Play/Pause, Stop,
    /// +10 s, next song. Without a loaded song they are disabled, only
    /// Play starts the queue.
    pub fn transport(&mut self, ui: &mut Ui, status: Option<&Status>, style: TransportStyle) {
        let st = status.filter(|s| s.state != PlayState::Empty);
        let (mut prev, mut next) = (false, false);
        let (mut back, mut forward, mut toggle, mut stop) = (false, false, false, false);
        let can_start = st.is_none() && self.first_readable(0).is_some();
        ui.horizontal(|ui| {
            let (w, small) = (style.wide, style.small);
            if style.center {
                ui.add_space(((ui.available_width() - 4.0 * w - 2.0 * small - 5.0 * ui.spacing().item_spacing.x) / 2.0).max(0.0));
            }
            let btn = |ui: &mut Ui, text: &str, enabled: bool, width: f32| {
                sized_button(ui, text, enabled, width, style.font, style.height)
            };
            // a song of another sample rate cannot be played
            let can_play = st.map(|s| s.song_rate == s.output_rate).unwrap_or(can_start);
            let has_prev = st.map(|s| s.index.map(|i| i > 0).unwrap_or(false) || s.position_seconds > 0.0).unwrap_or(false);
            let has_next = st.map(|s| s.next_index.is_some()).unwrap_or(false);
            let playing = st.map(|s| s.state == PlayState::Playing).unwrap_or(false);
            prev = btn(ui, "\u{23ee}", has_prev, small).on_hover_text("Previous song").clicked();
            back = btn(ui, "\u{2212}10 s", st.is_some(), w).clicked();
            toggle = btn(ui, if playing { "Pause" } else { "Play" }, can_play, w).clicked();
            stop = btn(ui, "Stop", st.is_some(), w).clicked();
            forward = btn(ui, "+10 s", st.is_some(), w).clicked();
            next = btn(ui, "\u{23ed}", has_next, small).on_hover_text("Next song").clicked();
        });
        if prev {
            self.prev_song();
        }
        if next {
            self.next_song();
        }
        if let Some(st) = st {
            if back {
                self.seek_by(st, -STEP_SECONDS);
            }
            if forward {
                self.seek_by(st, STEP_SECONDS);
            }
            if toggle {
                self.toggle_play(st);
            }
            if stop {
                self.stop();
            }
        } else if toggle {
            self.play_first();
        }
    }

    fn player_area(&mut self, ui: &mut Ui, status: Option<&Status>) {
        let loaded = status.map(|s| s.state != PlayState::Empty).unwrap_or(false);
        if let Some(st) = status {
            self.keys(ui, st);
        }
        ui.add_space(12.0);
        if let Some(n) = &self.notice {
            ui.add(egui::Label::new(RichText::new(n).size(15.0).color(ORANGE)).wrap(true));
            ui.add_space(10.0);
        }
        match (status, loaded) {
            (Some(st), true) => self.song_view(ui, st),
            _ => self.empty_view(ui),
        }
        if let Some(e) = &self.device_error {
            ui.add_space(16.0);
            ui.add(egui::Label::new(RichText::new(e).size(15.0).color(RED)).wrap(true));
        }
        if let Some(st) = status {
            if loaded && !st.warnings.is_empty() {
                ui.add_space(14.0);
                for w in st.warnings.iter() {
                    ui.add(egui::Label::new(RichText::new(format!("Warning: {}", w)).size(13.0).color(ORANGE)).wrap(true));
                }
            } else if !loaded {
                for w in st.warnings.iter() {
                    ui.add(egui::Label::new(RichText::new(w).size(13.0).color(ORANGE)).wrap(true));
                }
            }
        }
    }

    fn empty_view(&mut self, ui: &mut Ui) {
        ui.label(RichText::new("Rehearsal").size(30.0).color(TEXT));
        ui.add_space(16.0);
        let has_queue = !self.entries.is_empty();
        if has_queue && self.first_readable(0).is_none() {
            ui.add(
                egui::Label::new(RichText::new("None of the songs of this playlist can be played.").size(15.0).color(RED))
                    .wrap(true),
            );
        } else if has_queue {
            // a playlist is chosen, nothing is loaded yet
            if big_button(ui, "Play", true, 110.0).clicked() {
                if let Some(first) = self.first_readable(0) {
                    self.start(first, true, true);
                }
            }
        } else if self.notice.is_none() {
            ui.add(
                egui::Label::new(
                    RichText::new(
                        "No song loaded. Choose a playlist on the left, or right-click a song or a playlist in Home \
                         and choose \"Play in Rehearsal\".",
                    )
                    .size(15.0)
                    .color(DIM),
                )
                .wrap(true),
            );
        }
    }

    fn song_view(&mut self, ui: &mut Ui, st: &Status) {
        if let Some(caption) = next_caption(st.state, &st.song_name, st.countdown_seconds) {
            ui.add(egui::Label::new(RichText::new(caption).size(28.0).strong().color(ORANGE)).wrap(true));
            ui.add_space(10.0);
        }
        ui.add(egui::Label::new(RichText::new(&*st.song_name).size(32.0).strong().color(TEXT)).wrap(true));
        ui.add_space(14.0);
        // while dragging, the time shows where the jump will go
        let (time, dragging) = self.time_text(st);
        ui.label(RichText::new(time).size(44.0).color(if dragging { ORANGE } else { TEXT }));
        ui.add_space(14.0);
        let width = (ui.available_width() - 60.0).clamp(100.0, 900.0);
        self.progress(ui, st, width);
        ui.add_space(18.0);
        self.transport(ui, Some(st), TRANSPORT_BIG);
        ui.add_space(10.0);
        ui.add(
            egui::Label::new(
                RichText::new(
                    "Space: play/pause   \u{2190} \u{2192}: \u{b1}10 s   Shift + \u{2190} \u{2192}: \u{b1}30 s   \
                     \u{2191} \u{2193}: select song   Home: to start   Esc: stop",
                )
                .size(13.0)
                .color(DIM),
            )
            .wrap(true),
        );
        ui.add_space(14.0);
        if st.underruns > 0 {
            ui.label(
                RichText::new(format!("Dropouts: {} (the audio buffer ran empty)", st.underruns))
                    .size(14.0)
                    .color(RED),
            );
        }
        let output = match &self.player {
            Some(player) => {
                format!("Output: {}, {} Hz \u{2013} song: {} Hz", player.device_name(), st.output_rate, st.song_rate)
            }
            None => format!("No audio output open \u{2013} Play opens it. Song: {} Hz", st.song_rate),
        };
        ui.label(RichText::new(output).size(12.0).color(DIM));
        self.midi_line(ui, st);
    }

    /// "MIDI: <Port> - <n> events" with an indicator that lights up for
    /// 150 ms at every message; "MIDI: no file" / "MIDI: off" otherwise.
    fn midi_line(&self, ui: &mut Ui, st: &Status) {
        let (text, lit) = midi_summary(&self.midi_port, st);
        ui.horizontal(|ui| {
            ui.add_space(((ui.available_width() - 260.0) / 2.0).max(0.0));
            let (rect, _) = ui.allocate_exact_size(vec2(12.0, 12.0), Sense::hover());
            let color = if lit.is_some() { ORANGE } else { LINE };
            ui.painter().circle_filled(rect.center(), 5.0, color);
            ui.label(RichText::new(text).size(12.0).color(DIM));
        });
        if let Some(left) = lit {
            ui.ctx().request_repaint_after(left);
        }
        if !self.midi_port.is_empty() {
            if let Some(e) = self.midi.status().error {
                ui.add(egui::Label::new(RichText::new(e).size(13.0).color(ORANGE)).wrap(true));
            }
        }
    }
}

/// The MIDI line of the status, and how much longer the indicator is lit.
fn midi_summary(port: &str, st: &Status) -> (String, Option<Duration>) {
    if port.is_empty() {
        return ("MIDI: off".to_string(), None);
    }
    if !st.midi.has_file {
        return ("MIDI: no file".to_string(), None);
    }
    let lit = st.midi.last_event.and_then(|t| MIDI_FLASH.checked_sub(t.elapsed())).filter(|d| !d.is_zero());
    (format!("MIDI: {} \u{b7} {} events", port, st.midi.events_sent), lit)
}

fn big_button(ui: &mut Ui, text: &str, enabled: bool, width: f32) -> egui::Response {
    sized_button(ui, text, enabled, width, 22.0, 44.0)
}

fn sized_button(ui: &mut Ui, text: &str, enabled: bool, width: f32, font: f32, height: f32) -> egui::Response {
    ui.add_enabled(
        enabled,
        egui::Button::new(RichText::new(text).size(font).color(TEXT))
            .fill(Color32::from_rgb(0x3a, 0x3a, 0x3a))
            .stroke(Stroke::new(1.5_f32, ORANGE))
            .rounding(4.0)
            .min_size(vec2(width, height)),
    )
}

/// Size of the transport buttons.
#[derive(Clone, Copy)]
pub struct TransportStyle {
    pub font: f32,
    pub height: f32,
    /// width of -10 s, Play/Pause, Stop and +10 s
    pub wide: f32,
    /// width of previous and next song
    pub small: f32,
    /// centre the row
    pub center: bool,
}

/// The big buttons of the Rehearsal tab.
pub const TRANSPORT_BIG: TransportStyle = TransportStyle { font: 22.0, height: 44.0, wide: 100.0, small: 56.0, center: true };
/// The buttons in the bar of the Mixer tab.
pub const TRANSPORT_COMPACT: TransportStyle = TransportStyle { font: 15.0, height: 28.0, wide: 62.0, small: 38.0, center: false };

/// Position in the song. Click or drag to jump: while dragging `drag`
/// holds the target (seconds), the jump is returned when the mouse is
/// released; a click returns it at once.
fn progress_bar(ui: &mut Ui, st: &Status, drag: &mut Option<f64>, w: f32) -> Option<f64> {
    let (rect, resp) = ui.allocate_exact_size(vec2(w, 22.0), Sense::click_and_drag());
    let target_at = |x: f32| ((x - rect.left()) / rect.width()).clamp(0.0, 1.0) as f64 * st.length_seconds;
    let mut jump = None;
    if resp.dragged() {
        if let Some(pos) = resp.interact_pointer_pos() {
            *drag = Some(target_at(pos.x));
        }
    }
    if resp.drag_stopped() {
        jump = drag.take();
    } else if resp.clicked() {
        jump = resp.interact_pointer_pos().map(|p| target_at(p.x));
        *drag = None;
    }
    let shown = drag.unwrap_or(st.position_seconds);
    let frac = if st.length_seconds <= 0.0 { 0.0 } else { (shown / st.length_seconds).clamp(0.0, 1.0) as f32 };
    let bar = rect.shrink2(vec2(0.0, 4.0));
    let p = ui.painter();
    p.rect_filled(bar, 3.0, PANEL);
    p.rect_stroke(bar, 3.0, Stroke::new(1.0_f32, LINE));
    let mut fill = bar;
    fill.set_width(bar.width() * frac);
    p.rect_filled(fill, 3.0, ORANGE);
    if resp.hovered() || drag.is_some() {
        let x = bar.left() + bar.width() * frac;
        p.circle_filled(egui::pos2(x, rect.center().y), 8.0, TEXT);
    }
    jump
}

#[cfg(test)]
mod tests {
    use super::*;
    use lp16_tool::rehearsal_queue::SongRow;

    fn rows(n: usize) -> Vec<SongRow> {
        (1..=n)
            .map(|i| SongRow {
                number: i,
                name: format!("Song {}", i),
                length: "3:00".into(),
                midi: false,
                error: None,
                current: false,
                marked: i == 1,
                next: false,
            })
            .collect()
    }

    /// The height the list's scroll area shows for `n` songs when the child
    /// gets `room` pixels, laid out like in `RehearsalPanel::ui`; and the
    /// height its content needs.
    fn shown_height(n: usize, room: f32) -> (f32, f32) {
        let ctx = egui::Context::default();
        let mut result = (0.0, 0.0);
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::pos2(0.0, 0.0), vec2(1000.0, 700.0))),
            ..Default::default()
        };
        let _ = ctx.run(input, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                ui.horizontal_top(|ui| {
                    ui.allocate_ui_with_layout(vec2(LIST_WIDTH, room), Layout::top_down(Align::LEFT), |ui| {
                        ui.set_width(LIST_WIDTH);
                        let out = egui::ScrollArea::vertical()
                            .max_height(room)
                            .auto_shrink([false, true])
                            .show(ui, |ui| {
                                gui_playlist::ui(ui, &[], None, false, &rows(n));
                            });
                        result = (out.inner_rect.height(), out.content_size.y);
                    });
                });
            });
        });
        result
    }

    #[test]
    fn the_song_list_shows_all_songs_when_it_has_the_room() {
        // the old layout gave the child 10 px: one row at most was visible
        let (shown, needed) = shown_height(12, 10.0);
        assert!(shown < needed / 2.0, "documents the old failure: {} of {}", shown, needed);
        let room = list_room(700.0, 300.0);
        let (shown, needed) = shown_height(12, room);
        assert!(needed > 12.0 * 26.0, "{}", needed);
        assert!(shown + 0.5 >= needed, "all 12 songs are visible: {} of {}", shown, needed);
    }

    #[test]
    fn the_list_room_is_the_window_height_but_at_least_the_minimum() {
        assert_eq!(list_room(900.0, 300.0), 600.0);
        assert_eq!(list_room(900.0, 800.0), LIST_MAX_HEIGHT);
        assert_eq!(list_room(900.0, 1200.0), LIST_MAX_HEIGHT);
    }
}

//! The "Rehearsal" tab: the playlist and song list on the left, the song in
//! the rehearsal player in the middle, the mixer below it. The list and the
//! mixer can be dragged wider and taller; the strips scale with the mixer's
//! frame. The mixer edits the rehearsal mix or, in song mode, the song mix of
//! the loaded song (its uTool2 mixer values, saved in its .songprj). All the
//! logic (mixing, transport, the queue, reading) is in the library
//! (`lp16_tool::rehearsal`, `lp16_tool::rehearsal_queue`,
//! `lp16_tool::audio_out`); this only draws and sends commands.

use crate::gui_app::{DIM, LINE, ORANGE, PANEL, RED, TEXT};
use crate::gui_mixer::MixerOutcome;
use crate::gui_mixer_tab::{ConsoleMode, MixerConsole};
use crate::gui_playlist;
use eframe::egui::{self, vec2, Align, Color32, Key, Layout, Margin, Modifiers, RichText, Sense, Stroke, Ui};
use lp16_tool::audio_out::{AudioSettings, Command, Player, StreamInfo};
use lp16_tool::midi_out::{MidiLink, PortStatus};
use lp16_tool::mix::{set_song_strip, song_strips, MixLibrary, MixParams, SongStrip, STRIPS};
use lp16_tool::read_midi_events;
use lp16_tool::rehearsal::{fmt_clock, Engine, Levels, PlayState, Status, StereoGains};
use lp16_tool::rehearsal_midi::start_messages;
use lp16_tool::rehearsal_queue::{
    entries_from_playlist, find_path, next_caption, same_path, same_songs, song_rows, QueueEntry, SongRow,
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
/// Width of the left part (playlist and song list) at the start, and the
/// narrowest it can be dragged; at most it takes this share of the tab.
const LIST_WIDTH: f32 = 290.0;
const MIN_LIST_WIDTH: f32 = 160.0;
const MAX_LIST_SHARE: f32 = 0.6;
/// Share of the height the mixer gets at the start, the lowest it can be
/// dragged, and the room it always leaves to the player above it.
const MIXER_SHARE: f32 = 0.6;
const MIN_MIXER_HEIGHT: f32 = 120.0;
const MIN_PLAYER_HEIGHT: f32 = 120.0;
/// A changed song mix is saved this long after the last change (and when
/// the song changes, the mode is left or the program ends).
const SONG_SAVE_DELAY: Duration = Duration::from_millis(800);
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
    mixer: MixerConsole,
    /// the mix the engine should use; sent again to every new stream
    mix: MixParams,
    /// left/right gains instead of the mix (the preview of the MP3 backup
    /// tab); sent again to every new stream
    stereo: Option<StereoGains>,
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
    /// the mixer edits the song mix of the loaded song
    song_mode: bool,
    /// the song mix being edited (follows the loaded song in song mode)
    song_edit: Option<SongEdit>,
    /// why the last save of a song mix failed
    song_error: Option<String>,
    /// song files whose mix was saved (see `take_saved_songs`)
    saved_songs: Vec<String>,
}

/// The song mix of one song while it is edited.
struct SongEdit {
    /// queue entry
    index: usize,
    project: SongProject,
    strips: [SongStrip; STRIPS],
    /// outputs the song has a track on
    used: [bool; STRIPS],
    /// changed and not saved yet, since
    dirty: Option<Instant>,
}

/// The range the list can be dragged in, for a tab `total` wide.
fn list_width_range(total: f32) -> std::ops::RangeInclusive<f32> {
    MIN_LIST_WIDTH..=(total * MAX_LIST_SHARE).max(MIN_LIST_WIDTH)
}

/// The range the mixer can be dragged in, for a tab `total` high.
fn mixer_height_range(total: f32) -> std::ops::RangeInclusive<f32> {
    MIN_MIXER_HEIGHT..=(total - MIN_PLAYER_HEIGHT).max(MIN_MIXER_HEIGHT)
}

/// The track names per strip: tracks on the same output are joined.
pub(crate) fn track_names(project: &SongProject) -> [String; STRIPS] {
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

    /// Play with these left/right gains per strip instead of the rehearsal
    /// mix (the "Export Stereo-MP3" tab while it is shown); None = the
    /// rehearsal mix. Only a change is sent.
    pub fn set_stereo(&mut self, stereo: Option<StereoGains>) {
        if stereo == self.stereo {
            return;
        }
        self.stereo = stereo;
        if let Some(player) = &self.player {
            player.send(Command::SetStereo(stereo));
        }
    }

    /// Name of the loaded playlist (None: a single song or nothing).
    pub fn loaded_playlist(&self) -> Option<&str> {
        self.playlist.as_deref()
    }

    /// Song `index` of playlist `name`: the playlist is loaded if it is not
    /// the loaded one, then the song is started (`play`) or loaded stopped
    /// at its beginning.
    pub fn preview_song(&mut self, name: &str, index: usize, play: bool) {
        if self.playlist.as_deref() != Some(name) {
            self.play_playlist(name);
        }
        if play {
            self.start(index, true, false);
        } else {
            self.click_song(index);
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

    // -- the song mix ----------------------------------------------------------

    /// Show the song mix of the loaded song in the mixer (Home > song >
    /// Mixer, or the "Song mix" button).
    pub fn edit_song_mix(&mut self) {
        self.song_mode = true;
        self.song_error = None;
    }

    /// Back to the rehearsal mix; a changed song mix is saved now.
    pub fn leave_song_mix(&mut self) {
        self.save_song_mix(true);
        self.song_mode = false;
        self.song_edit = None;
    }

    /// The song files whose song mix was saved since the last call, so the
    /// owner reads them again (the symbol in the lists).
    pub fn take_saved_songs(&mut self) -> Vec<String> {
        std::mem::take(&mut self.saved_songs)
    }

    /// Called every frame, whichever tab is shown: saves a changed song mix
    /// once the user paused, and asks for the frame that does it.
    pub fn tick(&mut self, ctx: &egui::Context) {
        self.save_song_mix(false);
        if self.song_edit.as_ref().map(|e| e.dirty.is_some()).unwrap_or(false) {
            ctx.request_repaint_after(SONG_SAVE_DELAY);
        }
    }

    /// In song mode the edited song follows the loaded one (queue entry
    /// `index`); the song mix of the one before is saved first.
    fn follow_song(&mut self, index: Option<usize>) {
        if !self.song_mode {
            return;
        }
        let path = index.and_then(|i| self.entries.get(i)).and_then(|e| e.path()).map(str::to_string);
        let same = match (&self.song_edit, &path) {
            (Some(e), Some(p)) => Some(e.index) == index && same_path(&e.project.path, p),
            _ => false,
        };
        if same {
            return;
        }
        self.save_song_mix(true);
        self.song_edit = index.and_then(|i| {
            let project = self.entries.get(i)?.project.as_ref().ok()?.clone();
            let mut used = [false; STRIPS];
            for t in &project.tracks {
                if t.channel >= 0 && (t.channel as usize) < STRIPS {
                    used[t.channel as usize] = true;
                }
            }
            Some(SongEdit { index: i, strips: song_strips(&project), project, used, dirty: None })
        });
        if self.song_edit.is_none() {
            // nothing loaded (any more): the rehearsal mix again
            self.song_mode = false;
        }
    }

    /// The song mix was changed in the mixer: into the project, to the
    /// engine at once, saved a little later.
    fn song_mix_changed(&mut self) {
        let Some(edit) = self.song_edit.as_mut() else { return };
        let mut changed = false;
        for k in 0..STRIPS {
            changed |= set_song_strip(&mut edit.project, k, edit.strips[k]);
        }
        // what the project holds now (a level is stored with one decimal)
        edit.strips = song_strips(&edit.project);
        if !changed {
            return;
        }
        edit.dirty = Some(Instant::now());
        if let Some(player) = &self.player {
            player.send(Command::SetSongGains(edit.index, edit.strips.map(|s| s.gain())));
        }
    }

    /// Write a changed song mix into the song file (`force`: now, else only
    /// after `SONG_SAVE_DELAY`). The queue gets the new project too, so the
    /// song plays with it the next time it is opened.
    fn save_song_mix(&mut self, force: bool) {
        let Some(edit) = self.song_edit.as_mut() else { return };
        let Some(since) = edit.dirty else { return };
        if !force && since.elapsed() < SONG_SAVE_DELAY {
            return;
        }
        edit.dirty = None;
        if let Err(e) = edit.project.save() {
            self.song_error = Some(e.to_string());
            return;
        }
        self.song_error = None;
        let project = edit.project.clone();
        self.saved_songs.push(project.path.clone());
        let mut entries: Vec<QueueEntry> = (*self.entries).clone();
        let mut any = false;
        for e in entries.iter_mut() {
            if let Ok(p) = e.project.as_mut() {
                if same_path(&p.path, &project.path) {
                    *p = project.clone();
                    any = true;
                }
            }
        }
        if any {
            let entries = Arc::new(entries);
            self.entries = entries.clone();
            if let Some(player) = &self.player {
                player.send(Command::ReplaceQueue(entries));
            }
        }
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
        // a changed song mix goes into the file first (the editor keeps it)
        self.save_song_mix(true);
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
        self.song_edit = None;
        self.song_mode = false;
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
                p.send(Command::SetStereo(self.stereo));
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
        self.save_song_mix(true);
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

    /// Draw the tab: the song list on the left, the player, below it the
    /// rehearsal mix `mixes`. The list and the mixer are panels the user can
    /// drag wider and taller (egui keeps their size while the program runs).
    /// Returns true if the mix (values or presets) was changed, so the
    /// caller saves the config.
    pub fn ui(&mut self, ui: &mut Ui, mixes: &mut MixLibrary) -> bool {
        let status = self.begin_frame(ui.ctx());
        let levels = self.take_levels();
        let (total_w, total_h) = (ui.available_width(), ui.available_height());
        egui::SidePanel::left("rehearsal_list_panel")
            .resizable(true)
            .default_width(LIST_WIDTH)
            .width_range(list_width_range(total_w))
            .frame(egui::Frame::none().inner_margin(Margin { right: 6.0, ..Default::default() }))
            .show_inside(ui, |ui| self.list_side(ui, status.as_ref()));
        let mut out = MixerOutcome::default();
        egui::TopBottomPanel::bottom("rehearsal_mixer_panel")
            .resizable(true)
            .default_height((total_h * MIXER_SHARE).max(MIN_MIXER_HEIGHT))
            .height_range(mixer_height_range(total_h))
            .frame(egui::Frame::none().inner_margin(Margin { left: 6.0, top: 8.0, ..Default::default() }))
            .show_inside(ui, |ui| {
                let loaded = status.as_ref().filter(|s| s.state != PlayState::Empty);
                self.follow_song(loaded.and_then(|s| s.index));
                let names = self.track_names.clone();
                let error = self.song_error.clone();
                let mode = match self.song_edit.as_mut() {
                    Some(edit) if self.song_mode => ConsoleMode::Song {
                        name: &edit.project.name,
                        strips: &mut edit.strips,
                        used: edit.used,
                        error: error.as_deref(),
                    },
                    _ => {
                        let song = loaded
                            .and_then(|s| s.index)
                            .and_then(|i| self.entries.get(i))
                            .and_then(|e| e.project.as_ref().ok());
                        ConsoleMode::Rehearsal {
                            song: song.map(song_strips).unwrap_or([SongStrip::default(); STRIPS]),
                            song_name: song.map(|p| p.name.as_str()),
                        }
                    }
                };
                out = self.mixer.ui(ui, "rehearsal_mixer_scroll", mixes, mode, &names, &levels);
            });
        egui::CentralPanel::default()
            .frame(egui::Frame::none().inner_margin(Margin { left: 6.0, ..Default::default() }))
            .show_inside(ui, |ui| {
                egui::ScrollArea::vertical().id_source("rehearsal_player_scroll").auto_shrink([false, false]).show(
                    ui,
                    |ui| {
                        ui.with_layout(Layout::top_down(Align::Center), |ui| self.player_area(ui, status.as_ref()));
                    },
                );
            });
        if out.changed {
            self.set_mix(mixes.active().params());
        }
        if out.reset_clip {
            self.reset_clip();
        }
        if out.song_changed {
            self.song_mix_changed();
        }
        if out.leave_song {
            self.leave_song_mix();
        }
        if out.enter_song {
            self.edit_song_mix();
            ui.ctx().request_repaint();
        }
        out.changed
    }

    /// The left panel: autoplay, the playlist and the song list (it scrolls
    /// by itself), the skipped songs at the bottom.
    fn list_side(&mut self, ui: &mut Ui, status: Option<&Status>) {
        self.autoplay_option(ui);
        ui.add_space(4.0);
        if status.map(|s| !s.queue_warnings.is_empty()).unwrap_or(false) {
            egui::TopBottomPanel::bottom("rehearsal_list_warnings")
                .frame(egui::Frame::none())
                .show_separator_line(false)
                .show_inside(ui, |ui| self.list_warnings(ui, status));
        }
        egui::ScrollArea::vertical()
            .id_source("rehearsal_list_scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| self.list_area(ui, status));
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
            let (icon, tip) = if playing { (Icon::Pause, "Pause (Space)") } else { (Icon::Play, "Play (Space)") };
            toggle = icon_button(ui, icon, can_play, w, style.height).on_hover_text(tip).clicked();
            stop = icon_button(ui, Icon::Stop, st.is_some(), w, style.height).on_hover_text("Stop (Esc)").clicked();
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
            if icon_button(ui, Icon::Play, true, 110.0, 44.0).on_hover_text("Play (Space)").clicked() {
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

/// The symbols of the transport buttons, drawn (not taken from the font).
#[derive(Clone, Copy, PartialEq, Debug)]
enum Icon {
    Play,
    Pause,
    Stop,
}

/// A transport button with `icon`; the symbol is scaled to the height.
fn icon_button(ui: &mut Ui, icon: Icon, enabled: bool, width: f32, height: f32) -> egui::Response {
    let resp = sized_button(ui, "", enabled, width, 12.0, height);
    let color = if enabled { TEXT } else { DIM };
    let c = resp.rect.center();
    let s = height * 0.42; // size of the symbol
    let p = ui.painter();
    match icon {
        Icon::Play => {
            // a triangle pointing right, centred by its area
            let pts = vec![
                egui::pos2(c.x - s * 0.4, c.y - s / 2.0),
                egui::pos2(c.x + s * 0.55, c.y),
                egui::pos2(c.x - s * 0.4, c.y + s / 2.0),
            ];
            p.add(egui::Shape::convex_polygon(pts, color, Stroke::NONE));
        }
        Icon::Pause => {
            let bar = vec2(s * 0.3, s);
            for dx in [-s * 0.27, s * 0.27] {
                p.rect_filled(egui::Rect::from_center_size(egui::pos2(c.x + dx, c.y), bar), 1.0, color);
            }
        }
        Icon::Stop => {
            p.rect_filled(egui::Rect::from_center_size(c, vec2(s * 0.85, s * 0.85)), 1.5, color);
        }
    }
    resp
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

/// The small buttons of the preview player in the "Export Stereo-MP3" tab.
pub const TRANSPORT_SMALL: TransportStyle = TransportStyle { font: 14.0, height: 28.0, wide: 62.0, small: 38.0, center: false };

/// The big buttons of the Rehearsal tab.
pub const TRANSPORT_BIG: TransportStyle = TransportStyle { font: 22.0, height: 44.0, wide: 100.0, small: 56.0, center: true };

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

    /// A .songprj with tracks on `channels` (the WAVs need not exist).
    fn song_file(dir: &std::path::Path, channels: &[i32]) -> SongProject {
        let _ = std::fs::create_dir_all(dir);
        let xml = lp16_tool::generate_songprj("Mix Test", 0, 48000, 24, 1);
        let mut prj = SongProject::parse(&xml, "");
        prj.tracks = channels.iter().map(|c| lp16_tool::ProjectTrack::new(*c, &format!("t{}.wav", c), "")).collect();
        let path = dir.join("mixtest.songprj");
        prj.save_as(path.to_str().unwrap()).unwrap();
        SongProject::load(path.to_str().unwrap(), "").unwrap()
    }

    #[test]
    fn the_song_mix_is_saved_into_the_song_and_used_by_the_queue() {
        let dir = std::env::temp_dir().join(format!("lp16-gui-songmix-{}", std::process::id()));
        let prj = song_file(&dir, &[0, 2]);
        let mut panel = RehearsalPanel::default();
        panel.load_song(&prj);
        panel.edit_song_mix();
        let index = panel.current_status().and_then(|s| s.index);
        assert_eq!(index, Some(0));
        panel.follow_song(index);
        let edit = panel.song_edit.as_mut().expect("song mode edits the loaded song");
        assert_eq!(edit.used[0..3], [true, false, true]);
        edit.strips[2] = SongStrip { level_db: 2.0, mute: false };
        panel.song_mix_changed();
        // not yet on disk: it waits for a pause of the user
        let disk = SongProject::load(&prj.path, "").unwrap();
        assert_eq!(song_strips(&disk)[2], SongStrip::default());
        panel.save_song_mix(true);
        let disk = SongProject::load(&prj.path, "").unwrap();
        assert_eq!(song_strips(&disk)[2], SongStrip { level_db: 2.0, mute: false });
        assert!(lp16_tool::mix::has_song_mix(&disk));
        assert_eq!(panel.take_saved_songs(), vec![prj.path.clone()]);
        // the queue has it too: the next time the song is opened, it plays with it
        let queued = panel.entries[0].project.as_ref().unwrap();
        assert_eq!(song_strips(queued)[2].level_db, 2.0);
        assert!(panel.rows(panel.current_status().as_ref())[0].mixer, "the list shows the symbol");
        // back to the rehearsal mix
        panel.leave_song_mix();
        assert!(!panel.song_mode && panel.song_edit.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn song_mode_ends_when_nothing_is_loaded() {
        let mut panel = RehearsalPanel::default();
        panel.edit_song_mix();
        panel.follow_song(None);
        assert!(!panel.song_mode);
    }

    fn rows(n: usize) -> Vec<SongRow> {
        (1..=n)
            .map(|i| SongRow {
                number: i,
                name: format!("Song {}", i),
                length: "3:00".into(),
                midi: false,
                mixer: false,
                error: None,
                current: false,
                marked: i == 1,
                next: false,
            })
            .collect()
    }

    /// The height the list's scroll area shows for `n` songs in the left
    /// panel of a window `height` high, laid out like `RehearsalPanel::ui`;
    /// and the height its content needs.
    fn shown_height(n: usize, height: f32) -> (f32, f32) {
        let ctx = egui::Context::default();
        let mut result = (0.0, 0.0);
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::pos2(0.0, 0.0), vec2(1400.0, height))),
            ..Default::default()
        };
        let _ = ctx.run(input, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                egui::SidePanel::left("list").default_width(LIST_WIDTH).show_inside(ui, |ui| {
                    let out = egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                        gui_playlist::ui(ui, &[], None, false, &rows(n));
                    });
                    result = (out.inner_rect.height(), out.content_size.y);
                });
            });
        });
        result
    }

    #[test]
    fn the_song_list_uses_the_height_of_the_window() {
        let (shown, needed) = shown_height(12, 900.0);
        assert!(needed > 12.0 * 26.0, "{}", needed);
        assert!(shown + 0.5 >= needed, "all 12 songs are visible: {} of {}", shown, needed);
        // a low window: the list scrolls, but fills the panel
        let (shown, needed) = shown_height(40, 500.0);
        assert!(shown < needed && shown > 400.0, "{} of {}", shown, needed);
    }

    #[test]
    fn the_panels_can_be_dragged_within_limits() {
        assert_eq!(list_width_range(1000.0), MIN_LIST_WIDTH..=600.0);
        assert_eq!(mixer_height_range(900.0), MIN_MIXER_HEIGHT..=780.0);
        // a tiny window never gives an empty range
        assert_eq!(list_width_range(10.0), MIN_LIST_WIDTH..=MIN_LIST_WIDTH);
        assert_eq!(mixer_height_range(10.0), MIN_MIXER_HEIGHT..=MIN_MIXER_HEIGHT);
    }

    #[test]
    fn the_tab_draws_in_small_and_large_windows() {
        for (w, h) in [(300.0, 200.0), (1200.0, 800.0), (2600.0, 1400.0)] {
            let ctx = egui::Context::default();
            let mut panel = RehearsalPanel::default();
            let mut mixes = MixLibrary::default();
            for _ in 0..2 {
                let input = egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(egui::pos2(0.0, 0.0), vec2(w, h))),
                    ..Default::default()
                };
                let _ = ctx.run(input, |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        panel.ui(ui, &mut mixes);
                    });
                });
            }
        }
    }
}

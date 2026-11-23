//! The rehearsal session: what the player does between the user interface
//! and the audio output. It owns the queue (a playlist of the library or
//! one song), the stream and its life cycle (opened with the first Play,
//! opened again for another sample rate or other settings, parked when the
//! device is lost), the rehearsal mix, the stereo override of the backup
//! tab, autoplay, MIDI, and the song mix of the loaded song while it is
//! edited (saved into its .songprj a little after the last change).
//!
//! The output is reached through the `Device` seam. Two adapters satisfy
//! it: `CpalDevice` (the real `Player` and `MidiLink`) and
//! `memory::MemoryDevice` (an engine in memory, rendered by the test), so
//! everything here is tested without a sound card.

use crate::audio_out::{AudioSettings, Command, Player, StreamInfo};
use crate::midi_out::{MidiLink, PortStatus};
use crate::mix::{set_song_strip, song_strips, MixParams, SongStrip, STRIPS};
use crate::read_midi_events;
use crate::rehearsal::{Engine, Levels, PlayState, Status, StereoGains};
use crate::rehearsal_midi::{start_messages, MidiMsg};
use crate::rehearsal_queue::{entries_from_playlist, find_path, same_path, same_songs, song_rows, QueueEntry, SongRow};
use crate::{PlaylistSong, SongProject};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Quick jumps add up to the last target for this long, because the status
/// only shows it after the next audio block.
pub const SEEK_MEMORY: Duration = Duration::from_millis(400);
/// A changed song mix is saved this long after the last change (and when
/// the song changes, the mode is left or the program ends).
pub const SONG_SAVE_DELAY: Duration = Duration::from_millis(800);

/// Shown after the loaded song was renamed, deleted or saved.
pub const CHANGED_NOTICE: &str = "The song was changed in the library \u{2013} load it again.";

// ---------------------------------------------------------------------------
// the seam
// ---------------------------------------------------------------------------

/// An open output stream with its engine.
pub trait Output {
    /// Whether this stream fits `settings` and `rate`; if not, it has to be
    /// closed and opened again.
    fn matches(&self, settings: &AudioSettings, rate: u32) -> bool;
    fn output_rate(&self) -> u32;
    fn info(&self) -> &StreamInfo;
    /// Set once the stream failed (device unplugged); text for the user.
    fn error(&self) -> Option<String>;
    fn send(&self, cmd: Command);
    fn status(&self) -> Status;
    fn take_levels(&self) -> Levels;
}

/// What opens output streams, and the MIDI port next to them.
pub trait Device {
    /// Open a stream with `settings` at `rate`; the MIDI of the song goes
    /// to the port in time with the audio. The error text is for the user.
    fn open(&mut self, settings: &AudioSettings, rate: u32) -> Result<Box<dyn Output>, String>;
    /// Choose the MIDI port (empty = off).
    fn set_midi_port(&mut self, port: &str);
    /// Open the chosen port if it is not open (again after an error).
    fn open_midi(&mut self);
    /// Send messages to the port at once.
    fn send_midi(&mut self, msgs: &[MidiMsg]);
    fn midi_status(&self) -> PortStatus;
    /// All Notes Off for sounding notes, close the port.
    fn shutdown(&mut self);
}

impl Output for Player {
    fn matches(&self, settings: &AudioSettings, rate: u32) -> bool {
        Player::matches(self, settings, rate)
    }
    fn output_rate(&self) -> u32 {
        Player::output_rate(self)
    }
    fn info(&self) -> &StreamInfo {
        Player::info(self)
    }
    fn error(&self) -> Option<String> {
        Player::error(self)
    }
    fn send(&self, cmd: Command) {
        Player::send(self, cmd)
    }
    fn status(&self) -> Status {
        Player::status(self)
    }
    fn take_levels(&self) -> Levels {
        Player::take_levels(self)
    }
}

/// The sound card (cpal) and the MIDI port (midir).
#[derive(Default)]
pub struct CpalDevice {
    midi: MidiLink,
}

impl Device for CpalDevice {
    fn open(&mut self, settings: &AudioSettings, rate: u32) -> Result<Box<dyn Output>, String> {
        self.midi.open();
        Ok(Box::new(Player::open(settings, rate, Some(&self.midi))?))
    }
    fn set_midi_port(&mut self, port: &str) {
        self.midi.set_port(port);
    }
    fn open_midi(&mut self) {
        self.midi.open();
    }
    fn send_midi(&mut self, msgs: &[MidiMsg]) {
        self.midi.send(msgs);
    }
    fn midi_status(&self) -> PortStatus {
        self.midi.status()
    }
    fn shutdown(&mut self) {
        self.midi.shutdown();
    }
}

// ---------------------------------------------------------------------------
// the session
// ---------------------------------------------------------------------------

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

/// The song mix being edited, for the mixer.
pub struct SongMixEdit<'a> {
    pub name: &'a str,
    pub strips: &'a mut [SongStrip; STRIPS],
    pub used: [bool; STRIPS],
    /// why the last save failed
    pub error: Option<&'a str>,
}

/// The track names per strip: tracks on the same output are joined.
pub fn track_names(project: &SongProject) -> [String; STRIPS] {
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

pub struct Session {
    device: Box<dyn Device>,
    /// the audio stream, opened with the first Play and opened again when
    /// the rate of the song or the audio settings change
    player: Option<Box<dyn Output>>,
    /// the audio output chosen in the settings
    settings: AudioSettings,
    /// why the audio output could not be opened, or was lost
    device_error: Option<String>,
    /// the queue: the songs of the loaded playlist, all songs of the
    /// library ("single song"), or one song outside the library
    entries: Arc<Vec<QueueEntry>>,
    /// name of the loaded playlist; None for single songs
    playlist: Option<String>,
    /// the playlist as the queue was built from it, to see a change
    loaded_songs: Vec<PlaylistSong>,
    /// {$AudioLib} of the library, and its playlists
    audio_lib: String,
    library: Vec<(String, Vec<PlaylistSong>)>,
    /// all songs of the library, the list of "single song"
    library_songs: Vec<PlaylistSong>,
    /// the library list is loaded and a song of it was changed: the queue
    /// is built again with the next `set_library_songs`
    library_stale: bool,
    /// status of the loaded song while no stream is open (lost device,
    /// test tone, nothing played yet); the position is kept
    parked: Option<Status>,
    notice: Option<String>,
    /// the last target sent, and when
    last_seek: Option<(f64, Instant)>,
    /// the rehearsal mix; sent again to every new stream
    mix: MixParams,
    /// left/right gains instead of the mix (the preview of the MP3 backup);
    /// sent again to every new stream
    stereo: Option<StereoGains>,
    /// per LP-16 output the names of the tracks of the loaded song, and the
    /// queue index they are for
    track_names: [String; STRIPS],
    names_for: Option<usize>,
    /// the port chosen in the settings (empty = MIDI is off)
    midi_port: String,
    /// "Autoplay after 5 seconds"; sent again to every new stream
    autoplay: bool,
    /// the mixer edits the song mix of the loaded song
    song_mode: bool,
    song_edit: Option<SongEdit>,
    /// why the last save of a song mix failed
    song_error: Option<String>,
    /// song files whose mix was saved (see `take_saved_songs`)
    saved_songs: Vec<String>,
}

impl Default for Session {
    fn default() -> Session {
        Session::new(Box::<CpalDevice>::default())
    }
}

impl Session {
    pub fn new(device: Box<dyn Device>) -> Session {
        Session {
            device,
            player: None,
            settings: AudioSettings::default(),
            device_error: None,
            entries: Arc::new(Vec::new()),
            playlist: None,
            loaded_songs: Vec::new(),
            audio_lib: String::new(),
            library: Vec::new(),
            library_songs: Vec::new(),
            library_stale: false,
            parked: None,
            notice: None,
            last_seek: None,
            mix: MixParams::default(),
            stereo: None,
            track_names: Default::default(),
            names_for: None,
            midi_port: String::new(),
            autoplay: false,
            song_mode: false,
            song_edit: None,
            song_error: None,
            saved_songs: Vec::new(),
        }
    }

    fn send(&self, cmd: Command) {
        if let Some(player) = &self.player {
            player.send(cmd);
        }
    }

    // -- settings --------------------------------------------------------------

    /// The audio output of the settings; used the next time a stream is
    /// opened. A running stream is not touched.
    pub fn set_settings(&mut self, settings: AudioSettings) {
        self.settings = settings;
    }

    /// The MIDI output port of the settings (empty = off). It is opened
    /// by the next Play (or the MIDI test).
    pub fn set_midi_port(&mut self, port: &str) {
        self.midi_port = port.to_string();
        self.device.set_midi_port(port);
    }

    pub fn midi_port(&self) -> &str {
        &self.midi_port
    }

    pub fn midi_status(&self) -> PortStatus {
        self.device.midi_status()
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
            .and_then(|i| self.project(i))
            .and_then(|p| p.midi_path())
            .and_then(|path| read_midi_events(&path));
        let Some(events) = events else { return false };
        self.device.send_midi(&start_messages(&events));
        true
    }

    /// Autoplay on/off; the engine gets it now and with every stream
    /// opened later.
    pub fn set_autoplay(&mut self, on: bool) {
        self.autoplay = on;
        self.send(Command::SetAutoplay(on));
    }

    pub fn autoplay(&self) -> bool {
        self.autoplay
    }

    /// The rehearsal mix for the engine (kept for streams opened later).
    pub fn set_mix(&mut self, mix: MixParams) {
        self.mix = mix;
        self.send(Command::SetMix(mix));
    }

    /// Play with these left/right gains per strip instead of the rehearsal
    /// mix (the backup tab while it is shown); None = the rehearsal mix.
    /// Only a change is sent.
    pub fn set_stereo(&mut self, stereo: Option<StereoGains>) {
        if stereo == self.stereo {
            return;
        }
        self.stereo = stereo;
        self.send(Command::SetStereo(stereo));
    }

    /// The clip LED of the master was clicked.
    pub fn reset_clip(&mut self) {
        self.send(Command::ResetClip);
    }

    // -- what the user interface shows -------------------------------------------

    /// Name of the loaded playlist (None: a single song or nothing).
    pub fn loaded_playlist(&self) -> Option<&str> {
        self.playlist.as_deref()
    }

    pub fn playlist_names(&self) -> Vec<String> {
        self.library.iter().map(|(n, _)| n.clone()).collect()
    }

    /// Single songs (not a playlist) are loaded.
    pub fn is_single(&self) -> bool {
        self.playlist.is_none() && !self.entries.is_empty()
    }

    /// The library list ("single song") is loaded.
    fn library_loaded(&self) -> bool {
        self.playlist.is_none() && !self.loaded_songs.is_empty()
    }

    /// The library has songs for "single song".
    pub fn has_library_songs(&self) -> bool {
        !self.library_songs.is_empty()
    }

    pub fn has_queue(&self) -> bool {
        !self.entries.is_empty()
    }

    /// The queue has a song Play can start.
    pub fn can_start(&self) -> bool {
        self.first_readable(0).is_some()
    }

    /// The song of queue entry `index`, if it could be read.
    pub fn project(&self, index: usize) -> Option<&SongProject> {
        self.entries.get(index).and_then(|e| e.project.as_ref().ok())
    }

    /// What the open stream runs with.
    pub fn stream_info(&self) -> Option<&StreamInfo> {
        self.player.as_ref().map(|p| p.info())
    }

    /// The last error of the audio output, if any.
    pub fn error(&self) -> Option<&str> {
        self.device_error.as_deref()
    }

    /// Why the player stopped by itself (playlist deleted or changed, song
    /// changed in the library).
    pub fn notice(&self) -> Option<&str> {
        self.notice.as_deref()
    }

    /// Per LP-16 output the track names of the loaded song.
    pub fn track_names(&self) -> &[String; STRIPS] {
        &self.track_names
    }

    pub fn is_playing(&self) -> bool {
        self.player.as_ref().map(|p| p.status().state == PlayState::Playing).unwrap_or(false)
    }

    /// The meter peaks since the last call.
    pub fn take_levels(&self) -> Levels {
        self.player.as_ref().map(|p| p.take_levels()).unwrap_or_default()
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

    /// Once per frame: look after the stream (lost device, other sample
    /// rate), keep the track names and the edited song mix with the loaded
    /// song. Returns the status to draw.
    pub fn poll(&mut self) -> Option<Status> {
        self.check_stream();
        self.check_rate();
        let status = self.current_status();
        let index = status.as_ref().and_then(|s| s.index);
        if index != self.names_for {
            self.names_for = index;
            self.track_names = index.and_then(|i| self.project(i)).map(track_names).unwrap_or_default();
        }
        let loaded = status.as_ref().filter(|s| s.state != PlayState::Empty).and_then(|s| s.index);
        self.follow_song(loaded);
        status
    }

    /// The status of the player, or of the parked song.
    pub fn current_status(&self) -> Option<Status> {
        self.player.as_ref().map(|p| p.status()).or_else(|| self.parked.clone())
    }

    // -- the song mix ----------------------------------------------------------

    /// The mixer shows the song mix of the loaded song.
    pub fn edit_song_mix(&mut self) {
        self.song_mode = true;
        self.song_error = None;
        let loaded = self.current_status().filter(|s| s.state != PlayState::Empty).and_then(|s| s.index);
        self.follow_song(loaded);
    }

    /// Back to the rehearsal mix; a changed song mix is saved now.
    pub fn leave_song_mix(&mut self) {
        self.save_song_mix(true);
        self.song_mode = false;
        self.song_edit = None;
    }

    /// The song mix being edited, if the mixer shows one.
    pub fn song_mix(&mut self) -> Option<SongMixEdit<'_>> {
        if !self.song_mode {
            return None;
        }
        let error = self.song_error.as_deref();
        self.song_edit.as_mut().map(|e| SongMixEdit {
            name: &e.project.name,
            strips: &mut e.strips,
            used: e.used,
            error,
        })
    }

    /// The song files whose song mix was saved since the last call, so the
    /// owner reads them again.
    pub fn take_saved_songs(&mut self) -> Vec<String> {
        std::mem::take(&mut self.saved_songs)
    }

    /// Called every frame: saves a changed song mix once the user paused.
    /// True while a change waits to be saved (the caller asks for a frame
    /// after `SONG_SAVE_DELAY`).
    pub fn tick(&mut self) -> bool {
        self.save_song_mix(false);
        self.song_edit.as_ref().map(|e| e.dirty.is_some()).unwrap_or(false)
    }

    /// Save a changed song mix now.
    pub fn flush_song_mix(&mut self) {
        self.save_song_mix(true);
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
            let project = self.project(i)?.clone();
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
    pub fn song_mix_changed(&mut self) {
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
        let cmd = Command::SetSongGains(edit.index, edit.strips.map(|s| s.gain()));
        self.send(cmd);
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
        let project = match crate::project_lib::save_song_mix(&edit.project) {
            Ok(p) => p,
            Err(e) => {
                self.song_error = Some(e);
                return;
            }
        };
        edit.project = project.clone();
        self.song_error = None;
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
            self.send(Command::ReplaceQueue(entries));
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

    /// All songs of the library, in the order of the Home tab (called
    /// whenever the library is read again). If the library list is loaded
    /// and its songs changed, the queue is rebuilt: the song that plays goes
    /// on, if it is still in it.
    pub fn set_library_songs(&mut self, songs: Vec<PlaylistSong>) {
        self.library_songs = songs;
        if !self.library_loaded() {
            self.library_stale = false;
            return;
        }
        if self.library_stale || !same_songs(&self.library_songs, &self.loaded_songs) {
            self.library_stale = false;
            self.rebuild(self.library_songs.clone());
        }
    }

    /// The loaded playlist was renamed: the name follows.
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
        if find_path(&self.entries, path, None).is_none() {
            return;
        }
        if self.library_loaded() {
            // the whole library is listed: only the loaded song has to go,
            // the others are read again with the library
            let loaded = self.current_status().filter(|s| s.state != PlayState::Empty).and_then(|s| s.index);
            let is_loaded = loaded.and_then(|i| self.entries.get(i)).and_then(|e| e.path()).map(|p| same_path(p, path));
            if is_loaded != Some(true) {
                self.library_stale = true;
                return;
            }
        }
        self.clear(CHANGED_NOTICE.to_string());
    }

    /// Stop, unload everything and show `notice`.
    fn clear(&mut self, notice: String) {
        self.send(Command::Stop);
        self.send(Command::SetQueue(Arc::new(Vec::new())));
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
            let what = if self.playlist.is_some() { "playlist" } else { "library" };
            self.notice = Some(format!(
                "The {} was changed and the song that was loaded is no longer in it \u{2013} the player stopped.",
                what
            ));
        }
    }

    /// Load playlist `name` of the library, stopped on its first song. It
    /// does not start by itself: live, the first song waits for the button,
    /// too.
    pub fn play_playlist(&mut self, name: &str) {
        let Some(songs) = self.library.iter().find(|(n, _)| n == name).map(|(_, s)| s.clone()) else { return };
        let entries = entries_from_playlist(&songs, &self.audio_lib);
        self.load_queue(entries, Some(name.to_string()), songs, 0, false);
    }

    /// "Single song": all songs of the library as the list, stopped on the
    /// first one; Previous/Next go through it.
    pub fn play_library(&mut self) {
        let songs = self.library_songs.clone();
        let entries = entries_from_playlist(&songs, &self.audio_lib);
        self.library_stale = false;
        self.load_queue(entries, None, songs, 0, false);
    }

    /// Load `project` and leave it stopped: within the library list if it
    /// is a song of the library, else as a queue of one song.
    pub fn load_song(&mut self, project: &SongProject) {
        self.load_single(project, false);
    }

    /// Load `project` as `load_song` does and start it. If the audio output
    /// cannot be opened, `error` tells why, and Play tries again.
    pub fn play_song(&mut self, project: &SongProject) {
        self.load_single(project, true);
    }

    fn load_single(&mut self, project: &SongProject, play: bool) {
        let songs = self.library_songs.clone();
        let entries = entries_from_playlist(&songs, &self.audio_lib);
        match find_path(&entries, &project.path, None) {
            Some(at) => {
                self.library_stale = false;
                self.load_queue(entries, None, songs, at, play);
            }
            None => self.load_queue(vec![QueueEntry::single(project.clone())], None, Vec::new(), 0, play),
        }
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

    // -- the stream ------------------------------------------------------------

    /// Make sure a stream with `rate` and the current settings is open.
    /// The old one is closed first (it may hold the device).
    fn open_stream(&mut self, rate: u32) -> bool {
        if self.stream_fits(rate) {
            return true;
        }
        self.player = None;
        match self.device.open(&self.settings, rate) {
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
        self.project(index).map(|p| p.sampling_rate).filter(|r| *r > 0).unwrap_or(48000)
    }

    fn first_readable(&self, from: usize) -> Option<usize> {
        (from..self.entries.len()).find(|&i| self.entries[i].readable())
    }

    /// The queue goes to a stream that was just opened.
    fn send_queue(&self) {
        self.send(Command::SetQueue(self.entries.clone()));
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

    /// Use `entries` as the queue and load its first readable song from
    /// `at` on: playing (`autoplay`) or stopped.
    fn load_queue(
        &mut self,
        entries: Vec<QueueEntry>,
        playlist: Option<String>,
        songs: Vec<PlaylistSong>,
        at: usize,
        autoplay: bool,
    ) {
        self.entries = Arc::new(entries);
        self.playlist = playlist;
        self.loaded_songs = songs;
        self.notice = None;
        self.last_seek = None;
        self.parked = None;
        self.names_for = None;
        let Some(first) = self.first_readable(at) else {
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
            self.send(Command::Goto(first, false));
        } else {
            // no need to hold the audio device before Play
            self.player = None;
            self.parked = Some(self.offline_status(first, 0.0));
        }
    }

    /// Start song `index` (or the next readable one) at its beginning, as
    /// the play button of a song in the list does.
    pub fn play_index(&mut self, index: usize) {
        self.start(index, true, false);
    }

    /// Start the first readable song of the queue (nothing is loaded yet).
    pub fn play_first(&mut self) {
        if let Some(first) = self.first_readable(0) {
            self.start(first, true, true);
        }
    }

    /// Start song `index` (or the next readable one) at its beginning on a
    /// stream of its rate, opening one if needed. `new_queue`: the engine
    /// has to be given the queue even if the stream is kept.
    fn start(&mut self, index: usize, autoplay: bool, new_queue: bool) {
        let Some(i) = self.first_readable(index) else { return };
        if autoplay {
            self.device.open_midi(); // tries the port again if it failed before
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
        self.send(Command::Goto(i, autoplay));
    }

    /// Close the stream but keep the song and its position, so Play opens
    /// the output again (lost device, test tone, new settings).
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
        self.send(Command::Goto(index, false));
        if parked.position_seconds > 0.0 {
            self.send(Command::Seek(parked.position_seconds));
        }
        match parked.state {
            PlayState::Waiting => self.send(Command::Hold(None)),
            PlayState::Countdown => self.send(Command::Hold(Some(parked.countdown_seconds))),
            _ => {}
        }
        if play {
            self.send(Command::Play);
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
        self.send(Command::Goto(index, st.state == PlayState::Playing));
        match st.state {
            PlayState::Waiting => self.send(Command::Hold(None)),
            PlayState::Countdown => self.send(Command::Hold(Some(st.countdown_seconds))),
            _ => {}
        }
    }

    /// End the audio stream and the MIDI thread (closing the program).
    pub fn shutdown(&mut self) {
        self.save_song_mix(true);
        self.player = None;
        self.device.shutdown();
    }

    // -- transport -------------------------------------------------------------

    /// Jump to `seconds` (clamped to the song).
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
            self.play_first();
            return;
        }
        if st.state == PlayState::Playing {
            self.send(Command::Pause);
            return;
        }
        self.device.open_midi();
        // no stream (lost, test tone) or one that does not fit the settings
        // any more: open it again at the same position
        let stale = self.player.as_ref().map(|p| !p.matches(&self.settings, st.song_rate)).unwrap_or(true);
        if stale {
            self.park();
            self.resume(true);
        } else if st.song_rate == st.output_rate {
            self.send(Command::Play);
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

    /// Run `cmd` on the stream, or `offline` on the parked song.
    fn transport(&mut self, cmd: Command, offline: impl FnOnce(&mut Engine)) {
        if let Some(player) = &self.player {
            player.send(cmd);
        } else {
            self.offline(offline);
        }
        self.last_seek = None;
    }

    /// A click on song `index` of the list: the song is marked. While a
    /// song plays it goes on and `index` becomes the next song; otherwise
    /// `index` is loaded (stopped, position 0) so Play starts it.
    pub fn click_song(&mut self, index: usize) {
        self.transport(Command::Select(index), |e| e.select(index));
    }

    /// The mark moves to the next or previous song and is clicked.
    pub fn move_mark(&mut self, forward: bool) {
        self.transport(Command::MoveMark(forward), |e| e.move_mark(forward));
    }

    /// The next song, as the button does.
    pub fn next_song(&mut self) {
        self.transport(Command::Next, |e| e.next());
    }

    /// The previous song (or the start of this one), as the button does.
    pub fn prev_song(&mut self) {
        self.transport(Command::Prev, |e| e.prev());
    }
}

// ---------------------------------------------------------------------------
// the in-memory adapter
// ---------------------------------------------------------------------------

/// An output device in memory: the engine runs on the caller's thread and
/// renders only when `MemoryDevice::render` is called. For tests of the
/// session (and of the user interface around it).
pub mod memory {
    use super::{Device, Output};
    use crate::audio_out::{AudioSettings, Callback, Command, StreamInfo, Trash};
    use crate::midi_out::PortStatus;
    use crate::rehearsal::{Engine, Levels, PlayState, Status};
    use crate::rehearsal_midi::MidiMsg;
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::sync::mpsc::{channel, Receiver, Sender};
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct State {
        /// the callback of the stream opened last (it may be closed)
        current: Option<Rc<RefCell<Callback>>>,
        /// the error of the stream opened last (see `lose`)
        lost: Option<Rc<RefCell<Option<String>>>>,
        /// the next `open` fails with this
        fail: Option<String>,
        /// the rates streams were opened with
        opened: Vec<u32>,
        open_streams: usize,
        midi_port: String,
        midi_opened: usize,
        midi_sent: Vec<MidiMsg>,
        shut_down: bool,
    }

    /// A handle to the device; clones share it, so the test keeps one
    /// while the session owns another.
    #[derive(Clone, Default)]
    pub struct MemoryDevice {
        state: Rc<RefCell<State>>,
    }

    impl MemoryDevice {
        pub fn new() -> MemoryDevice {
            MemoryDevice::default()
        }

        /// Render `frames` of the stream that is open (if any).
        pub fn render(&self, frames: usize) {
            let cb = self.state.borrow().current.clone();
            if let Some(cb) = cb {
                let mut out = vec![0.0f32; frames * 2];
                let mut cb = cb.borrow_mut();
                cb.apply_pending();
                cb.fill(&mut out, 2);
            }
        }

        /// The next `open` fails with `error`; None = it works again.
        pub fn fail_open(&self, error: Option<&str>) {
            self.state.borrow_mut().fail = error.map(str::to_string);
        }

        /// The open stream fails (the device was unplugged).
        pub fn lose(&self, error: &str) {
            if let Some(lost) = &self.state.borrow().lost {
                *lost.borrow_mut() = Some(error.to_string());
            }
        }

        /// The rates of all streams opened so far.
        pub fn opened(&self) -> Vec<u32> {
            self.state.borrow().opened.clone()
        }

        /// Streams that are open now.
        pub fn open_streams(&self) -> usize {
            self.state.borrow().open_streams
        }

        pub fn midi_sent(&self) -> Vec<MidiMsg> {
            self.state.borrow().midi_sent.clone()
        }

        pub fn midi_opened(&self) -> usize {
            self.state.borrow().midi_opened
        }

        pub fn is_shut_down(&self) -> bool {
            self.state.borrow().shut_down
        }
    }

    struct MemoryOutput {
        commands: Sender<Command>,
        callback: Rc<RefCell<Callback>>,
        status: Arc<Mutex<Status>>,
        _trash: Receiver<Trash>,
        lost: Rc<RefCell<Option<String>>>,
        info: StreamInfo,
        settings: AudioSettings,
        device: Rc<RefCell<State>>,
    }

    impl MemoryOutput {
        /// Apply what was sent, so the status shows it (a real stream does
        /// this in its next callback).
        fn settle(&self) {
            self.callback.borrow_mut().apply_pending();
        }
    }

    impl Drop for MemoryOutput {
        fn drop(&mut self) {
            self.device.borrow_mut().open_streams -= 1;
        }
    }

    impl Output for MemoryOutput {
        fn matches(&self, settings: &AudioSettings, rate: u32) -> bool {
            self.info.rate == rate && self.settings == *settings && self.lost.borrow().is_none()
        }
        fn output_rate(&self) -> u32 {
            self.info.rate
        }
        fn info(&self) -> &StreamInfo {
            &self.info
        }
        fn error(&self) -> Option<String> {
            self.lost.borrow().clone()
        }
        fn send(&self, cmd: Command) {
            let _ = self.commands.send(cmd);
        }
        fn status(&self) -> Status {
            self.settle();
            let mut st = self.status.lock().unwrap().clone();
            if st.state == PlayState::Playing && self.error().is_some() {
                st.state = PlayState::Paused;
            }
            st
        }
        fn take_levels(&self) -> Levels {
            let mut st = self.status.lock().unwrap();
            let levels = st.levels;
            st.levels.clear_peaks();
            levels
        }
    }

    impl Device for MemoryDevice {
        fn open(&mut self, settings: &AudioSettings, rate: u32) -> Result<Box<dyn Output>, String> {
            let mut state = self.state.borrow_mut();
            if let Some(e) = state.fail.clone() {
                return Err(e);
            }
            let (commands, rx) = channel();
            let (trash_tx, trash_rx) = channel();
            let status = Arc::new(Mutex::new(Engine::new(rate).status()));
            let callback = Rc::new(RefCell::new(Callback::new(Engine::new(rate), rx, trash_tx, status.clone())));
            let lost = Rc::new(RefCell::new(None));
            state.current = Some(callback.clone());
            state.lost = Some(lost.clone());
            state.opened.push(rate);
            state.open_streams += 1;
            let info = StreamInfo {
                host: "memory".into(),
                device: "memory".into(),
                rate,
                channels: 2,
                buffer: 0,
                note: None,
            };
            Ok(Box::new(MemoryOutput {
                commands,
                callback,
                status,
                _trash: trash_rx,
                lost,
                info,
                settings: settings.clone(),
                device: self.state.clone(),
            }))
        }
        fn set_midi_port(&mut self, port: &str) {
            self.state.borrow_mut().midi_port = port.to_string();
        }
        fn open_midi(&mut self) {
            self.state.borrow_mut().midi_opened += 1;
        }
        fn send_midi(&mut self, msgs: &[MidiMsg]) {
            self.state.borrow_mut().midi_sent.extend_from_slice(msgs);
        }
        fn midi_status(&self) -> PortStatus {
            let port = self.state.borrow().midi_port.clone();
            PortStatus { open: !port.is_empty(), port, error: None }
        }
        fn shutdown(&mut self) {
            self.state.borrow_mut().shut_down = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::memory::MemoryDevice;
    use super::*;
    use crate::mix::has_song_mix;
    use crate::rehearsal::tests::{constant, song, wav24, RATE};
    use crate::tests::temp_dir;
    use crate::{generate_songprj, ProjectTrack};
    use std::path::Path;

    fn session() -> (Session, MemoryDevice) {
        let device = MemoryDevice::new();
        (Session::new(Box::new(device.clone())), device)
    }

    /// A song of `rate` with one constant track, saved as `<name>.songprj`.
    fn song_at(dir: &Path, name: &str, rate: u32, seconds: f64) -> SongProject {
        let wav = wav24(dir, name, rate, 1, &constant((rate as f64 * seconds) as usize, 0.25));
        let audio_lib = dir.to_str().unwrap();
        let xml = generate_songprj(name, 0, rate, 24, 1);
        let mut prj = SongProject::parse(&xml, audio_lib);
        prj.tracks = vec![ProjectTrack::new(0, &wav, audio_lib)];
        let file = dir.join(format!("{}.songprj", name));
        prj.save_as(file.to_str().unwrap()).unwrap();
        SongProject::load(file.to_str().unwrap(), audio_lib).unwrap()
    }

    fn entry(p: &SongProject) -> PlaylistSong {
        PlaylistSong {
            name: p.name.clone(),
            songprj: p.path.clone(),
            mode: crate::rehearsal_queue::WAIT_MODE.to_string(),
            delay: "0".into(),
        }
    }

    fn state(s: &Session) -> PlayState {
        s.current_status().map(|st| st.state).unwrap_or(PlayState::Empty)
    }

    #[test]
    fn loading_a_song_does_not_open_the_device() {
        let dir = temp_dir("session-load");
        let prj = song_at(&dir, "a", RATE, 1.0);
        let (mut s, dev) = session();
        s.load_song(&prj);
        let st = s.poll().expect("a parked status");
        assert_eq!(st.state, PlayState::Stopped);
        assert_eq!(st.index, Some(0));
        assert!(dev.opened().is_empty(), "no stream before Play");
        assert_eq!(s.track_names()[0], "a");
    }

    #[test]
    fn play_opens_a_stream_at_the_song_rate_and_plays() {
        let dir = temp_dir("session-play");
        let prj = song_at(&dir, "a", 44100, 1.0);
        let (mut s, dev) = session();
        s.play_song(&prj);
        dev.render(512);
        let st = s.poll().unwrap();
        assert_eq!(dev.opened(), vec![44100]);
        assert_eq!(st.state, PlayState::Playing);
        assert!(st.position_frames > 0);
        assert!(s.is_playing());
    }

    #[test]
    fn a_device_that_cannot_be_opened_parks_the_song_and_play_tries_again() {
        let dir = temp_dir("session-fail");
        let prj = song_at(&dir, "a", RATE, 1.0);
        let (mut s, dev) = session();
        dev.fail_open(Some("no device"));
        s.play_song(&prj);
        let st = s.poll().unwrap();
        assert_eq!(s.error(), Some("no device"));
        assert_eq!(st.state, PlayState::Stopped);
        dev.fail_open(None);
        s.toggle_play(&st);
        dev.render(256);
        assert_eq!(s.poll().unwrap().state, PlayState::Playing);
        assert_eq!(s.error(), None);
    }

    #[test]
    fn a_lost_device_parks_at_the_position_and_play_resumes_there() {
        let dir = temp_dir("session-lost");
        let prj = song_at(&dir, "a", RATE, 2.0);
        let (mut s, dev) = session();
        s.play_song(&prj);
        for _ in 0..20 {
            dev.render(1024);
        }
        let before = s.poll().unwrap().position_frames;
        assert!(before > 0);
        dev.lose("unplugged");
        let st = s.poll().unwrap();
        assert_eq!(s.error(), Some("unplugged"));
        assert_eq!(st.state, PlayState::Paused, "a parked song does not play");
        assert_eq!(dev.open_streams(), 0, "the stream is closed");
        s.toggle_play(&st);
        let st = s.poll().unwrap();
        assert_eq!(st.state, PlayState::Playing);
        assert_eq!(dev.opened().len(), 2);
        assert!((st.position_seconds - before as f64 / RATE as f64).abs() < 0.01, "it goes on where it stopped");
    }

    #[test]
    fn the_next_song_with_another_rate_reopens_the_stream() {
        let dir = temp_dir("session-rate");
        let a = song_at(&dir, "a", 48000, 1.0);
        let b = song_at(&dir, "b", 44100, 1.0);
        let songs = vec![entry(&a), entry(&b)];
        let (mut s, dev) = session();
        s.set_library(dir.to_str().unwrap(), vec![("Set".into(), songs)]);
        s.play_playlist("Set");
        s.play_first();
        dev.render(256);
        s.next_song();
        dev.render(256);
        let st = s.poll().unwrap();
        assert_eq!(dev.opened(), vec![48000, 44100]);
        assert_eq!(dev.open_streams(), 1);
        assert_eq!(st.index, Some(1));
        assert_eq!(st.output_rate, 44100);
    }

    #[test]
    fn transport_works_on_a_parked_song() {
        let dir = temp_dir("session-offline");
        let a = song_at(&dir, "a", RATE, 2.0);
        let b = song_at(&dir, "b", RATE, 2.0);
        let songs = vec![entry(&a), entry(&b)];
        let (mut s, dev) = session();
        s.set_library(dir.to_str().unwrap(), vec![("Set".into(), songs)]);
        s.play_playlist("Set");
        let st = s.poll().unwrap();
        s.seek(1.0, st.length_seconds);
        assert_eq!(s.poll().unwrap().position_seconds, 1.0);
        s.next_song();
        assert_eq!(s.poll().unwrap().index, Some(1));
        s.stop();
        assert_eq!(s.poll().unwrap().position_seconds, 0.0);
        assert!(dev.opened().is_empty(), "all without a device");
    }

    #[test]
    fn mix_stereo_and_autoplay_go_to_every_new_stream() {
        let dir = temp_dir("session-carry");
        let prj = song_at(&dir, "a", RATE, 1.0);
        let (mut s, dev) = session();
        s.set_autoplay(true);
        s.play_song(&prj);
        dev.render(256);
        assert!(s.poll().unwrap().autoplay);
        dev.lose("gone");
        let st = s.poll().unwrap();
        s.toggle_play(&st);
        assert!(s.poll().unwrap().autoplay, "the new stream has it too");
    }

    #[test]
    fn single_song_lists_the_library_and_next_prev_go_through_it() {
        let dir = temp_dir("session-library");
        let a = song_at(&dir, "a", RATE, 1.0);
        let b = song_at(&dir, "b", RATE, 1.0);
        let c = song_at(&dir, "c", RATE, 1.0);
        let (mut s, dev) = session();
        s.set_library(dir.to_str().unwrap(), Vec::new());
        s.set_library_songs(vec![entry(&a), entry(&b), entry(&c)]);
        assert!(s.has_library_songs());
        s.play_library();
        assert!(s.is_single());
        assert_eq!(s.loaded_playlist(), None);
        assert_eq!(s.rows(s.current_status().as_ref()).len(), 3);
        assert_eq!(s.poll().unwrap().index, Some(0));
        s.next_song();
        assert_eq!(s.poll().unwrap().index, Some(1));
        s.prev_song();
        assert_eq!(s.poll().unwrap().index, Some(0));
        // Home > Play in Rehearsal: the library list, on that song
        s.load_song(&c);
        let st = s.poll().unwrap();
        assert_eq!((st.index, s.rows(Some(&st)).len()), (Some(2), 3));
        s.prev_song();
        assert_eq!(s.poll().unwrap().index, Some(1));
        // another song changed: the loaded one stays
        s.song_changed(&a.path);
        assert!(s.has_queue() && s.notice().is_none());
        s.set_library_songs(vec![entry(&b), entry(&c)]);
        let st = s.poll().unwrap();
        assert_eq!((st.index, s.rows(Some(&st)).len()), (Some(0), 2), "b plays on");
        // the loaded song changed: unloaded
        s.song_changed(&b.path);
        assert!(!s.has_queue());
        assert!(dev.opened().is_empty());
    }

    #[test]
    fn a_deleted_playlist_unloads_with_a_notice() {
        let dir = temp_dir("session-deleted");
        let a = song_at(&dir, "a", RATE, 1.0);
        let (mut s, _dev) = session();
        s.set_library(dir.to_str().unwrap(), vec![("Set".into(), vec![entry(&a)])]);
        s.play_playlist("Set");
        s.playlist_deleted("Set");
        assert!(s.notice().unwrap().contains("Set"));
        assert!(!s.has_queue());
        assert_eq!(state(&s), PlayState::Empty);
    }

    #[test]
    fn the_song_mix_is_saved_into_the_song_and_used_by_the_queue() {
        let dir = temp_dir("session-songmix");
        let w0 = wav24(&dir, "t0", RATE, 1, &constant(4800, 0.25));
        let w2 = wav24(&dir, "t2", RATE, 1, &constant(4800, 0.25));
        let mut prj = song(&dir, &[&w0, &w0, &w2]);
        prj.tracks.remove(1);
        prj.save().unwrap();
        let (mut s, _dev) = session();
        s.load_song(&prj);
        s.edit_song_mix();
        {
            let edit = s.song_mix().expect("song mode edits the loaded song");
            assert_eq!(edit.used[0..3], [true, false, true]);
            edit.strips[2] = SongStrip { level_db: 2.0, mute: false };
        }
        s.song_mix_changed();
        // not yet on disk: it waits for a pause of the user
        assert!(s.tick(), "a save is pending");
        let disk = SongProject::load(&prj.path, "").unwrap();
        assert_eq!(song_strips(&disk)[2], SongStrip::default());
        s.flush_song_mix();
        let disk = SongProject::load(&prj.path, "").unwrap();
        assert_eq!(song_strips(&disk)[2], SongStrip { level_db: 2.0, mute: false });
        assert!(has_song_mix(&disk));
        assert_eq!(s.take_saved_songs(), vec![prj.path.clone()]);
        // the queue has it too: the next time the song is opened, it plays with it
        assert_eq!(song_strips(s.project(0).unwrap())[2].level_db, 2.0);
        assert!(s.rows(s.current_status().as_ref())[0].mixer, "the list shows the symbol");
        s.leave_song_mix();
        assert!(s.song_mix().is_none());
    }

    #[test]
    fn song_mode_ends_when_nothing_is_loaded() {
        let (mut s, _dev) = session();
        s.edit_song_mix();
        assert!(s.song_mix().is_none());
        s.poll();
        assert!(s.song_mix().is_none());
    }

    #[test]
    fn shutdown_saves_the_song_mix_and_closes_midi() {
        let dir = temp_dir("session-shutdown");
        let prj = song_at(&dir, "a", RATE, 1.0);
        let (mut s, dev) = session();
        s.load_song(&prj);
        s.edit_song_mix();
        s.song_mix().unwrap().strips[0].mute = true;
        s.song_mix_changed();
        s.shutdown();
        assert!(song_strips(&SongProject::load(&prj.path, "").unwrap())[0].mute);
        assert!(dev.is_shut_down());
    }
}

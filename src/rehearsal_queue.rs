//! The queue of the rehearsal player: the songs of a playlist (or one single
//! song) as the engine plays them, and the mailbox through which the engine
//! gets songs opened without touching a file in the audio callback.
//!
//! `QueueEntry` holds what the engine needs per song: the loaded
//! `SongProject` or the reason why it could not be loaded, the start mode of
//! the playlist entry and the delay in seconds. Opening the WAV files of a
//! song (`LoadedSong::open`) is done by an `Opener`: the engine posts a
//! request (an index, no allocation) and takes the finished song with
//! `try_lock`; someone else calls `Opener::work` (the adapter runs a thread
//! that does it, the tests let `Engine::wait_ready` do it).

use crate::rehearsal::LoadedSong;
use crate::{PlaylistSong, SongProject};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

/// Start mode that waits for the Play button (also an empty mode).
pub const WAIT_MODE: &str = "wait_for_button_press";

/// One song of the queue.
#[derive(Clone, Debug)]
pub struct QueueEntry {
    /// name to show (the playlist's name of the song)
    pub name: String,
    /// the song, or why it could not be loaded
    pub project: Result<SongProject, String>,
    /// `start_mode` of the playlist entry. Kept as read, but the player no
    /// longer uses it: the autoplay option decides (see `Engine::set_autoplay`)
    pub start_mode: String,
    /// `start_mode_delay` in seconds
    pub delay: f64,
}

/// The delay of a playlist entry in seconds: empty, invalid, negative or
/// not finite gives 0.
pub fn parse_delay(text: &str) -> f64 {
    match text.trim().replace(',', ".").parse::<f64>() {
        Ok(v) if v.is_finite() && v > 0.0 => v,
        _ => 0.0,
    }
}

impl QueueEntry {
    /// An entry from its parts; `delay` is the text of `start_mode_delay`.
    pub fn new(name: &str, project: Result<SongProject, String>, start_mode: &str, delay: &str) -> QueueEntry {
        QueueEntry { name: name.to_string(), project, start_mode: start_mode.to_string(), delay: parse_delay(delay) }
    }

    /// A single song (Home > Play in Rehearsal): waits for the button.
    pub fn single(project: SongProject) -> QueueEntry {
        QueueEntry { name: project.name.clone(), project: Ok(project), start_mode: WAIT_MODE.to_string(), delay: 0.0 }
    }

    /// Load the .songprj of a playlist entry (`audio_lib` replaces
    /// {$AudioLib}); a song that cannot be read becomes an error entry.
    pub fn from_playlist_song(song: &PlaylistSong, audio_lib: &str) -> QueueEntry {
        let project = SongProject::load(&song.songprj, audio_lib).map_err(|e| e.to_string());
        QueueEntry::new(&song.name, project, &song.mode, &song.delay)
    }

    pub fn readable(&self) -> bool {
        self.project.is_ok()
    }

    /// The name of the song project if there is one, else the entry name.
    pub fn display_name(&self) -> &str {
        match &self.project {
            Ok(p) if !p.name.is_empty() => &p.name,
            _ => &self.name,
        }
    }

    /// Path of the .songprj, if it was loaded.
    pub fn path(&self) -> Option<&str> {
        self.project.as_ref().ok().map(|p| p.path.as_str())
    }
}

/// The entry that holds the song file `path`: `prefer` if it does, else
/// the first one.
pub fn find_path(entries: &[QueueEntry], path: &str, prefer: Option<usize>) -> Option<usize> {
    let same = |i: usize| entries.get(i).and_then(|e| e.path()).map(|p| same_path(p, path)).unwrap_or(false);
    match prefer {
        Some(c) if same(c) => Some(c),
        _ => (0..entries.len()).find(|&i| same(i)),
    }
}

/// The queue of a whole playlist.
pub fn entries_from_playlist(songs: &[PlaylistSong], audio_lib: &str) -> Vec<QueueEntry> {
    songs.iter().map(|s| QueueEntry::from_playlist_song(s, audio_lib)).collect()
}

/// Same songs with the same start modes and delays in the same order?
pub fn same_songs(a: &[PlaylistSong], b: &[PlaylistSong]) -> bool {
    a.len() == b.len()
        && a.iter().zip(b).all(|(x, y)| {
            same_path(&x.songprj, &y.songprj) && x.mode == y.mode && x.delay == y.delay && x.name == y.name
        })
}

/// Same file? See [`crate::path_key`].
pub use crate::path_key::same_path;

// ---------------------------------------------------------------------------
// what the song list shows
// ---------------------------------------------------------------------------

/// One line of the song list of the Rehearsal tab.
#[derive(Clone, Debug, PartialEq)]
pub struct SongRow {
    /// 1-based
    pub number: usize,
    pub name: String,
    /// "m:ss", empty for an unreadable song
    pub length: String,
    /// the song has a MIDI file
    pub midi: bool,
    /// the song has its own mixer values (`crate::mix::has_song_mix`)
    pub mixer: bool,
    /// why the song cannot be played
    pub error: Option<String>,
    /// the song that plays right now
    pub current: bool,
    /// the highlighted song: what Play starts (while playing: the clicked
    /// next song, else the playing one)
    pub marked: bool,
    /// the song that follows the playing one
    pub next: bool,
}

/// The rows for `entries`. `index`, `next_index` and `marked` come from the
/// status (`index`, `next_index`, `marked_index`). Only while `playing`
/// (state `Playing`) a song is current and one is the next one; otherwise
/// the highlight (`marked`) alone shows the song that Play starts, also
/// while the next song waits (`Waiting`, `Countdown`).
pub fn song_rows(
    entries: &[QueueEntry],
    index: Option<usize>,
    next_index: Option<usize>,
    marked: Option<usize>,
    playing: bool,
) -> Vec<SongRow> {
    let (current, next) = if playing { (index, next_index) } else { (None, None) };
    entries
        .iter()
        .enumerate()
        .map(|(i, e)| SongRow {
            number: i + 1,
            name: e.display_name().to_string(),
            length: match &e.project {
                Ok(p) => crate::rehearsal::fmt_clock(p.duration()),
                Err(_) => String::new(),
            },
            midi: e.project.as_ref().map(|p| !p.midi_file.is_empty()).unwrap_or(false),
            mixer: e.project.as_ref().map(crate::mix::has_song_mix).unwrap_or(false),
            error: e.project.as_ref().err().cloned(),
            current: current == Some(i),
            marked: marked == Some(i),
            next: next == Some(i),
        })
        .collect()
}

/// The big line while the player waits: "Next: <Song> - press Play" or
/// "Next: <Song> in <n> s" (the countdown rounded up). None otherwise.
pub fn next_caption(state: crate::rehearsal::PlayState, song_name: &str, countdown_seconds: f64) -> Option<String> {
    use crate::rehearsal::PlayState;
    match state {
        PlayState::Waiting => Some(tr!("queue.next_wait", name = song_name)),
        PlayState::Countdown => Some(tr!("queue.next_in", name = song_name, s = countdown_seconds.max(0.0).ceil() as u64)),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// the mailbox
// ---------------------------------------------------------------------------

pub(crate) struct Request {
    pub ticket: u64,
    pub queue: Arc<Vec<QueueEntry>>,
    pub index: usize,
}

pub(crate) struct Done {
    pub ticket: u64,
    pub index: usize,
    pub song: LoadedSong,
}

/// Opens the songs the engine asks for. One request at a time; a newer one
/// replaces an older one that has not been started. The engine only ever
/// uses `try_lock`, so it never blocks on this.
#[derive(Default)]
pub struct Opener {
    request: Mutex<Option<Request>>,
    done: Mutex<Option<Done>>,
}

impl Opener {
    /// The engine posts a request; false if the mailbox was busy (try again).
    pub(crate) fn post(&self, request: Request) -> Result<(), Request> {
        match self.request.try_lock() {
            Ok(mut slot) => {
                *slot = Some(request);
                Ok(())
            }
            Err(_) => Err(request),
        }
    }

    /// The finished song, if there is one and the mailbox is not busy.
    pub(crate) fn take(&self) -> Option<Done> {
        self.done.try_lock().ok().and_then(|mut d| d.take())
    }

    /// Whether a request waits to be worked on.
    pub fn has_work(&self) -> bool {
        self.request.lock().map(|r| r.is_some()).unwrap_or(false)
    }

    /// Open the requested song (reads files, so never call it from the
    /// audio callback). Returns whether there was something to do.
    pub fn work(&self) -> bool {
        let request = match self.request.lock() {
            Ok(mut r) => r.take(),
            Err(p) => p.into_inner().take(),
        };
        let Some(request) = request else { return false };
        if let Some(Ok(project)) = request.queue.get(request.index).map(|e| e.project.as_ref()) {
            let song = LoadedSong::open(project);
            let done = Done { ticket: request.ticket, index: request.index, song };
            let mut slot = match self.done.lock() {
                Ok(s) => s,
                Err(p) => p.into_inner(),
            };
            // an older result nobody took is dropped here, on this thread
            *slot = Some(done);
        }
        true
    }
}

/// A thread that calls `Opener::work` until it is dropped.
pub struct OpenerThread {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl OpenerThread {
    pub fn spawn(opener: Arc<Opener>) -> OpenerThread {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let handle = std::thread::Builder::new()
            .name("rehearsal-opener".into())
            .spawn(move || {
                while !flag.load(Ordering::Acquire) {
                    if !opener.work() {
                        std::thread::sleep(Duration::from_millis(3));
                    }
                }
            })
            .ok();
        OpenerThread { stop, handle }
    }
}

impl Drop for OpenerThread {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delay_text_to_seconds() {
        assert_eq!(parse_delay("1.5"), 1.5);
        assert_eq!(parse_delay(" 2 "), 2.0);
        assert_eq!(parse_delay("1,5"), 1.5);
        assert_eq!(parse_delay(""), 0.0);
        assert_eq!(parse_delay("abc"), 0.0);
        assert_eq!(parse_delay("-3"), 0.0);
        assert_eq!(parse_delay("NaN"), 0.0);
        assert_eq!(parse_delay("inf"), 0.0);
    }

    fn project(name: &str, secs: f64, midi: &str) -> SongProject {
        let mut p = SongProject::parse("", "");
        p.name = name.to_string();
        p.duration_seconds = secs;
        p.midi_file = midi.to_string();
        p
    }

    #[test]
    fn the_rows_of_the_song_list() {
        let entries = vec![
            QueueEntry::new("a", Ok(project("A", 185.4, "x.mid")), "", ""),
            QueueEntry::new("b", Err("no such file".into()), "", ""),
            QueueEntry::new("c", Ok(project("C", 59.0, "")), "", ""),
        ];
        let rows = song_rows(&entries, Some(0), Some(2), Some(0), true);
        assert_eq!(rows.len(), 3);
        assert_eq!((rows[0].number, rows[0].name.as_str(), rows[0].length.as_str(), rows[0].midi), (1, "A", "3:05", true));
        assert!(rows[0].current && rows[0].marked && !rows[0].next && rows[0].error.is_none());
        assert_eq!(rows[1].error.as_deref(), Some("no such file"));
        assert_eq!((rows[1].name.as_str(), rows[1].length.as_str(), rows[1].midi), ("b", "", false));
        assert!(!rows[2].current && !rows[2].marked && rows[2].next);
        assert_eq!((rows[2].length.as_str(), rows[2].midi), ("0:59", false));
        // a click while playing: the clicked song is marked and comes next
        let rows = song_rows(&entries, Some(0), Some(2), Some(2), true);
        assert!(rows[0].current && !rows[0].marked && rows[2].marked && rows[2].next);
        assert_eq!(rows.iter().filter(|r| r.marked).count(), 1);
        // not playing (stopped, waiting): only the marked song shows
        let rows = song_rows(&entries, Some(2), None, Some(2), false);
        assert!(rows.iter().all(|r| !r.current && !r.next));
        assert_eq!(rows.iter().filter(|r| r.marked).count(), 1);
        assert!(rows[2].marked);
    }

    #[test]
    fn the_caption_while_waiting() {
        use crate::rehearsal::PlayState::*;
        assert_eq!(next_caption(Waiting, "Song B", 0.0).as_deref(), Some("Next: Song B \u{2013} press Play"));
        assert_eq!(next_caption(Countdown, "Song B", 2.01).as_deref(), Some("Next: Song B in 3 s"));
        assert_eq!(next_caption(Countdown, "Song B", 0.0).as_deref(), Some("Next: Song B in 0 s"));
        assert_eq!(next_caption(Playing, "Song B", 1.0), None);
        assert_eq!(next_caption(Stopped, "Song B", 1.0), None);
    }

    #[test]
    fn same_paths_and_songs() {
        assert!(same_path("C:/A/b.songprj", "c:\\a\\B.songprj"));
        assert!(!same_path("C:/A/b.songprj", "c:\\a\\c.songprj"));
        let s = |p: &str, d: &str| PlaylistSong { name: "n".into(), songprj: p.into(), mode: "m".into(), delay: d.into() };
        assert!(same_songs(&[s("A", "0")], &[s("a", "0")]));
        assert!(!same_songs(&[s("A", "0")], &[s("a", "1")]));
        assert!(!same_songs(&[s("A", "0")], &[]));
    }
}

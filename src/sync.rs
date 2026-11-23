//! Stick Sync: bring songs and playlists of the project library onto an
//! LP-16 stick (or a copy of it) the way uTool2 does. The CLI and the GUI
//! use the same three steps:
//!
//! - [`plan`]  – what would an export do? (per song / playlist: action,
//!   reason, bytes to copy; read-only)
//! - [`run`]   – do it: songs first, then the .play files, each verified
//! - [`SyncPlan::is_up_to_date`] – the read-only check behind `verify`
//!
//! A song project that cannot be read is never exported as an empty song:
//! it ends up in [`SyncPlan::problems`] and [`run`] refuses the whole plan.

use crate::{
    export_plan, generate_play, now_ms, plain_tag, plan_song, write_bytes_atomic,
    PlaylistSong, SongPlan, SongProject, STICK_WAV_HEADER_LEN,
};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

/// `<root>\Recording`: the LP-16's multitrack songs and playlists
pub const RECORDING: &str = "Recording";
/// the playlist folder inside `Recording` (and inside `Music`)
pub const PLAYLISTS: &str = "#PLAYLISTS";
/// `<root>\Music`: the LP-16's stereo files (not written by this tool)
pub const MUSIC: &str = "Music";

/// Folder layout of a stick: `<root>\Recording\<Song>\…` and
/// `<root>\Recording\#PLAYLISTS\<Playlist>.play`.
#[derive(Clone, Debug)]
pub struct Stick {
    root: PathBuf,
}

impl Stick {
    pub fn new(root: &str) -> Self {
        Stick { root: PathBuf::from(root) }
    }
    pub fn root(&self) -> &Path {
        &self.root
    }
    /// `<root>\Recording`: one folder per song, plus `#PLAYLISTS`
    pub fn recording(&self) -> PathBuf {
        self.root.join(RECORDING)
    }
    pub fn song_dir(&self, name: &str) -> PathBuf {
        self.recording().join(name)
    }
    pub fn playlists_dir(&self) -> PathBuf {
        self.recording().join(PLAYLISTS)
    }
    pub fn play_path(&self, playlist: &str) -> PathBuf {
        self.playlists_dir().join(format!("{}.play", playlist))
    }

    /// Song folder names in `Recording` (without `#PLAYLISTS`), sorted.
    pub fn songs(&self) -> Vec<String> {
        let mut v: Vec<String> = fs::read_dir(self.recording())
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .filter(|e| e.path().is_dir())
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .filter(|n| n != PLAYLISTS)
                    .collect()
            })
            .unwrap_or_default();
        v.sort_by_key(|n| n.to_lowercase());
        v
    }

    /// Playlist names (the .play files without extension), sorted.
    pub fn playlists(&self) -> Vec<String> {
        let mut v: Vec<String> = fs::read_dir(self.playlists_dir())
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .map(|e| e.path())
                    .filter(|p| p.is_file() && p.extension().map(|x| x.eq_ignore_ascii_case("play")).unwrap_or(false))
                    .filter_map(|p| p.file_stem().map(|s| s.to_string_lossy().into_owned()))
                    .collect()
            })
            .unwrap_or_default();
        v.sort_by_key(|n| n.to_lowercase());
        v
    }

    /// Song names listed in a .play on the stick (in order).
    pub fn playlist_songs(&self, playlist: &str) -> Vec<String> {
        let text = fs::read_to_string(self.play_path(playlist)).unwrap_or_default();
        play_song_names(&text)
    }

    /// Playlists on the stick that list `song` (case-insensitive, like FAT).
    pub fn playlists_with(&self, song: &str) -> Vec<String> {
        let key = song.to_lowercase();
        self.playlists()
            .into_iter()
            .filter(|p| self.playlist_songs(p).iter().any(|n| n.to_lowercase() == key))
            .collect()
    }

    /// Song folders that no playlist on the stick lists. A playlist that
    /// cannot be read counts as "lists nothing", so if any .play is
    /// unreadable this returns an error instead of a list to delete.
    pub fn songs_not_in_playlists(&self) -> Result<Vec<String>, String> {
        let mut used: std::collections::HashSet<String> = std::collections::HashSet::new();
        for p in self.playlists() {
            let text = fs::read_to_string(self.play_path(&p)).map_err(|e| format!("{}.play: {}", p, e))?;
            used.extend(play_song_names(&text).into_iter().map(|n| n.to_lowercase()));
        }
        Ok(self.songs().into_iter().filter(|s| !used.contains(&s.to_lowercase())).collect())
    }

    /// The actual name of the song folder `name` on the stick if it differs
    /// only in upper / lower case (None: same spelling or no such folder).
    pub fn song_case_differs(&self, name: &str) -> Option<String> {
        let key = name.to_lowercase();
        self.songs().into_iter().find(|n| n != name && n.to_lowercase() == key)
    }

    /// Give the song folder `name` exactly this spelling (it may exist as
    /// "abc" for "ABC"; see [`crate::fix_name_case`]).
    pub fn fix_song_case(&self, name: &str) -> Result<(), String> {
        check_entry_name(name)?;
        let dir = self.song_dir(name);
        crate::fix_name_case(&dir).map(|_| ()).map_err(|e| format!("rename to {}: {}", dir.display(), e))
    }

    /// Delete one song folder from the stick.
    pub fn delete_song(&self, name: &str) -> Result<(), String> {
        check_entry_name(name)?;
        if name.eq_ignore_ascii_case(PLAYLISTS) {
            return Err(format!("{} is not a song", PLAYLISTS));
        }
        let dir = self.song_dir(name);
        if !dir.is_dir() {
            return Err(format!("Song \"{}\" not found on the stick", name));
        }
        fs::remove_dir_all(&dir).map_err(|e| format!("{}: {}", dir.display(), e))
    }

    /// Delete one .play file from the stick (the songs stay).
    pub fn delete_playlist(&self, name: &str) -> Result<(), String> {
        check_entry_name(name)?;
        let path = self.play_path(name);
        if !path.is_file() {
            return Err(format!("Playlist \"{}\" not found on the stick", name));
        }
        fs::remove_file(&path).map_err(|e| format!("{}: {}", path.display(), e))
    }

    /// `<root>\Music` exists (the stick has the LP-16's stereo folder).
    pub fn has_music(&self) -> bool {
        self.root.join(MUSIC).is_dir()
    }

    /// Bytes of the files in a song folder on the stick.
    pub fn song_size(&self, name: &str) -> u64 {
        dir_size(&self.song_dir(name))
    }

    /// What the song.set and the WAVs of a song folder on the stick say.
    pub fn song_info(&self, name: &str) -> StickSong {
        let dir = self.song_dir(name);
        let set_path = dir.join("song.set");
        let set = fs::read_to_string(&set_path).unwrap_or_default();
        let tag = |t: &str| plain_tag(&set, t).unwrap_or_default();
        let sample_rate: u32 = tag("sampling_rate").parse().unwrap_or(0);
        let bits: u32 = tag("resolution").parse().unwrap_or(0);
        let midi_length: f64 = tag("midi_length").parse().unwrap_or(0.0);
        // the length of the largest WAV payload
        let max_wav = fs::read_dir(&dir)
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .filter(|e| e.path().extension().map(|x| x.eq_ignore_ascii_case("wav")).unwrap_or(false))
                    .filter_map(|e| e.metadata().ok())
                    .map(|m| m.len())
                    .max()
                    .unwrap_or(0)
            })
            .unwrap_or(0);
        let wav_length = if sample_rate > 0 && bits > 0 {
            max_wav.saturating_sub(STICK_WAV_HEADER_LEN) as f64 / (sample_rate as f64 * bits as f64 / 8.0)
        } else {
            0.0
        };
        StickSong {
            sample_rate,
            bits,
            channels: tag("channel_count"),
            midi_file: tag("midi_file"),
            length: wav_length.max(midi_length),
            size: dir_size(&dir),
            modified: crate::mtime_ms(&set_path.to_string_lossy()),
        }
    }

    /// What a .play on the stick says, with the size of its song folders.
    pub fn playlist_info(&self, name: &str) -> StickPlaylist {
        let path = self.play_path(name);
        let text = fs::read_to_string(&path).unwrap_or_default();
        let entries = play_entries(&text);
        StickPlaylist {
            starts: play_starts(&text),
            length: entries.iter().map(|(_, d)| d).sum(),
            size: entries.iter().map(|(n, _)| self.song_size(n)).sum(),
            songs: entries.into_iter().map(|(n, _)| n).collect(),
            modified: crate::mtime_ms(&path.to_string_lossy()),
        }
    }

    /// The Export preview: the songs and playlists on the stick merged with
    /// the ones `plan` would write, sorted; the action is None for the
    /// entries the export does not touch.
    pub fn preview(&self, plan: &SyncPlan) -> StickPreview {
        fn merge(mut on_stick: Vec<String>, planned: Vec<(String, Action)>) -> Vec<(String, Option<Action>)> {
            let actions: HashMap<String, Action> = planned.into_iter().collect();
            for n in actions.keys() {
                if !on_stick.contains(n) {
                    on_stick.push(n.clone());
                }
            }
            on_stick.sort_by_key(|n| n.to_lowercase());
            on_stick.into_iter().map(|n| { let a = actions.get(&n).copied(); (n, a) }).collect()
        }
        StickPreview {
            songs: merge(self.songs(), plan.songs.iter().map(|s| (s.plan.name.clone(), s.action)).collect()),
            playlists: merge(self.playlists(), plan.playlists.iter().map(|p| (p.name.clone(), p.action)).collect()),
        }
    }
}

/// A song folder on the stick, as [`Stick::song_info`] reads it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StickSong {
    pub sample_rate: u32,
    pub bits: u32,
    /// as written in song.set
    pub channels: String,
    pub midi_file: String,
    /// seconds: the longer of the largest WAV and the MIDI file
    pub length: f64,
    pub size: u64,
    /// ms since 1970 of the song.set (0 = missing)
    pub modified: i64,
}

/// A .play on the stick, as [`Stick::playlist_info`] reads it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StickPlaylist {
    /// song names in order
    pub songs: Vec<String>,
    /// start mode and delay (`pause_mode/mode`, `pause_mode/time`) of each song
    pub starts: Vec<(String, String)>,
    /// seconds, the sum of the durations in the .play
    pub length: f64,
    /// bytes of the song folders it lists (missing ones count 0)
    pub size: u64,
    /// ms since 1970 of the .play (0 = missing)
    pub modified: i64,
}

/// The Export preview, see [`Stick::preview`].
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StickPreview {
    pub songs: Vec<(String, Option<Action>)>,
    pub playlists: Vec<(String, Option<Action>)>,
}

fn dir_size(dir: &Path) -> u64 {
    fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter_map(|e| e.metadata().ok())
                .filter(|m| m.is_file())
                .map(|m| m.len())
                .sum()
        })
        .unwrap_or(0)
}

/// Name and duration (seconds) of every <song> of a .play text.
pub fn play_entries(text: &str) -> Vec<(String, f64)> {
    text.split("<song>")
        .skip(1)
        .map(|block| {
            let block = block.split("</song>").next().unwrap_or("");
            let name = plain_tag(block, "name").unwrap_or_default();
            let duration = plain_tag(block, "duration").and_then(|d| d.trim().parse().ok()).unwrap_or(0.0);
            (name, duration)
        })
        .collect()
}

/// The start mode and delay (`pause_mode/mode`, `pause_mode/time`) of
/// each <song> of a .play text.
pub fn play_starts(text: &str) -> Vec<(String, String)> {
    text.split("<song>")
        .skip(1)
        .map(|block| {
            let block = block.split("</song>").next().unwrap_or("");
            (plain_tag(block, "mode").unwrap_or_default(), plain_tag(block, "time").unwrap_or_default())
        })
        .collect()
}

/// The song names (<name> of each <song>) of a .play text.
pub fn play_song_names(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(p) = rest.find("<name>") {
        let s = p + "<name>".len();
        let Some(e) = rest[s..].find("</name>") else { break };
        out.push(crate::xml_unescape(&rest[s..s + e]));
        rest = &rest[s + e..];
    }
    out
}

/// A song or playlist name must be one plain path component.
fn check_entry_name(name: &str) -> Result<(), String> {
    let bad = name.trim().is_empty()
        || name == "."
        || name == ".."
        || name.contains(['/', '\\', ':'])
        || Path::new(name).components().count() != 1;
    if bad {
        Err(format!("invalid name \"{}\"", name))
    } else {
        Ok(())
    }
}

/// uTool2's two export modes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExportMode {
    /// only new and changed songs are written
    Quick,
    /// every selected song is written again
    Force,
}

/// What the export does with one song or playlist (uTool2's marks in
/// "Target Content").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// up to date, nothing written
    Skip,
    /// not on the stick yet
    New,
    /// on the stick, written because it changed ("Quick Export")
    Quick,
    /// on the stick, written again ("Force Overwrite")
    Force,
}

pub struct PlannedSong {
    pub plan: SongPlan,
    pub action: Action,
    /// The song folder on the stick is spelled differently (only in upper /
    /// lower case, e.g. "Abcdefu_…" for "ABCDEFU_…"). FAT does not care, but
    /// the .play files name the song exactly, so run() renames the folder.
    pub case_from: Option<String>,
}

impl PlannedSong {
    /// Bytes shown as "To Copy" (like uTool2: a Quick Export counts only the
    /// changed files).
    pub fn bytes_to_copy(&self) -> u64 {
        match self.action {
            Action::Skip => 0,
            Action::Quick => self.plan.bytes_changed,
            Action::New | Action::Force => self.plan.bytes_full,
        }
    }
}

pub struct PlannedPlaylist {
    pub name: String,
    pub songs: Vec<PlaylistSong>,
    /// New or, if the .play exists, Quick / Force. A selected playlist is
    /// always written (uTool2 does the same).
    pub action: Action,
    /// the .play on the stick has the same content (its own timestamp aside)
    pub up_to_date: bool,
    /// "up to date", "missing" or "content differs"
    pub reason: String,
}

pub struct SyncPlan {
    pub stick: Stick,
    pub mode: ExportMode,
    /// every song once: the given songs first, then those of the playlists
    pub songs: Vec<PlannedSong>,
    pub playlists: Vec<PlannedPlaylist>,
    /// song projects that cannot be read; run() refuses while there are any
    pub problems: Vec<String>,
    /// song length per .songprj (lower-case path), for the .play files
    durations: HashMap<String, f64>,
}

impl SyncPlan {
    /// Read-only check: every song and every .play on the stick is current.
    pub fn is_up_to_date(&self) -> bool {
        self.problems.is_empty()
            && self.songs.iter().all(|s| !s.plan.needs_export && s.case_from.is_none())
            && self.playlists.iter().all(|p| p.up_to_date)
    }

    /// Sum of the songs' "To Copy" bytes.
    pub fn bytes_to_copy(&self) -> u64 {
        self.songs.iter().map(|s| s.bytes_to_copy()).sum()
    }

    /// Number of progress steps of run(): one per song and per playlist.
    pub fn jobs(&self) -> usize {
        self.songs.len() + self.playlists.len()
    }

    fn play_text(&self, pl: &PlannedPlaylist, timestamp_ms: i64) -> String {
        generate_play(&pl.name, timestamp_ms, &pl.songs, |s| {
            self.durations.get(&key(&s.songprj)).copied().unwrap_or(0.0)
        })
    }
}

fn key(path: &str) -> String {
    crate::path_key::path_key(path)
}

/// Plan an export of `songs` (.songprj paths) and `playlists` (name and
/// songs) to the stick at `stick_root`. Only reads.
pub fn plan(
    stick_root: &str,
    audio_lib: &str,
    songs: &[String],
    playlists: &[(String, Vec<PlaylistSong>)],
    mode: ExportMode,
) -> SyncPlan {
    let stick = Stick::new(stick_root);
    let rec = stick.recording().to_string_lossy().into_owned();
    let mut planned = Vec::new();
    let mut problems = Vec::new();
    let mut durations = HashMap::new();
    let mut seen = std::collections::HashSet::new();
    let all = songs
        .iter()
        .chain(playlists.iter().flat_map(|(_, s)| s.iter().map(|s| &s.songprj)));
    for prj in all {
        if !seen.insert(key(prj)) {
            continue;
        }
        match SongProject::load(prj, audio_lib) {
            Err(e) => problems.push(e.to_string()),
            Ok(project) => {
                durations.insert(key(prj), project.duration());
                let mut plan = plan_song(&project, &rec);
                let case_from = stick.song_case_differs(&plan.name);
                let action = if !stick.song_dir(&plan.name).is_dir() {
                    Action::New
                } else if mode == ExportMode::Force {
                    Action::Force
                } else if plan.needs_export || case_from.is_some() {
                    Action::Quick
                } else {
                    Action::Skip
                };
                if let Some(old) = &case_from {
                    let note = format!("folder on the stick is spelled \"{}\"", old);
                    plan.reason = if plan.needs_export { format!("{}; {}", plan.reason, note) } else { note };
                }
                planned.push(PlannedSong { plan, action, case_from });
            }
        }
    }
    let mut sync = SyncPlan {
        stick,
        mode,
        songs: planned,
        playlists: Vec::new(),
        problems,
        durations,
    };
    for (name, songs) in playlists {
        let path = sync.stick.play_path(name);
        let mut pl = PlannedPlaylist {
            name: name.clone(),
            songs: songs.clone(),
            action: Action::New,
            up_to_date: false,
            reason: "missing".into(),
        };
        if let Ok(on_stick) = fs::read_to_string(&path) {
            pl.action = if mode == ExportMode::Force { Action::Force } else { Action::Quick };
            // a different timestamp alone is not a content change
            let ts = plain_tag(&on_stick, "timestamp")
                .and_then(|t| t.parse::<i64>().ok())
                .unwrap_or(0);
            let readable = songs.iter().all(|s| sync.durations.contains_key(&key(&s.songprj)));
            let case = crate::name_case_differs(&path);
            pl.up_to_date = readable && case.is_none() && sync.play_text(&pl, ts) == on_stick;
            pl.reason = match case {
                Some(old) => format!("file on the stick is spelled \"{}\"", old),
                None if pl.up_to_date => "up to date".into(),
                None => "content differs".into(),
            };
        }
        sync.playlists.push(pl);
    }
    sync
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Song,
    Playlist,
}

/// Result of one step of run().
#[derive(Clone, Debug)]
pub struct SyncLine {
    pub kind: Kind,
    pub name: String,
    pub ok: bool,
    /// "OK, verified, 812 ms", "OK, up to date" or "ERROR: …"
    pub message: String,
}

pub enum Progress<'a> {
    /// a step starts (song or playlist name)
    Start(&'a str),
    /// a step is done
    Done(&'a SyncLine),
}

/// Run a plan: songs (Skip ones are only reported), then every playlist's
/// .play. Every file is written atomically and checked afterwards.
/// `progress` returning false stops after the current step (cancel).
/// Err: song projects are unreadable; then nothing is written.
pub fn run(plan: &SyncPlan, mut progress: impl FnMut(Progress) -> bool) -> Result<Vec<SyncLine>, String> {
    if !plan.problems.is_empty() {
        return Err(format!(
            "{} song project(s) cannot be read, nothing exported:\n{}",
            plan.problems.len(),
            plan.problems.join("\n")
        ));
    }
    let rec = plan.stick.recording().to_string_lossy().into_owned();
    let mut lines = Vec::new();
    let steps = plan
        .songs
        .iter()
        .map(|s| (Kind::Song, s.plan.name.as_str(), Some(s), None))
        .chain(plan.playlists.iter().map(|p| (Kind::Playlist, p.name.as_str(), None, Some(p))));
    for (kind, name, song, playlist) in steps {
        if !progress(Progress::Start(name)) {
            break;
        }
        let t0 = Instant::now();
        let verified = || format!("OK, verified, {} ms", t0.elapsed().as_millis());
        let result = match (song, playlist) {
            (Some(s), _) if s.action == Action::Skip => Ok("OK, up to date".to_string()),
            (Some(s), _) => {
                let renamed = match &s.case_from {
                    Some(_) => plan.stick.fix_song_case(&s.plan.name).map(|_| true),
                    None => Ok(false),
                };
                match renamed {
                    Err(e) => Err(e),
                    // only the spelling was wrong: nothing else to write
                    Ok(true) if !s.plan.needs_export && s.action == Action::Quick => {
                        Ok(format!("OK, folder renamed from \"{}\"", s.case_from.as_deref().unwrap_or("")))
                    }
                    Ok(_) => export_plan(&s.plan, &rec).map(|_| verified()).map_err(|e| e.to_string()),
                }
            }
            (_, Some(p)) => write_play(&plan.stick.play_path(&p.name), &plan.play_text(p, now_ms()))
                .map(|_| verified())
                .map_err(|e| e.to_string()),
            _ => unreachable!(),
        };
        let line = SyncLine {
            kind,
            name: name.to_string(),
            ok: result.is_ok(),
            message: result.unwrap_or_else(|e| format!("ERROR: {}", e)),
        };
        let go_on = progress(Progress::Done(&line));
        lines.push(line);
        if !go_on {
            break;
        }
    }
    Ok(lines)
}

/// Write a .play atomically and read it back.
fn write_play(dest: &Path, text: &str) -> std::io::Result<()> {
    if let Some(dir) = dest.parent() {
        fs::create_dir_all(dir)?;
    }
    write_bytes_atomic(dest, text.as_bytes())?;
    if fs::read(dest)? != text.as_bytes() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("verification failed: {} differs", dest.display()),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{mini_song, temp_dir};

    /// A library with one song "Rock & Roll" and a playlist "Set" with it,
    /// and an empty stick folder. Returns (root, audio_lib, songprj, stick).
    fn setup(name: &str) -> (PathBuf, String, String, String) {
        let root = temp_dir(name);
        let (xml, audio) = mini_song(&root, "Rock & Roll");
        let prj = root.join("Rock & Roll.songprj");
        fs::write(&prj, xml).unwrap();
        let stick = root.join("STICK");
        fs::create_dir_all(&stick).unwrap();
        (root, audio, prj.to_string_lossy().into_owned(), stick.to_string_lossy().into_owned())
    }

    fn playlist(prj: &str) -> Vec<(String, Vec<PlaylistSong>)> {
        let song = PlaylistSong {
            name: "Rock & Roll".into(),
            songprj: prj.to_string(),
            mode: "wait_for_button_press".into(),
            delay: "0".into(),
        };
        vec![("Set".to_string(), vec![song])]
    }

    #[test]
    fn export_then_up_to_date_then_force() {
        let (root, audio, prj, stick) = setup("sync-roundtrip");
        // the song comes in through the playlist only, and twice: planned once
        let pls = playlist(&prj);
        let p = plan(&stick, &audio, &[prj.to_uppercase()], &pls, ExportMode::Quick);
        assert!(p.problems.is_empty(), "{:?}", p.problems);
        assert_eq!(p.songs.len(), 1);
        assert_eq!(p.songs[0].action, Action::New);
        assert_eq!(p.playlists[0].action, Action::New);
        assert!(!p.is_up_to_date());
        assert_eq!(p.jobs(), 2);
        assert_eq!(p.bytes_to_copy(), p.songs[0].plan.bytes_full);

        let mut seen = Vec::new();
        let lines = run(&p, |e| {
            if let Progress::Start(n) = e {
                seen.push(n.to_string());
            }
            true
        })
        .unwrap();
        assert_eq!(seen, ["Rock & Roll", "Set"]);
        assert!(lines.iter().all(|l| l.ok), "{:?}", lines);
        assert_eq!(lines[1].kind, Kind::Playlist);
        let stick_dirs = Stick::new(&stick);
        assert!(stick_dirs.song_dir("Rock & Roll").join("song.set").is_file());
        assert!(stick_dirs.play_path("Set").is_file());

        // a new .play timestamp alone does not count as a change
        let again = plan(&stick, &audio, &[], &pls, ExportMode::Quick);
        assert!(again.is_up_to_date());
        assert_eq!(again.songs[0].action, Action::Skip);
        assert_eq!(again.bytes_to_copy(), 0);
        assert_eq!((again.playlists[0].action, again.playlists[0].reason.as_str()), (Action::Quick, "up to date"));
        let lines = run(&again, |_| true).unwrap();
        assert_eq!(lines[0].message, "OK, up to date");

        let force = plan(&stick, &audio, &[], &pls, ExportMode::Force);
        assert_eq!(force.songs[0].action, Action::Force);
        assert_eq!(force.playlists[0].action, Action::Force);
        assert_eq!(force.bytes_to_copy(), force.songs[0].plan.bytes_full);

        // a changed .play on the stick is found by the check
        fs::write(stick_dirs.play_path("Set"), "x").unwrap();
        let changed = plan(&stick, &audio, &[], &pls, ExportMode::Quick);
        assert_eq!(changed.playlists[0].reason, "content differs");
        assert!(!changed.is_up_to_date());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn unreadable_song_project_blocks_the_export() {
        let (root, audio, prj, stick) = setup("sync-missing");
        let gone = root.join("Gone.songprj").to_string_lossy().into_owned();
        let mut pls = playlist(&prj);
        let missing = PlaylistSong { songprj: gone.clone(), ..pls[0].1[0].clone() };
        pls[0].1.push(missing);
        let p = plan(&stick, &audio, &[], &pls, ExportMode::Quick);
        // no phantom song "song" for the missing project
        assert_eq!(p.songs.len(), 1);
        assert_eq!(p.problems.len(), 1);
        assert!(p.problems[0].contains("Gone.songprj"), "{:?}", p.problems);
        assert!(!p.playlists[0].up_to_date);
        assert!(!p.is_up_to_date());
        let err = run(&p, |_| true).unwrap_err();
        assert!(err.contains("Gone.songprj"), "{}", err);
        assert!(!Stick::new(&stick).recording().exists(), "nothing may be written");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn cancel_writes_nothing() {
        let (root, audio, prj, stick) = setup("sync-cancel");
        let p = plan(&stick, &audio, &[prj], &[], ExportMode::Quick);
        let lines = run(&p, |e| !matches!(e, Progress::Start(_))).unwrap();
        assert!(lines.is_empty());
        assert!(!Stick::new(&stick).recording().exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn folder_spelled_differently_is_renamed() {
        let (root, audio, prj, stick) = setup("sync-case");
        let pls = playlist(&prj);
        let p = plan(&stick, &audio, &[], &pls, ExportMode::Quick);
        assert!(run(&p, |_| true).unwrap().iter().all(|l| l.ok));
        // an older export left the folder in another spelling
        let s = Stick::new(&stick);
        fs::rename(s.song_dir("Rock & Roll"), s.song_dir("tmp")).unwrap();
        fs::rename(s.song_dir("tmp"), s.song_dir("ROCK & roll")).unwrap();

        let p = plan(&stick, &audio, &[], &pls, ExportMode::Quick);
        assert_eq!(p.songs[0].case_from.as_deref(), Some("ROCK & roll"));
        assert_eq!(p.songs[0].action, Action::Quick);
        assert!(!p.is_up_to_date());
        let lines = run(&p, |_| true).unwrap();
        assert!(lines[0].ok && lines[0].message.contains("renamed"), "{:?}", lines);
        assert_eq!(s.songs(), vec!["Rock & Roll"]);
        assert!(plan(&stick, &audio, &[], &pls, ExportMode::Quick).is_up_to_date());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn files_spelled_differently_are_rewritten_with_the_right_name() {
        let (root, audio, prj, stick) = setup("sync-file-case");
        let pls = playlist(&prj);
        let p = plan(&stick, &audio, &[], &pls, ExportMode::Quick);
        assert!(run(&p, |_| true).unwrap().iter().all(|l| l.ok));
        let s = Stick::new(&stick);
        let respell = |path: &Path, wrong: &str| {
            let tmp = path.with_file_name("x.tmp");
            fs::rename(path, &tmp).unwrap();
            fs::rename(&tmp, path.with_file_name(wrong)).unwrap();
        };
        respell(&s.song_dir("Rock & Roll").join("song.set"), "SONG.SET");
        respell(&s.play_path("Set"), "SET.play");

        let p = plan(&stick, &audio, &[], &pls, ExportMode::Quick);
        assert!(p.songs[0].plan.needs_export, "{}", p.songs[0].plan.reason);
        assert!(!p.playlists[0].up_to_date, "{}", p.playlists[0].reason);
        assert!(run(&p, |_| true).unwrap().iter().all(|l| l.ok));
        assert!(fs::read_dir(s.song_dir("Rock & Roll")).unwrap().flatten().any(|e| e.file_name() == "song.set"));
        assert_eq!(s.playlists(), vec!["Set"]);
        assert!(plan(&stick, &audio, &[], &pls, ExportMode::Quick).is_up_to_date());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn delete_songs_and_playlists_on_stick() {
        let root = temp_dir("stick-delete");
        let stick = Stick::new(&root.to_string_lossy());
        for song in ["A & B", "Unused", "used-case"] {
            fs::create_dir_all(stick.song_dir(song)).unwrap();
            fs::write(stick.song_dir(song).join("song.set"), "x").unwrap();
        }
        fs::create_dir_all(stick.playlists_dir()).unwrap();
        let play = generate_play("Set", 0, &[], |_| 0.0).replace(
            "</playlist>",
            "  <song>\r\n    <name>A &amp; B</name>\r\n  </song>\r\n  \
             <song>\r\n    <name>USED-CASE</name>\r\n  </song>\r\n</playlist>",
        );
        fs::write(stick.play_path("Set"), play).unwrap();

        assert_eq!(stick.songs(), vec!["A & B", "Unused", "used-case"]);
        assert_eq!(stick.playlists(), vec!["Set"]);
        assert_eq!(stick.playlist_songs("Set"), vec!["A & B", "USED-CASE"]);
        assert_eq!(stick.playlists_with("a & b"), vec!["Set"]);
        assert_eq!(stick.songs_not_in_playlists().unwrap(), vec!["Unused"]);

        assert!(stick.delete_song("..").is_err());
        assert!(stick.delete_song("#PLAYLISTS").is_err());
        assert!(stick.delete_song(r"x\..").is_err());
        assert!(stick.delete_song("missing").is_err());
        stick.delete_song("Unused").unwrap();
        assert_eq!(stick.songs(), vec!["A & B", "used-case"]);

        assert_eq!(stick.song_case_differs("USED-CASE").as_deref(), Some("used-case"));
        assert_eq!(stick.song_case_differs("used-case"), None);
        stick.fix_song_case("USED-CASE").unwrap();
        assert_eq!(stick.songs(), vec!["A & B", "USED-CASE"]);
        assert_eq!(stick.song_case_differs("USED-CASE"), None);

        stick.delete_playlist("Set").unwrap();
        assert!(stick.playlists().is_empty());
        assert_eq!(stick.songs_not_in_playlists().unwrap(), vec!["A & B", "USED-CASE"]);
        let _ = fs::remove_dir_all(&root);
    }
    #[test]
    fn stick_preview_and_details_after_export() {
        let (root, audio, prj, stick_root) = setup("sync-preview");
        let pls = playlist(&prj);
        let p = plan(&stick_root, &audio, &[], &pls, ExportMode::Quick);
        run(&p, |_| true).unwrap();
        let stick = Stick::new(&stick_root);
        // a song folder the export does not touch
        fs::create_dir_all(stick.song_dir("old song")).unwrap();

        let again = plan(&stick_root, &audio, &[], &pls, ExportMode::Quick);
        let pv = stick.preview(&again);
        assert_eq!(pv.songs, vec![("old song".to_string(), None), ("Rock & Roll".to_string(), Some(Action::Skip))]);
        assert_eq!(pv.playlists, vec![("Set".to_string(), Some(Action::Quick))]);
        // on an empty stick the planned entries show up as New
        let empty = Stick::new(&root.join("EMPTY").to_string_lossy());
        assert_eq!(empty.preview(&p).songs, vec![("Rock & Roll".to_string(), Some(Action::New))]);

        let song = stick.song_info("Rock & Roll");
        assert!(song.sample_rate > 0 && song.bits > 0, "{:?}", song);
        assert!(song.length > 0.0 && song.modified > 0, "{:?}", song);
        assert_eq!(song.size, stick.song_size("Rock & Roll"));
        assert!(song.size > 0);
        assert_eq!(stick.song_info("missing"), StickSong::default());

        let pl = stick.playlist_info("Set");
        assert_eq!(pl.songs, vec!["Rock & Roll"]);
        assert_eq!(pl.size, song.size);
        assert!(pl.modified > 0);
        assert!(!stick.has_music());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn play_entries_reads_names_and_durations() {
        let text = "<playlist><playlistname>X</playlistname>            <song><name>A &amp; B</name><duration>61.500</duration></song>            <song><name>C</name><duration>x</duration></song></playlist>";
        assert_eq!(play_entries(text), vec![("A & B".to_string(), 61.5), ("C".to_string(), 0.0)]);
        assert_eq!(play_song_names(text), vec!["A & B", "C"]);
    }

    #[test]
    fn play_starts_reads_mode_and_time() {
        let text = "<playlist><song><name>A</name><pause_mode><mode>wait_period</mode><time>5</time></pause_mode></song>            <song><name>B</name></song></playlist>";
        assert_eq!(play_starts(text), vec![("wait_period".to_string(), "5".to_string()), (String::new(), String::new())]);
    }
}

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
    export_plan, generate_play, is_protected_target, now_ms, plain_tag, plan_song,
    write_bytes_atomic, Config, PlaylistSong, SongPlan, SongProject,
};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

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
        self.root.join("Recording")
    }
    pub fn song_dir(&self, name: &str) -> PathBuf {
        self.recording().join(name)
    }
    pub fn playlists_dir(&self) -> PathBuf {
        self.recording().join("#PLAYLISTS")
    }
    pub fn play_path(&self, playlist: &str) -> PathBuf {
        self.playlists_dir().join(format!("{}.play", playlist))
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
            && self.songs.iter().all(|s| !s.plan.needs_export)
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
    path.to_lowercase()
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
                let plan = plan_song(&project, &rec);
                let action = if !stick.song_dir(&plan.name).is_dir() {
                    Action::New
                } else if mode == ExportMode::Force {
                    Action::Force
                } else if plan.needs_export {
                    Action::Quick
                } else {
                    Action::Skip
                };
                planned.push(PlannedSong { plan, action });
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
            pl.up_to_date = readable && sync.play_text(&pl, ts) == on_stick;
            pl.reason = if pl.up_to_date { "up to date" } else { "content differs" }.into();
        }
        sync.playlists.push(pl);
    }
    sync
}

/// Can the export write to `stick_root`? Refuses protected targets
/// (protected_targets in lp16-tool.toml, default D:).
pub fn check_target(stick_root: &str, config: &Config) -> Result<(), String> {
    if is_protected_target(stick_root, config) {
        return Err(format!(
            "Writing to {} is blocked (protected_targets = \"{}\" in lp16-tool.toml).\nExport to a copy of the stick instead.",
            stick_root, config.protected_targets
        ));
    }
    Ok(())
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
/// Err: the target is protected or song projects are unreadable; then
/// nothing is written.
pub fn run(
    plan: &SyncPlan,
    config: &Config,
    mut progress: impl FnMut(Progress) -> bool,
) -> Result<Vec<SyncLine>, String> {
    check_target(&plan.stick.root().to_string_lossy(), config)?;
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
            (Some(s), _) => export_plan(&s.plan, &rec).map(|_| verified()).map_err(|e| e.to_string()),
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

    fn open_config() -> Config {
        Config { protected_targets: String::new(), ..Config::default() }
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
        let lines = run(&p, &open_config(), |e| {
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
        let lines = run(&again, &open_config(), |_| true).unwrap();
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
        let err = run(&p, &open_config(), |_| true).unwrap_err();
        assert!(err.contains("Gone.songprj"), "{}", err);
        assert!(!Stick::new(&stick).recording().exists(), "nothing may be written");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn protected_target_and_cancel_write_nothing() {
        let (root, audio, prj, stick) = setup("sync-blocked");
        let p = plan(&stick, &audio, &[prj], &[], ExportMode::Quick);
        let blocked = Config { protected_targets: stick.clone(), ..Config::default() };
        assert!(run(&p, &blocked, |_| true).unwrap_err().contains("blocked"));
        let lines = run(&p, &open_config(), |e| !matches!(e, Progress::Start(_))).unwrap();
        assert!(lines.is_empty());
        assert!(!Stick::new(&stick).recording().exists());
        let _ = fs::remove_dir_all(&root);
    }
}

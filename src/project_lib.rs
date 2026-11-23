//! Project Library: the uTool2 folder with the song projects
//! (`LP16\Multitrack Songs\*.songprj`) and the playlists that point at them
//! (`LP16\Multitrack Playlists\*.playlistprj`).
//!
//! Every change to these files goes through [`ProjectLibrary`], which keeps
//! the library consistent:
//!
//! - a playlist never points at a song project that was renamed or deleted
//!   (renaming rewrites the playlists *before* the old file is removed;
//!   deleting refuses a song that a playlist on disk still uses)
//! - every file is written atomically (temporary file + rename)
//! - names are valid FAT32 names and never overwrite another song/playlist
//! - the concert markers of a playlist (`<name>.setlist`, see
//!   [`crate::setlist`]) follow it when it is renamed, copied or deleted
//! - a song file has two writers, the song editor and the Song Mix of the
//!   Rehearsal tab; each one only changes its own part of the file
//!   ([`ProjectLibrary::save_song`], [`save_song_mix`])

use crate::setlist::SetMarkers;
use crate::{
    generate_playlistprj, generate_songprj, now_ms, parse_playlist, valid_fat32_name,
    write_bytes_atomic, PlaylistSong, ProjectTrack, SongProject,
};
use std::fs;
use std::path::{Path, PathBuf};

/// Song projects below the project library root.
pub const SONGS_SUBDIR: &str = "LP16\\Multitrack Songs";
/// Playlists below the project library root.
pub const PLAYLISTS_SUBDIR: &str = "LP16\\Multitrack Playlists";

/// One playlist of the library, as stored in its .playlistprj.
#[derive(Clone)]
pub struct Playlist {
    /// file stem of the .playlistprj
    pub name: String,
    pub path: PathBuf,
    /// songs with `songprj` resolved to full paths
    pub songs: Vec<PlaylistSong>,
    /// encore / end of concert markers (from the .setlist next to it)
    pub markers: SetMarkers,
}

#[derive(Clone, Debug)]
pub struct ProjectLibrary {
    root: String,
    audio_lib: String,
}

impl ProjectLibrary {
    /// `project_lib` replaces {$ProjectLib}, `audio_lib` replaces {$AudioLib}.
    pub fn new(project_lib: &str, audio_lib: &str) -> Self {
        ProjectLibrary { root: project_lib.to_string(), audio_lib: audio_lib.to_string() }
    }

    pub fn songs_dir(&self) -> PathBuf {
        Path::new(&self.root).join(SONGS_SUBDIR)
    }

    pub fn playlists_dir(&self) -> PathBuf {
        Path::new(&self.root).join(PLAYLISTS_SUBDIR)
    }

    pub fn song_path(&self, name: &str) -> PathBuf {
        self.songs_dir().join(format!("{}.songprj", name))
    }

    pub fn playlist_path(&self, name: &str) -> PathBuf {
        self.playlists_dir().join(format!("{}.playlistprj", name))
    }

    /// The concert markers of playlist `name` (uTool2 only lists
    /// *.playlistprj, so it ignores this file).
    pub fn markers_path(&self, name: &str) -> PathBuf {
        self.playlists_dir().join(format!("{}.setlist", name))
    }

    /// All .songprj files, sorted by path (case-insensitive).
    pub fn song_files(&self) -> Vec<PathBuf> {
        list(&self.songs_dir(), "songprj")
    }

    /// All playlists, sorted by path. A file that cannot be read shows up
    /// with no songs; use [`Self::read_playlists`] where that matters.
    pub fn playlists(&self) -> Vec<Playlist> {
        list(&self.playlists_dir(), "playlistprj")
            .into_iter()
            .map(|path| {
                let xml = fs::read_to_string(&path).unwrap_or_default();
                self.playlist_from(path, &xml)
            })
            .collect()
    }

    /// All playlists; Err if any of them cannot be read.
    fn read_playlists(&self) -> Result<Vec<Playlist>, String> {
        list(&self.playlists_dir(), "playlistprj")
            .into_iter()
            .map(|path| {
                let xml = fs::read_to_string(&path).map_err(|e| format!("{}: {}", path.display(), e))?;
                Ok(self.playlist_from(path, &xml))
            })
            .collect()
    }

    fn playlist_from(&self, path: PathBuf, xml: &str) -> Playlist {
        let (_, songs) = parse_playlist(xml, &self.root);
        let markers = fs::read_to_string(path.with_extension("setlist"))
            .map(|t| SetMarkers::parse(&t).clamp(songs.len()))
            .unwrap_or_default();
        Playlist { name: stem(&path), path, songs, markers }
    }

    /// Names of the playlists that contain `song` (a .songprj path).
    pub fn playlists_using(&self, song: &Path) -> Result<Vec<String>, String> {
        let k = key(song);
        Ok(self
            .read_playlists()?
            .into_iter()
            .filter(|pl| pl.songs.iter().any(|s| key(Path::new(&s.songprj)) == k))
            .map(|pl| pl.name)
            .collect())
    }

    /// Write an empty song project `name` (uTool2's "New Multitrack Song").
    pub fn create_song(&self, name: &str, sampling_rate: u32, resolution: u32) -> Result<PathBuf, String> {
        let path = self.song_path(name);
        check_new_name(name, &path, None)?;
        let xml = generate_songprj(name, now_ms(), sampling_rate, resolution, 1);
        write_new(&path, &xml)?;
        Ok(path)
    }

    /// Copy the song project `song` to `new_name`. The playlists keep
    /// pointing at the original.
    pub fn copy_song(&self, song: &Path, new_name: &str) -> Result<PathBuf, String> {
        let new_path = self.song_path(new_name);
        check_new_name(new_name, &new_path, Some(song))?;
        self.write_song_as(song, new_name, &new_path)?;
        Ok(new_path)
    }

    /// Rename the song project `song` to `new_name` and point every playlist
    /// that uses it at the new file. Order: write the new project, rewrite
    /// the playlists, then remove the old project. If a step fails the old
    /// project stays, so no playlist ever points at a missing file.
    pub fn rename_song(&self, song: &Path, new_name: &str) -> Result<PathBuf, String> {
        let new_path = self.song_path(new_name);
        check_new_name(new_name, &new_path, Some(song))?;
        let playlists = self.read_playlists()?;
        self.write_song_as(song, new_name, &new_path)?;
        let old = key(song);
        for pl in playlists.iter().filter(|pl| pl.songs.iter().any(|s| key(Path::new(&s.songprj)) == old)) {
            let songs: Vec<PlaylistSong> = pl
                .songs
                .iter()
                .map(|s| {
                    let mut s = s.clone();
                    if key(Path::new(&s.songprj)) == old {
                        s.songprj = new_path.to_string_lossy().into_owned();
                        s.name = new_name.to_string();
                    }
                    s
                })
                .collect();
            self.write_playlist(&pl.path, &pl.name, &songs).map_err(|e| {
                format!("{}\nThe song was copied to \"{}\"; the old song project was kept.", e, new_name)
            })?;
        }
        if key(song) != key(&new_path) {
            fs::remove_file(song).map_err(|e| format!("{}: {}", song.display(), e))?;
        }
        Ok(new_path)
    }

    /// Delete the song project `song`. Refused while a playlist uses it
    /// (checked against the playlist files, not a cached list).
    pub fn delete_song(&self, song: &Path) -> Result<(), String> {
        let used = self.playlists_using(song)?;
        if !used.is_empty() {
            return Err(format!(
                "The Song is used in these Playlists and can't be deleted:\n{}",
                used.join(", ")
            ));
        }
        fs::remove_file(song).map_err(|e| format!("Cannot delete {}:\n{}", song.display(), e))
    }

    /// Write playlist `name` with `songs` and `markers` (new or replacing
    /// the playlist of that name).
    pub fn save_playlist(&self, name: &str, songs: &[PlaylistSong], markers: &SetMarkers) -> Result<PathBuf, String> {
        if !valid_fat32_name(name) {
            return Err(format!("\"{}\" is not a valid Playlist Name.", name));
        }
        let path = self.playlist_path(name);
        self.write_playlist(&path, name, songs)?;
        self.write_markers(name, &markers.clamp(songs.len()))?;
        Ok(path)
    }

    /// Write playlist `new_name` with `songs`, refusing an existing one.
    pub fn create_playlist(&self, new_name: &str, songs: &[PlaylistSong], markers: &SetMarkers) -> Result<PathBuf, String> {
        check_new_name(new_name, &self.playlist_path(new_name), None)?;
        self.save_playlist(new_name, songs, markers)
    }

    /// Save `songs` under `new_name` and remove the playlist `old_name`
    /// (rename) or keep it (`copy`). The new file is written first.
    pub fn save_playlist_as(
        &self,
        old_name: &str,
        new_name: &str,
        songs: &[PlaylistSong],
        markers: &SetMarkers,
        copy: bool,
    ) -> Result<PathBuf, String> {
        let old_path = self.playlist_path(old_name);
        let new_path = self.playlist_path(new_name);
        check_new_name(new_name, &new_path, Some(&old_path))?;
        self.save_playlist(new_name, songs, markers)?;
        if !copy && key(&old_path) != key(&new_path) {
            fs::remove_file(&old_path).map_err(|e| format!("{}: {}", old_path.display(), e))?;
            remove_if_there(&self.markers_path(old_name))?;
        }
        Ok(new_path)
    }

    /// Rename (`copy` = false) or copy the playlist `old_name` as stored on disk.
    pub fn rename_playlist(&self, old_name: &str, new_name: &str, copy: bool) -> Result<PathBuf, String> {
        let old_path = self.playlist_path(old_name);
        let xml = fs::read_to_string(&old_path).map_err(|e| format!("{}: {}", old_path.display(), e))?;
        let pl = self.playlist_from(old_path, &xml);
        self.save_playlist_as(old_name, new_name, &pl.songs, &pl.markers, copy)
    }

    pub fn delete_playlist(&self, name: &str) -> Result<(), String> {
        let path = self.playlist_path(name);
        fs::remove_file(&path).map_err(|e| format!("Cannot delete {}:\n{}", path.display(), e))?;
        remove_if_there(&self.markers_path(name))
    }

    /// Save a song changed in the song editor (tracks, MIDI file, lengths).
    /// The Song Mix in the file wins: it may have been changed in the
    /// Rehearsal tab since the editor read the song. Returns what is stored.
    pub fn save_song(&self, edited: &SongProject) -> Result<SongProject, String> {
        let mut prj = edited.clone();
        if let Ok(disk) = SongProject::load(&prj.path, prj.audio_lib()) {
            copy_song_mix(&disk.tracks, &mut prj.tracks);
        }
        prj.save().map_err(|e| e.to_string())?;
        Ok(prj)
    }

    /// Load `song` and save it as `new_name` at `new_path`. An unreadable
    /// project is refused (it would be written empty).
    fn write_song_as(&self, song: &Path, new_name: &str, new_path: &Path) -> Result<(), String> {
        let mut prj = SongProject::load(&song.to_string_lossy(), &self.audio_lib).map_err(|e| e.to_string())?;
        prj.name = new_name.to_string();
        prj.save_as(&new_path.to_string_lossy()).map_err(|e| e.to_string())
    }

    fn write_playlist(&self, path: &Path, name: &str, songs: &[PlaylistSong]) -> Result<(), String> {
        let xml = generate_playlistprj(name, now_ms(), &self.root, songs);
        write_new(path, &xml)
    }

    /// Write the .setlist of playlist `name`; without markers there is none.
    fn write_markers(&self, name: &str, markers: &SetMarkers) -> Result<(), String> {
        let path = self.markers_path(name);
        if markers.is_empty() {
            remove_if_there(&path)
        } else {
            write_new(&path, &markers.generate())
        }
    }
}

fn remove_if_there(path: &Path) -> Result<(), String> {
    match fs::remove_file(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(format!("Cannot delete {}:\n{}", path.display(), e)),
        _ => Ok(()),
    }
}

/// Name check for a new song/playlist file: valid FAT32 name, and no other
/// file of that name (`current` = the file being renamed, which may only
/// change case).
fn check_new_name(name: &str, path: &Path, current: Option<&Path>) -> Result<(), String> {
    if !valid_fat32_name(name) {
        return Err(format!("\"{}\" contains invalid characters.", name));
    }
    let same_file = current.map(|c| key(c) == key(path)).unwrap_or(false);
    if path.exists() && !same_file {
        return Err(format!("\"{}\" already exists.", name));
    }
    Ok(())
}

/// Write atomically; a fresh library has no sub folders yet.
fn write_new(path: &Path, text: &str) -> Result<(), String> {
    let write = || -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        write_bytes_atomic(path, text.as_bytes())
    };
    write().map_err(|e| format!("Cannot write {}:\n{}", path.display(), e))
}

fn list(dir: &Path, ext: &str) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.extension().map(|x| x.eq_ignore_ascii_case(ext)).unwrap_or(false))
                .collect()
        })
        .unwrap_or_default();
    v.sort_by_key(|p| p.to_string_lossy().to_lowercase());
    v
}

/// Comparable form of a path: case-insensitive, `\` only, no doubled `\`
/// (a project_lib with a trailing `\` resolves to `…\\LP16\…`).
fn key(path: &Path) -> String {
    let mut s = path.to_string_lossy().to_lowercase().replace('/', "\\");
    while s.contains("\\\\") {
        s = s.replace("\\\\", "\\");
    }
    s
}

fn stem(path: &Path) -> String {
    path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
}

/// Save the Song Mix (monitor level, pan, mute) of `mixed` into its song
/// file; everything else stays as it is in the file, so a project read
/// before the song editor saved does not undo the editor's changes. A
/// missing file is an error (it is not created again). Returns what is stored.
pub fn save_song_mix(mixed: &SongProject) -> Result<SongProject, String> {
    let mut disk = SongProject::load(&mixed.path, mixed.audio_lib()).map_err(|e| e.to_string())?;
    copy_song_mix(&mixed.tracks, &mut disk.tracks);
    disk.save().map_err(|e| e.to_string())?;
    Ok(disk)
}

/// The Song Mix of `from` onto the same tracks (same WAV and WAV track) of
/// `to`; tracks without a partner keep their values.
fn copy_song_mix(from: &[ProjectTrack], to: &mut [ProjectTrack]) {
    for t in to.iter_mut() {
        if let Some(f) = from.iter().find(|f| f.audiofile == t.audiofile && f.ati == t.ati) {
            t.monitor_level = f.monitor_level.clone();
            t.monitor_pan = f.monitor_pan.clone();
            t.monitor_mute = f.monitor_mute.clone();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{mini_song, temp_dir};

    /// A library with songs "Intro" and "Ballad" and playlists "Set"
    /// (Intro, Ballad) and "Short" (Intro).
    fn setup(name: &str) -> (PathBuf, ProjectLibrary) {
        let root = temp_dir(name);
        let (xml, audio) = mini_song(&root, "Intro");
        let lib = ProjectLibrary::new(&root.join("LIB").to_string_lossy(), &audio);
        fs::create_dir_all(lib.songs_dir()).unwrap();
        fs::write(lib.song_path("Intro"), &xml).unwrap();
        fs::write(lib.song_path("Ballad"), xml.replace("Intro", "Ballad")).unwrap();
        let entry = |n: &str| PlaylistSong {
            name: n.into(),
            songprj: lib.song_path(n).to_string_lossy().into_owned(),
            mode: "wait_for_button_press".into(),
            delay: "0".into(),
        };
        lib.create_playlist("Set", &[entry("Intro"), entry("Ballad")], &SetMarkers::default()).unwrap();
        lib.create_playlist("Short", &[entry("Intro")], &SetMarkers::default()).unwrap();
        (root, lib)
    }

    fn songs_of(lib: &ProjectLibrary, playlist: &str) -> Vec<String> {
        lib.playlists()
            .into_iter()
            .find(|p| p.name == playlist)
            .unwrap()
            .songs
            .iter()
            .map(|s| stem(Path::new(&s.songprj)))
            .collect()
    }

    #[test]
    fn rename_song_repoints_every_playlist() {
        let (root, lib) = setup("plib-rename");
        let new = lib.rename_song(&lib.song_path("Intro"), "Opener").unwrap();
        assert_eq!(new, lib.song_path("Opener"));
        assert!(!lib.song_path("Intro").exists());
        assert_eq!(songs_of(&lib, "Set"), ["Opener", "Ballad"]);
        assert_eq!(songs_of(&lib, "Short"), ["Opener"]);
        let prj = SongProject::load(&new.to_string_lossy(), &lib.audio_lib).unwrap();
        assert_eq!(prj.name, "Opener");
        // only a case change: same file, no "already exists"
        lib.rename_song(&lib.song_path("Opener"), "OPENER").unwrap();
        assert_eq!(lib.song_files().len(), 2);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn rename_song_keeps_old_file_when_a_playlist_cannot_be_written() {
        let (root, lib) = setup("plib-rename-fail");
        let short = lib.playlist_path("Short");
        let mut perm = fs::metadata(&short).unwrap().permissions();
        perm.set_readonly(true);
        fs::set_permissions(&short, perm.clone()).unwrap();
        let err = lib.rename_song(&lib.song_path("Intro"), "Opener").unwrap_err();
        assert!(err.contains("old song project was kept"), "{}", err);
        // the old project is still there, so every playlist entry still resolves
        assert!(lib.song_path("Intro").exists());
        for pl in lib.playlists() {
            for s in &pl.songs {
                assert!(Path::new(&s.songprj).exists(), "{} -> {}", pl.name, s.songprj);
            }
        }
        #[allow(clippy::permissions_set_readonly_false)]
        perm.set_readonly(false);
        fs::set_permissions(&short, perm).unwrap();
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn copy_song_leaves_playlists_alone() {
        let (root, lib) = setup("plib-copy");
        lib.copy_song(&lib.song_path("Intro"), "Intro 2").unwrap();
        assert!(lib.song_path("Intro").exists() && lib.song_path("Intro 2").exists());
        assert_eq!(songs_of(&lib, "Short"), ["Intro"]);
        let err = lib.copy_song(&lib.song_path("Intro"), "Ballad").unwrap_err();
        assert!(err.contains("already exists"), "{}", err);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn delete_song_is_refused_while_a_playlist_on_disk_uses_it() {
        let (root, lib) = setup("plib-delete");
        let err = lib.delete_song(&lib.song_path("Intro")).unwrap_err();
        assert!(err.contains("Set") && err.contains("Short"), "{}", err);
        assert!(lib.song_path("Intro").exists());
        lib.delete_playlist("Short").unwrap();
        lib.save_playlist("Set", &[], &SetMarkers::default()).unwrap();
        lib.delete_song(&lib.song_path("Intro")).unwrap();
        assert!(!lib.song_path("Intro").exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn playlist_rename_copy_and_names() {
        let (root, lib) = setup("plib-playlists");
        lib.rename_playlist("Short", "Encore", false).unwrap();
        assert!(!lib.playlist_path("Short").exists());
        assert_eq!(songs_of(&lib, "Encore"), ["Intro"]);
        let xml = fs::read_to_string(lib.playlist_path("Encore")).unwrap();
        assert!(xml.contains("<ns0:playlist_name>Encore</ns0:playlist_name>"));
        lib.rename_playlist("Encore", "Encore 2", true).unwrap();
        assert!(lib.playlist_path("Encore").exists());
        assert!(lib.rename_playlist("Encore", "Set", false).unwrap_err().contains("already exists"));
        assert!(lib.create_playlist("Bad:Name", &[], &SetMarkers::default()).is_err());
        assert!(lib.create_song("Set?", 48000, 24).is_err());
        let _ = fs::remove_dir_all(&root);
    }

    fn markers_of(lib: &ProjectLibrary, playlist: &str) -> SetMarkers {
        lib.playlists().into_iter().find(|p| p.name == playlist).unwrap().markers
    }

    #[test]
    fn markers_follow_the_playlist() {
        let (root, lib) = setup("plib-markers");
        let set = lib.playlists().into_iter().find(|p| p.name == "Set").unwrap();
        let m = SetMarkers { encore: Some(1), end: Some(2) };
        lib.save_playlist("Set", &set.songs, &m).unwrap();
        assert!(lib.markers_path("Set").exists());
        assert_eq!(markers_of(&lib, "Set"), m);
        // the .playlistprj stays as uTool2 writes it
        let xml = fs::read_to_string(lib.playlist_path("Set")).unwrap();
        assert!(!xml.contains("encore"));
        // a renamed song keeps the markers
        lib.rename_song(&lib.song_path("Ballad"), "Slow").unwrap();
        assert_eq!(markers_of(&lib, "Set"), m);
        lib.rename_playlist("Set", "Gig", true).unwrap();
        assert_eq!(markers_of(&lib, "Gig"), m);
        assert_eq!(markers_of(&lib, "Set"), m);
        lib.rename_playlist("Set", "Tour", false).unwrap();
        assert!(!lib.markers_path("Set").exists());
        assert_eq!(markers_of(&lib, "Tour"), m);
        lib.delete_playlist("Tour").unwrap();
        assert!(!lib.markers_path("Tour").exists());
        // saved without markers: no file
        let gig = lib.playlists().into_iter().find(|p| p.name == "Gig").unwrap();
        lib.save_playlist("Gig", &gig.songs, &SetMarkers::default()).unwrap();
        assert!(!lib.markers_path("Gig").exists());
        // markers beyond the songs are clamped
        fs::write(lib.markers_path("Short"), "encore = 9\r\n").unwrap();
        assert_eq!(markers_of(&lib, "Short").encore, Some(1));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_fresh_library_gets_its_folders_on_first_write() {
        let root = temp_dir("plib-fresh");
        let lib = ProjectLibrary::new(&root.join("EMPTY\\").to_string_lossy(), "");
        assert!(lib.song_files().is_empty() && lib.playlists().is_empty());
        let song = lib.create_song("First", 48000, 24).unwrap();
        lib.create_playlist(
            "One",
            &[PlaylistSong {
                name: "First".into(),
                songprj: song.to_string_lossy().into_owned(),
                mode: "wait_for_button_press".into(),
                delay: "0".into(),
            }],
            &SetMarkers::default(),
        )
        .unwrap();
        // trailing "\" in project_lib: the reference still matches
        assert_eq!(lib.playlists_using(&song).unwrap(), ["One"]);
        let _ = fs::remove_dir_all(&root);
    }
    #[test]
    fn song_editor_and_song_mix_only_write_their_own_part() {
        let (root, lib) = setup("plib-two-writers");
        let path = lib.song_path("Intro").to_string_lossy().into_owned();
        let load = || SongProject::load(&path, &lib.audio_lib).unwrap();
        // both read the song before anyone saves
        let mut editor = load();
        let mut rehearsal = load();

        // the Rehearsal tab changes the Song Mix
        rehearsal.tracks[1].monitor_level = "-6.0".into();
        let stored = save_song_mix(&rehearsal).unwrap();
        assert_eq!(stored.tracks[1].monitor_level, "-6.0");

        // the editor saves later: its tracks and length, the mix from the file
        editor.duration_seconds = 99.0;
        editor.tracks[0].name = "Click".into();
        let stored = lib.save_song(&editor).unwrap();
        assert_eq!(stored.tracks[1].monitor_level, "-6.0");
        let disk = load();
        assert_eq!((disk.duration_seconds, disk.tracks[0].name.as_str()), (99.0, "Click"));
        assert_eq!(disk.tracks[1].monitor_level, "-6.0");

        // a Song Mix saved from the old copy does not undo the editor
        rehearsal.tracks[0].monitor_mute = "true".into();
        save_song_mix(&rehearsal).unwrap();
        let disk = load();
        assert_eq!((disk.duration_seconds, disk.tracks[0].name.as_str()), (99.0, "Click"));
        assert_eq!(disk.tracks[0].monitor_mute, "true");

        // a deleted song is not written again
        fs::remove_file(&path).unwrap();
        assert!(save_song_mix(&rehearsal).is_err());
        assert!(!Path::new(&path).exists());
        let _ = fs::remove_dir_all(&root);
    }
}

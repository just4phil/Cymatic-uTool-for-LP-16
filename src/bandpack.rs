//! Band package (`.lp16pack`): one playlist with everything it needs, for
//! band members who rehearse at home with their own lp16-tool.
//!
//! A band package is a plain ZIP file:
//!
//! ```text
//! manifest.txt                  format, playlist, songs, files (see below)
//! Playlists/<name>.playlistprj  the playlist
//! Playlists/<name>.setlist      encore / end of concert markers (if any)
//! Songs/<song>.songprj          every song of the playlist (once)
//! AudioLibrary/<path>           every WAV and MIDI file a song uses
//! ```
//!
//! In the packed .songprj every file path is `{$AudioLib}\<path>`, so the
//! songs resolve against the audio library of the receiver. A file outside
//! the audio library of the sender is packed as `_extern\<song>\<file>`.
//! The WAVs are stored, not compressed (audio hardly shrinks), and the ZIP
//! uses ZIP64, so a package may be larger than 4 GB.
//!
//! The import never touches the settings of the receiver (audio device,
//! paths, rehearsal mix, ...). A song the receiver already has keeps his
//! own Song Mix (monitor level, pan, mute); everything else comes from the
//! package. Files that are already there (same size and modification time)
//! are not copied again. Order: audio files, then songs, then the playlist,
//! so a playlist never points at a song that is not there yet.
//!
//! The manifest is UTF-8 text, one entry per line, fields separated by tabs:
//!
//! ```text
//! lp16pack <TAB> 1
//! app <TAB> lp16-tool 0.2.0
//! created <TAB> <ms since 1970>
//! playlist <TAB> <name>
//! song <TAB> <file stem>
//! file <TAB> <mtime ms> <TAB> <size> <TAB> <path below AudioLibrary, with \>
//! ```

use crate::project_lib::{copy_song_mix, ProjectLibrary};
use crate::setlist::SetMarkers;
use crate::{
    generate_playlistprj, mtime_ms, now_ms, parse_playlist, valid_fat32_name, write_atomic,
    write_bytes_atomic, PlaylistSong, SongProject,
};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

/// File extension of a band package.
pub const EXT: &str = "lp16pack";
/// Version of the package layout this program writes and reads.
pub const FORMAT: u32 = 1;

const MANIFEST: &str = "manifest.txt";
const PLAYLISTS: &str = "Playlists/";
const SONGS: &str = "Songs/";
const AUDIO: &str = "AudioLibrary/";
const AUDIO_LIB: &str = "{$AudioLib}";
/// Folder (below the audio library) for files outside the sender's audio library.
const EXTERN: &str = "_extern";
const BUF: usize = 1 << 20;

/// One audio file of a package.
#[derive(Clone, Debug, PartialEq)]
pub struct PackFile {
    /// below the audio library, with `\` (e.g. `Song\2026-05-10\a-1-klick.wav`)
    pub rel: String,
    pub size: u64,
    /// modification time in ms; the .songprj's validationtimestamp refers to it
    pub mtime: i64,
}

impl PackFile {
    fn zip_name(&self) -> String {
        format!("{}{}", AUDIO, self.rel.replace('\\', "/"))
    }
}

// ---------------------------------------------------------------------------
// Export
// ---------------------------------------------------------------------------

/// What an export writes; built by [`plan_export`] (reads only).
pub struct ExportPlan {
    pub playlist: String,
    playlist_xml: String,
    markers: SetMarkers,
    /// (file stem, .songprj text with the paths of the package)
    songs: Vec<(String, String)>,
    /// (full path on this computer, file in the package)
    files: Vec<(String, PackFile)>,
}

impl ExportPlan {
    pub fn song_count(&self) -> usize {
        self.songs.len()
    }
    pub fn file_count(&self) -> usize {
        self.files.len()
    }
    /// Bytes of all audio files.
    pub fn bytes(&self) -> u64 {
        self.files.iter().map(|(_, f)| f.size).sum()
    }
}

/// Collect playlist `name` with its songs and their files. Refused if a
/// song cannot be read or a file is missing: a package must be complete.
pub fn plan_export(lib: &ProjectLibrary, name: &str) -> Result<ExportPlan, String> {
    let audio_lib = lib.audio_lib();
    let pl = lib
        .playlists()
        .into_iter()
        .find(|p| p.name == name)
        .ok_or_else(|| tr!("pack.no_playlist", name = name))?;
    let mut problems: Vec<String> = Vec::new();
    let mut songs: Vec<(String, String)> = Vec::new();
    let mut seen_songs: HashSet<String> = HashSet::new();
    let mut files: Vec<(String, PackFile)> = Vec::new();
    // full path (key) -> path in the package
    let mut packed: HashMap<String, String> = HashMap::new();
    let mut used_rel: HashSet<String> = HashSet::new();
    let mut entries: Vec<PlaylistSong> = Vec::new();

    for s in &pl.songs {
        let stem = stem(&s.songprj);
        entries.push(PlaylistSong {
            name: s.name.clone(),
            songprj: lib.song_path(&stem).to_string_lossy().into_owned(),
            mode: s.mode.clone(),
            delay: s.delay.clone(),
        });
        if !seen_songs.insert(stem.to_lowercase()) {
            continue;
        }
        let mut prj = match SongProject::load(&s.songprj, audio_lib) {
            Ok(p) => p,
            Err(e) => {
                problems.push(e.to_string());
                continue;
            }
        };
        // every file once; the stored path becomes {$AudioLib}\<rel>
        let mut pack = |stored: &str, problems: &mut Vec<String>| -> String {
            let full = crate::resolve(stored, audio_lib);
            let k = crate::path_key::path_key(&full);
            if let Some(rel) = packed.get(&k) {
                return format!("{}\\{}", AUDIO_LIB, rel);
            }
            let meta = match fs::metadata(&full) {
                Ok(m) if m.is_file() => m,
                _ => {
                    problems.push(tr!("pack.missing_file", song = stem, path = full));
                    return stored.to_string();
                }
            };
            let rel = match stored.strip_prefix(AUDIO_LIB) {
                Some(r) if valid_rel(r.trim_start_matches('\\')) => r.trim_start_matches('\\').to_string(),
                _ => {
                    // outside the audio library: _extern\<song>\<file>
                    let file = Path::new(&full).file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default();
                    let mut rel = format!("{}\\{}\\{}", EXTERN, stem, file);
                    let mut n = 2;
                    while used_rel.contains(&rel.to_lowercase()) {
                        rel = format!("{}\\{}\\{} ({})", EXTERN, stem, n, file);
                        n += 1;
                    }
                    rel
                }
            };
            used_rel.insert(rel.to_lowercase());
            packed.insert(k, rel.clone());
            files.push((full.clone(), PackFile { rel: rel.clone(), size: meta.len(), mtime: mtime_ms(&full) }));
            format!("{}\\{}", AUDIO_LIB, rel)
        };
        for t in prj.tracks.iter_mut() {
            if !t.audiofile.is_empty() {
                t.audiofile = pack(&t.audiofile, &mut problems);
            }
        }
        if !prj.midi_file.is_empty() {
            prj.midi_file = pack(&prj.midi_file, &mut problems);
        }
        songs.push((stem, prj.to_xml()));
    }
    if !problems.is_empty() {
        return Err(tr!("pack.incomplete", list = problems.join("\n")));
    }
    let playlist_xml = generate_playlistprj(&pl.name, now_ms(), lib.root(), &entries);
    Ok(ExportPlan { playlist: pl.name, playlist_xml, markers: pl.markers, songs, files })
}

fn manifest(plan: &ExportPlan) -> String {
    let mut m = String::new();
    m.push_str(&format!("lp16pack\t{}\r\n", FORMAT));
    m.push_str(&format!("app\tlp16-tool {}\r\n", env!("CARGO_PKG_VERSION")));
    m.push_str(&format!("created\t{}\r\n", now_ms()));
    m.push_str(&format!("playlist\t{}\r\n", plan.playlist));
    for (stem, _) in &plan.songs {
        m.push_str(&format!("song\t{}\r\n", stem));
    }
    for (_, f) in &plan.files {
        m.push_str(&format!("file\t{}\t{}\t{}\r\n", f.mtime, f.size, f.rel));
    }
    m
}

/// Write the package to `dest` (atomically: a temporary file is renamed at
/// the end). `progress` gets the bytes of audio copied so far; returning
/// false cancels (Err with kind Interrupted, `dest` is not written).
pub fn write_package(plan: &ExportPlan, dest: &Path, progress: &mut dyn FnMut(u64, &str) -> bool) -> io::Result<()> {
    write_atomic(dest, |w| {
        let mut zip = ZipWriter::new(&mut *w);
        let small = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
        let stored = SimpleFileOptions::default().compression_method(CompressionMethod::Stored).large_file(true);
        zip.start_file(MANIFEST, small).map_err(io::Error::other)?;
        zip.write_all(manifest(plan).as_bytes())?;
        zip.start_file(format!("{}{}.playlistprj", PLAYLISTS, plan.playlist), small).map_err(io::Error::other)?;
        zip.write_all(plan.playlist_xml.as_bytes())?;
        if !plan.markers.is_empty() {
            zip.start_file(format!("{}{}.setlist", PLAYLISTS, plan.playlist), small).map_err(io::Error::other)?;
            zip.write_all(plan.markers.generate().as_bytes())?;
        }
        for (stem, xml) in &plan.songs {
            zip.start_file(format!("{}{}.songprj", SONGS, stem), small).map_err(io::Error::other)?;
            zip.write_all(xml.as_bytes())?;
        }
        let mut done = 0u64;
        let mut buf = vec![0u8; BUF];
        for (full, f) in &plan.files {
            if !progress(done, &f.rel) {
                return Err(io::Error::new(io::ErrorKind::Interrupted, "canceled"));
            }
            zip.start_file(f.zip_name(), stored).map_err(io::Error::other)?;
            let mut input = fs::File::open(full).map_err(|e| io::Error::new(e.kind(), format!("{}: {}", full, e)))?;
            let mut copied = 0u64;
            loop {
                let n = input.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                zip.write_all(&buf[..n])?;
                copied += n as u64;
                done += n as u64;
                if !progress(done, &f.rel) {
                    return Err(io::Error::new(io::ErrorKind::Interrupted, "canceled"));
                }
            }
            if copied != f.size {
                return Err(io::Error::other(tr!("pack.changed", path = full)));
            }
        }
        zip.finish().map_err(io::Error::other)?;
        Ok(())
    })
}

// ---------------------------------------------------------------------------
// Import
// ---------------------------------------------------------------------------

/// A package as read by [`open_package`]: the manifest and the small files.
pub struct Package {
    pub path: PathBuf,
    pub playlist: String,
    /// "lp16-tool 0.2.0" of the sender
    pub app: String,
    pub created: i64,
    playlist_xml: String,
    markers: SetMarkers,
    /// (file stem, .songprj text)
    songs: Vec<(String, String)>,
    files: Vec<PackFile>,
}

/// Read the manifest, the playlist and the song projects of a package and
/// check that every name is safe (no `..`, no drive, only the known folders)
/// and every listed file is in the ZIP.
pub fn open_package(path: &Path) -> Result<Package, String> {
    let err = |e: &dyn std::fmt::Display| tr!("pack.cannot_read", path = path.display(), err = e);
    let file = fs::File::open(path).map_err(|e| err(&e))?;
    let mut zip = ZipArchive::new(io::BufReader::new(file)).map_err(|e| err(&e))?;
    let text = read_text(&mut zip, MANIFEST).map_err(|_| tr!("pack.no_manifest", path = path.display()))?;
    let mut format = 0u32;
    let (mut playlist, mut app, mut created) = (String::new(), String::new(), 0i64);
    let mut stems: Vec<String> = Vec::new();
    let mut files: Vec<PackFile> = Vec::new();
    for line in text.lines() {
        let f: Vec<&str> = line.split('\t').collect();
        match f.as_slice() {
            ["lp16pack", v] => format = v.trim().parse().unwrap_or(0),
            ["app", v] => app = v.to_string(),
            ["created", v] => created = v.trim().parse().unwrap_or(0),
            ["playlist", v] => playlist = v.to_string(),
            ["song", v] => stems.push(v.to_string()),
            ["file", mtime, size, rel] => files.push(PackFile {
                rel: rel.to_string(),
                size: size.trim().parse().map_err(|_| err(&line))?,
                mtime: mtime.trim().parse().map_err(|_| err(&line))?,
            }),
            _ => {}
        }
    }
    if format == 0 || format > FORMAT {
        return Err(tr!("pack.format", path = path.display(), v = format));
    }
    if !valid_fat32_name(&playlist) {
        return Err(err(&tr!("pack.bad_name", name = playlist)));
    }
    let playlist_xml = read_text(&mut zip, &format!("{}{}.playlistprj", PLAYLISTS, playlist)).map_err(|e| err(&e))?;
    let markers = read_text(&mut zip, &format!("{}{}.setlist", PLAYLISTS, playlist))
        .map(|t| SetMarkers::parse(&t))
        .unwrap_or_default();
    let mut songs = Vec::new();
    for stem in stems {
        if !valid_fat32_name(&stem) {
            return Err(err(&tr!("pack.bad_name", name = stem)));
        }
        let xml = read_text(&mut zip, &format!("{}{}.songprj", SONGS, stem)).map_err(|e| err(&e))?;
        songs.push((stem, xml));
    }
    for f in &files {
        if !valid_rel(&f.rel) {
            return Err(err(&tr!("pack.bad_name", name = f.rel)));
        }
        let entry = zip.by_name(&f.zip_name()).map_err(|_| err(&tr!("pack.file_missing", name = f.rel)))?;
        if entry.size() != f.size {
            return Err(err(&tr!("pack.file_missing", name = f.rel)));
        }
    }
    Ok(Package { path: path.to_path_buf(), playlist, app, created, playlist_xml, markers, songs, files })
}

fn read_text<R: Read + Seek>(zip: &mut ZipArchive<R>, name: &str) -> Result<String, String> {
    let mut entry = zip.by_name(name).map_err(|e| format!("{}: {}", name, e))?;
    let mut text = String::new();
    entry.read_to_string(&mut text).map_err(|e| format!("{}: {}", name, e))?;
    Ok(text)
}

/// A relative path below the audio library: no drive, no `..`, no empty part.
fn valid_rel(rel: &str) -> bool {
    !rel.is_empty()
        && rel.split('\\').all(|part| part != "." && part != ".." && valid_fat32_name(part))
}

/// What the import does with a song.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SongAction {
    /// not in the library yet
    New,
    /// in the library, the package has the same song (files and project)
    Same,
    /// in the library, the package has another version; the local Song Mix stays
    Update,
}

pub struct ImportSong {
    pub name: String,
    pub action: SongAction,
    /// the .songprj to write (package version, local Song Mix)
    xml: String,
    /// bytes of audio files to copy for this song
    pub bytes: u64,
}

/// What an import would do; built by [`plan_import`] (reads only).
pub struct ImportPlan {
    pub playlist: String,
    /// a playlist of that name is replaced
    pub playlist_exists: bool,
    pub songs: Vec<ImportSong>,
    /// files to copy (the others are there already)
    copy: Vec<PackFile>,
}

impl ImportPlan {
    pub fn bytes(&self) -> u64 {
        self.copy.iter().map(|f| f.size).sum()
    }
    pub fn file_count(&self) -> usize {
        self.copy.len()
    }
    /// Nothing to write at all (the playlist is still written again).
    pub fn up_to_date(&self) -> bool {
        self.copy.is_empty() && self.songs.iter().all(|s| s.action == SongAction::Same)
    }
}

fn local_file(lib: &ProjectLibrary, rel: &str) -> PathBuf {
    Path::new(&crate::path_key::join_root(lib.audio_lib(), rel)).to_path_buf()
}

/// Compare the package with the library of `lib`.
pub fn plan_import(pkg: &Package, lib: &ProjectLibrary) -> Result<ImportPlan, String> {
    if lib.audio_lib().trim().is_empty() || lib.root().trim().is_empty() {
        return Err(tr!("pack.no_library").to_string());
    }
    let copy: Vec<PackFile> = pkg
        .files
        .iter()
        .filter(|f| {
            let local = local_file(lib, &f.rel);
            let same = fs::metadata(&local).map(|m| m.is_file() && m.len() == f.size).unwrap_or(false)
                && mtime_ms(&local.to_string_lossy()) == f.mtime;
            !same
        })
        .cloned()
        .collect();
    let copy_keys: HashSet<String> = copy.iter().map(|f| f.rel.to_lowercase()).collect();
    let mut songs = Vec::new();
    for (stem, xml) in &pkg.songs {
        let path = lib.song_path(stem);
        let mut prj = SongProject::parse(xml, lib.audio_lib());
        let mut files = prj.tracks.iter().map(|t| t.audiofile.clone()).collect::<Vec<_>>();
        files.push(prj.midi_file.clone());
        let rels: HashSet<String> = files
            .iter()
            .filter_map(|f| f.strip_prefix(AUDIO_LIB))
            .map(|r| r.trim_start_matches('\\').to_lowercase())
            .collect();
        let bytes = pkg.files.iter().filter(|f| copy_keys.contains(&f.rel.to_lowercase()) && rels.contains(&f.rel.to_lowercase())).map(|f| f.size).sum();
        let action = if path.exists() {
            let local = SongProject::load(&path.to_string_lossy(), lib.audio_lib()).map_err(|e| e.to_string())?;
            copy_song_mix(&local.tracks, &mut prj.tracks);
            let same = prj.to_xml() == local.to_xml() && bytes == 0;
            if same { SongAction::Same } else { SongAction::Update }
        } else {
            SongAction::New
        };
        songs.push(ImportSong { name: stem.clone(), action, xml: prj.to_xml(), bytes });
    }
    Ok(ImportPlan {
        playlist: pkg.playlist.clone(),
        playlist_exists: lib.playlist_path(&pkg.playlist).exists(),
        songs,
        copy,
    })
}

/// Do the import: copy the files, write the songs, then the playlist.
/// `progress` as for [`write_package`]; a canceled import keeps the files
/// copied so far but writes no song and no playlist.
pub fn run_import(pkg: &Package, plan: &ImportPlan, lib: &ProjectLibrary, progress: &mut dyn FnMut(u64, &str) -> bool) -> io::Result<()> {
    let file = fs::File::open(&pkg.path)?;
    let mut zip = ZipArchive::new(io::BufReader::new(file)).map_err(io::Error::other)?;
    let mut done = 0u64;
    let mut buf = vec![0u8; BUF];
    for f in &plan.copy {
        if !progress(done, &f.rel) {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "canceled"));
        }
        let dest = local_file(lib, &f.rel);
        if let Some(dir) = dest.parent() {
            fs::create_dir_all(dir)?;
        }
        let mut entry = zip.by_name(&f.zip_name()).map_err(io::Error::other)?;
        write_atomic(&dest, |w| {
            let mut copied = 0u64;
            loop {
                // the zip reader checks the CRC at the end of the entry
                let n = entry.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                w.write_all(&buf[..n])?;
                copied += n as u64;
                if !progress(done + copied, &f.rel) {
                    return Err(io::Error::new(io::ErrorKind::Interrupted, "canceled"));
                }
            }
            if copied != f.size {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, f.rel.clone()));
            }
            Ok(())
        })
        .map_err(|e| io::Error::new(e.kind(), format!("{}: {}", dest.display(), e)))?;
        // the song projects refer to the time stamp of the sender
        set_mtime(&dest, f.mtime)?;
        done += f.size;
    }
    progress(done, "");
    fs::create_dir_all(lib.songs_dir())?;
    for s in plan.songs.iter().filter(|s| s.action != SongAction::Same) {
        let path = lib.song_path(&s.name);
        write_bytes_atomic(&path, s.xml.as_bytes()).map_err(|e| io::Error::new(e.kind(), format!("{}: {}", path.display(), e)))?;
    }
    let (_, songs) = parse_playlist(&pkg.playlist_xml, lib.root());
    let songs: Vec<PlaylistSong> = songs
        .into_iter()
        .map(|s| PlaylistSong { songprj: lib.song_path(&stem(&s.songprj)).to_string_lossy().into_owned(), ..s })
        .collect();
    lib.save_playlist(&pkg.playlist, &songs, &pkg.markers).map_err(io::Error::other)?;
    Ok(())
}

fn set_mtime(path: &Path, ms: i64) -> io::Result<()> {
    if ms <= 0 {
        return Ok(());
    }
    let t = UNIX_EPOCH + Duration::from_millis(ms as u64);
    let f = fs::File::options().write(true).open(path)?;
    f.set_modified(t)
}

fn stem(path: &str) -> String {
    let file = path.rsplit(['\\', '/']).next().unwrap_or(path);
    match file.rfind('.') {
        Some(i) if i > 0 => file[..i].to_string(),
        _ => file.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{mini_song, temp_dir};
    use crate::project_lib::save_song_mix;

    /// Sender library with songs "Intro" and "Ballad" in playlist "Probe".
    fn sender(root: &Path) -> ProjectLibrary {
        let (xml, audio) = mini_song(root, "Intro");
        let lib = ProjectLibrary::new(&root.join("LIB").to_string_lossy(), &audio);
        fs::create_dir_all(lib.songs_dir()).unwrap();
        fs::write(lib.song_path("Intro"), &xml).unwrap();
        fs::write(lib.song_path("Ballad"), xml.replace("Intro", "Ballad")).unwrap();
        let entry = |n: &str| PlaylistSong {
            name: n.into(),
            songprj: lib.song_path(n).to_string_lossy().into_owned(),
            mode: "wait_period".into(),
            delay: "5".into(),
        };
        let m = SetMarkers { encore: Some(1), end: None };
        lib.create_playlist("Probe", &[entry("Intro"), entry("Ballad"), entry("Intro")], &m).unwrap();
        lib
    }

    fn receiver(root: &Path) -> ProjectLibrary {
        let audio = root.join("THEIR_AUDIO");
        ProjectLibrary::new(&root.join("THEIR_LIB").to_string_lossy(), &audio.to_string_lossy())
    }

    fn import(pack: &Path, lib: &ProjectLibrary) -> ImportPlan {
        let pkg = open_package(pack).unwrap();
        let plan = plan_import(&pkg, lib).unwrap();
        run_import(&pkg, &plan, lib, &mut |_, _| true).unwrap();
        plan
    }

    #[test]
    fn export_and_import_a_playlist() {
        let root = temp_dir("pack-roundtrip");
        let from = sender(&root);
        let plan = plan_export(&from, "Probe").unwrap();
        assert_eq!((plan.song_count(), plan.file_count()), (2, 3), "songs once, files once");
        let pack = root.join("Probe.lp16pack");
        write_package(&plan, &pack, &mut |_, _| true).unwrap();

        let to = receiver(&root);
        let first = import(&pack, &to);
        assert!(!first.playlist_exists);
        assert!(first.songs.iter().all(|s| s.action == SongAction::New));
        assert_eq!(first.file_count(), 3);

        // the playlist with start modes and markers
        let pl = to.playlists().into_iter().find(|p| p.name == "Probe").unwrap();
        let names: Vec<String> = pl.songs.iter().map(|s| stem(&s.songprj)).collect();
        assert_eq!(names, ["Intro", "Ballad", "Intro"]);
        assert!(pl.songs.iter().all(|s| s.mode == "wait_period" && s.delay == "5"));
        assert!(pl.songs.iter().all(|s| Path::new(&s.songprj).exists()));
        assert_eq!(pl.markers, SetMarkers { encore: Some(1), end: None });

        // the songs resolve in the new audio library, with the sender's time stamps
        let prj = SongProject::load(&to.song_path("Intro").to_string_lossy(), to.audio_lib()).unwrap();
        for t in &prj.tracks {
            let wav = prj.wav(t);
            assert!(wav.starts_with(to.audio_lib()), "{}", wav);
            assert_eq!(mtime_ms(&wav), t.vts, "{}", wav);
        }
        assert!(Path::new(&prj.midi_path().unwrap()).exists());

        // a second import copies nothing
        let again = import(&pack, &to);
        assert!(again.playlist_exists && again.up_to_date());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn the_local_song_mix_stays() {
        let root = temp_dir("pack-mix");
        let from = sender(&root);
        let pack = root.join("p.lp16pack");
        write_package(&plan_export(&from, "Probe").unwrap(), &pack, &mut |_, _| true).unwrap();
        let to = receiver(&root);
        import(&pack, &to);

        // the receiver turns his bass down
        let path = to.song_path("Intro").to_string_lossy().into_owned();
        let mut mine = SongProject::load(&path, to.audio_lib()).unwrap();
        mine.tracks[1].monitor_level = "-12.0".into();
        mine.tracks[1].monitor_mute = "true".into();
        save_song_mix(&mine).unwrap();

        // the sender changes the song (and his own mix) and sends it again
        let mut theirs = SongProject::load(&from.song_path("Intro").to_string_lossy(), from.audio_lib()).unwrap();
        theirs.duration_seconds = 123.0;
        theirs.tracks[1].monitor_level = "3.0".into();
        theirs.save().unwrap();
        write_package(&plan_export(&from, "Probe").unwrap(), &pack, &mut |_, _| true).unwrap();

        let plan = import(&pack, &to);
        let intro = plan.songs.iter().find(|s| s.name == "Intro").unwrap();
        assert_eq!(intro.action, SongAction::Update);
        assert_eq!(plan.songs.iter().find(|s| s.name == "Ballad").unwrap().action, SongAction::Same);
        let now = SongProject::load(&path, to.audio_lib()).unwrap();
        assert_eq!(now.duration_seconds, 123.0);
        assert_eq!(now.tracks[1].monitor_level, "-12.0");
        assert_eq!(now.tracks[1].monitor_mute, "true");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_file_outside_the_audio_library_is_packed_too() {
        let root = temp_dir("pack-extern");
        let from = sender(&root);
        let outside = root.join("ELSEWHERE");
        fs::create_dir_all(&outside).unwrap();
        let path = from.song_path("Ballad").to_string_lossy().into_owned();
        let mut prj = SongProject::load(&path, from.audio_lib()).unwrap();
        let wav = outside.join("guitar.wav");
        fs::copy(prj.wav(&prj.tracks[0]), &wav).unwrap();
        prj.tracks[0].audiofile = wav.to_string_lossy().into_owned();
        prj.save().unwrap();

        let plan = plan_export(&from, "Probe").unwrap();
        assert_eq!(plan.file_count(), 4);
        let pack = root.join("p.lp16pack");
        write_package(&plan, &pack, &mut |_, _| true).unwrap();
        let to = receiver(&root);
        import(&pack, &to);
        let prj = SongProject::load(&to.song_path("Ballad").to_string_lossy(), to.audio_lib()).unwrap();
        assert_eq!(prj.tracks[0].audiofile, "{$AudioLib}\\_extern\\Ballad\\guitar.wav");
        assert!(Path::new(&prj.wav(&prj.tracks[0])).exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_missing_file_stops_the_export() {
        let root = temp_dir("pack-missing");
        let from = sender(&root);
        let prj = SongProject::load(&from.song_path("Intro").to_string_lossy(), from.audio_lib()).unwrap();
        fs::remove_file(prj.wav(&prj.tracks[1])).unwrap();
        let err = plan_export(&from, "Probe").err().unwrap();
        assert!(err.contains("a-2-bass.wav"), "{}", err);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn names_that_leave_the_library_are_refused() {
        assert!(valid_rel("Song\\v1\\a.wav"));
        for bad in ["..\\x.wav", "Song\\..\\..\\x.wav", "C:\\x.wav", "\\x.wav", "a\\\\b", ""] {
            assert!(!valid_rel(bad), "{}", bad);
        }
        let root = temp_dir("pack-evil");
        let pack = root.join("evil.lp16pack");
        let f = fs::File::create(&pack).unwrap();
        let mut z = ZipWriter::new(f);
        z.start_file(MANIFEST, SimpleFileOptions::default()).unwrap();
        z.write_all(b"lp16pack\t1\r\nplaylist\tP\r\nfile\t1\t1\t..\\..\\evil.wav\r\n").unwrap();
        z.start_file("Playlists/P.playlistprj", SimpleFileOptions::default()).unwrap();
        z.finish().unwrap();
        assert!(open_package(&pack).err().unwrap().contains("evil.wav"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn canceled_export_leaves_no_file() {
        let root = temp_dir("pack-cancel");
        let from = sender(&root);
        let plan = plan_export(&from, "Probe").unwrap();
        let pack = root.join("p.lp16pack");
        let e = write_package(&plan, &pack, &mut |_, _| false).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::Interrupted);
        assert!(!pack.exists());
        assert_eq!(fs::read_dir(&root).unwrap().filter(|e| e.as_ref().unwrap().path().is_file()).count(), 0);
        let _ = fs::remove_dir_all(&root);
    }
}


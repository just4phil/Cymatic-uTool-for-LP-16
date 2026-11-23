//! Facts of a song that uTool2's .songprj has no field for. They live in a
//! small file next to the song project (`<name>.songinfo`), so the .songprj
//! stays exactly in uTool2's format (uTool2 only lists *.songprj).
//!
//! For now this is the print title: the name of the song on the printed
//! setlist, e.g. `I love it (Icona Pop)` for `I-LOVE-IT__INTRO_(2026-05-10_RF)`.
//!
//! A new version of a song (a new date folder imported from the
//! AudioLibrary) takes the print title of the newest other version of the
//! same song folder, see [`inherit`].

use crate::{natural_cmp, resolve, write_bytes_atomic, SongProject};
use std::fs;
use std::path::{Path, PathBuf};

/// File extension of the file next to the .songprj.
pub const EXT: &str = "songinfo";

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SongInfo {
    /// name on the printed setlist; empty = not set (the setlist then
    /// shows a name made from the file name, see [`default_print_title`])
    pub print_title: String,
}

impl SongInfo {
    pub fn is_empty(&self) -> bool {
        self.print_title.trim().is_empty()
    }

    /// `key = value` lines; unknown or broken lines are ignored.
    pub fn parse(text: &str) -> SongInfo {
        let mut info = SongInfo::default();
        for line in text.lines() {
            let Some((key, value)) = line.split_once('=') else { continue };
            if key.trim() == "print_title" {
                info.print_title = one_line(value);
            }
        }
        info
    }

    pub fn generate(&self) -> String {
        let mut out = String::new();
        if !self.print_title.trim().is_empty() {
            out.push_str(&format!("print_title = {}\r\n", one_line(&self.print_title)));
        }
        out
    }
}

/// Trimmed, line breaks become spaces (one value per line in the file).
fn one_line(text: &str) -> String {
    text.split(['\r', '\n']).map(str::trim).filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" ")
}

/// The .songinfo of the song project `song` (a .songprj path).
pub fn path_of(song: &Path) -> PathBuf {
    song.with_extension(EXT)
}

/// The facts of `song`; a missing or unreadable file gives the empty ones.
pub fn load(song: &Path) -> SongInfo {
    fs::read_to_string(path_of(song)).map(|t| SongInfo::parse(&t)).unwrap_or_default()
}

/// Write the facts of `song`; empty facts remove the file.
pub fn save(song: &Path, info: &SongInfo) -> std::io::Result<()> {
    let path = path_of(song);
    if info.is_empty() {
        match fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    } else {
        write_bytes_atomic(&path, info.generate().as_bytes())
    }
}

/// A print title made from the song name when none is set:
/// `I-LOVE-IT__INTRO_(2026-05-10_RF)` -> `I Love It Intro`.
pub fn default_print_title(song_name: &str) -> String {
    // the date folder part of imported songs: "_(...)" at the end
    let base = match song_name.rfind("_(") {
        Some(i) if song_name.ends_with(')') && i > 0 => &song_name[..i],
        _ => song_name,
    };
    base.split(['_', '-', ' '])
        .filter(|w| !w.is_empty())
        .map(|w| {
            let lower = w.to_lowercase();
            let mut c = lower.chars();
            match c.next() {
                Some(f) => f.to_uppercase().chain(c).collect::<String>(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The song folder and date folder of a song project: the folder of its
/// audio files (`...\<song>\<date>\x.wav`), or None without files.
fn version_dirs(prj: &SongProject) -> Option<(PathBuf, String)> {
    let file = prj
        .tracks
        .iter()
        .map(|t| t.audiofile.clone())
        .chain(std::iter::once(prj.midi_file.clone()))
        .find(|f| !f.is_empty())?;
    let full = PathBuf::from(resolve(&file, prj.audio_lib()));
    let date_dir = full.parent()?;
    let date = date_dir.file_name()?.to_string_lossy().into_owned();
    Some((date_dir.parent()?.to_path_buf(), date))
}

/// Give the new song `song` (its audio files in `date_dir`, a date folder
/// of a song folder) the facts of the newest other version of that song
/// folder that has some. A song that has facts already keeps them.
/// Returns whether something was taken over.
pub fn inherit(song: &Path, date_dir: &Path, audio_lib: &str) -> std::io::Result<bool> {
    if !load(song).is_empty() {
        return Ok(false);
    }
    let (Some(song_dir), Some(own_date)) = (date_dir.parent(), date_dir.file_name()) else { return Ok(false) };
    let song_key = crate::path_key::path_key(&song_dir.to_string_lossy());
    let own_date = own_date.to_string_lossy().to_lowercase();
    let Some(songs_dir) = song.parent() else { return Ok(false) };
    let mut best: Option<(String, SongInfo)> = None;
    let Ok(rd) = fs::read_dir(songs_dir) else { return Ok(false) };
    for e in rd.flatten() {
        let info_path = e.path();
        if !info_path.extension().map(|x| x.eq_ignore_ascii_case(EXT)).unwrap_or(false) {
            continue;
        }
        let prj_path = info_path.with_extension("songprj");
        let info = load(&prj_path);
        if info.is_empty() {
            continue;
        }
        let Ok(prj) = SongProject::load(&prj_path.to_string_lossy(), audio_lib) else { continue };
        let Some((dir, date)) = version_dirs(&prj) else { continue };
        if crate::path_key::path_key(&dir.to_string_lossy()) != song_key || date.to_lowercase() == own_date {
            continue;
        }
        let newer = best.as_ref().map(|(d, _)| natural_cmp(&date, d).is_gt()).unwrap_or(true);
        if newer {
            best = Some((date, info));
        }
    }
    match best {
        Some((_, info)) => save(song, &info).map(|_| true),
        None => Ok(false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_roundtrip() {
        let i = SongInfo { print_title: "I love it (Icona Pop)".into() };
        assert_eq!(SongInfo::parse(&i.generate()), i);
        assert_eq!(SongInfo::parse("junk\nprint_title =  A = B \n").print_title, "A = B");
        assert!(SongInfo::default().generate().is_empty());
        let multi = SongInfo { print_title: "two\r\nlines".into() };
        assert_eq!(SongInfo::parse(&multi.generate()).print_title, "two lines");
    }

    #[test]
    fn default_titles() {
        assert_eq!(default_print_title("I-LOVE-IT__INTRO_(2026-05-10_RF)"), "I Love It Intro");
        assert_eq!(default_print_title("APT_(2026-05-10_RF)"), "Apt");
        assert_eq!(default_print_title("Ballad"), "Ballad");
        assert_eq!(default_print_title("_APT_(x)"), "Apt");
    }
}

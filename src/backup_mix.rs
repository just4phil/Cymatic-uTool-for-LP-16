//! The backup mix: the stereo MP3 that is played from a phone or an MP3
//! player when the LP-16 fails. Each side is a mono mix of the 16 LP-16
//! outputs with its own faders: typically the backing tracks go left and
//! the click (and cues) go right.
//!
//! This is only the data and the maths: one global mix (no presets), the
//! target folder and the file name pattern, stored in `lp16-tool.toml`
//! (`backup.*`, `backup_dir`, `backup_pattern`). `backup.rs` renders and
//! encodes, the "Export Stereo-MP3" tab edits it.

use crate::mix::{clamp_db, fmt_db, level_gain, parse_db, parse_flag, STRIPS};

/// File name pattern when none is set.
pub const DEFAULT_PATTERN: &str = "{nr} {song}";

/// One channel of one side.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BackupStrip {
    /// -60.0 ..= +6.0, `f32::NEG_INFINITY` = off
    pub level_db: f32,
    pub mute: bool,
}

impl Default for BackupStrip {
    /// Off: nothing reaches a side unless it is turned up.
    fn default() -> BackupStrip {
        BackupStrip { level_db: f32::NEG_INFINITY, mute: false }
    }
}

/// One side of the stereo file: a mono mix of the 16 outputs.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BackupSide {
    pub master_db: f32,
    pub strips: [BackupStrip; STRIPS],
}

impl Default for BackupSide {
    fn default() -> BackupSide {
        BackupSide { master_db: 0.0, strips: [BackupStrip::default(); STRIPS] }
    }
}

impl BackupSide {
    /// Gain per output: fader x master, 0 if muted or off.
    pub fn gains(&self) -> [f32; STRIPS] {
        let master = level_gain(self.master_db);
        self.strips.map(|s| if s.mute { 0.0 } else { level_gain(s.level_db) * master })
    }

    /// Output `k` reaches this side.
    pub fn uses(&self, k: usize) -> bool {
        self.gains().get(k).map(|g| *g > 0.0).unwrap_or(false)
    }

    pub fn set_level(&mut self, k: usize, db: f32) {
        if let Some(s) = self.strips.get_mut(k) {
            s.level_db = clamp_db(db);
        }
    }

    pub fn set_mute(&mut self, k: usize, mute: bool) {
        if let Some(s) = self.strips.get_mut(k) {
            s.mute = mute;
        }
    }

    pub fn set_master(&mut self, db: f32) {
        self.master_db = clamp_db(db);
    }
}

/// The two sides.
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct BackupMix {
    pub left: BackupSide,
    pub right: BackupSide,
}

impl BackupMix {
    /// (left, right) gain per output.
    pub fn gains(&self) -> ([f32; STRIPS], [f32; STRIPS]) {
        (self.left.gains(), self.right.gains())
    }

    pub fn side(&self, right: bool) -> &BackupSide {
        if right {
            &self.right
        } else {
            &self.left
        }
    }

    pub fn side_mut(&mut self, right: bool) -> &mut BackupSide {
        if right {
            &mut self.right
        } else {
            &mut self.left
        }
    }
}

/// Everything of the backup export that is kept in the config.
#[derive(Clone, Debug, PartialEq)]
pub struct BackupSettings {
    pub mix: BackupMix,
    /// target folder of the MP3s
    pub dir: String,
    /// file name pattern, see `file_name`
    pub pattern: String,
    /// own names of the 16 outputs in the backup mixers; empty = the track
    /// names of the songs
    pub names: [String; STRIPS],
}

impl Default for BackupSettings {
    fn default() -> BackupSettings {
        BackupSettings {
            mix: BackupMix::default(),
            dir: String::new(),
            pattern: DEFAULT_PATTERN.to_string(),
            names: Default::default(),
        }
    }
}

/// A strip name as it can be stored: no double quotes, no line breaks,
/// no spaces around it.
pub fn clean_label(text: &str) -> String {
    let s: String = text.chars().filter(|c| *c != '"' && !c.is_control()).collect();
    s.trim().to_string()
}

impl BackupSettings {
    /// Give output `k` the name `text`; empty or the same as `auto` (the
    /// name from the songs) removes the own name.
    pub fn set_name(&mut self, k: usize, text: &str, auto: &str) {
        let Some(n) = self.names.get_mut(k) else { return };
        let t = clean_label(text);
        *n = if t == auto.trim() { String::new() } else { t };
    }

    /// The name shown for output `k`: the own name, else `auto`.
    pub fn name<'a>(&'a self, k: usize, auto: &'a str) -> &'a str {
        match self.names.get(k) {
            Some(n) if !n.is_empty() => n,
            _ => auto,
        }
    }

    /// The `backup*` lines of `lp16-tool.toml` (CRLF).
    pub fn to_config_text(&self) -> String {
        let mut t = String::new();
        t.push_str(&format!("backup_dir = \"{}\"\r\n", self.dir));
        t.push_str(&format!("backup_pattern = \"{}\"\r\n", self.pattern));
        for (k, n) in self.names.iter().enumerate().filter(|(_, n)| !n.is_empty()) {
            t.push_str(&format!("backup.name.ch{} = \"{}\"\r\n", k + 1, n));
        }
        for (name, side) in [("left", &self.mix.left), ("right", &self.mix.right)] {
            t.push_str(&format!("backup.{}.master = \"{}\"\r\n", name, fmt_db(side.master_db)));
            for (k, s) in side.strips.iter().enumerate() {
                t.push_str(&format!("backup.{}.ch{} = \"{} {}\"\r\n", name, k + 1, fmt_db(s.level_db), s.mute as u8));
            }
        }
        t
    }
}

/// A key that belongs to the backup settings.
pub fn is_backup_key(key: &str) -> bool {
    key == "backup_dir" || key == "backup_pattern" || key.starts_with("backup.")
}

/// Collects the backup lines while the config is read.
#[derive(Default)]
pub struct BackupParser {
    settings: BackupSettings,
}

impl BackupParser {
    /// One `key = value` pair (value without the quotes). Lines that cannot
    /// be read are ignored.
    pub fn feed(&mut self, key: &str, value: &str) {
        let s = &mut self.settings;
        match key {
            "backup_dir" => s.dir = value.to_string(),
            "backup_pattern" => {
                if !value.trim().is_empty() {
                    s.pattern = value.to_string();
                }
            }
            _ => {
                let Some(rest) = key.strip_prefix("backup.") else { return };
                let Some((side, field)) = rest.split_once('.') else { return };
                if side == "name" {
                    if let Some(k) = field.strip_prefix("ch").and_then(|k| k.parse::<usize>().ok()) {
                        if (1..=STRIPS).contains(&k) {
                            s.names[k - 1] = clean_label(value);
                        }
                    }
                    return;
                }
                let side = match side {
                    "left" => &mut s.mix.left,
                    "right" => &mut s.mix.right,
                    _ => return,
                };
                if field == "master" {
                    if let Some(db) = parse_db(value) {
                        side.master_db = db;
                    }
                } else if let Some(k) = field.strip_prefix("ch").and_then(|k| k.parse::<usize>().ok()) {
                    if (1..=STRIPS).contains(&k) {
                        if let Some(strip) = parse_strip(value) {
                            side.strips[k - 1] = strip;
                        }
                    }
                }
            }
        }
    }

    pub fn finish(self) -> BackupSettings {
        self.settings
    }
}

/// `"<level> <mute>"`; None if it cannot be read.
fn parse_strip(value: &str) -> Option<BackupStrip> {
    let mut parts = value.split_whitespace();
    let level_db = parse_db(parts.next()?)?;
    let mute = parse_flag(parts.next()?)?;
    if parts.next().is_some() {
        return None;
    }
    Some(BackupStrip { level_db, mute })
}

/// The MP3 file name of song `nr` (1-based) of `total`: the pattern with
/// `{nr}` (zero-padded to at least 2 digits), `{song}` and `{playlist}`
/// replaced, characters Windows does not allow replaced by "_", ".mp3"
/// appended. An empty result becomes the song name.
pub fn file_name(pattern: &str, nr: usize, total: usize, song: &str, playlist: &str) -> String {
    let width = total.max(nr).to_string().len().max(2);
    let pattern = if pattern.trim().is_empty() { DEFAULT_PATTERN } else { pattern };
    let raw = pattern
        .replace("{nr}", &format!("{:0width$}", nr, width = width))
        .replace("{song}", song)
        .replace("{playlist}", playlist);
    let mut name = clean_name(&raw);
    if name.is_empty() {
        name = clean_name(song);
    }
    if name.is_empty() {
        name = format!("{:0width$}", nr, width = width);
    }
    name.push_str(".mp3");
    name
}

/// Invalid characters become "_"; no leading or trailing spaces and no
/// trailing dots (Windows drops them).
fn clean_name(raw: &str) -> String {
    let s: String = raw
        .chars()
        .map(|c| {
            if matches!(c, '"' | '*' | '/' | ':' | '<' | '>' | '?' | '\\' | '|') || c.is_control() {
                '_'
            } else {
                c
            }
        })
        .collect();
    s.trim().trim_end_matches(['.', ' ']).to_string()
}

/// Names that occur more than once (ignoring case, like Windows) get
/// " (2)", " (3)", ... before the extension.
pub fn unique_names(names: Vec<String>) -> Vec<String> {
    let mut seen: Vec<String> = Vec::new();
    let mut out = Vec::with_capacity(names.len());
    for name in names {
        let (stem, ext) = match name.rfind('.') {
            Some(i) => (name[..i].to_string(), name[i..].to_string()),
            None => (name.clone(), String::new()),
        };
        let mut candidate = name.clone();
        let mut n = 2;
        while seen.contains(&candidate.to_lowercase()) {
            candidate = format!("{} ({}){}", stem, n, ext);
            n += 1;
        }
        seen.push(candidate.to_lowercase());
        out.push(candidate);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-5
    }

    #[test]
    fn nothing_is_mixed_by_default() {
        let m = BackupMix::default();
        let (l, r) = m.gains();
        assert_eq!(l, [0.0; STRIPS]);
        assert_eq!(r, [0.0; STRIPS]);
        assert_eq!(BackupSettings::default().pattern, DEFAULT_PATTERN);
    }

    #[test]
    fn gains_follow_fader_mute_and_master() {
        let mut m = BackupMix::default();
        m.left.set_level(0, 0.0);
        m.left.set_level(1, -6.0);
        m.right.set_level(15, 0.0);
        m.right.set_master(-6.0);
        let (l, r) = m.gains();
        assert_eq!(l[0], 1.0);
        assert!(close(l[1], 0.501187));
        assert_eq!(l[15], 0.0);
        assert!(close(r[15], 0.501187));
        assert!(m.left.uses(0) && !m.right.uses(0) && m.right.uses(15));
        m.left.set_mute(0, true);
        assert_eq!(m.left.gains()[0], 0.0);
        assert!(!m.left.uses(0));
        // limited, ignored beyond the strips
        m.left.set_level(2, 40.0);
        m.left.set_level(99, 0.0);
        assert_eq!(m.left.strips[2].level_db, 6.0);
        // side by flag
        m.side_mut(true).set_level(3, -3.0);
        assert_eq!(m.side(true).strips[3].level_db, -3.0);
        assert_eq!(m.side(false).strips[3].level_db, f32::NEG_INFINITY);
    }

    fn parse(text: &str) -> BackupSettings {
        let mut p = BackupParser::default();
        for line in text.lines() {
            if let Some((k, v)) = line.split_once('=') {
                if is_backup_key(k.trim()) {
                    p.feed(k.trim(), v.trim().trim_matches('"'));
                }
            }
        }
        p.finish()
    }

    #[test]
    fn config_text_roundtrip() {
        let mut s = BackupSettings { dir: "D:\\Backup MP3".into(), pattern: "{playlist} - {nr} {song}".into(), ..Default::default() };
        s.mix.left.set_level(0, -3.5);
        s.mix.left.set_mute(4, true);
        s.mix.left.set_master(-1.0);
        s.mix.right.set_level(15, 2.0);
        s.set_name(15, "Click + Cues", "Click");
        s.set_name(0, "Synth", "Synth");
        let text = s.to_config_text();
        assert!(text.contains("backup.name.ch16 = \"Click + Cues\"\r\n"), "{}", text);
        assert!(!text.contains("backup.name.ch1 "), "the same as the song name: not stored");
        assert!(text.contains("backup.left.ch1 = \"-3.5 0\"\r\n"), "{}", text);
        assert!(text.contains("backup.left.ch5 = \"off 1\"\r\n"));
        assert!(text.contains("backup.right.ch16 = \"2.0 0\"\r\n"));
        assert_eq!(parse(&text), s);
    }

    #[test]
    fn broken_lines_are_ignored() {
        let s = parse(
            "backup.left.ch1 = \"-6.0 0\"\n\
             backup.left.ch2 = \"-6.0\"\n\
             backup.left.ch3 = \"x 0\"\n\
             backup.left.ch17 = \"0.0 0\"\n\
             backup.middle.ch1 = \"0.0 0\"\n\
             backup.right.master = \"banana\"\n\
             backup.right.ch4 = \"99 1\"\n\
             backup_pattern = \"\"\n",
        );
        assert_eq!(s.mix.left.strips[0], BackupStrip { level_db: -6.0, mute: false });
        assert_eq!(s.mix.left.strips[1], BackupStrip::default());
        assert_eq!(s.mix.left.strips[2], BackupStrip::default());
        assert_eq!(s.mix.right.master_db, 0.0);
        assert_eq!(s.mix.right.strips[3], BackupStrip { level_db: 6.0, mute: true });
        assert_eq!(s.pattern, DEFAULT_PATTERN, "an empty pattern keeps the default");
    }

    #[test]
    fn own_strip_names() {
        let mut s = BackupSettings::default();
        assert_eq!(s.name(3, "Bass"), "Bass");
        s.set_name(3, "  Bass \"DI\"\n ", "Bass");
        assert_eq!(s.name(3, "Bass"), "Bass DI");
        s.set_name(3, "", "Bass");
        assert_eq!(s.name(3, "Bass"), "Bass", "empty: back to the song name");
        s.set_name(99, "x", ""); // ignored
        assert_eq!(clean_label(" a\"b\tc "), "abc");
    }

    #[test]
    fn file_names_from_the_pattern() {
        assert_eq!(file_name("{nr} {song}", 3, 12, "Intro", "Tour"), "03 Intro.mp3");
        assert_eq!(file_name("{nr} {song}", 7, 120, "Intro", "Tour"), "007 Intro.mp3");
        assert_eq!(file_name("{playlist}_{nr}_{song}", 1, 1, "A", "Sommer 26"), "Sommer 26_01_A.mp3");
        assert_eq!(file_name("", 2, 5, "Song", "P"), "02 Song.mp3", "empty pattern: the default");
        assert_eq!(file_name("{song}", 1, 1, "AC/DC: Live?", "P"), "AC_DC_ Live_.mp3");
        assert_eq!(file_name("{song}...", 1, 1, "Ende", "P"), "Ende.mp3");
        assert_eq!(file_name("{song}", 4, 9, "", "P"), "04.mp3");
        assert_eq!(file_name("Fix", 1, 2, "A", "P"), "Fix.mp3");
    }

    #[test]
    fn duplicate_names_are_numbered() {
        let names = vec!["01 A.mp3".to_string(), "x.mp3".into(), "X.mp3".into(), "x.mp3".into()];
        assert_eq!(unique_names(names), ["01 A.mp3", "x.mp3", "X (2).mp3", "x (3).mp3"]);
    }
}

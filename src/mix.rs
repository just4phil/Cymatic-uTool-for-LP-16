//! The rehearsal mix: 16 channel strips (LP-16 outputs 1-16) plus master,
//! global for all songs, with named presets.
//!
//! This is only the data and the maths; the engine (`rehearsal.rs`) applies
//! it, the Rehearsal tab edits it and `lp16-tool.toml` stores it. The
//! rehearsal mix is never written into a .songprj, a playlist, a song.set or
//! a .play.
//!
//! The song mix (`SongStrip`) is the other way round: the uTool2 mixer
//! values of a song (`monitor_level`, `monitor_mute` of its tracks), stored
//! in the .songprj and exported to the stick. In the rehearsal they are
//! offsets on top of the rehearsal mix, for that song only: level = rehearsal
//! level + song level, a muted track is silent. The song's pan is not used
//! (the LP-16 only pans its headphone output).

use crate::SongProject;
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

/// Channel strips = LP-16 outputs.
pub const STRIPS: usize = 16;
/// Lowest fader value in dB; below it comes "off" (`f32::NEG_INFINITY`).
pub const LEVEL_MIN_DB: f32 = -60.0;
pub const LEVEL_MAX_DB: f32 = 6.0;
/// Pan runs from -PAN_MAX to +PAN_MAX (uTool2 scale).
pub const PAN_MAX: i32 = 127;
/// Gain of a centred strip with the constant-power law.
pub const CENTER_GAIN: f32 = 0.707_106_77;
/// Changes are saved at most this often.
pub const SAVE_INTERVAL: Duration = Duration::from_secs(1);

/// A dB value within the range: only -infinity stays "off", other values
/// are limited to -60 ..= +6. NaN becomes 0 dB.
pub fn clamp_db(db: f32) -> f32 {
    if db.is_nan() {
        0.0
    } else if db == f32::NEG_INFINITY {
        db
    } else {
        db.clamp(LEVEL_MIN_DB, LEVEL_MAX_DB)
    }
}

/// A level typed by the user: "-0.5", "-0,5", "+3", "-12 dB"; "-inf",
/// "inf", "off" or "-\u{221e}" for off. Rounded to 0.1 dB and limited to the
/// range. None if it is not a number.
pub fn parse_level(text: &str) -> Option<f32> {
    let t = text.trim().to_lowercase();
    let t = t.strip_suffix("db").unwrap_or(&t).trim().replace(',', ".");
    if matches!(t.as_str(), "-inf" | "inf" | "off" | "-\u{221e}" | "\u{221e}") {
        return Some(f32::NEG_INFINITY);
    }
    let v: f32 = t.strip_prefix('+').unwrap_or(&t).parse().ok()?;
    if !v.is_finite() {
        return None;
    }
    Some(clamp_db((v * 10.0).round() / 10.0))
}

/// 10^(dB/20); -infinity gives 0.
pub fn level_gain(db: f32) -> f32 {
    if db == f32::NEG_INFINITY {
        0.0
    } else {
        10f64.powf(clamp_db(db) as f64 / 20.0) as f32
    }
}

/// (left, right) of the constant-power pan law: theta = (pan + 127) / 254 x
/// pi/2, L = cos, R = sin. The ends are exact (-127: 1/0, +127: 0/1).
pub fn pan_gains(pan: i32) -> (f32, f32) {
    let pan = pan.clamp(-PAN_MAX, PAN_MAX);
    if pan == -PAN_MAX {
        return (1.0, 0.0);
    }
    if pan == PAN_MAX {
        return (0.0, 1.0);
    }
    let theta = (pan + PAN_MAX) as f64 / (2 * PAN_MAX) as f64 * std::f64::consts::FRAC_PI_2;
    (theta.cos() as f32, theta.sin() as f32)
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Strip {
    /// -60.0 ..= +6.0, `f32::NEG_INFINITY` = off
    pub level_db: f32,
    /// -127 ..= +127
    pub pan: i32,
    pub mute: bool,
    pub solo: bool,
}

impl Default for Strip {
    fn default() -> Strip {
        Strip { level_db: 0.0, pan: 0, mute: false, solo: false }
    }
}

/// Gains of one strip, mute and solo included.
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct StripGains {
    /// fader gain, 0 while the strip is not audible (meter point)
    pub level: f32,
    /// level x pan, per side
    pub left: f32,
    pub right: f32,
}

/// The numbers of a mix without its name: what the audio callback gets
/// (plain `Copy` data, so applying it does not allocate or free anything).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MixParams {
    pub master_db: f32,
    pub strips: [Strip; STRIPS],
}

impl Default for MixParams {
    fn default() -> MixParams {
        MixParams { master_db: 0.0, strips: [Strip::default(); STRIPS] }
    }
}

impl MixParams {
    /// Gain of every strip and of the master. With at least one solo,
    /// strips without solo are silent; mute beats solo.
    pub fn gains(&self) -> ([StripGains; STRIPS], f32) {
        let any_solo = self.strips.iter().any(|s| s.solo);
        let mut out = [StripGains::default(); STRIPS];
        for (g, s) in out.iter_mut().zip(self.strips.iter()) {
            let audible = !s.mute && (!any_solo || s.solo);
            if audible {
                let level = level_gain(s.level_db);
                let (l, r) = pan_gains(s.pan);
                *g = StripGains { level, left: level * l, right: level * r };
            }
        }
        (out, level_gain(self.master_db))
    }
}

/// The song mix of one LP-16 output: the uTool2 mixer values of the tracks
/// on it. Off is not a level here (uTool2 has mute for that), so the level
/// stays within `LEVEL_MIN_DB ..= LEVEL_MAX_DB`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SongStrip {
    pub level_db: f32,
    pub mute: bool,
}

impl Default for SongStrip {
    fn default() -> SongStrip {
        SongStrip { level_db: 0.0, mute: false }
    }
}

impl SongStrip {
    /// The gain the rehearsal applies on top of the rehearsal mix.
    pub fn gain(&self) -> f32 {
        if self.mute {
            0.0
        } else {
            level_gain(self.level_db)
        }
    }
}

/// `monitor_level` of a track in dB (empty or unreadable: 0 dB).
fn track_level(text: &str) -> f32 {
    match text.trim().parse::<f32>() {
        Ok(v) if v.is_finite() => v.clamp(LEVEL_MIN_DB, LEVEL_MAX_DB),
        _ => 0.0,
    }
}

/// `monitor_mute` of a track ("1" or "true").
fn track_muted(text: &str) -> bool {
    matches!(text.trim().to_lowercase().as_str(), "1" | "true")
}

/// The song mix per output. Several tracks on one output share the values;
/// the first track on it gives them. Outputs without a track are neutral.
pub fn song_strips(project: &SongProject) -> [SongStrip; STRIPS] {
    let mut out = [SongStrip::default(); STRIPS];
    let mut seen = [false; STRIPS];
    for t in &project.tracks {
        if t.channel < 0 || t.channel as usize >= STRIPS || seen[t.channel as usize] {
            continue;
        }
        seen[t.channel as usize] = true;
        out[t.channel as usize] = SongStrip { level_db: track_level(&t.monitor_level), mute: track_muted(&t.monitor_mute) };
    }
    out
}

/// The gains of `song_strips`, as the engine applies them.
pub fn song_gains(project: &SongProject) -> [f32; STRIPS] {
    song_strips(project).map(|s| s.gain())
}

/// Set the song mix of output `k` in all its tracks (uTool2's format:
/// level with one decimal, mute "1"/"0"). The pan is left as it is. True
/// if a track changed.
pub fn set_song_strip(project: &mut SongProject, k: usize, strip: SongStrip) -> bool {
    let level = format!("{:.1}", clamp_db(strip.level_db).max(LEVEL_MIN_DB));
    let mute = if strip.mute { "1" } else { "0" };
    let mut changed = false;
    for t in project.tracks.iter_mut().filter(|t| t.channel == k as i32) {
        if track_level(&t.monitor_level) != track_level(&level) || t.monitor_level.trim().is_empty() {
            t.monitor_level = level.clone();
            changed = true;
        }
        if track_muted(&t.monitor_mute) != strip.mute {
            t.monitor_mute = mute.to_string();
            changed = true;
        }
    }
    changed
}

/// The song's mixer values differ from uTool2's defaults (0 dB, centre, not
/// muted) in some track: the lists show a symbol for it.
pub fn has_song_mix(project: &SongProject) -> bool {
    project.tracks.iter().any(|t| {
        track_level(&t.monitor_level) != 0.0
            || track_muted(&t.monitor_mute)
            || t.monitor_pan.trim().parse::<f32>().map(|p| p != 0.0).unwrap_or(false)
    })
}

/// A named preset.
#[derive(Clone, Debug, PartialEq)]
pub struct RehearsalMix {
    pub name: String,
    pub master_db: f32,
    pub strips: [Strip; STRIPS],
}

impl RehearsalMix {
    /// All strips 0 dB, centre, no mute or solo, master 0 dB.
    pub fn neutral(name: &str) -> RehearsalMix {
        RehearsalMix { name: name.to_string(), master_db: 0.0, strips: [Strip::default(); STRIPS] }
    }

    pub fn params(&self) -> MixParams {
        MixParams { master_db: self.master_db, strips: self.strips }
    }

    pub fn is_neutral(&self) -> bool {
        self.params() == MixParams::default()
    }

    /// Setters limit the value to its range; a strip beyond 15 is ignored.
    pub fn set_level(&mut self, strip: usize, db: f32) {
        if let Some(s) = self.strips.get_mut(strip) {
            s.level_db = clamp_db(db);
        }
    }

    pub fn set_pan(&mut self, strip: usize, pan: i32) {
        if let Some(s) = self.strips.get_mut(strip) {
            s.pan = pan.clamp(-PAN_MAX, PAN_MAX);
        }
    }

    pub fn set_mute(&mut self, strip: usize, mute: bool) {
        if let Some(s) = self.strips.get_mut(strip) {
            s.mute = mute;
        }
    }

    pub fn set_solo(&mut self, strip: usize, solo: bool) {
        if let Some(s) = self.strips.get_mut(strip) {
            s.solo = solo;
        }
    }

    pub fn set_master(&mut self, db: f32) {
        self.master_db = clamp_db(db);
    }
}

/// A preset name: not empty, no `"`, no line break (or other control
/// character). Returns the trimmed name.
pub fn validate_name(name: &str) -> Result<String, String> {
    let n = name.trim();
    if n.is_empty() {
        return Err("The name must not be empty.".to_string());
    }
    if n.contains('"') {
        return Err("The name must not contain a double quote.".to_string());
    }
    if n.chars().any(|c| c.is_control()) {
        return Err("The name must not contain a line break.".to_string());
    }
    Ok(n.to_string())
}

/// The presets and the active one. There is always at least one preset;
/// without any, a neutral "Default" appears.
#[derive(Clone, Debug, PartialEq)]
pub struct MixLibrary {
    mixes: Vec<RehearsalMix>,
    active: usize,
}

impl Default for MixLibrary {
    fn default() -> MixLibrary {
        MixLibrary { mixes: vec![RehearsalMix::neutral("Default")], active: 0 }
    }
}

impl MixLibrary {
    pub fn presets(&self) -> &[RehearsalMix] {
        &self.mixes
    }

    pub fn active_index(&self) -> usize {
        self.active
    }

    pub fn active_name(&self) -> &str {
        &self.mixes[self.active].name
    }

    pub fn active(&self) -> &RehearsalMix {
        &self.mixes[self.active]
    }

    pub fn active_mut(&mut self) -> &mut RehearsalMix {
        &mut self.mixes[self.active]
    }

    fn taken(&self, name: &str) -> bool {
        self.mixes.iter().any(|m| m.name == name)
    }

    /// Make `name` the active preset.
    pub fn select(&mut self, name: &str) -> Result<(), String> {
        match self.mixes.iter().position(|m| m.name == name) {
            Some(i) => {
                self.active = i;
                Ok(())
            }
            None => Err(format!("There is no mix \"{}\".", name)),
        }
    }

    /// A neutral preset "Mix N" (N = smallest free number) at the end;
    /// it becomes the active one. Returns its name.
    pub fn new_mix(&mut self) -> String {
        let mut n = 1;
        while self.taken(&format!("Mix {}", n)) {
            n += 1;
        }
        let name = format!("Mix {}", n);
        self.mixes.push(RehearsalMix::neutral(&name));
        self.active = self.mixes.len() - 1;
        name
    }

    /// A copy of the active preset, "<Name> copy" (then "<Name> copy 2", ...),
    /// right behind it; it becomes the active one. Returns its name.
    pub fn copy_active(&mut self) -> String {
        let base = format!("{} copy", self.active_name());
        let mut name = base.clone();
        let mut n = 2;
        while self.taken(&name) {
            name = format!("{} {}", base, n);
            n += 1;
        }
        let mut copy = self.active().clone();
        copy.name = name.clone();
        self.mixes.insert(self.active + 1, copy);
        self.active += 1;
        name
    }

    /// Rename the active preset. Invalid or taken names change nothing.
    pub fn rename_active(&mut self, name: &str) -> Result<(), String> {
        let name = validate_name(name)?;
        if self.mixes.iter().enumerate().any(|(i, m)| i != self.active && m.name == name) {
            return Err(format!("There is already a mix \"{}\".", name));
        }
        self.mixes[self.active].name = name;
        Ok(())
    }

    /// Delete the active preset; the one before it (or the first) becomes
    /// active. The last preset cannot be deleted.
    pub fn delete_active(&mut self) -> Result<(), String> {
        if self.mixes.len() <= 1 {
            return Err("The last mix cannot be deleted.".to_string());
        }
        self.mixes.remove(self.active);
        self.active = self.active.saturating_sub(1);
        Ok(())
    }

    /// Back to neutral, the name stays.
    pub fn reset_active(&mut self) {
        let name = self.active_name().to_string();
        self.mixes[self.active] = RehearsalMix::neutral(&name);
    }

    /// The `mix.*` and `mix_active` lines of `lp16-tool.toml` (CRLF).
    pub fn to_config_text(&self) -> String {
        let mut t = String::new();
        for (i, m) in self.mixes.iter().enumerate() {
            let n = i + 1;
            t.push_str(&format!("mix.{}.name = \"{}\"\r\n", n, m.name));
            t.push_str(&format!("mix.{}.master = \"{}\"\r\n", n, fmt_db(m.master_db)));
            for (k, s) in m.strips.iter().enumerate() {
                t.push_str(&format!(
                    "mix.{}.ch{} = \"{} {} {} {}\"\r\n",
                    n,
                    k + 1,
                    fmt_db(s.level_db),
                    s.pan,
                    s.mute as u8,
                    s.solo as u8
                ));
            }
        }
        t.push_str(&format!("mix_active = \"{}\"\r\n", self.active_name()));
        t
    }
}

/// One decimal, or "off".
pub(crate) fn fmt_db(db: f32) -> String {
    if db == f32::NEG_INFINITY {
        "off".to_string()
    } else {
        format!("{:.1}", clamp_db(db))
    }
}

pub(crate) fn parse_db(s: &str) -> Option<f32> {
    if s == "off" {
        return Some(f32::NEG_INFINITY);
    }
    s.parse::<f32>().ok().filter(|v| v.is_finite()).map(clamp_db)
}

pub(crate) fn parse_flag(s: &str) -> Option<bool> {
    match s {
        "0" => Some(false),
        "1" => Some(true),
        _ => None,
    }
}

/// `"<level> <pan> <mute> <solo>"`; None if it cannot be read.
fn parse_strip(value: &str) -> Option<Strip> {
    let mut parts = value.split_whitespace();
    let level_db = parse_db(parts.next()?)?;
    let pan = parts.next()?.parse::<i32>().ok()?.clamp(-PAN_MAX, PAN_MAX);
    let mute = parse_flag(parts.next()?)?;
    let solo = parse_flag(parts.next()?)?;
    if parts.next().is_some() {
        return None;
    }
    Some(Strip { level_db, pan, mute, solo })
}

/// Collects the `mix.*` lines while the config is read.
#[derive(Default)]
pub struct MixParser {
    entries: BTreeMap<u32, (Option<String>, RehearsalMix)>,
    active: Option<String>,
}

impl MixParser {
    /// One `key = value` pair (value without the quotes). Keys that are not
    /// part of the mix and lines that cannot be read are ignored.
    pub fn feed(&mut self, key: &str, value: &str) {
        if key == "mix_active" {
            self.active = Some(value.to_string());
            return;
        }
        let Some(rest) = key.strip_prefix("mix.") else { return };
        let Some((n, field)) = rest.split_once('.') else { return };
        let Ok(n) = n.parse::<u32>() else { return };
        let entry = || (None, RehearsalMix::neutral(""));
        if field == "name" {
            if let Ok(name) = validate_name(value) {
                self.entries.entry(n).or_insert_with(entry).0 = Some(name);
            }
        } else if field == "master" {
            if let Some(db) = parse_db(value) {
                self.entries.entry(n).or_insert_with(entry).1.master_db = db;
            }
        } else if let Some(k) = field.strip_prefix("ch").and_then(|k| k.parse::<usize>().ok()) {
            if (1..=STRIPS).contains(&k) {
                if let Some(strip) = parse_strip(value) {
                    self.entries.entry(n).or_insert_with(entry).1.strips[k - 1] = strip;
                }
            }
        }
    }

    /// The presets in file order. Presets without a valid name or with a
    /// name that is already taken are dropped; no preset at all gives the
    /// neutral "Default". An unknown `mix_active` selects the first.
    pub fn finish(self) -> MixLibrary {
        let mut lib = MixLibrary { mixes: Vec::new(), active: 0 };
        for (_, (name, mut mix)) in self.entries {
            let Some(name) = name else { continue };
            if lib.taken(&name) {
                continue;
            }
            mix.name = name;
            lib.mixes.push(mix);
        }
        if lib.mixes.is_empty() {
            return MixLibrary::default();
        }
        if let Some(active) = self.active {
            let _ = lib.select(&active);
        }
        lib
    }
}

/// Decides when changed mix values are written to the config: at most once
/// per `SAVE_INTERVAL`, bundled.
#[derive(Default)]
pub struct MixSaver {
    dirty: bool,
    last: Option<Instant>,
}

impl MixSaver {
    /// Something changed.
    pub fn mark(&mut self) {
        self.dirty = true;
    }

    /// True once if it is time to save now (and counts as saved).
    pub fn poll(&mut self, now: Instant) -> bool {
        if self.wait(now) == Some(Duration::ZERO) {
            self.dirty = false;
            self.last = Some(now);
            return true;
        }
        false
    }

    /// How long until a pending change is saved; None if nothing is pending.
    pub fn wait(&self, now: Instant) -> Option<Duration> {
        if !self.dirty {
            return None;
        }
        Some(match self.last {
            Some(l) => SAVE_INTERVAL.saturating_sub(now.saturating_duration_since(l)),
            None => Duration::ZERO,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-5
    }

    /// a song with tracks on `channels` (no WAVs needed)
    fn song_on(channels: &[i32]) -> SongProject {
        let mut p = SongProject::parse("", "");
        p.tracks = channels.iter().map(|c| crate::ProjectTrack::new(*c, "x.wav", "")).collect();
        p
    }

    #[test]
    fn the_song_mix_is_read_per_output() {
        let mut p = song_on(&[0, 3, 3]);
        assert!(!has_song_mix(&p), "uTool2's defaults");
        assert_eq!(song_strips(&p), [SongStrip::default(); STRIPS]);
        p.tracks[0].monitor_level = "2.5".into();
        p.tracks[1].monitor_mute = "1".into();
        p.tracks[2].monitor_level = "-9.0".into(); // second track on output 4: the first one counts
        let s = song_strips(&p);
        assert_eq!(s[0], SongStrip { level_db: 2.5, mute: false });
        assert_eq!(s[3], SongStrip { level_db: 0.0, mute: true });
        assert_eq!(s[1], SongStrip::default());
        assert!(has_song_mix(&p));
        let g = song_gains(&p);
        assert!((g[0] - level_gain(2.5)).abs() < 1e-6);
        assert_eq!(g[3], 0.0);
        assert_eq!(g[1], 1.0);
        // odd values: empty, unreadable, out of range
        p.tracks[0].monitor_level = "".into();
        assert_eq!(song_strips(&p)[0].level_db, 0.0);
        p.tracks[0].monitor_level = "99".into();
        assert_eq!(song_strips(&p)[0].level_db, LEVEL_MAX_DB);
        p.tracks[1].monitor_mute = "true".into();
        assert!(song_strips(&p)[3].mute);
    }

    #[test]
    fn a_pan_alone_counts_as_a_song_mix() {
        let mut p = song_on(&[0]);
        p.tracks[0].monitor_pan = "-20.0".into();
        assert!(has_song_mix(&p));
        assert_eq!(song_gains(&p), [1.0; STRIPS], "but the rehearsal does not use it");
    }

    #[test]
    fn setting_the_song_mix_writes_all_tracks_of_the_output() {
        let mut p = song_on(&[0, 2, 2]);
        p.tracks[1].monitor_pan = "30.0".into();
        assert!(set_song_strip(&mut p, 2, SongStrip { level_db: -1.25, mute: true }));
        for t in &p.tracks[1..] {
            assert_eq!(t.monitor_mute, "1");
            assert!(t.monitor_level == "-1.2" || t.monitor_level == "-1.3", "{}", t.monitor_level);
        }
        assert_eq!(p.tracks[1].monitor_pan, "30.0", "the pan stays");
        assert_eq!(p.tracks[0].monitor_level, "0.0");
        // the same values again: nothing changes
        let s = song_strips(&p)[2];
        assert!(!set_song_strip(&mut p, 2, s));
        // off is not a song level: it becomes the lowest level
        set_song_strip(&mut p, 0, SongStrip { level_db: f32::NEG_INFINITY, mute: false });
        assert_eq!(p.tracks[0].monitor_level, "-60.0");
        // back to neutral
        set_song_strip(&mut p, 0, SongStrip::default());
        set_song_strip(&mut p, 2, SongStrip::default());
        assert_eq!(p.tracks[2].monitor_level, "0.0");
        assert_eq!(p.tracks[2].monitor_mute, "0");
        // an output without a track: nothing to write
        assert!(!set_song_strip(&mut p, 7, SongStrip { level_db: 3.0, mute: false }));
    }

    #[test]
    fn typed_levels() {
        assert_eq!(parse_level("-0.5"), Some(-0.5));
        assert_eq!(parse_level(" -0,5 "), Some(-0.5));
        assert_eq!(parse_level("+3"), Some(3.0));
        assert_eq!(parse_level("0"), Some(0.0));
        assert_eq!(parse_level("-12 dB"), Some(-12.0));
        assert_eq!(parse_level("-7.04"), Some(-7.0));
        // limited to the range
        assert_eq!(parse_level("20"), Some(LEVEL_MAX_DB));
        assert_eq!(parse_level("-100"), Some(LEVEL_MIN_DB));
        for off in ["-inf", "inf", "OFF", "-\u{221e}"] {
            assert_eq!(parse_level(off), Some(f32::NEG_INFINITY), "{}", off);
        }
        for bad in ["", "abc", "1.2.3", "NaN", "--1"] {
            assert_eq!(parse_level(bad), None, "{}", bad);
        }
    }

    #[test]
    fn level_gain_follows_db() {
        assert!(close(level_gain(-6.0), 0.501187));
        assert!(close(level_gain(6.0), 1.995262));
        assert_eq!(level_gain(0.0), 1.0);
        assert_eq!(level_gain(f32::NEG_INFINITY), 0.0);
    }

    #[test]
    fn pan_is_constant_power() {
        assert_eq!(pan_gains(-127), (1.0, 0.0));
        assert_eq!(pan_gains(127), (0.0, 1.0));
        assert_eq!(pan_gains(0), (CENTER_GAIN, CENTER_GAIN));
        let (l, r) = pan_gains(40);
        assert!(close(l * l + r * r, 1.0));
        assert_eq!(pan_gains(500), pan_gains(127), "limited");
    }

    #[test]
    fn mute_solo_and_master() {
        let mut m = RehearsalMix::neutral("t");
        let (g, master) = m.params().gains();
        assert!(g.iter().all(|g| g.level == 1.0 && g.left == CENTER_GAIN));
        assert_eq!(master, 1.0);
        m.set_mute(1, true);
        assert_eq!(m.params().gains().0[1], StripGains::default());
        assert_eq!(m.params().gains().0[0].level, 1.0);
        // solo silences the others
        m.set_solo(2, true);
        let (g, _) = m.params().gains();
        assert_eq!(g[2].level, 1.0);
        assert_eq!(g[0].level, 0.0);
        // solo on two strips
        m.set_solo(3, true);
        let (g, _) = m.params().gains();
        assert!(g[2].level == 1.0 && g[3].level == 1.0 && g[4].level == 0.0);
        // mute beats solo
        m.set_mute(3, true);
        let (g, _) = m.params().gains();
        assert_eq!(g[3].level, 0.0);
        assert_eq!(g[2].level, 1.0);
        m.set_master(-6.0);
        assert!(close(m.params().gains().1, 0.501187));
    }

    #[test]
    fn values_are_limited() {
        let mut m = RehearsalMix::neutral("t");
        m.set_level(0, 40.0);
        m.set_level(1, -100.0);
        m.set_level(2, f32::NEG_INFINITY);
        m.set_level(3, f32::NAN);
        m.set_pan(0, 900);
        m.set_pan(1, -900);
        m.set_master(99.0);
        m.set_level(99, 1.0); // ignored
        assert_eq!(m.strips[0].level_db, 6.0);
        assert_eq!(m.strips[1].level_db, -60.0);
        assert_eq!(m.strips[2].level_db, f32::NEG_INFINITY);
        assert_eq!(m.strips[3].level_db, 0.0);
        assert_eq!((m.strips[0].pan, m.strips[1].pan), (127, -127));
        assert_eq!(m.master_db, 6.0);
    }

    #[test]
    fn preset_operations() {
        let mut lib = MixLibrary::default();
        assert_eq!(lib.active_name(), "Default");
        assert_eq!(lib.new_mix(), "Mix 1");
        assert_eq!(lib.new_mix(), "Mix 2");
        lib.select("Mix 1").unwrap();
        lib.delete_active().unwrap();
        assert_eq!(lib.new_mix(), "Mix 1", "the smallest free number");
        // copy
        lib.select("Default").unwrap();
        lib.active_mut().set_level(4, -12.0);
        assert_eq!(lib.copy_active(), "Default copy");
        assert_eq!(lib.active_name(), "Default copy");
        assert_eq!(lib.active().strips[4].level_db, -12.0);
        lib.select("Default").unwrap();
        assert_eq!(lib.copy_active(), "Default copy 2");
        // rename
        assert!(lib.rename_active("").is_err());
        assert!(lib.rename_active("  ").is_err());
        assert!(lib.rename_active("a\"b").is_err());
        assert!(lib.rename_active("a\nb").is_err());
        assert!(lib.rename_active("Mix 1").is_err(), "taken");
        assert_eq!(lib.active_name(), "Default copy 2", "nothing changed");
        lib.rename_active(" B\u{fc}hne laut ").unwrap();
        assert_eq!(lib.active_name(), "B\u{fc}hne laut");
        lib.rename_active("B\u{fc}hne laut").unwrap(); // its own name is fine
        // reset
        lib.active_mut().set_level(0, -3.0);
        lib.reset_active();
        assert!(lib.active().is_neutral());
        assert_eq!(lib.active_name(), "B\u{fc}hne laut");
        // select
        assert!(lib.select("nope").is_err());
        assert_eq!(lib.active_name(), "B\u{fc}hne laut");
        // delete down to one
        while lib.presets().len() > 1 {
            lib.delete_active().unwrap();
        }
        assert!(lib.delete_active().is_err());
        assert_eq!(lib.presets().len(), 1);
    }

    fn sample() -> MixLibrary {
        let mut lib = MixLibrary::default();
        lib.rename_active("Probe S\u{fc}d").unwrap();
        lib.active_mut().set_master(-3.5);
        lib.active_mut().set_level(4, f32::NEG_INFINITY);
        lib.active_mut().set_mute(4, true);
        lib.active_mut().set_level(0, -12.3);
        lib.active_mut().set_pan(0, -64);
        lib.new_mix();
        lib.active_mut().set_solo(15, true);
        lib.active_mut().set_level(15, 6.0);
        lib.new_mix();
        lib.rename_active("Mix mit Leerzeichen").unwrap();
        lib.select("Mix 1").unwrap();
        lib
    }

    fn parse(text: &str) -> MixLibrary {
        let mut p = MixParser::default();
        for line in text.lines() {
            if let Some((k, v)) = line.split_once('=') {
                p.feed(k.trim(), v.trim().trim_matches('"'));
            }
        }
        p.finish()
    }

    #[test]
    fn config_text_roundtrip() {
        let lib = sample();
        let text = lib.to_config_text();
        assert!(text.contains("mix.1.ch5 = \"off 0 1 0\"\r\n"), "{}", text);
        assert!(text.contains("mix_active = \"Mix 1\""));
        assert_eq!(parse(&text), lib);
    }

    #[test]
    fn config_without_presets_gives_default() {
        assert_eq!(parse("stick_root = \"E:\\\\\"\r\n"), MixLibrary::default());
        assert_eq!(parse("mix_active = \"x\""), MixLibrary::default());
    }

    #[test]
    fn broken_config_lines_are_ignored() {
        let text = "mix.1.name = \"A\"\n\
                    mix.1.master = \"banana\"\n\
                    mix.1.ch1 = \"-6.0 10 0 0\"\n\
                    mix.1.ch2 = \"-6.0 10\"\n\
                    mix.1.ch3 = \"x 0 0 0\"\n\
                    mix.1.ch4 = \"0.0 0 2 0\"\n\
                    mix.1.ch17 = \"0.0 0 1 0\"\n\
                    mix.1.ch0 = \"0.0 0 1 0\"\n\
                    mix.x.name = \"B\"\n\
                    mix.2.master = \"-3.0\"\n\
                    mix.3.name = \"A\"\n\
                    mix.4.name = \"C\"\n\
                    mix.4.ch1 = \"99.0 500 1 1\"\n\
                    mix_active = \"unknown\"\n";
        let lib = parse(text);
        let names: Vec<_> = lib.presets().iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, ["A", "C"], "no name and duplicate names are dropped");
        let a = &lib.presets()[0];
        assert_eq!(a.master_db, 0.0);
        assert_eq!(a.strips[0], Strip { level_db: -6.0, pan: 10, mute: false, solo: false });
        assert_eq!(a.strips[1], Strip::default());
        assert_eq!(a.strips[2], Strip::default());
        assert_eq!(a.strips[3], Strip::default());
        assert_eq!(lib.presets()[1].strips[0], Strip { level_db: 6.0, pan: 127, mute: true, solo: true });
        assert_eq!(lib.active_name(), "A", "unknown active: the first");
    }

    #[test]
    fn missing_channels_are_neutral() {
        let lib = parse("mix.1.name = \"Only\"\nmix.1.ch2 = \"off 5 0 1\"\n");
        let m = lib.active();
        assert_eq!(m.strips[0], Strip::default());
        assert_eq!(m.strips[1], Strip { level_db: f32::NEG_INFINITY, pan: 5, mute: false, solo: true });
    }

    #[test]
    fn saver_saves_at_most_once_a_second() {
        let t0 = Instant::now();
        let mut s = MixSaver::default();
        assert!(!s.poll(t0));
        assert_eq!(s.wait(t0), None);
        s.mark();
        assert!(s.poll(t0), "the first change goes out at once");
        assert!(!s.poll(t0), "saved");
        s.mark();
        s.mark();
        assert!(!s.poll(t0 + Duration::from_millis(400)));
        assert_eq!(s.wait(t0 + Duration::from_millis(400)), Some(Duration::from_millis(600)));
        assert!(s.poll(t0 + Duration::from_millis(1000)), "bundled");
        assert!(!s.poll(t0 + Duration::from_secs(5)));
    }
}
